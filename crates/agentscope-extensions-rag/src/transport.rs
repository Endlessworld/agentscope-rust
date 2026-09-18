//! Generic injectable HTTP transport used by every remote RAG backend.
//!
//! The `dify` submodule ships its own [`crate::dify::DifyTransport`] because it was the
//! first backend ported; the later backends (`ragflow`, `haystack`, `bailian`) and the
//! external vector stores (`qdrant`, `pgvector`) share the small abstraction defined here
//! instead of re-declaring one seam per module.
//!
//! Production code uses [`ReqwestTransport`]; tests inject a recording/scripted
//! implementation and never open a socket.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::{RagError, Result};
use crate::retry::{retry_async, RetryPolicy};

/// HTTP verbs used by the backends in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
}

impl HttpMethod {
    /// Uppercase wire representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
        }
    }
}

/// One HTTP exchange: method, URL, headers and an optional body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// HTTP verb.
    pub method: HttpMethod,
    /// Absolute URL.
    pub url: String,
    /// Request headers (auth, content type, custom headers).
    pub headers: Vec<(String, String)>,
    /// Serialized request body, absent for `GET`/`DELETE`.
    pub body: Option<String>,
}

impl HttpRequest {
    /// Creates a request without a body.
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// Creates a JSON `POST` request.
    pub fn post_json(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Post,
            url: url.into(),
            headers: vec![(
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: Some(body.into()),
        }
    }

    /// Appends a header, replacing any previous value with the same name.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        self.headers.retain(|(existing, _)| existing != &name);
        self.headers.push((name, value.into()));
        self
    }

    /// Adds a `Authorization: Bearer <token>` header.
    pub fn with_bearer_token(self, token: &str) -> Self {
        self.with_header("Authorization", format!("Bearer {token}"))
    }

    /// Returns a header value by name (case-insensitive, first match).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One HTTP response as the backends need it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status_code: u16,
    /// Raw response body.
    pub body: String,
}

impl HttpResponse {
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

/// Sends an [`HttpRequest`] and returns an [`HttpResponse`].
///
/// Implementations must not interpret the status code; each client does that, because the
/// error taxonomy differs per vendor.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Performs one HTTP exchange.
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse>;
}

/// Timeouts and retry budget shared by the `reqwest`-backed transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportOptions {
    /// Connect timeout.
    pub connect_timeout: Duration,
    /// Read timeout.
    pub read_timeout: Duration,
    /// Maximum retries after the initial attempt.
    pub max_retries: u32,
    /// Base retry backoff.
    pub base_delay: Duration,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(30),
            max_retries: 3,
            base_delay: Duration::from_millis(200),
        }
    }
}

/// Real `reqwest`-backed transport with exponential-backoff retries.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
    retry: RetryPolicy,
    extra_headers: HashMap<String, String>,
}

impl ReqwestTransport {
    /// Builds a transport from the given options.
    pub fn new(options: &TransportOptions) -> Result<Self> {
        Self::with_extra_headers(options, HashMap::new())
    }

    /// Builds a transport that appends `extra_headers` to every request.
    pub fn with_extra_headers(
        options: &TransportOptions,
        extra_headers: HashMap<String, String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .timeout(options.read_timeout)
            .build()
            .map_err(|e| RagError::Http(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            retry: RetryPolicy {
                max_retries: options.max_retries,
                base_delay: options.base_delay,
            },
            extra_headers,
        })
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        retry_async(&self.retry, || {
            let client = self.client.clone();
            let request = request.clone();
            let extra_headers = self.extra_headers.clone();
            async move {
                let mut builder = match request.method {
                    HttpMethod::Get => client.get(&request.url),
                    HttpMethod::Post => client.post(&request.url),
                    HttpMethod::Put => client.put(&request.url),
                    HttpMethod::Delete => client.delete(&request.url),
                };

                for (name, value) in extra_headers.iter() {
                    builder = builder.header(name, value);
                }
                for (name, value) in &request.headers {
                    builder = builder.header(name, value);
                }
                if let Some(body) = &request.body {
                    builder = builder.body(body.clone());
                }

                let response = builder
                    .send()
                    .await
                    .map_err(|e| RagError::Http(e.to_string()))?;
                let status_code = response.status().as_u16();
                let body = response
                    .text()
                    .await
                    .map_err(|e| RagError::Http(e.to_string()))?;
                Ok(HttpResponse { status_code, body })
            }
        })
        .await
    }
}

/// Deserializes a JSON body, treating an empty / `null` body as `T::default()`.
///
/// Java's Jackson clients in this module react to a `null` body by returning an empty
/// envelope rather than throwing, so every client in this crate follows the same rule.
pub fn parse_json_or_default<T>(body: &str, context: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    if body.trim().is_empty() {
        return Ok(T::default());
    }
    let parsed: Option<T> = serde_json::from_str(body).map_err(|e| {
        RagError::Json(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to parse {context} response: {e}"),
        )))
    })?;
    Ok(parsed.unwrap_or_default())
}

/// Maps a non-2xx response onto [`RagError::Auth`] (401/403) or [`RagError::Api`].
///
/// `message` is parsed out of a JSON body when the body holds an object with a
/// human-readable `message` / `error` / `msg` field, otherwise the raw body is used —
/// mirroring the Java clients' `handleErrorResponse` helpers.
pub fn map_http_error(status_code: u16, body: &str) -> RagError {
    let mut message = if body.is_empty() {
        format!("HTTP {status_code}")
    } else {
        body.to_string()
    };
    let mut error_code: Option<String> = None;

    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["message", "error", "msg", "error_msg"] {
            if let Some(value) = map.get(key) {
                message = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                break;
            }
        }
        if let Some(code) = map.get("code") {
            error_code = Some(match code {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            });
        }
    }

    if status_code == 401 || status_code == 403 {
        RagError::Auth {
            status_code,
            message: format!("Authentication failed: {message}"),
        }
    } else {
        RagError::Api {
            status_code,
            error_code,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_helpers_set_headers() {
        let req = HttpRequest::post_json("http://localhost/retrieve", "{}")
            .with_header("X-Trace", "1")
            .with_bearer_token("k");
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(req.header("content-type"), Some("application/json; charset=utf-8"));
        assert_eq!(req.header("x-trace"), Some("1"));
        assert_eq!(req.header("Authorization"), Some("Bearer k"));
        assert_eq!(req.body.as_deref(), Some("{}"));
    }

    #[test]
    fn header_replacement_keeps_single_entry() {
        let req = HttpRequest::new(HttpMethod::Get, "http://x")
            .with_header("X-A", "1")
            .with_header("X-A", "2");
        assert_eq!(req.headers.len(), 1);
        assert_eq!(req.header("X-A"), Some("2"));
    }

    #[test]
    fn response_success_range() {
        assert!(HttpResponse::new(200, "").is_success());
        assert!(HttpResponse::new(299, "").is_success());
        assert!(!HttpResponse::new(300, "").is_success());
    }

    #[test]
    fn parse_json_defaults_on_empty_and_null() {
        let empty: serde_json::Value = parse_json_or_default("", "test").unwrap();
        assert_eq!(empty, serde_json::Value::Null);
        let null: serde_json::Value = parse_json_or_default("null", "test").unwrap();
        assert_eq!(null, serde_json::Value::Null);
    }

    #[test]
    fn parse_json_surfaces_invalid_body() {
        let err = parse_json_or_default::<serde_json::Value>("{not json", "test").unwrap_err();
        assert!(matches!(err, RagError::Json(_)));
    }

    #[test]
    fn map_http_error_splits_auth_from_api() {
        let auth = map_http_error(401, "nope");
        assert!(matches!(auth, RagError::Auth { status_code: 401, .. }));
        let api = map_http_error(500, r#"{"code": 500, "message": "boom"}"#);
        match api {
            RagError::Api {
                status_code,
                error_code,
                message,
            } => {
                assert_eq!(status_code, 500);
                assert_eq!(error_code.as_deref(), Some("500"));
                assert_eq!(message, "boom");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn method_wire_names() {
        assert_eq!(HttpMethod::Get.as_str(), "GET");
        assert_eq!(HttpMethod::Delete.as_str(), "DELETE");
    }
}
