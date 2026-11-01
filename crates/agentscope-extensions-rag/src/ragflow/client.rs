//! RAGFlow HTTP client.
//!
//! Ports `io.agentscope.core.rag.integration.ragflow.RAGFlowClient`: it builds the
//! `POST /api/v1/retrieval` request, maps non-2xx statuses to [`RagError::Auth`] (401/403)
//! or [`RagError::Api`] (404 "Dataset not found", 429 "Rate limit exceeded", 5xx
//! "RAGFlow server error"), and surfaces a non-zero API `code` as an API error.
//!
//! The transport is injectable ([`HttpTransport`]), so the whole test suite runs offline.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::error::{RagError, Result};
use crate::ragflow::config::RAGFlowConfig;
use crate::ragflow::model::RAGFlowResponse;
use crate::ragflow::request::build_retrieval_request_body;
use crate::transport::{HttpRequest, HttpTransport, TransportOptions};

/// Java `RAGFlowClient.RetryInterceptor` caps one backoff sleep at 10s
/// (`Thread.sleep(Math.min(waitTime, 10000))`).
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Transport options for a RAGFlow client: Java's `2^attempt * 1s` backoff capped at 10s.
fn retry_options(config: &RAGFlowConfig) -> TransportOptions {
    TransportOptions {
        connect_timeout: config.timeout,
        read_timeout: config.timeout,
        max_retries: config.max_retries,
        max_delay: Some(MAX_RETRY_DELAY),
        ..TransportOptions::default()
    }
}

/// Client for the RAGFlow retrieval API.
pub struct RAGFlowClient {
    config: RAGFlowConfig,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for RAGFlowClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RAGFlowClient")
            .field("base_url", &self.config.base_url)
            .field("dataset_ids", &self.config.dataset_ids)
            .finish_non_exhaustive()
    }
}

impl RAGFlowClient {
    /// Creates a client with the real `reqwest` transport.
    pub fn from_config(config: RAGFlowConfig) -> Result<Self> {
        let options = retry_options(&config);
        let transport = crate::transport::ReqwestTransport::with_extra_headers(
            &options,
            config.custom_headers.clone(),
        )?;
        Ok(Self::with_transport(config, Arc::new(transport)))
    }

    /// Creates a client with an injected transport (tests / custom HTTP stacks).
    pub fn with_transport(config: RAGFlowConfig, transport: Arc<dyn HttpTransport>) -> Self {
        Self { config, transport }
    }

    /// The active configuration.
    pub fn config(&self) -> &RAGFlowConfig {
        &self.config
    }

    /// Builds the HTTP request for a retrieve call without sending it.
    ///
    /// Exposed so tests can assert on the exact wire contract.
    pub fn build_retrieve_http_request(
        &self,
        question: &str,
        top_k: Option<i32>,
        similarity_threshold: Option<f64>,
        metadata_condition: Option<&Value>,
    ) -> HttpRequest {
        let body = build_retrieval_request_body(
            &self.config,
            question,
            top_k,
            similarity_threshold,
            metadata_condition,
        );
        HttpRequest::post_json(
            self.config.retrieval_url(),
            serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string()),
        )
        .with_bearer_token(&self.config.api_key)
    }

    /// Retrieves chunks for `question`.
    pub async fn retrieve(
        &self,
        question: &str,
        top_k: Option<i32>,
        similarity_threshold: Option<f64>,
        metadata_condition: Option<&Value>,
    ) -> Result<RAGFlowResponse> {
        if question.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Question cannot be null or empty".to_string(),
            ));
        }

        let request = self.build_retrieve_http_request(
            question,
            top_k,
            similarity_threshold,
            metadata_condition,
        );
        let response = self.transport.execute(request).await?;
        let status_code = response.status_code;

        if !(200..300).contains(&status_code) {
            return Err(map_status_error(status_code, &response.body));
        }

        let parsed: RAGFlowResponse =
            crate::transport::parse_json_or_default(&response.body, "RAGFlow")?;

        // A non-zero API code is an error even on HTTP 200 (Java does the same).
        if parsed.is_api_error() {
            return Err(RagError::Api {
                status_code,
                error_code: parsed.code.map(|c| c.to_string()),
                message: format!(
                    "RAGFlow API error: {}",
                    parsed.message.as_deref().unwrap_or("")
                ),
            });
        }

        Ok(parsed)
    }
}

/// Maps a non-2xx status onto the Java `handleErrorResponse` taxonomy.
fn map_status_error(status_code: u16, body: &str) -> RagError {
    let detail = body.to_string();
    match status_code {
        401 | 403 => RagError::Auth {
            status_code,
            message: format!("Authentication failed: {detail}"),
        },
        404 => RagError::Api {
            status_code,
            error_code: None,
            message: format!("Dataset not found: {detail}"),
        },
        429 => RagError::Api {
            status_code,
            error_code: None,
            message: format!("Rate limit exceeded: {detail}"),
        },
        code if code >= 500 => RagError::Api {
            status_code,
            error_code: None,
            message: format!("RAGFlow server error (HTTP {code}): {detail}"),
        },
        code => RagError::Api {
            status_code,
            error_code: None,
            message: format!("RAGFlow API error (HTTP {code}): {detail}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<Vec<crate::transport::HttpResponse>>,
        requests: Mutex<Vec<HttpRequest>>,
        fail_transport: bool,
    }

    impl MockTransport {
        fn with_responses(responses: Vec<crate::transport::HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().rev().collect()),
                requests: Mutex::new(Vec::new()),
                fail_transport: false,
            }
        }

        fn failing_transport() -> Self {
            Self {
                fail_transport: true,
                ..Default::default()
            }
        }
    }

    #[async_trait]
    impl HttpTransport for MockTransport {
        async fn execute(&self, request: HttpRequest) -> Result<crate::transport::HttpResponse> {
            self.requests.lock().unwrap().push(request);
            if self.fail_transport {
                return Err(RagError::Http("connection refused".to_string()));
            }
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| RagError::Http("no scripted response".to_string()))
        }
    }

    #[derive(Clone, Default)]
    struct SharedMock(Arc<MockTransport>);

    #[async_trait]
    impl HttpTransport for SharedMock {
        async fn execute(&self, request: HttpRequest) -> Result<crate::transport::HttpResponse> {
            self.0.execute(request).await
        }
    }

    fn config(max_retries: u32) -> RAGFlowConfig {
        RAGFlowConfig::builder()
            .api_key("ragflow-key")
            .base_url("http://localhost:9380")
            .add_dataset_id("ds-1")
            .max_retries(max_retries)
            .build()
            .unwrap()
    }

    fn client(responses: Vec<crate::transport::HttpResponse>) -> (RAGFlowClient, SharedMock) {
        let shared = SharedMock(Arc::new(MockTransport::with_responses(responses)));
        (
            RAGFlowClient::with_transport(config(0), Arc::new(shared.clone())),
            shared,
        )
    }

    #[test]
    fn build_request_targets_retrieval_endpoint_with_bearer_auth() {
        let (client, _) = client(vec![]);
        let request = client.build_retrieve_http_request("q", Some(5), Some(0.3), None);
        assert_eq!(request.method, crate::transport::HttpMethod::Post);
        assert_eq!(request.url, "http://localhost:9380/api/v1/retrieval");
        assert_eq!(request.header("Authorization"), Some("Bearer ragflow-key"));
        assert_eq!(
            request.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["question"], serde_json::json!("q"));
        assert_eq!(body["top_k"], serde_json::json!(5));
        assert_eq!(body["similarity_threshold"], serde_json::json!(0.3));
    }

    #[tokio::test]
    async fn retrieve_parses_success() {
        let (client, _) = client(vec![crate::transport::HttpResponse::new(
            200,
            r#"{"code": 0, "data": {"chunks": [{"id": "c1", "document_id": "d1", "content": "hi", "score": 0.9}], "total": 1}}"#,
        )]);
        let response = client.retrieve("q", Some(5), None, None).await.unwrap();
        assert_eq!(response.chunk_count(), 1);
    }

    #[tokio::test]
    async fn retrieve_rejects_blank_question() {
        let (client, shared) = client(vec![]);
        let err = client.retrieve("   ", None, None, None).await.unwrap_err();
        assert!(matches!(err, RagError::InvalidArgument(_)));
        assert!(shared.0.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retrieve_maps_auth_status() {
        let (client, _) = client(vec![crate::transport::HttpResponse::new(401, "denied")]);
        let err = client.retrieve("q", None, None, None).await.unwrap_err();
        match err {
            RagError::Auth {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 401);
                assert!(message.contains("Authentication failed"));
            }
            other => panic!("expected auth error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrieve_maps_404_429_and_5xx() {
        let cases = [
            (404, "Dataset not found"),
            (429, "Rate limit exceeded"),
            (503, "server error"),
        ];
        for (status, expected) in cases {
            let (client, _) = client(vec![crate::transport::HttpResponse::new(status, "body")]);
            let err = client.retrieve("q", None, None, None).await.unwrap_err();
            match err {
                RagError::Api { message, .. } => {
                    assert!(message.contains(expected), "{message} missing {expected}")
                }
                other => panic!("expected api error, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn retrieve_treats_non_zero_code_as_api_error() {
        let (client, _) = client(vec![crate::transport::HttpResponse::new(
            200,
            r#"{"code": 102, "message": "no permission", "data": false}"#,
        )]);
        let err = client.retrieve("q", None, None, None).await.unwrap_err();
        match err {
            RagError::Api {
                status_code,
                error_code,
                message,
            } => {
                assert_eq!(status_code, 200);
                assert_eq!(error_code.as_deref(), Some("102"));
                assert!(message.contains("no permission"));
            }
            other => panic!("expected api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retrieve_propagates_transport_failure() {
        let transport = Arc::new(MockTransport::failing_transport());
        let client = RAGFlowClient::with_transport(config(0), transport);
        let err = client.retrieve("q", None, None, None).await.unwrap_err();
        assert!(matches!(err, RagError::Http(_)));
    }

    /// A `429` enters the retry loop and the retried call can succeed (Java `RetryInterceptor`).
    #[tokio::test]
    async fn a_429_is_retried_and_the_next_attempt_succeeds() {
        let shared = SharedMock(Arc::new(MockTransport::with_responses(vec![
            crate::transport::HttpResponse::new(429, "slow down"),
            crate::transport::HttpResponse::new(
                200,
                r#"{"code": 0, "data": {"chunks": [], "total": 0}}"#,
            ),
        ])));
        let client = RAGFlowClient::with_transport(
            config(1),
            Arc::new(crate::transport::RetryingTransport::new(
                Arc::new(shared.clone()),
                crate::retry::RetryPolicy::new(1),
            )),
        );

        let response = client.retrieve("q", None, None, None).await.unwrap();
        assert_eq!(response.chunk_count(), 0);
        assert_eq!(shared.0.requests.lock().unwrap().len(), 2);
    }

    /// A `400` is terminal: the retry loop must send it exactly once.
    #[tokio::test]
    async fn a_400_is_sent_exactly_once() {
        let shared = SharedMock(Arc::new(MockTransport::with_responses(vec![
            crate::transport::HttpResponse::new(400, "bad request"),
        ])));
        let client = RAGFlowClient::with_transport(
            config(3),
            Arc::new(crate::transport::RetryingTransport::new(
                Arc::new(shared.clone()),
                crate::retry::RetryPolicy::new(3),
            )),
        );

        let err = client.retrieve("q", None, None, None).await.unwrap_err();
        assert!(matches!(
            err,
            RagError::Api {
                status_code: 400,
                ..
            }
        ));
        assert_eq!(shared.0.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retrieve_passes_metadata_condition_through() {
        let (client, shared) = client(vec![crate::transport::HttpResponse::new(200, "{}")]);
        let condition = serde_json::json!({
            "logic": "and",
            "conditions": [{"name": "author", "comparison_operator": "=", "value": "Toby"}]
        });
        client
            .retrieve("q", None, None, Some(&condition))
            .await
            .unwrap();

        let requests = shared.0.requests.lock().unwrap();
        let wire: Value = serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(wire["metadata_condition"], condition);
    }

    #[tokio::test]
    async fn retrieve_returns_empty_response_for_null_body() {
        let (client, _) = client(vec![crate::transport::HttpResponse::new(200, "")]);
        let response = client.retrieve("q", None, None, None).await.unwrap();
        assert!(response.data.is_none());
        assert_eq!(response.code, None);
    }

    #[test]
    fn client_from_config_uses_configured_timeouts() {
        let cfg = config(2);
        let client = RAGFlowClient::from_config(cfg).unwrap();
        assert_eq!(client.config().max_retries, 2);
    }

    /// The Java interceptor waits `2^attempt * 1s`, capped at 10s.
    #[test]
    fn retry_options_match_the_java_backoff_cap() {
        let options = retry_options(&config(3));
        assert_eq!(options.max_retries, 3);
        assert_eq!(options.base_delay, Duration::from_secs(1));
        assert_eq!(options.max_delay, Some(Duration::from_secs(10)));
        let policy = crate::retry::RetryPolicy {
            max_retries: options.max_retries,
            base_delay: options.base_delay,
            max_delay: options.max_delay,
        };
        let sequence: Vec<u64> = (0..5).map(|a| policy.delay_for(a).as_secs()).collect();
        assert_eq!(sequence, vec![1, 2, 4, 8, 10]);
    }
}
