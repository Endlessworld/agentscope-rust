//! Provider-agnostic HTTP seam used by every outbound client.
//!
//! The Java outbound clients each build a Spring `WebClient` and return a Reactor `Mono`. To keep
//! the Rust adapters unit-testable without network access, request construction is separated from
//! execution: adapters produce a [`PreparedRequest`] and hand it to an [`HttpTransport`].
//! [`ReqwestTransport`] is the production implementation; `common::testing::RecordingTransport`
//! is the offline double used by tests.

use std::time::Duration;

use async_trait::async_trait;

use super::error::{ChannelError, Result};

/// Default per-request timeout, mirroring the Java `Duration.ofSeconds(10)` used by the token
/// providers and the WeCom/Feishu/DingTalk send calls (GitHub uses 15 s).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

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
}

/// Production [`HttpTransport`] backed by `reqwest` (the Rust analogue of Spring `WebClient`).
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build a transport with a default `reqwest` client.
    ///
    /// Returns an error rather than panicking when the client cannot be constructed (e.g. a bad
    /// TLS backend in the build).
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| ChannelError::Http(e.to_string()))?;
        Ok(Self { client })
    }

    /// Wrap an existing (already configured) `reqwest` client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, request: PreparedRequest) -> Result<PreparedResponse> {
        let mut builder = match request.method {
            HttpMethod::Get => self.client.get(&request.url),
            HttpMethod::Post => self.client.post(&request.url),
        };
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_json_sets_content_type_and_body() {
        let req = PreparedRequest::post_json(
            "https://example.test/x",
            serde_json::json!({ "a": 1 }),
        );
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
}
