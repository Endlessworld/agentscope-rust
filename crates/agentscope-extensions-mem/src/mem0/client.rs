//! HTTP client for the Mem0 API, mirroring `Mem0Client`.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{MemError, Result};
use crate::http::{HttpTransport, ReqwestTransport};
use crate::mem0::dto::{
    Mem0AddRequest, Mem0AddResponse, Mem0ApiType, Mem0SearchRequest, Mem0SearchResponse,
    Mem0SearchResult,
};
use crate::retry::{RetryPolicy, RetryTransport};
use crate::telemetry::{InstrumentedTransport, MemMetrics};

/// Platform endpoints (official cloud service).
pub const PLATFORM_MEMORIES_ENDPOINT: &str = "/v1/memories/";
/// Platform search endpoint.
pub const PLATFORM_SEARCH_ENDPOINT: &str = "/v2/memories/search/";
/// Self-hosted add endpoint.
pub const SELF_HOSTED_MEMORIES_ENDPOINT: &str = "/memories";
/// Self-hosted search endpoint.
pub const SELF_HOSTED_SEARCH_ENDPOINT: &str = "/search";

/// HTTP client for the Mem0 API.
///
/// The transport is injectable so tests can run fully offline; see
/// [`Mem0Client::with_transport`].
pub struct Mem0Client {
    transport: Arc<dyn HttpTransport>,
    api_base_url: String,
    api_key: Option<String>,
    api_type: Mem0ApiType,
    add_endpoint: String,
    search_endpoint: String,
    /// Present for the `reqwest`-backed constructors, so a host can scrape counters.
    metrics: Option<Arc<MemMetrics>>,
}

impl std::fmt::Debug for Mem0Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mem0Client")
            .field("api_base_url", &self.api_base_url)
            .field("api_type", &self.api_type)
            .field("add_endpoint", &self.add_endpoint)
            .field("search_endpoint", &self.search_endpoint)
            .finish_non_exhaustive()
    }
}

impl Mem0Client {
    /// Builds a client with the Java default timeout (60s) and [`Mem0ApiType::Platform`].
    ///
    /// Returns [`MemError::Config`] with `"apiBaseUrl is required"` when
    /// `api_base_url` is empty or blank.
    pub fn new(api_base_url: impl Into<String>, api_key: Option<String>) -> Result<Self> {
        Self::with_api_type(
            api_base_url,
            api_key,
            Mem0ApiType::Platform,
            Duration::from_secs(60),
        )
    }

    /// Builds a client with an explicit deployment type and timeout.
    pub fn with_api_type(
        api_base_url: impl Into<String>,
        api_key: Option<String>,
        api_type: Mem0ApiType,
        timeout: Duration,
    ) -> Result<Self> {
        Self::with_api_type_and_policy(
            api_base_url,
            api_key,
            api_type,
            timeout,
            RetryPolicy::default(),
        )
    }

    /// Builds a client with an explicit deployment type, timeout and retry policy.
    ///
    /// The transport chain is `Retry(Instrumented(Reqwest))`: retries are counted by
    /// [`RetryTransport`], per-attempt counters/span by [`InstrumentedTransport`].
    /// [`RetryPolicy::default`] performs a single attempt, matching Java.
    pub fn with_api_type_and_policy(
        api_base_url: impl Into<String>,
        api_key: Option<String>,
        api_type: Mem0ApiType,
        timeout: Duration,
        policy: RetryPolicy,
    ) -> Result<Self> {
        let metrics = MemMetrics::shared();
        let instrumented: Arc<dyn HttpTransport> = Arc::new(InstrumentedTransport::new(
            Arc::new(ReqwestTransport::new(timeout)?),
            Arc::clone(&metrics),
        ));
        let transport: Arc<dyn HttpTransport> =
            Arc::new(RetryTransport::new(instrumented, policy));
        let mut client = Self::assemble(transport, api_base_url.into(), api_key, api_type)?;
        client.metrics = Some(metrics);
        Ok(client)
    }

    /// Builds a client on top of an injected transport (used by tests).
    ///
    /// Fails when `api_base_url` is empty or blank, mirroring Java
    /// `Mem0LongTermMemory.Builder.build()` (`"apiBaseUrl is required"`).
    pub fn with_transport(
        transport: Arc<dyn HttpTransport>,
        api_base_url: impl Into<String>,
        api_key: Option<String>,
        api_type: Mem0ApiType,
    ) -> Result<Self> {
        Self::assemble(transport, api_base_url.into(), api_key, api_type)
    }

    fn assemble(
        transport: Arc<dyn HttpTransport>,
        api_base_url: String,
        api_key: Option<String>,
        api_type: Mem0ApiType,
    ) -> Result<Self> {
        // Java `Mem0LongTermMemory.Builder.build()`:
        // `if (apiBaseUrl == null || apiBaseUrl.isEmpty()) throw new IllegalArgumentException(...)`.
        // Rust additionally rejects whitespace-only URLs, which would otherwise
        // produce the same host-less request URLs.
        let trimmed = api_base_url.trim();
        if trimmed.is_empty() {
            return Err(MemError::Config("apiBaseUrl is required".into()));
        }
        // Java strips a single trailing slash from the base URL.
        let api_base_url = trimmed.strip_suffix('/').unwrap_or(trimmed).to_string();
        let (add_endpoint, search_endpoint) = match api_type {
            Mem0ApiType::Platform => (
                PLATFORM_MEMORIES_ENDPOINT.to_string(),
                PLATFORM_SEARCH_ENDPOINT.to_string(),
            ),
            Mem0ApiType::SelfHosted => (
                SELF_HOSTED_MEMORIES_ENDPOINT.to_string(),
                SELF_HOSTED_SEARCH_ENDPOINT.to_string(),
            ),
        };
        Ok(Self {
            transport,
            api_base_url,
            api_key,
            api_type,
            add_endpoint,
            search_endpoint,
            metrics: None,
        })
    }

    /// The counters fed by the instrumented transport, when the client was built by
    /// one of the `reqwest`-backed constructors.
    pub fn metrics(&self) -> Option<&MemMetrics> {
        self.metrics.as_deref()
    }

    /// The configured API type.
    pub fn api_type(&self) -> Mem0ApiType {
        self.api_type
    }

    /// The resolved add endpoint path.
    pub fn add_endpoint(&self) -> &str {
        &self.add_endpoint
    }

    /// The resolved search endpoint path.
    pub fn search_endpoint(&self) -> &str {
        &self.search_endpoint
    }

    /// Builds the authentication headers, mirroring `Mem0Client.executePostRaw`:
    /// self-hosted uses `X-API-Key`, platform uses `Authorization: Token <key>`.
    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(key) = self.api_key.as_ref().filter(|k| !k.is_empty()) {
            match self.api_type {
                Mem0ApiType::SelfHosted => {
                    headers.push(("X-API-Key".to_string(), key.clone()));
                }
                Mem0ApiType::Platform => {
                    headers.push(("Authorization".to_string(), format!("Token {key}")));
                }
            }
        }
        headers
    }

    /// POSTs `body` to `endpoint`, tagging a non-2xx failure with the Java vendor and
    /// operation names (`Mem0Client#executePostRaw`).
    async fn post(&self, operation: &'static str, endpoint: &str, body: &str) -> Result<String> {
        let url = format!("{}{}", self.api_base_url, endpoint);
        self.transport
            .post_json(&url, &self.headers(), body)
            .await
            .map_err(|err| err.with_context("Mem0", operation))
    }

    /// Adds memories: `POST /v1/memories/` (platform) or `POST /memories` (self-hosted).
    pub async fn add(&self, request: &Mem0AddRequest) -> Result<Mem0AddResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self.post("add request", &self.add_endpoint, &body).await?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Searches memories.
    ///
    /// Supports both API response shapes, matching Java `Mem0Client.search`:
    /// a bare JSON array (v1.0) or an object with a `results` field (v1.1).
    ///
    /// `user_id` is re-mirrored into `filters` before serialization so a request
    /// built by field assignment / deserialization can never lose the tenant filter
    /// (Java `Mem0SearchRequest.setUserId` guarantees the same invariant).
    pub async fn search(&self, request: &Mem0SearchRequest) -> Result<Mem0SearchResponse> {
        let mut request = request.clone();
        request.sync_filters();
        let body = serde_json::to_string(&request)?;
        let raw = self.post("search request", &self.search_endpoint, &body).await?;
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            let results: Vec<Mem0SearchResult> = serde_json::from_str(trimmed)?;
            return Ok(Mem0SearchResponse {
                results: Some(results),
            });
        }
        if trimmed.is_empty() {
            return Err(MemError::Transport(
                "Mem0 API returned empty response".into(),
            ));
        }
        Ok(serde_json::from_str(trimmed)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::MockTransport;

    fn client(
        transport: Arc<MockTransport>,
        api_key: Option<&str>,
        api_type: Mem0ApiType,
    ) -> Mem0Client {
        Mem0Client::with_transport(
            transport,
            "https://mem0.test/",
            api_key.map(str::to_string),
            api_type,
        )
        .expect("valid base url")
    }

    #[test]
    fn blank_base_url_is_rejected_at_construction() {
        let transport = Arc::new(MockTransport::new());
        for blank in ["", "   ", "  \u{0009} "] {
            let err =
                Mem0Client::with_transport(transport.clone(), blank, None, Mem0ApiType::Platform)
                    .unwrap_err();
            assert!(matches!(&err, MemError::Config(_)));
            assert!(err.to_string().contains("apiBaseUrl is required"));
        }
    }

    #[test]
    fn new_rejects_blank_base_url() {
        assert!(matches!(
            Mem0Client::new("", None),
            Err(MemError::Config(_))
        ));
        assert!(matches!(
            Mem0Client::new("   ", None),
            Err(MemError::Config(_))
        ));
        assert!(Mem0Client::new("https://mem0.test", None).is_ok());
    }

    #[test]
    fn strips_single_trailing_slash_from_base_url() {
        let transport = Arc::new(MockTransport::new());
        let client = client(transport, None, Mem0ApiType::Platform);
        assert_eq!(client.add_endpoint(), PLATFORM_MEMORIES_ENDPOINT);
        assert_eq!(client.search_endpoint(), PLATFORM_SEARCH_ENDPOINT);
    }

    #[tokio::test]
    async fn base_url_is_trimmed_before_building_the_request_url() {
        let mock = MockTransport::new();
        mock.push_body("{}");
        let client = Mem0Client::with_transport(
            Arc::new(mock.clone()),
            "  https://mem0.test/  ",
            None,
            Mem0ApiType::Platform,
        )
        .unwrap();
        client.add(&Mem0AddRequest::new()).await.unwrap();
        assert_eq!(mock.requests()[0].0, "https://mem0.test/v1/memories/");
    }

    #[test]
    fn self_hosted_uses_self_hosted_endpoints() {
        let transport = Arc::new(MockTransport::new());
        let client = client(transport, None, Mem0ApiType::SelfHosted);
        assert_eq!(client.api_type(), Mem0ApiType::SelfHosted);
        assert_eq!(client.add_endpoint(), SELF_HOSTED_MEMORIES_ENDPOINT);
        assert_eq!(client.search_endpoint(), SELF_HOSTED_SEARCH_ENDPOINT);
    }

    #[tokio::test]
    async fn platform_auth_uses_token_scheme() {
        let mock = MockTransport::new();
        mock.push_body("{}");
        let client = client(
            Arc::new(mock.clone()),
            Some("secret"),
            Mem0ApiType::Platform,
        );
        let req = Mem0AddRequest::new().with_user_id("u");
        client.add(&req).await.unwrap();
        let headers = &mock.requests()[0].1;
        assert!(headers.contains(&("Authorization".to_string(), "Token secret".to_string())));
        assert!(mock.requests()[0].0.ends_with(PLATFORM_MEMORIES_ENDPOINT));
    }

    #[tokio::test]
    async fn self_hosted_auth_uses_x_api_key() {
        let mock = MockTransport::new();
        mock.push_body("{}");
        let client = client(
            Arc::new(mock.clone()),
            Some("secret"),
            Mem0ApiType::SelfHosted,
        );
        client.search(&Mem0SearchRequest::new()).await.unwrap();
        let headers = &mock.requests()[0].1;
        assert!(headers.contains(&("X-API-Key".to_string(), "secret".to_string())));
    }

    #[tokio::test]
    async fn blank_api_key_omits_authorization_header() {
        let mock = MockTransport::new();
        mock.push_body("{}");
        let client = client(Arc::new(mock.clone()), Some(""), Mem0ApiType::Platform);
        client.add(&Mem0AddRequest::new()).await.unwrap();
        let headers = &mock.requests()[0].1;
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Content-Type");
    }

    #[tokio::test]
    async fn search_rejects_empty_body() {
        let mock = MockTransport::new();
        mock.push_body("   ");
        let client = client(Arc::new(mock), None, Mem0ApiType::Platform);
        let err = client.search(&Mem0SearchRequest::new()).await.unwrap_err();
        assert!(matches!(err, MemError::Transport(_)));
    }

    /// Java `Mem0ClientTest.testAddRequestHttpError`: the status and body stay visible.
    #[tokio::test]
    async fn add_propagates_http_status_and_body() {
        let mock = MockTransport::new();
        mock.push_response(Err(MemError::http_status(
            401,
            "{\"detail\":\"unauthorized\"}",
        )));
        let client = client(Arc::new(mock), Some("bad"), Mem0ApiType::Platform);
        let err = client.add(&Mem0AddRequest::new()).await.unwrap_err();
        match err {
            MemError::HttpStatus { status, body, .. } => {
                assert_eq!(status, 401);
                assert!(body.contains("unauthorized"));
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    /// Java `Mem0ClientTest.testAddRequestInvalidJson` / `testSearchRequestInvalidJson`.
    #[tokio::test]
    async fn invalid_json_surfaces_as_a_json_error() {
        let mock = MockTransport::new();
        mock.push_body("not json");
        let client = client(Arc::new(mock.clone()), None, Mem0ApiType::Platform);
        assert!(matches!(
            client.add(&Mem0AddRequest::new()).await.unwrap_err(),
            MemError::Json(_)
        ));

        mock.push_body("also not json");
        assert!(matches!(
            client.search(&Mem0SearchRequest::new()).await.unwrap_err(),
            MemError::Json(_)
        ));
    }

    /// The tenant filter is restored from a deserialized request before it hits the wire.
    #[tokio::test]
    async fn search_wire_payload_always_carries_the_user_id_filter() {
        let mock = MockTransport::new();
        mock.push_body("{}");
        let client = client(Arc::new(mock.clone()), None, Mem0ApiType::Platform);
        let request: Mem0SearchRequest =
            serde_json::from_value(serde_json::json!({"query": "q", "user_id": "u"})).unwrap();
        assert!(request.filters.get("user_id").is_none());
        client.search(&request).await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&mock.requests()[0].2).unwrap();
        assert_eq!(body["filters"]["user_id"], "u");
    }

    /// The `reqwest`-backed constructors install the instrumented transport, so a host
    /// can scrape counters without rebuilding the client.
    #[test]
    fn default_construction_exposes_metrics() {
        let client = Mem0Client::new("https://mem0.test", None).unwrap();
        let metrics = client.metrics().expect("instrumented transport installed");
        assert_eq!(metrics.snapshot().requests, 0);
        // Injected transports are not instrumented: the caller owns that seam.
        let injected = self::client(Arc::new(MockTransport::new()), None, Mem0ApiType::Platform);
        assert!(injected.metrics().is_none());
    }

    /// MEM-02: a non-2xx failure carries the Java vendor *and* operation name, so a
    /// `401` can be told apart by the operation that raised it.
    #[tokio::test]
    async fn http_failures_name_the_vendor_and_the_operation() {
        let mock = MockTransport::new();
        mock.push_response(Err(MemError::http_status(401, "unauthorized")));
        let client = client(Arc::new(mock.clone()), Some("bad"), Mem0ApiType::Platform);
        let err = client.add(&Mem0AddRequest::new()).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Mem0 API add request failed with status 401: unauthorized"
        );

        mock.push_response(Err(MemError::http_status(500, "boom")));
        let err = client
            .search(&Mem0SearchRequest::new())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Mem0 API search request failed with status 500: boom"
        );
    }

    /// MEM-05: `add` is a write, but the retry decorator cannot tell it apart from a
    /// read, so it re-sends the identical body. Pinned here so the duplicate-write
    /// risk is a documented, tested property rather than an accident.
    #[tokio::test]
    async fn add_is_re_sent_verbatim_when_retries_are_enabled() {
        let mock = MockTransport::new();
        mock.push_response(Err(MemError::http_status(503, "busy")));
        mock.push_body("{}");
        let transport: Arc<dyn HttpTransport> = Arc::new(RetryTransport::new(
            Arc::new(mock.clone()),
            RetryPolicy::new(2, Duration::ZERO, Duration::ZERO, 1.0),
        ));
        let client = Mem0Client::with_transport(
            transport,
            "https://mem0.test",
            Some("k".to_string()),
            Mem0ApiType::Platform,
        )
        .unwrap();
        client
            .add(&Mem0AddRequest::new().with_user_id("u"))
            .await
            .unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "the add POST is retried");
        assert_eq!(requests[0].0, requests[1].0);
        assert_eq!(requests[0].2, requests[1].2, "the same write is re-sent");
    }
}
