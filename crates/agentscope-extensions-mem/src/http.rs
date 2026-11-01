//! HTTP seam for the MEM vendor clients.
//!
//! Java talks to the vendors through `HttpTransport` / OkHttp. In Rust the clients
//! depend on the [`HttpTransport`] trait so that assembly can inject either the real
//! [`ReqwestTransport`] or an offline mock (see [`mock`]).

use std::time::Duration;

use async_trait::async_trait;

use crate::error::{MemError, Result};

/// Minimal JSON-over-HTTP POST transport.
///
/// Implementations must return [`MemError::HttpStatus`] for non-2xx responses and
/// the raw response body (UTF-8) on success.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// POST `body` (already serialized JSON) to `url` with the given headers and
    /// return the raw response body.
    async fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<String>;
}

/// Connection-establishment timeout, mirroring Java's `connectTimeout(30s)`
/// (`Mem0Client` / `BailianMemoryClient` / `ReMeClient` all hard-code 30s).
///
/// Java additionally sets `writeTimeout(30s)`; `reqwest` exposes no separate
/// write timeout, so the per-request [`ReqwestTransport::new`] `timeout` covers
/// the whole request including the body write. This is a documented limitation,
/// not a behavioural gap that can be closed with `reqwest`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// `reqwest`-backed [`HttpTransport`] used in production.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Builds a transport with the given per-request (read) timeout and the Java
    /// default 30s connect timeout.
    ///
    /// The per-request timeout is Java's `readTimeout(timeout)` (default 60s);
    /// [`DEFAULT_CONNECT_TIMEOUT`] is Java's `connectTimeout(30s)`.
    pub fn new(timeout: Duration) -> Result<Self> {
        agentscope_core::tls::ensure_crypto_provider();
        let client = reqwest::Client::builder()
            .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
            .timeout(timeout)
            .build()
            .map_err(|e| MemError::Transport(e.to_string()))?;
        Ok(Self { client })
    }

    /// Wraps an existing [`reqwest::Client`].
    pub fn from_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<String> {
        let mut request = self.client.post(url).body(body.to_string());
        for (key, value) in headers {
            request = request.header(key.as_str(), value.as_str());
        }
        let response = request
            .send()
            .await
            .map_err(|e| MemError::Transport(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| MemError::Transport(e.to_string()))?;
        if !status.is_success() {
            // Vendor / operation labels are attached by the client that owns the call
            // (`MemError::with_context`); this seam is deliberately agnostic.
            return Err(MemError::http_status(status.as_u16(), text));
        }
        Ok(text)
    }
}

/// Offline test double recording every request and replaying canned responses.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A captured request `(url, headers, body)`.
    pub(crate) type CapturedRequest = (String, Vec<(String, String)>, String);

    #[derive(Default)]
    struct Inner {
        requests: Vec<CapturedRequest>,
        responses: VecDeque<Result<String>>,
    }

    /// In-memory [`HttpTransport`] for deterministic, offline tests.
    #[derive(Clone, Default)]
    pub(crate) struct MockTransport {
        inner: Arc<Mutex<Inner>>,
    }

    impl MockTransport {
        /// Creates an empty mock (no queued responses → returns a transport error).
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Queues a response to be returned by the next `post_json` call.
        pub(crate) fn push_response(&self, response: Result<String>) {
            self.inner.lock().unwrap().responses.push_back(response);
        }

        /// Queues a successful JSON body.
        pub(crate) fn push_body(&self, body: impl Into<String>) {
            self.push_response(Ok(body.into()));
        }

        /// Returns every captured request in arrival order.
        pub(crate) fn requests(&self) -> Vec<CapturedRequest> {
            self.inner.lock().unwrap().requests.clone()
        }

        /// Returns the body of the last captured request, if any.
        pub(crate) fn last_body(&self) -> Option<String> {
            self.inner
                .lock()
                .unwrap()
                .requests
                .last()
                .map(|(_, _, body)| body.clone())
        }
    }

    #[async_trait]
    impl HttpTransport for MockTransport {
        async fn post_json(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &str,
        ) -> Result<String> {
            let mut inner = self.inner.lock().unwrap();
            inner
                .requests
                .push((url.to_string(), headers.to_vec(), body.to_string()));
            inner
                .responses
                .pop_front()
                .unwrap_or_else(|| Err(MemError::Transport("no mock response queued".into())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockTransport;
    use super::*;

    #[tokio::test]
    async fn mock_transport_captures_and_replays() {
        let mock = MockTransport::new();
        mock.push_body("{\"ok\":true}");
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let body = mock.post_json("http://x/y", &headers, "{}").await.unwrap();
        assert_eq!(body, "{\"ok\":true}");
        let captured = mock.requests();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, "http://x/y");
        assert_eq!(captured[0].2, "{}");
        assert_eq!(mock.last_body().as_deref(), Some("{}"));
    }

    #[test]
    fn reqwest_transport_builds() {
        let transport = ReqwestTransport::new(Duration::from_secs(5));
        assert!(transport.is_ok());
    }

    #[test]
    fn connect_timeout_matches_java_thirty_seconds() {
        // Java: connectTimeout(Duration.ofSeconds(30)) in Mem0Client / BailianMemoryClient /
        // ReMeClient. reqwest has no separate write timeout, so the per-request timeout
        // (Java readTimeout) stays the load-bearing knob.
        assert_eq!(DEFAULT_CONNECT_TIMEOUT, Duration::from_secs(30));
        let transport = ReqwestTransport::new(Duration::from_secs(60));
        assert!(transport.is_ok());
    }
}
