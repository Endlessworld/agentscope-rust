//! HayStack HTTP client.
//!
//! Ports `HayStackClient`: it `POST`s the retrieve body to the configured `baseUrl`, maps a
//! non-zero API `code` onto [`RagError::Api`] (Java `HayStackApiException`) and maps transport
//! / non-2xx failures onto [`RagError::Http`] / [`RagError::Api`] through the shared
//! [`crate::transport`] helpers.
//!
//! Two Java behaviours are preserved deliberately:
//!
//! * the body is parsed even for non-2xx responses, and only then is the API `code` checked —
//!   so a `200` with `code != successCode` is an API error, and a `500` with a parseable body
//!   surfaces whichever error the body describes;
//! * retries cover transport failures, `429` and `5xx` only (the Java `RetryInterceptor` did not
//!   retry below 500, except that it also skipped 4xx), which is exactly what
//!   [`crate::reqwest`]-backed [`crate::transport::ReqwestTransport`] implements.

use std::sync::Arc;

use crate::error::{RagError, Result};
use crate::haystack::config::HayStackConfig;
use crate::haystack::model::HayStackResponse;
use crate::haystack::request::build_retrieve_request_body;
use crate::transport::{HttpRequest, HttpTransport};

/// Client for the custom Haystack RAG retrieval API.
pub struct HayStackClient {
    config: HayStackConfig,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for HayStackClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HayStackClient")
            .field("base_url", &self.config.base_url)
            .finish_non_exhaustive()
    }
}

impl HayStackClient {
    /// Creates a client with the real `reqwest` transport.
    pub fn from_config(config: HayStackConfig) -> Result<Self> {
        let options = crate::transport::TransportOptions {
            connect_timeout: config.timeout,
            read_timeout: config.timeout,
            max_retries: config.max_retries,
            ..crate::transport::TransportOptions::default()
        };
        let transport = crate::transport::ReqwestTransport::with_extra_headers(
            &options,
            config.custom_headers.clone(),
        )?;
        Ok(Self::with_transport(config, Arc::new(transport)))
    }

    /// Creates a client with an injected transport (tests / custom HTTP stacks).
    pub fn with_transport(config: HayStackConfig, transport: Arc<dyn HttpTransport>) -> Self {
        Self { config, transport }
    }

    /// The active configuration.
    pub fn config(&self) -> &HayStackConfig {
        &self.config
    }

    /// Builds the HTTP request for a retrieve call without sending it.
    ///
    /// Exposed so tests can assert on the exact wire contract.
    pub fn build_retrieve_http_request(
        &self,
        query: &str,
        top_k: Option<i32>,
        score_threshold: Option<f64>,
    ) -> HttpRequest {
        let body = build_retrieve_request_body(&self.config, query, top_k, score_threshold);
        let serialized =
            serde_json::to_string(&body).unwrap_or_else(|_| "{\"query\":\"\"}".to_string());
        // `Content-Type: application/json` matches the Java `Request.Builder` header exactly
        // (the Java client called `addHeader` after `MediaType`-setting the body).
        HttpRequest::post_json(self.config.base_url.clone(), serialized)
            .with_header("Content-Type", "application/json")
    }

    /// Retrieves documents for `query`.
    ///
    /// `top_k` and `score_threshold` override the configured values when supplied, mirroring
    /// `retrieve(query, topK, scoreThreshold)`. A blank query is rejected, like the Java
    /// `IllegalArgumentException`.
    pub async fn retrieve(
        &self,
        query: &str,
        top_k: Option<i32>,
        score_threshold: Option<f64>,
    ) -> Result<HayStackResponse> {
        if query.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Query cannot be null or empty".to_string(),
            ));
        }

        let request = self.build_retrieve_http_request(query, top_k, score_threshold);
        let response = self.transport.execute(request).await?;

        // Java only *logged* the HTTP status and always attempted to parse the body
        // (`JsonUtils.fromJson(responseBody, HayStackResponse.class)`), so an unparseable / empty
        // body is the only transport-level error and the API `code` is the only business error.
        // This port keeps that split instead of inventing a status-based one.
        if response.body.trim().is_empty() {
            return Err(RagError::Http(format!(
                "HayStack returned an empty body (status {})",
                response.status_code
            )));
        }
        let parsed: HayStackResponse = serde_json::from_str(&response.body).map_err(|e| {
            RagError::Http(format!(
                "HayStack returned an unparseable body (status {}): {e}",
                response.status_code
            ))
        })?;

        // `code != successCode` is the Java `HayStackApiException`.
        if let Some(code) = parsed.code {
            if code != self.config.success_code {
                return Err(RagError::Api {
                    status_code: response.status_code,
                    error_code: Some(code.to_string()),
                    message: format!(
                        "HayStack API error: {}",
                        parsed.error.as_deref().unwrap_or("")
                    ),
                });
            }
        }

        tracing::debug!(
            status = response.status_code,
            documents = parsed.effective_documents().len(),
            "retrieved HayStack documents"
        );
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::HttpResponse;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Records outgoing requests and replays scripted responses (LIFO), never touching a socket.
    #[derive(Default)]
    struct ScriptedTransport {
        responses: Mutex<Vec<HttpResponse>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl ScriptedTransport {
        fn with_responses(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn last_request(&self) -> HttpRequest {
            self.requests.lock().unwrap().last().cloned().unwrap()
        }
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| RagError::Http("no scripted response".to_string()))
        }
    }

    fn config() -> HayStackConfig {
        HayStackConfig::builder()
            .base_url("http://localhost:8080/retrieve")
            .top_k(3)
            .max_retries(0)
            .build()
            .unwrap()
    }

    fn client(transport: Arc<ScriptedTransport>) -> HayStackClient {
        HayStackClient::with_transport(config(), transport)
    }

    fn success_body() -> String {
        r#"{"code":0,"documents":[
            {"id":"doc-1","content":"Haystack is a RAG","score":0.91,"meta":{"file_path":"intro.txt"}},
            {"id":"doc-2","content":"It supports pipelines","score":0.85,"meta":{"file_path":"intro1.txt"}}
        ]}"#
            .to_string()
    }

    #[tokio::test]
    async fn retrieve_posts_to_the_configured_base_url() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
        let client = client(transport.clone());

        let response = client
            .retrieve("What is Haystack?", None, None)
            .await
            .unwrap();
        assert_eq!(response.code, Some(0));
        assert_eq!(response.effective_documents().len(), 2);

        let request = transport.last_request();
        assert_eq!(request.method, crate::transport::HttpMethod::Post);
        assert_eq!(request.url, "http://localhost:8080/retrieve");
        assert_eq!(request.header("Content-Type"), Some("application/json"));
    }

    #[tokio::test]
    async fn request_body_contains_query_and_configured_top_k() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
        let client = client(transport.clone());
        client.retrieve("test query", None, None).await.unwrap();

        let sent = transport.last_request();
        let body: serde_json::Value = serde_json::from_str(sent.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["query"], "test query");
        assert_eq!(body["top_k"], 3);
    }

    #[tokio::test]
    async fn call_site_top_k_overrides_the_config() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
        let client = client(transport.clone());
        client.retrieve("test query", Some(10), None).await.unwrap();

        let sent = transport.last_request();
        let body: serde_json::Value = serde_json::from_str(sent.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["top_k"], 10);
    }

    #[tokio::test]
    async fn empty_result_is_not_an_error() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(
            200,
            r#"{"code":0,"documents":[]}"#,
        )]);
        let client = client(transport);
        let response = client.retrieve("unknown query", None, None).await.unwrap();
        assert!(response.effective_documents().is_empty());
    }

    #[tokio::test]
    async fn blank_query_is_rejected_without_sending() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let client = client(transport.clone());
        assert!(client.retrieve("", None, None).await.is_err());
        assert!(client.retrieve("   ", None, None).await.is_err());
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    /// A `429` enters the retry loop and the retried call can succeed (Java `RetryInterceptor`).
    #[tokio::test]
    async fn a_429_is_retried_and_the_next_attempt_succeeds() {
        // `ScriptedTransport` pops from the back, so the scripted order is reversed here.
        let transport = ScriptedTransport::with_responses(vec![
            HttpResponse::new(200, success_body()),
            HttpResponse::new(429, "slow down"),
        ]);
        let client = HayStackClient::with_transport(
            config(),
            Arc::new(crate::transport::RetryingTransport::new(
                transport.clone(),
                crate::retry::RetryPolicy::new(1),
            )),
        );

        let response = client.retrieve("q", None, None).await.unwrap();
        assert_eq!(response.effective_documents().len(), 2);
        assert_eq!(transport.requests.lock().unwrap().len(), 2);
    }

    /// A `400` is terminal: sent once, and its body is still parsed (Java only logged the status).
    #[tokio::test]
    async fn a_400_is_sent_exactly_once() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(
            400,
            r#"{"code":1001,"error":"bad request"}"#,
        )]);
        let client = HayStackClient::with_transport(
            config(),
            Arc::new(crate::transport::RetryingTransport::new(
                transport.clone(),
                crate::retry::RetryPolicy::new(3),
            )),
        );

        let error = client.retrieve("q", None, None).await.unwrap_err();
        assert!(error.to_string().contains("bad request"), "{error}");
        assert_eq!(error.error_code(), Some("1001"));
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn non_zero_code_is_an_api_error_carrying_the_message() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(
            200,
            r#"{"code":1001,"error":"Invalid query"}"#,
        )]);
        let client = client(transport);
        let error = client.retrieve("bad query", None, None).await.unwrap_err();
        assert!(matches!(error, RagError::Api { .. }), "{error:?}");
        assert!(error.to_string().contains("Invalid query"), "{error}");
        assert_eq!(error.error_code(), Some("1001"));
    }

    #[tokio::test]
    async fn custom_success_code_is_respected() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(
            200,
            r#"{"code":200,"documents":[]}"#,
        )]);
        let config = HayStackConfig::builder()
            .base_url("http://localhost:8080/retrieve")
            .success_code(200)
            .build()
            .unwrap();
        let client = HayStackClient::with_transport(config, transport);
        assert!(client.retrieve("q", None, None).await.is_ok());
    }

    #[tokio::test]
    async fn non_2xx_with_a_parseable_envelope_is_parsed_like_java() {
        // Java never enforced the HTTP status: it logged it and parsed the body, so a 500 that
        // still carries a valid `code` is judged purely by that code.
        let transport = ScriptedTransport::with_responses(vec![
            HttpResponse::new(500, r#"{"code":1001,"error":"boom"}"#),
            HttpResponse::new(500, r#"{"code":0,"documents":[]}"#),
        ]);
        let client = client(transport);

        let response = client.retrieve("q", None, None).await.unwrap();
        assert!(response.effective_documents().is_empty());

        let error = client.retrieve("q", None, None).await.unwrap_err();
        assert!(matches!(error, RagError::Api { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn unparseable_body_is_a_transport_error_regardless_of_status() {
        let transport = ScriptedTransport::with_responses(vec![
            HttpResponse::new(200, "not json"),
            HttpResponse::new(400, "not json"),
            HttpResponse::new(500, "<html>server error</html>"),
        ]);
        let client = client(transport);
        for _ in 0..3 {
            let error = client.retrieve("q", None, None).await.unwrap_err();
            assert!(matches!(error, RagError::Http(_)), "{error:?}");
        }
    }

    #[tokio::test]
    async fn empty_body_is_a_transport_error() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, "")]);
        let client = client(transport);
        let error = client.retrieve("q", None, None).await.unwrap_err();
        assert!(matches!(error, RagError::Http(_)), "{error:?}");
    }
}
