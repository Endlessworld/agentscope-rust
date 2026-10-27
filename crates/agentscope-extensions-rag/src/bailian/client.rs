//! Bailian HTTP client.
//!
//! Ports `BailianClient`. Java delegated to the `bailian20231229` SDK, which issued
//! `POST https://{endpoint}/{workspaceId}/index/retrieve` with an ACS4-HMAC-SHA256
//! `Authorization` header. This port performs the same call through the shared
//! [`crate::transport`] seam so the whole path is testable offline.
//!
//! The per-request volatile signing inputs (the `x-acs-date` timestamp and the
//! `x-acs-signature-nonce`) are parameters of [`BailianClient::build_retrieve_http_request`]
//! rather than being read from the clock inside it, which keeps the wire contract
//! deterministically testable. [`BailianClient::retrieve`] supplies `Utc::now()` and a fresh
//! random nonce.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::bailian::config::BailianConfig;
use crate::bailian::model::{RetrieveRequest, RetrieveResponseBody};
use crate::bailian::query_history::QueryHistoryEntry;
use crate::bailian::request::build_retrieve_request;
use crate::bailian::signing::{
    api_date, build_authorization, hashed_payload, resolve_region, short_date, SignInput,
    API_ACTION, API_VERSION,
};
use crate::error::{RagError, Result};
use crate::transport::{HttpMethod, HttpRequest, HttpTransport};

/// Client for the Bailian knowledge base retrieve API.
pub struct BailianClient {
    config: BailianConfig,
    transport: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for BailianClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BailianClient")
            .field("workspace_id", &self.config.workspace_id)
            .field("endpoint", &self.config.endpoint)
            .finish_non_exhaustive()
    }
}

impl BailianClient {
    /// Creates a client with the real `reqwest` transport.
    pub fn from_config(config: BailianConfig) -> Result<Self> {
        let options = crate::transport::TransportOptions {
            connect_timeout: config.timeout,
            read_timeout: config.timeout,
            max_retries: config.max_retries,
            ..crate::transport::TransportOptions::default()
        };
        let transport = crate::transport::ReqwestTransport::new(&options)?;
        Ok(Self::with_transport(config, Arc::new(transport)))
    }

    /// Creates a client with an injected transport (tests / custom HTTP stacks).
    pub fn with_transport(config: BailianConfig, transport: Arc<dyn HttpTransport>) -> Self {
        Self { config, transport }
    }

    /// The active configuration.
    pub fn config(&self) -> &BailianConfig {
        &self.config
    }

    /// The workspace id (Java `getWorkspaceId`).
    pub fn workspace_id(&self) -> &str {
        &self.config.workspace_id
    }

    /// Builds the signed HTTP request without sending it.
    ///
    /// Exposed so tests can assert on the exact wire contract; `now` and `nonce` are the
    /// volatile signing inputs.
    pub fn build_retrieve_http_request(
        &self,
        request: &RetrieveRequest,
        now: DateTime<Utc>,
        nonce: &str,
    ) -> Result<HttpRequest> {
        let body = serde_json::to_string(request)?;
        let hashed = hashed_payload(&body);
        let date = api_date(now);
        let compact_date = short_date(&date);
        let region = resolve_region(crate::bailian::signing::PRODUCT, &self.config.endpoint);
        let path = format!(
            "/{}{}",
            request_path_segment(&self.config.workspace_id),
            super::RETRIEVE_PATH
        );

        // Lowercase names so the signed material and the emitted headers cannot diverge.
        let headers: Vec<(String, String)> = vec![
            ("host".to_string(), self.config.endpoint.clone()),
            ("x-acs-version".to_string(), API_VERSION.to_string()),
            ("x-acs-action".to_string(), API_ACTION.to_string()),
            ("x-acs-date".to_string(), date),
            ("x-acs-signature-nonce".to_string(), nonce.to_string()),
            ("accept".to_string(), "application/json".to_string()),
            (
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("x-acs-content-sha256".to_string(), hashed.clone()),
        ];

        let sign_input = SignInput {
            method: HttpMethod::Post.as_str(),
            path: &path,
            query: &[],
            headers: &headers,
            hashed_payload: &hashed,
            access_key_id: &self.config.access_key_id,
            access_key_secret: &self.config.access_key_secret,
            product: crate::bailian::signing::PRODUCT,
            region: &region,
            short_date: &compact_date,
        };
        let authorization = build_authorization(&sign_input);

        let mut http_request = HttpRequest::new(HttpMethod::Post, self.config.retrieve_url());
        http_request.headers = headers;
        http_request
            .headers
            .push(("authorization".to_string(), authorization));
        http_request.body = Some(body);
        Ok(http_request)
    }

    /// Retrieves documents for `query`.
    ///
    /// `limit` and `conversation_history` come from the call site, mirroring
    /// `retrieve(indexId, query, limit, conversationHistory)`. Both `indexId` and `query` are
    /// required; Java returned a failed `Mono`, the Rust port returns
    /// [`RagError::InvalidArgument`].
    pub async fn retrieve(
        &self,
        index_id: &str,
        query: &str,
        limit: Option<usize>,
        conversation_history: Option<&[QueryHistoryEntry]>,
    ) -> Result<RetrieveResponseBody> {
        if index_id.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "IndexId cannot be null or empty".to_string(),
            ));
        }
        if query.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "Query cannot be null or empty".to_string(),
            ));
        }

        let body =
            build_retrieve_request(&self.config, index_id, query, limit, conversation_history);
        let http_request =
            self.build_retrieve_http_request(&body, Utc::now(), &generate_nonce())?;

        let response = self.transport.execute(http_request).await?;
        if !response.is_success() {
            return Err(crate::transport::map_http_error(
                response.status_code,
                &response.body,
            ));
        }

        // The SDK reacted to a null body by logging a warning; an empty body is treated as an
        // empty envelope here, matching `parse_json_or_default` used by the sibling backends.
        let parsed: RetrieveResponseBody =
            crate::transport::parse_json_or_default(&response.body, "Bailian retrieve")?;

        if parsed.data.is_none() {
            tracing::warn!(
                workspace_id = %self.config.workspace_id,
                request_id = ?parsed.request_id,
                code = ?parsed.code,
                message = ?parsed.message,
                "Bailian returned no data for the query"
            );
        }
        tracing::debug!(
            index_id = %index_id,
            nodes = parsed.nodes().len(),
            "retrieved Bailian documents"
        );

        Ok(parsed)
    }
}

/// Percent-encoded workspace-id path segment (SDK `getEncodeParam`).
fn request_path_segment(workspace_id: &str) -> String {
    crate::bailian::signing::percent_encode(workspace_id)
}

/// Generates a fresh `x-acs-signature-nonce`.
///
/// The SDK used `Common.getNonce()` (a random hex string); a v4 UUID rendered without dashes
/// gives 32 hex characters of the same shape from an OS-entropy source.
fn generate_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bailian::config::{RerankConfig, RewriteConfig};
    use crate::bailian::query_history::QueryHistoryEntry;
    use crate::transport::HttpResponse;
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::collections::HashMap;
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

    fn config() -> BailianConfig {
        BailianConfig::builder()
            .access_key_id("testAccessKeyId")
            .access_key_secret("testAccessKeySecret")
            .workspace_id("llm-abc")
            .index_id("idx-1")
            .build()
            .unwrap()
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 19, 0, 0, 0).unwrap()
    }

    fn client(transport: Arc<ScriptedTransport>) -> BailianClient {
        BailianClient::with_transport(config(), transport)
    }

    fn ok_body() -> String {
        r#"{"Code":"0","Success":true,"Data":{"Nodes":[{"Text":"chunk","Score":0.9,"Metadata":{"doc_id":"d1","_id":"c1"}}]}}"#
            .to_string()
    }

    #[test]
    fn request_targets_the_java_sdk_endpoint_and_path() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let client = client(transport);
        let body = build_retrieve_request(client.config(), "idx-1", "hello", None, None);

        let request = client
            .build_retrieve_http_request(&body, fixed_now(), "abcdef0123456789")
            .unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(
            request.url,
            "https://bailian.cn-beijing.aliyuncs.com/llm-abc/index/retrieve"
        );
    }

    #[test]
    fn signed_inputs_and_authorization_header_match_the_sdk_golden_vector() {
        // Same secret/date/nonce/body as the Java probe => byte-identical signature.
        let transport = ScriptedTransport::with_responses(vec![]);
        let client = BailianClient::with_transport(
            BailianConfig::builder()
                .access_key_id("testAccessKeyId")
                .access_key_secret("testAccessKeySecret")
                .workspace_id("llm-abc")
                .build()
                .unwrap(),
            transport,
        );
        let body: RetrieveRequest =
            serde_json::from_str(r#"{"indexId":"idx-1","query":"hello"}"#).unwrap();

        let request = client
            .build_retrieve_http_request(&body, fixed_now(), "abcdef0123456789")
            .unwrap();

        // Body is serialized with the SDK's camelCase keys.
        assert_eq!(
            request.body.as_deref(),
            Some(r#"{"indexId":"idx-1","query":"hello"}"#)
        );
        assert_eq!(
            request.header("x-acs-content-sha256"),
            Some("9586b1cc0697f446ec91d6e171c8fde14dd3f0b194ae571d11f17ca6e2086d13")
        );
        assert_eq!(request.header("x-acs-action"), Some("Retrieve"));
        assert_eq!(request.header("x-acs-version"), Some("2023-12-29"));
        assert_eq!(request.header("x-acs-date"), Some("2026-09-19T00:00:00Z"));
        assert_eq!(
            request.header("host"),
            Some("bailian.cn-beijing.aliyuncs.com")
        );
        // No query parameters are sent, so the canonical query is empty.
        assert_eq!(
            request.header("authorization"),
            Some(
                "ACS4-HMAC-SHA256 \
                 Credential=testAccessKeyId/20260919/cn-beijing/bailian/aliyun_v4_request,\
                 SignedHeaders=content-type;host;x-acs-action;x-acs-content-sha256;\
                 x-acs-date;x-acs-signature-nonce;x-acs-version,\
                 Signature=fc8ca7b56fe6b406b74f52a8c3023b55d9acf0b41ce4eeaeec798058f7e0e0b6"
            )
        );
    }

    #[test]
    fn signed_headers_exclude_authorization_and_accept() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let client = client(transport);
        let body = build_retrieve_request(client.config(), "idx-1", "q", None, None);
        let request = client
            .build_retrieve_http_request(&body, fixed_now(), "nonce")
            .unwrap();
        let authorization = request.header("authorization").unwrap();
        assert!(!authorization.contains("accept"));
        assert!(!authorization.contains("authorization"));
        assert!(authorization.contains("x-acs-action"));
    }

    #[tokio::test]
    async fn retrieve_parses_the_response_and_forwards_config_knobs() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, ok_body())]);
        let cfg = BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("llm-abc")
            .index_id("idx-1")
            .dense_similarity_top_k(50)
            .sparse_similarity_top_k(40)
            .enable_reranking(true)
            .rerank_config(RerankConfig::default())
            .enable_rewrite(true)
            .rewrite_config(RewriteConfig::default())
            .save_retriever_history(true)
            .build()
            .unwrap();
        let mut filter = HashMap::new();
        filter.insert("tag".to_string(), "a".to_string());
        let transport2 = transport.clone();
        let client = BailianClient::with_transport(
            BailianConfig {
                search_filters: Some(vec![filter.clone()]),
                ..cfg
            },
            transport2,
        );

        let history = vec![
            QueryHistoryEntry::user("hi"),
            QueryHistoryEntry::assistant("hello"),
        ];
        let response = client
            .retrieve("idx-1", "q", Some(5), Some(&history))
            .await
            .unwrap();
        assert_eq!(response.code.as_deref(), Some("0"));
        assert_eq!(response.nodes().len(), 1);

        let sent = transport.last_request();
        let body: serde_json::Value = serde_json::from_str(sent.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["indexId"], "idx-1");
        assert_eq!(body["query"], "q");
        // Config top-K wins over the call-site limit.
        assert_eq!(body["denseSimilarityTopK"], 50);
        assert_eq!(body["sparseSimilarityTopK"], 40);
        assert_eq!(body["enableReranking"], true);
        assert_eq!(body["rerank"][0]["modelName"], "gte-rerank-hybrid");
        assert_eq!(body["enableRewrite"], true);
        assert_eq!(body["rewrite"][0]["modelName"], "conv-rewrite-qwen-1.8b");
        assert_eq!(body["saveRetrieverHistory"], true);
        assert_eq!(body["searchFilters"][0]["tag"], "a");
        assert_eq!(body["queryHistory"][0]["role"], "user");
        assert_eq!(body["queryHistory"][1]["content"], "hello");
    }

    #[tokio::test]
    async fn retrieve_rejects_blank_index_id_and_query() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let client = client(transport.clone());
        assert!(client.retrieve("", "q", None, None).await.is_err());
        assert!(client.retrieve("   ", "q", None, None).await.is_err());
        assert!(client.retrieve("idx", "", None, None).await.is_err());
        assert!(client.retrieve("idx", "  ", None, None).await.is_err());
        // Nothing was sent for the rejected calls.
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_success_status_maps_to_api_or_auth_errors() {
        // Scripted responses are replayed LIFO, so they are pushed in reverse call order.
        let transport = ScriptedTransport::with_responses(vec![
            HttpResponse::new(500, r#"{"message":"boom"}"#),
            HttpResponse::new(403, r#"{"message":"Forbidden"}"#),
            HttpResponse::new(200, ok_body()),
        ]);
        let client = client(transport);

        assert!(client.retrieve("idx-1", "q", None, None).await.is_ok());

        let auth = client.retrieve("idx-1", "q", None, None).await.unwrap_err();
        assert!(
            matches!(
                auth,
                RagError::Auth {
                    status_code: 403,
                    ..
                }
            ),
            "{auth:?}"
        );

        let api = client.retrieve("idx-1", "q", None, None).await.unwrap_err();
        assert!(
            matches!(
                api,
                RagError::Api {
                    status_code: 500,
                    ..
                }
            ),
            "{api:?}"
        );
    }

    #[tokio::test]
    async fn empty_response_body_parses_to_an_empty_envelope() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, "")]);
        let client = client(transport);
        let response = client.retrieve("idx-1", "q", None, None).await.unwrap();
        assert!(response.nodes().is_empty());
        assert_eq!(response.code, None);
    }

    #[test]
    fn nonce_is_a_32_char_lowercase_hex_string() {
        let a = super::generate_nonce();
        let b = super::generate_nonce();
        assert_eq!(a.len(), 32);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b, "nonces must differ between requests");
    }
}
