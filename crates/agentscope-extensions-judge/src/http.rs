//! Injectable HTTP seam shared by both judge clients.
//!
//! Java wires the wire protocol through two different injectable layers: Jev uses
//! `io.agentscope.core.model.transport.HttpTransport` (`JevClient.Builders.transport(...)`)
//! while the Decisions client borrows an official OpenAI SDK client
//! (`OpenAIDecisionsClient.Builder.client(...)`). The port keeps the same seam, narrowed to
//! what a judge call needs:
//!
//! * [`JudgeHttpRequest`] / [`JudgeHttpResponse`] — one JSON `POST` exchange.
//! * [`JudgeHttpTransport`] — the injectable trait; [`ReqwestJudgeTransport`] is the
//!   production implementation, unit tests inject [`testing::ScriptedTransport`].
//! * [`backoff_ms`] — the exponential backoff schedule both retry policies share
//!   (`initialBackoff * 2^n`, capped at `initialBackoff * 8`, exactly like Java's
//!   `Retry.backoff(...).maxBackoff(initialBackoff * 8)`).
//!
//! Every judge endpoint is a JSON `POST`, so a request carries no method.

use std::time::Duration;

use async_trait::async_trait;

/// `Content-Type` used by both judge endpoints.
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// Multiplier capping the exponential backoff (Java `maxBackoff(initialBackoff * 8)`).
pub const MAX_BACKOFF_MULTIPLIER: u64 = 8;

/// Connect timeout used by [`ReqwestJudgeTransport`]; the per-call timeout is enforced by the
/// clients (`tokio::time::timeout`), mirroring Java's reactor `.timeout(...)`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One JSON `POST` issued by a judge client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeHttpRequest {
    /// Absolute target URL.
    pub url: String,
    /// Request headers, in insertion order.
    pub headers: Vec<(String, String)>,
    /// Serialized JSON body.
    pub body: String,
}

impl JudgeHttpRequest {
    /// Creates a request with `Content-Type: application/json`.
    pub fn post_json(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: vec![("Content-Type".to_string(), JSON_CONTENT_TYPE.to_string())],
            body: body.into(),
        }
    }

    /// Appends a header, replacing any previous value with the same (case-insensitive) name.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        self.headers
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
        self.headers.push((name, value.into()));
        self
    }

    /// Returns a header value by name (case-insensitive, first match).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One HTTP response as the judge clients need it (status plus raw body).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JudgeHttpResponse {
    /// HTTP status code.
    pub status_code: u16,
    /// Raw response body.
    pub body: String,
}

impl JudgeHttpResponse {
    /// Creates a response.
    pub fn new(status_code: u16, body: impl Into<String>) -> Self {
        Self {
            status_code,
            body: body.into(),
        }
    }

    /// Whether the status is in the 2xx range.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status_code)
    }
}

/// A failed exchange: no response arrived, or the transport itself rejected it.
///
/// Mirrors Java `HttpTransportException`, which already carries `statusCode`,
/// `responseBody` and `retryable`; the Jev client forwards all three onto its own error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeTransportError {
    /// Human readable message.
    pub message: String,
    /// The HTTP status when a response did arrive.
    pub status_code: Option<u16>,
    /// The raw body when a response did arrive.
    pub response_body: Option<String>,
    /// Whether the caller's retry policy may retry this failure.
    pub retryable: bool,
}

impl JudgeTransportError {
    /// Creates a transport error.
    pub fn new(
        message: impl Into<String>,
        status_code: Option<u16>,
        response_body: Option<String>,
        retryable: bool,
    ) -> Self {
        Self {
            message: message.into(),
            status_code,
            response_body,
            retryable,
        }
    }

    /// A failure before any response arrived.
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(message, None, None, false)
    }
}

/// Sends a [`JudgeHttpRequest`] and returns a [`JudgeHttpResponse`].
///
/// Implementations must not interpret the status code: each client owns its retryability rules
/// (Jev retries `408/409/429/529/5xx`, Decisions retries `429/5xx`) and its error taxonomy.
#[async_trait]
pub trait JudgeHttpTransport: Send + Sync {
    /// Performs one exchange. Non-2xx responses are returned as `Ok`, not as an error.
    async fn execute(
        &self,
        request: JudgeHttpRequest,
    ) -> Result<JudgeHttpResponse, JudgeTransportError>;
}

/// `reqwest`-backed production transport.
///
/// Deliberately does **no** retrying: the retry budget belongs to the client that knows
/// whether a status is retryable (Java likewise retries outside the transport).
#[derive(Debug, Clone)]
pub struct ReqwestJudgeTransport {
    client: reqwest::Client,
}

impl ReqwestJudgeTransport {
    /// Builds a transport with [`DEFAULT_CONNECT_TIMEOUT`].
    pub fn new() -> Result<Self, JudgeTransportError> {
        Self::with_connect_timeout(DEFAULT_CONNECT_TIMEOUT)
    }

    /// Builds a transport with an explicit connect timeout.
    pub fn with_connect_timeout(timeout: Duration) -> Result<Self, JudgeTransportError> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .build()
            .map_err(|error| {
                JudgeTransportError::new(
                    format!("failed to build the HTTP client: {error}"),
                    None,
                    None,
                    false,
                )
            })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl JudgeHttpTransport for ReqwestJudgeTransport {
    async fn execute(
        &self,
        request: JudgeHttpRequest,
    ) -> Result<JudgeHttpResponse, JudgeTransportError> {
        let JudgeHttpRequest { url, headers, body } = request;
        let mut builder = self.client.post(&url);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let sent = builder.body(body).send().await.map_err(|error| {
            JudgeTransportError::new(
                format!("{url}: {error}"),
                None,
                None,
                error.is_timeout() || error.is_connect(),
            )
        })?;
        let status_code = sent.status().as_u16();
        let body = sent.text().await.map_err(|error| {
            JudgeTransportError::new(
                format!("{url}: {error}"),
                Some(status_code),
                None,
                error.is_timeout() || error.is_connect(),
            )
        })?;
        Ok(JudgeHttpResponse::new(status_code, body))
    }
}

/// Delay before retry number `retry_index` (`0` = the first retry).
///
/// Doubles per retry and saturates at [`MAX_BACKOFF_MULTIPLIER`] times the initial backoff,
/// which is the Java schedule (`initialBackoff`, `2x`, `4x`, then `8x` for every later retry).
/// Java additionally applies `jitter(0.2)`; the port keeps the schedule deterministic so a
/// retry test observes exact attempts (see the tracker's execution record).
#[must_use]
pub fn backoff_ms(initial_backoff_ms: u64, retry_index: u32) -> u64 {
    let cap = initial_backoff_ms.saturating_mul(MAX_BACKOFF_MULTIPLIER);
    let multiplier = 1u64.checked_shl(retry_index).unwrap_or(u64::MAX);
    initial_backoff_ms.saturating_mul(multiplier).min(cap)
}

/// Offline transport doubles used by the client unit tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::{JudgeHttpRequest, JudgeHttpResponse, JudgeHttpTransport, JudgeTransportError};

    /// Replays a scripted list of results and records every request it received.
    pub(crate) struct ScriptedTransport {
        scripted: Mutex<VecDeque<Result<JudgeHttpResponse, JudgeTransportError>>>,
        repeat_last: bool,
        requests: Mutex<Vec<JudgeHttpRequest>>,
    }

    impl ScriptedTransport {
        /// Replays `scripted` in order, then fails with a non-retryable exhaustion error.
        pub(crate) fn new(scripted: Vec<Result<JudgeHttpResponse, JudgeTransportError>>) -> Self {
            Self {
                scripted: Mutex::new(scripted.into()),
                repeat_last: false,
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Always answers with the same result.
        pub(crate) fn repeating(result: Result<JudgeHttpResponse, JudgeTransportError>) -> Self {
            Self {
                scripted: Mutex::new(VecDeque::from(vec![result])),
                repeat_last: true,
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Every request the client sent, in order.
        pub(crate) fn requests(&self) -> Vec<JudgeHttpRequest> {
            self.requests.lock().expect("requests lock").clone()
        }

        /// The last request the client sent.
        pub(crate) fn last_request(&self) -> JudgeHttpRequest {
            self.requests()
                .pop()
                .expect("the client did not send a request")
        }

        /// How many times the client called the transport.
        pub(crate) fn call_count(&self) -> usize {
            self.requests.lock().expect("requests lock").len()
        }
    }

    #[async_trait]
    impl JudgeHttpTransport for ScriptedTransport {
        async fn execute(
            &self,
            request: JudgeHttpRequest,
        ) -> Result<JudgeHttpResponse, JudgeTransportError> {
            self.requests.lock().expect("requests lock").push(request);
            let mut scripted = self.scripted.lock().expect("script lock");
            if self.repeat_last {
                return scripted
                    .front()
                    .expect("repeating transport without a result")
                    .clone();
            }
            match scripted.pop_front() {
                Some(result) => result,
                None => Err(JudgeTransportError::new(
                    "scripted transport exhausted",
                    None,
                    None,
                    false,
                )),
            }
        }
    }

    /// A transport that never answers, used to exercise the client timeout.
    pub(crate) struct HangingTransport;

    #[async_trait]
    impl JudgeHttpTransport for HangingTransport {
        async fn execute(
            &self,
            _request: JudgeHttpRequest,
        ) -> Result<JudgeHttpResponse, JudgeTransportError> {
            tokio::time::sleep(Duration::from_secs(3_600)).await;
            Err(JudgeTransportError::transport("unreachable"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_helpers_set_headers_and_body() {
        let request = JudgeHttpRequest::post_json("http://localhost/v1/systemone", "{}")
            .with_header("Authorization", "Bearer k");
        assert_eq!(request.url, "http://localhost/v1/systemone");
        assert_eq!(request.body, "{}");
        assert_eq!(request.header("content-type"), Some(JSON_CONTENT_TYPE));
        assert_eq!(request.header("AUTHORIZATION"), Some("Bearer k"));
        assert_eq!(request.header("Accept"), None);
    }

    #[test]
    fn header_replacement_keeps_a_single_entry() {
        let request = JudgeHttpRequest::post_json("http://x", "{}")
            .with_header("X-A", "1")
            .with_header("x-a", "2");
        assert_eq!(request.headers.len(), 2, "content-type plus x-a");
        assert_eq!(request.header("X-A"), Some("2"));
    }

    #[test]
    fn response_success_range_is_2xx() {
        assert!(JudgeHttpResponse::new(200, "").is_success());
        assert!(JudgeHttpResponse::new(299, "").is_success());
        assert!(!JudgeHttpResponse::new(300, "").is_success());
        assert!(!JudgeHttpResponse::new(199, "").is_success());
    }

    #[test]
    fn transport_errors_keep_http_context() {
        let error = JudgeTransportError::new("boom", Some(503), Some("body".into()), true);
        assert_eq!(error.message, "boom");
        assert_eq!(error.status_code, Some(503));
        assert_eq!(error.response_body.as_deref(), Some("body"));
        assert!(error.retryable);

        let bare = JudgeTransportError::transport("io");
        assert_eq!(bare.status_code, None);
        assert!(!bare.retryable);
    }

    #[test]
    fn backoff_doubles_then_caps_at_eight_times_initial() {
        assert_eq!(backoff_ms(500, 0), 500);
        assert_eq!(backoff_ms(500, 1), 1_000);
        assert_eq!(backoff_ms(500, 2), 2_000);
        assert_eq!(backoff_ms(500, 3), 4_000);
        assert_eq!(backoff_ms(500, 4), 4_000);
        assert_eq!(backoff_ms(500, 99), 4_000);
    }

    #[test]
    fn backoff_saturates_instead_of_overflowing() {
        assert_eq!(backoff_ms(u64::MAX, 0), u64::MAX);
        assert_eq!(backoff_ms(u64::MAX, 5), u64::MAX);
        assert_eq!(backoff_ms(1, 64), 8);
    }

    #[tokio::test]
    async fn scripted_transport_replays_then_exhausts() {
        let transport = testing::ScriptedTransport::new(vec![Ok(JudgeHttpResponse::new(200, "a"))]);
        let request = JudgeHttpRequest::post_json("http://x", "{}");
        let first = transport.execute(request.clone()).await.unwrap();
        assert_eq!(first.body, "a");
        let second = transport.execute(request).await.unwrap_err();
        assert_eq!(second.message, "scripted transport exhausted");
        assert!(!second.retryable);
        assert_eq!(transport.call_count(), 2);
        assert_eq!(transport.last_request().url, "http://x");
    }

    #[tokio::test]
    async fn scripted_transport_repeats_its_last_result() {
        let transport = testing::ScriptedTransport::repeating(Err(JudgeTransportError::new(
            "down", None, None, true,
        )));
        for _ in 0..3 {
            let error = transport
                .execute(JudgeHttpRequest::post_json("http://x", "{}"))
                .await
                .unwrap_err();
            assert_eq!(error.message, "down");
        }
        assert_eq!(transport.call_count(), 3);
    }
}
