//! Injectable HTTP seam for Studio's REST API.
//!
//! Java's `StudioClient` builds an [`OkHttpClient`] inline, so the network path can only be
//! exercised against a real server (`StudioClientTest` uses a `WebServer` or an `Interceptor`).
//! The port keeps the same request shape but moves the wire call behind
//! [`StudioHttpTransport`], the same seam used by `agentscope-extensions-judge`
//! (`JudgeHttpTransport`):
//!
//! * [`StudioHttpRequest`] / [`StudioHttpResponse`] — one JSON exchange (method, URL, headers,
//!   body / status, body).
//! * [`StudioHttpTransport`] — the injectable trait. [`ReqwestStudioTransport`] is the
//!   production implementation; unit tests inject a scripted transport.
//!
//! Contract (mirrors Java's `executeWithRetry`):
//!
//! * a non-2xx response is returned as `Ok`, **not** as an error — the client owns the status
//!   interpretation and retry policy;
//! * a network/IO failure is `Err(StudioError::Transport(_))`, the only retryable variant;
//! * implementations must not retry by themselves.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::StudioResult;

/// `Content-Type` Java sends for every Studio call
/// (`StudioClient.JSON_MEDIA_TYPE`, `application/json; charset=utf-8`).
pub const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

/// The only HTTP method Studio's REST API uses (`registerRun` / `pushMessage` /
/// `requestUserInput` are all `POST`).
pub const HTTP_POST: &str = "POST";

/// Connect timeout used by [`ReqwestStudioTransport`] (Java `connectTimeout(10s)`).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-request timeout used by [`ReqwestStudioTransport`] (Java `readTimeout`/`writeTimeout`).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One HTTP exchange issued by [`StudioClient`](super::StudioClient).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct StudioHttpRequest {
    /// HTTP method (`POST` for every Studio endpoint).
    pub method: String,
    /// Absolute target URL (`{studioUrl}/trpc/<endpoint>`).
    pub url: String,
    /// Request headers in insertion order. Java adds no auth header; only `Content-Type`.
    pub headers: Vec<(String, String)>,
    /// Serialized JSON body.
    pub body: String,
}

impl StudioHttpRequest {
    /// Creates a `POST` request with `Content-Type: application/json; charset=utf-8`.
    pub fn post_json(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: HTTP_POST.to_string(),
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

/// One HTTP response as [`StudioClient`](super::StudioClient) needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct StudioHttpResponse {
    /// HTTP status code.
    pub status_code: u16,
    /// Raw response body (ignored by every Studio endpoint, kept for diagnostics).
    pub body: String,
}

impl StudioHttpResponse {
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

/// Sends a [`StudioHttpRequest`] and returns a [`StudioHttpResponse`].
///
/// Implementations must not interpret the status code and must not retry; see the module docs.
#[async_trait]
pub trait StudioHttpTransport: Send + Sync {
    /// Performs one exchange. Non-2xx responses are returned as `Ok`.
    async fn execute(&self, request: StudioHttpRequest) -> StudioResult<StudioHttpResponse>;
}

/// `reqwest`-backed production transport (behaviour matches the previous inline client:
/// 10s connect timeout, 30s request timeout, body read failures tolerated).
#[derive(Debug, Clone)]
pub struct ReqwestStudioTransport {
    client: reqwest::Client,
}

impl Default for ReqwestStudioTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestStudioTransport {
    /// Builds a transport with [`DEFAULT_CONNECT_TIMEOUT`] / [`DEFAULT_REQUEST_TIMEOUT`].
    pub fn new() -> Self {
        Self::with_timeouts(DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT)
    }

    /// Builds a transport with explicit connect / request timeouts.
    ///
    /// Falls back to a default client if the builder rejects the configuration, matching the
    /// previous `unwrap_or_else(|_| reqwest::Client::new())` behaviour.
    pub fn with_timeouts(connect_timeout: Duration, request_timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }

    /// Wraps a caller-provided `reqwest::Client` (lets a host share one connection pool).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl StudioHttpTransport for ReqwestStudioTransport {
    async fn execute(&self, request: StudioHttpRequest) -> StudioResult<StudioHttpResponse> {
        use reqwest::header::{HeaderName, HeaderValue};

        let StudioHttpRequest {
            method,
            url,
            headers,
            body,
        } = request;
        let method = method.parse::<reqwest::Method>().map_err(|e| {
            crate::error::StudioError::Transport(format!("{url}: invalid method: {e}"))
        })?;
        let mut builder = self.client.request(method, &url);
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                crate::error::StudioError::Transport(format!("{url}: invalid header name: {e}"))
            })?;
            let value = HeaderValue::from_str(&value).map_err(|e| {
                crate::error::StudioError::Transport(format!("{url}: invalid header value: {e}"))
            })?;
            builder = builder.header(name, value);
        }

        let response = builder
            .body(body)
            .send()
            .await
            .map_err(|e| crate::error::StudioError::Transport(format!("{url}: {e}")))?;
        let status_code = response.status().as_u16();
        // The body is diagnostic only; a read failure must not turn a 2xx into an error (same as
        // the previous `unwrap_or_default()` handling).
        let body = response.text().await.unwrap_or_default();
        Ok(StudioHttpResponse::new(status_code, body))
    }
}

/// Offline transport doubles used by the client / user-agent unit tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::{StudioHttpRequest, StudioHttpResponse, StudioHttpTransport};
    use crate::error::{StudioError, StudioResult};

    /// One scripted outcome.
    #[derive(Debug, Clone)]
    pub(crate) enum Outcome {
        /// A response was received (any status, including non-2xx).
        Status { status_code: u16, body: String },
        /// The wire call failed before any response arrived (retryable).
        Transport(String),
    }

    impl Outcome {
        pub(crate) fn ok(status_code: u16, body: &str) -> Self {
            Self::Status {
                status_code,
                body: body.to_string(),
            }
        }

        pub(crate) fn transport(message: &str) -> Self {
            Self::Transport(message.to_string())
        }

        fn into_result(self) -> StudioResult<StudioHttpResponse> {
            match self {
                Self::Status { status_code, body } => {
                    Ok(StudioHttpResponse::new(status_code, body))
                }
                Self::Transport(message) => Err(StudioError::Transport(message)),
            }
        }
    }

    /// A transport that replays a script and records every request it received.
    #[derive(Debug)]
    pub(crate) struct ScriptedTransport {
        queue: Mutex<VecDeque<Outcome>>,
        fallback: Option<Outcome>,
        requests: Mutex<Vec<StudioHttpRequest>>,
    }

    impl ScriptedTransport {
        /// Replays `script` in order, then repeats the last entry forever.
        pub(crate) fn new(script: Vec<Outcome>) -> Self {
            Self {
                fallback: script.last().cloned(),
                queue: Mutex::new(script.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Always answers the same way.
        pub(crate) fn always(outcome: Outcome) -> Self {
            Self {
                queue: Mutex::new(VecDeque::new()),
                fallback: Some(outcome),
                requests: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn requests(&self) -> Vec<StudioHttpRequest> {
            self.requests.lock().expect("requests lock").clone()
        }

        pub(crate) fn last_request(&self) -> StudioHttpRequest {
            self.requests
                .lock()
                .expect("requests lock")
                .last()
                .cloned()
                .expect("at least one request was sent")
        }

        pub(crate) fn attempt_count(&self) -> usize {
            self.requests.lock().expect("requests lock").len()
        }
    }

    #[async_trait]
    impl StudioHttpTransport for ScriptedTransport {
        async fn execute(&self, request: StudioHttpRequest) -> StudioResult<StudioHttpResponse> {
            self.requests.lock().expect("requests lock").push(request);
            let next = self.queue.lock().expect("queue lock").pop_front();
            match next.or_else(|| self.fallback.clone()) {
                Some(outcome) => outcome.into_result(),
                None => Err(StudioError::Transport(
                    "scripted transport exhausted".to_string(),
                )),
            }
        }
    }
}
