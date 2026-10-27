//! Provider-agnostic HTTP seam used by every outbound client.
//!
//! The Java outbound clients each build a Spring `WebClient` and return a Reactor `Mono`. To keep
//! the Rust adapters unit-testable without network access, request construction is separated from
//! execution: adapters produce a [`PreparedRequest`] and hand it to an [`HttpTransport`].
//! [`ReqwestTransport`] is the production implementation; `common::testing::RecordingTransport`
//! is the offline double used by tests.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::error::{ChannelError, Result};

/// Default per-request timeout, mirroring the Java `Duration.ofSeconds(10)` used by the token
/// providers and the WeCom/Feishu/DingTalk send calls (GitHub uses 15 s).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reject a non-positive timeout, mirroring the Java guard
/// `IllegalArgumentException("responseTimeout must be positive")` in `FeishuAccessTokenProvider`
/// and `FeishuOutboundClient`. Rust's [`Duration`] is unsigned, so only zero can be rejected.
pub(crate) fn ensure_positive_timeout(timeout: Duration) -> Result<()> {
    if timeout.is_zero() {
        return Err(ChannelError::Config(
            "responseTimeout must be positive".to_string(),
        ));
    }
    Ok(())
}

/// HTTP verb used by the outbound clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// A fully-built outbound request, ready to be executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRequest {
    pub method: HttpMethod,
    /// Absolute request URL (base + path + query, already URL-encoded).
    pub url: String,
    /// Ordered header list; duplicates are preserved.
    pub headers: Vec<(String, String)>,
    /// Optional JSON body.
    pub body: Option<serde_json::Value>,
}

impl PreparedRequest {
    /// Build a GET request.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// Build a POST request with a JSON body.
    pub fn post_json(url: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            method: HttpMethod::Post,
            url: url.into(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Some(body),
        }
    }

    /// Append a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Case-insensitive header lookup (HTTP header names are case-insensitive).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The raw provider response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body as text.
    pub body: String,
}

impl PreparedResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// `true` for 2xx statuses.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Executes [`PreparedRequest`]s. Implementations must be safe to share across tasks.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Send `request` and return the provider's response.
    async fn execute(&self, request: PreparedRequest) -> Result<PreparedResponse>;

    /// Return a transport equivalent to `self` whose per-request timeout is `timeout`.
    ///
    /// The default implementation returns `None`, meaning "this transport cannot be
    /// reconfigured" (e.g. an injected fake or a client owned elsewhere). Callers then keep the
    /// transport they were given and treat the timeout as caller-owned configuration.
    fn with_response_timeout(&self, _timeout: Duration) -> Option<Arc<dyn HttpTransport>> {
        None
    }
}

/// Reconfigure `transport` with `timeout` when it supports it, otherwise return it unchanged.
pub(crate) fn apply_response_timeout(
    transport: Arc<dyn HttpTransport>,
    timeout: Duration,
) -> Arc<dyn HttpTransport> {
    match transport.with_response_timeout(timeout) {
        Some(reconfigured) => reconfigured,
        None => transport,
    }
}

/// Production [`HttpTransport`] backed by `reqwest` (the Rust analogue of Spring `WebClient`).
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
    /// Per-request timeout applied to every request. `None` means "defer to whatever timeout the
    /// wrapped [`reqwest::Client`] was configured with" (the [`Self::with_client`] case).
    timeout: Option<Duration>,
}

impl ReqwestTransport {
    /// Build a transport with a default `reqwest` client and [`DEFAULT_TIMEOUT`].
    ///
    /// Returns an error rather than panicking when the client cannot be constructed (e.g. a bad
    /// TLS backend in the build).
    pub fn new() -> Result<Self> {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    /// Build a transport whose per-request timeout is `timeout` (Java's `responseTimeout`).
    ///
    /// Returns [`ChannelError::Config`] when `timeout` is zero, mirroring the Java
    /// `"responseTimeout must be positive"` constructor guard.
    pub fn with_timeout(timeout: Duration) -> Result<Self> {
        ensure_positive_timeout(timeout)?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ChannelError::Http(e.to_string()))?;
        Ok(Self {
            client,
            timeout: Some(timeout),
        })
    }

    /// Wrap an existing (already configured) `reqwest` client.
    ///
    /// The wrapped client keeps whatever timeout it was built with, so no per-request override is
    /// recorded ([`Self::timeout`] returns `None`).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            timeout: None,
        }
    }

    /// The per-request timeout this transport applies, if one was recorded.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, request: PreparedRequest) -> Result<PreparedResponse> {
        let mut builder = match request.method {
            HttpMethod::Get => self.client.get(&request.url),
            HttpMethod::Post => self.client.post(&request.url),
        };
        if let Some(timeout) = self.timeout {
            builder = builder.timeout(timeout);
        }
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.json(&body);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| ChannelError::Http(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| ChannelError::Http(e.to_string()))?;
        Ok(PreparedResponse { status, body })
    }

    fn with_response_timeout(&self, timeout: Duration) -> Option<Arc<dyn HttpTransport>> {
        if timeout.is_zero() {
            return None;
        }
        // Reuse the underlying client (connection pool) and only override the per-request timeout.
        Some(Arc::new(Self {
            client: self.client.clone(),
            timeout: Some(timeout),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_json_sets_content_type_and_body() {
        let req =
            PreparedRequest::post_json("https://example.test/x", serde_json::json!({ "a": 1 }));
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(req.header_value("Content-Type"), Some("application/json"));
        assert_eq!(req.body, Some(serde_json::json!({ "a": 1 })));
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_keeps_order() {
        let req = PreparedRequest::get("https://example.test")
            .header("X-First", "1")
            .header("x-first", "2");
        assert_eq!(req.header_value("x-FIRST"), Some("1"));
        assert_eq!(req.headers.len(), 2);
        assert!(req.header_value("missing").is_none());
    }

    #[test]
    fn success_status_detection() {
        assert!(PreparedResponse::new(200, "ok").is_success());
        assert!(PreparedResponse::new(299, "ok").is_success());
        assert!(!PreparedResponse::new(400, "bad").is_success());
        assert!(!PreparedResponse::new(500, "bad").is_success());
    }

    #[test]
    fn reqwest_transport_defaults_to_ten_seconds() {
        let transport = ReqwestTransport::new().expect("default transport builds offline");
        assert_eq!(transport.timeout(), Some(DEFAULT_TIMEOUT));
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn reqwest_transport_accepts_explicit_timeout() {
        let transport =
            ReqwestTransport::with_timeout(Duration::from_millis(250)).expect("positive timeout");
        assert_eq!(transport.timeout(), Some(Duration::from_millis(250)));
    }

    #[test]
    fn reqwest_transport_rejects_zero_timeout() {
        let err = ReqwestTransport::with_timeout(Duration::ZERO).expect_err("zero is rejected");
        assert!(
            err.to_string().contains("responseTimeout must be positive"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn with_client_defers_to_the_wrapped_client_timeout() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("client builds offline");
        let transport = ReqwestTransport::with_client(client);
        assert_eq!(transport.timeout(), None);
    }

    #[test]
    fn with_response_timeout_reconfigures_reqwest_transport() {
        let transport = ReqwestTransport::new().unwrap();
        assert!(transport
            .with_response_timeout(Duration::from_secs(2))
            .is_some());
    }

    #[test]
    fn with_response_timeout_rejects_zero() {
        let transport = ReqwestTransport::new().unwrap();
        assert!(transport.with_response_timeout(Duration::ZERO).is_none());
    }
}
