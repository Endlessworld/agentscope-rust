//! Dify API client and HTTP transport abstraction.
//!
//! Ports `io.agentscope.core.rag.integration.dify.DifyRAGClient` (OkHttp + RetryInterceptor)
//! onto `reqwest` + [`crate::retry`]. The transport is an injectable trait so tests exercise
//! the full request/response path without opening a socket.

use async_trait::async_trait;

use crate::dify::config::DifyRAGConfig;
use crate::dify::model::DifyResponse;
use crate::dify::request::build_retrieve_request_body;
use crate::error::{RagError, Result};
use crate::retry::{retry_async, RetryPolicy};

/// One HTTP exchange: method, URL, headers and optional JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// HTTP method (always `POST` for the retrieve endpoint).
    pub method: String,
    /// Absolute URL.
    pub url: String,
    /// Request headers, including `Authorization`.
    pub headers: Vec<(String, String)>,
    /// Serialized JSON body.
    pub body: String,
}

/// The HTTP response as the client needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status_code: u16,
    /// Raw response body.
    pub body: String,
}

impl HttpResponse {
    /// Whether the status is in the 2xx range.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status_code)
    }
}

/// Sends an [`HttpRequest`] and returns an [`HttpResponse`].
///
/// Implementations must not interpret the status; the client does that.
#[async_trait]
pub trait DifyTransport: Send + Sync {
    /// Performs one HTTP exchange.
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse>;
}

/// Real `reqwest`-backed transport.
#[derive(Debug, Clone)]
pub struct HttpDifyTransport {
    client: reqwest::Client,
    retry: RetryPolicy,
}

impl HttpDifyTransport {
    /// Builds a transport from the Dify configuration's timeouts and retry budget.
    pub fn from_config(config: &DifyRAGConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.read_timeout)
            .build()
            .map_err(|e| RagError::Http(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            retry: RetryPolicy {
                max_retries: config.max_retries,
                ..RetryPolicy::default()
            },
        })
    }
}

#[async_trait]
impl DifyTransport for HttpDifyTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        retry_async(&self.retry, || {
            let client = self.client.clone();
            let request = request.clone();
            async move {
                let mut builder = client.post(&request.url).body(request.body.clone());
                for (name, value) in &request.headers {
                    builder = builder.header(name, value);
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

                if (200..300).contains(&status_code) {
                    Ok(HttpResponse { status_code, body })
                } else {
                    // Surface non-2xx as a typed error so the retry helper can decide:
                    // 429 / 5xx are retried, other 4xx propagate immediately.
                    Err(map_error_response(status_code, &body))
                }
            }
        })
        .await
    }
}

/// Client for the Dify Knowledge Base retrieve API.
pub struct DifyRAGClient {
    config: DifyRAGConfig,
    transport: Box<dyn DifyTransport>,
}

impl std::fmt::Debug for DifyRAGClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DifyRAGClient")
            .field("dataset_id", &self.config.dataset_id)
            .field("api_base_url", &self.config.api_base_url)
            .finish_non_exhaustive()
    }
}

impl DifyRAGClient {
    /// Creates a client using the real `reqwest` transport.
    pub fn new(config: DifyRAGConfig) -> Result<Self> {
        let transport = HttpDifyTransport::from_config(&config)?;
        Ok(Self::with_transport(config, Box::new(transport)))
    }

    /// Creates a client with an injected transport (used by tests and by callers who need a
    /// custom HTTP stack).
    pub fn with_transport(config: DifyRAGConfig, transport: Box<dyn DifyTransport>) -> Self {
        Self { config, transport }
    }

    /// The active configuration.
    pub fn config(&self) -> &DifyRAGConfig {
        &self.config
    }

    /// Builds the HTTP request for a retrieve call (without sending it).
    ///
    /// Exposed so tests think about the wire contract without a transport.
    pub fn build_retrieve_http_request(&self, query: &str, limit: Option<i32>) -> HttpRequest {
        let body = build_retrieve_request_body(&self.config, query, limit);
        let mut headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.config.api_key),
            ),
            (
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
        ];
        for (name, value) in &self.config.custom_headers {
            headers.push((name.clone(), value.clone()));
        }

        HttpRequest {
            method: "POST".to_string(),
            url: self.config.retrieve_url(),
            headers,
            body: serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string()),
        }
    }

    /// Retrieves records for `query`.
    ///
    /// Mirrors Java `DifyRAGClient.retrieve`: builds the request, sends it, maps non-2xx
    /// responses to [`RagError::Auth`] (401/403) or [`RagError::Api`], and deserializes the
    /// body. A body that is not a valid object is treated as an empty response, like Java's
    /// `null` check.
    pub async fn retrieve(&self, query: &str, limit: Option<i32>) -> Result<DifyResponse> {
        if query.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Query cannot be null or empty".to_string(),
            ));
        }

        let request = self.build_retrieve_http_request(query, limit);
        let response = self.transport.execute(request).await?;
        let status_code = response.status_code;

        if !(200..300).contains(&status_code) {
            return Err(map_error_response(status_code, &response.body));
        }

        if response.body.trim().is_empty() {
            return Ok(DifyResponse::empty());
        }

        // A JSON `null` body deserializes to `None`; Java returns an empty response there.
        let parsed: Option<DifyResponse> =
            serde_json::from_str(&response.body).map_err(|e| RagError::Api {
                status_code,
                error_code: None,
                message: format!("failed to parse Dify response: {e}"),
            })?;

        Ok(parsed.unwrap_or_default())
    }
}

/// Maps a non-2xx response into a typed error.
///
/// Mirrors Java `DifyRAGClient.handleErrorResponse`: parse `message` / `code` from the JSON
/// body when possible, otherwise use the raw body; 401/403 become auth errors, everything
/// else an API error.
pub fn map_error_response(status_code: u16, body: &str) -> RagError {
    let mut message = "Dify API error".to_string();
    let mut error_code: Option<String> = None;

    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(m) = map.get("message") {
            message = value_to_string(m);
        }
        if let Some(c) = map.get("code") {
            error_code = Some(value_to_string(c));
        }
    } else if !body.is_empty() {
        message = body.to_string();
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

/// Renders a JSON scalar the way Java's `Object.toString()` would.
fn value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<Vec<HttpResponse>>,
        seen: Mutex<Vec<HttpRequest>>,
    }

    impl MockTransport {
        fn with_responses(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().collect()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn last_request(&self) -> HttpRequest {
            self.seen
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("transport recorded no request")
        }
    }

    /// Keeps a shared handle on the mock's captured requests while the client owns the
    /// boxed transport.
    #[derive(Clone, Default)]
    struct SharedMock {
        inner: std::sync::Arc<MockTransport>,
    }

    #[async_trait]
    impl DifyTransport for SharedMock {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.inner.execute(request).await
        }
    }

    #[async_trait]
    impl DifyTransport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.seen.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| RagError::Http("no more mock responses".to_string()))
        }
    }

    fn client(transport: MockTransport) -> DifyRAGClient {
        let config = DifyRAGConfig::builder()
            .api_key("secret-key")
            .dataset_id("ds-1")
            .build_or_panic();
        DifyRAGClient::with_transport(config, Box::new(transport))
    }

    const OK_BODY: &str = r#"{
      "query": {"content": "q"},
      "records": [{"segment": {"id": "s", "document_id": "d", "content": "text"}, "score": 0.9}]
    }"#;

    #[tokio::test]
    async fn builds_expected_request_line_and_headers() {
        let mock = SharedMock::default();
        mock.inner.responses.lock().unwrap().push(HttpResponse {
            status_code: 200,
            body: OK_BODY.to_string(),
        });

        let config = DifyRAGConfig::builder()
            .api_key("secret-key")
            .dataset_id("ds-1")
            .build_or_panic();
        let client = DifyRAGClient::with_transport(config, Box::new(mock.clone()));

        let response = client.retrieve("q", Some(4)).await.unwrap();
        assert_eq!(response.record_count(), 1);

        let seen = mock.inner.last_request();
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.url, "https://api.dify.ai/v1/datasets/ds-1/retrieve");
        assert!(seen
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer secret-key"));
        assert!(seen.headers.iter().any(|(k, _)| k == "Content-Type"));

        let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
        assert_eq!(body["query"], "q");
        assert_eq!(body["retrieval_model"]["top_k"], 4);
    }

    #[tokio::test]
    async fn custom_headers_are_merged() {
        let config = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .add_custom_header("X-Trace", "abc")
            .build_or_panic();
        let transport = MockTransport::default();
        let client = DifyRAGClient::with_transport(config, Box::new(transport));
        let request = client.build_retrieve_http_request("q", None);
        assert!(request
            .headers
            .iter()
            .any(|(k, v)| k == "X-Trace" && v == "abc"));
    }

    #[tokio::test]
    async fn auth_error_is_typed() {
        let transport = MockTransport::with_responses(vec![HttpResponse {
            status_code: 401,
            body: r#"{"code":"unauthorized","message":"bad key"}"#.to_string(),
        }]);
        let client = client(transport);
        let err = client.retrieve("q", None).await.unwrap_err();
        assert!(matches!(
            err,
            RagError::Auth {
                status_code: 401,
                ..
            }
        ));
        assert!(err.is_client_error());
    }

    #[tokio::test]
    async fn api_error_parses_code_and_message() {
        let transport = MockTransport::with_responses(vec![HttpResponse {
            status_code: 400,
            body: r#"{"code":"invalid_param","message":"bad request"}"#.to_string(),
        }]);
        let client = client(transport);
        let err = client.retrieve("q", None).await.unwrap_err();
        assert_eq!(err.status_code(), Some(400));
        assert_eq!(err.error_code(), Some("invalid_param"));
        assert!(err.to_string().contains("bad request"));
    }

    #[test]
    fn error_mapping_handles_non_json_bodies() {
        let err = map_error_response(500, "<html>oops</html>");
        assert!(matches!(
            err,
            RagError::Api {
                status_code: 500,
                ..
            }
        ));
        assert!(err.to_string().contains("<html>oops</html>"));
        assert!(err.is_server_error());
    }

    #[test]
    fn error_mapping_handles_forbidden_as_auth() {
        let err = map_error_response(403, "{}");
        assert!(matches!(
            err,
            RagError::Auth {
                status_code: 403,
                ..
            }
        ));
        // Falls back to the default message when no `message` key exists.
        assert!(err.to_string().contains("Dify API error"));
    }

    #[tokio::test]
    async fn empty_query_is_rejected_before_any_transport_call() {
        let transport = MockTransport::default();
        let client = client(transport);
        assert!(matches!(
            client.retrieve("   ", None).await,
            Err(RagError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn empty_body_is_an_empty_response() {
        let transport = MockTransport::with_responses(vec![HttpResponse {
            status_code: 200,
            body: String::new(),
        }]);
        let client = client(transport);
        let response = client.retrieve("q", None).await.unwrap();
        assert_eq!(response.record_count(), 0);
    }

    #[tokio::test]
    async fn null_body_is_an_empty_response() {
        let transport = MockTransport::with_responses(vec![HttpResponse {
            status_code: 200,
            body: "null".to_string(),
        }]);
        let client = client(transport);
        let response = client.retrieve("q", None).await.unwrap();
        assert_eq!(response.record_count(), 0);
    }

    #[tokio::test]
    async fn malformed_body_is_an_api_error() {
        let transport = MockTransport::with_responses(vec![HttpResponse {
            status_code: 200,
            body: "not json at all".to_string(),
        }]);
        let client = client(transport);
        assert!(matches!(
            client.retrieve("q", None).await,
            Err(RagError::Api { .. })
        ));
    }

    #[test]
    fn request_builder_is_public_and_correct() {
        let config = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .build_or_panic();
        let transport = MockTransport::default();
        let client = DifyRAGClient::with_transport(config, Box::new(transport));
        let request = client.build_retrieve_http_request("hello", Some(2));
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["retrieval_model"]["top_k"], 2);
    }
}
