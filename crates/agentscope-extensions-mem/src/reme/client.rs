//! HTTP client for the ReMe Memory API, mirroring `ReMeClient`.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{MemError, Result};
use crate::http::{HttpTransport, ReqwestTransport};
use crate::reme::dto::{ReMeAddRequest, ReMeAddResponse, ReMeSearchRequest, ReMeSearchResponse};
use crate::retry::{RetryPolicy, RetryTransport};
use crate::telemetry::{InstrumentedTransport, MemMetrics};

/// Add-memory endpoint used when no override is configured.
pub const SUMMARY_ENDPOINT: &str = "/summary_personal_memory";
/// Search-memory endpoint used when no override is configured.
pub const RETRIEVE_ENDPOINT: &str = "/retrieve_personal_memory";

/// Endpoint paths used by [`ReMeClient`].
///
/// ReMe 0.4.x routes both operations to the job path (`POST /{job_name}`), which the
/// hard-coded defaults do not match; construct the client with [`ReMeEndpoints::for_job`] and
/// [`ReMeClient::with_endpoints`] to talk to that layout (#2005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReMeEndpoints {
    /// Path used by [`ReMeClient::add`].
    pub summary: String,
    /// Path used by [`ReMeClient::search`].
    pub retrieve: String,
}

impl Default for ReMeEndpoints {
    fn default() -> Self {
        Self {
            summary: SUMMARY_ENDPOINT.to_string(),
            retrieve: RETRIEVE_ENDPOINT.to_string(),
        }
    }
}

impl ReMeEndpoints {
    /// Both operations on one job path (`POST /{job_name}`), the ReMe 0.4.x layout.
    ///
    /// Fails with [`MemError::Config`] when `job_name` is blank.
    pub fn for_job(job_name: &str) -> Result<Self> {
        let job_name = job_name.trim().trim_matches('/');
        if job_name.is_empty() {
            return Err(MemError::Config("job_name is required".into()));
        }
        let path = format!("/{job_name}");
        Ok(Self {
            summary: path.clone(),
            retrieve: path,
        })
    }

    /// Ensures both paths are absolute (a leading `/`), as the URL is built by concatenation.
    fn normalized(self) -> Self {
        Self {
            summary: normalize_endpoint(&self.summary),
            retrieve: normalize_endpoint(&self.retrieve),
        }
    }
}

fn normalize_endpoint(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

/// HTTP client for the ReMe service.
///
/// Java's `ReMeClient` sends no authentication headers; only `Content-Type`.
pub struct ReMeClient {
    transport: Arc<dyn HttpTransport>,
    api_base_url: String,
    /// Endpoint paths (defaults: [`SUMMARY_ENDPOINT`] / [`RETRIEVE_ENDPOINT`]).
    endpoints: ReMeEndpoints,
    /// Present for the `reqwest`-backed constructors, so a host can scrape counters.
    metrics: Option<Arc<MemMetrics>>,
}

impl std::fmt::Debug for ReMeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReMeClient")
            .field("api_base_url", &self.api_base_url)
            .finish_non_exhaustive()
    }
}

impl ReMeClient {
    /// Builds a client with the Java default timeout (60s).
    ///
    /// Returns [`MemError::Config`] with `"apiBaseUrl is required"` when
    /// `api_base_url` is empty or blank.
    pub fn new(api_base_url: impl Into<String>) -> Result<Self> {
        Self::with_timeout(api_base_url, Duration::from_secs(60))
    }

    /// Builds a client with an explicit timeout.
    pub fn with_timeout(api_base_url: impl Into<String>, timeout: Duration) -> Result<Self> {
        Self::with_timeout_and_policy(api_base_url, timeout, RetryPolicy::default())
    }

    /// Builds a client with an explicit timeout and retry policy.
    ///
    /// The transport chain is `Retry(Instrumented(Reqwest))`; counters are exposed
    /// through [`Self::metrics`]. The default policy is a single attempt (Java parity).
    pub fn with_timeout_and_policy(
        api_base_url: impl Into<String>,
        timeout: Duration,
        policy: RetryPolicy,
    ) -> Result<Self> {
        let metrics = MemMetrics::shared();
        let instrumented: Arc<dyn HttpTransport> = Arc::new(InstrumentedTransport::new(
            Arc::new(ReqwestTransport::new(timeout)?),
            Arc::clone(&metrics),
        ));
        let transport: Arc<dyn HttpTransport> = Arc::new(RetryTransport::new(instrumented, policy));
        let mut client = Self::with_transport(transport, api_base_url)?;
        client.metrics = Some(metrics);
        Ok(client)
    }

    /// Builds a client on top of an injected transport (used by tests).
    ///
    /// Fails when `api_base_url` is empty or blank, mirroring the Java
    /// `ReMeLongTermMemory` builder (`"apiBaseUrl is required"`).
    pub fn with_transport(
        transport: Arc<dyn HttpTransport>,
        api_base_url: impl Into<String>,
    ) -> Result<Self> {
        let api_base_url = api_base_url.into();
        // Java `ReMeLongTermMemory.Builder`: `if (builder.apiBaseUrl == null ||
        // builder.apiBaseUrl.isEmpty()) throw new IllegalArgumentException(...)`.
        // Rust additionally rejects whitespace-only URLs, which would otherwise
        // produce the same host-less request URLs.
        let trimmed = api_base_url.trim();
        if trimmed.is_empty() {
            return Err(MemError::Config("apiBaseUrl is required".into()));
        }
        let api_base_url = trimmed.strip_suffix('/').unwrap_or(trimmed).to_string();
        Ok(Self {
            transport,
            api_base_url,
            endpoints: ReMeEndpoints::default(),
            metrics: None,
        })
    }

    /// Overrides the endpoint paths (e.g. [`ReMeEndpoints::for_job`] for ReMe 0.4.x).
    ///
    /// The paths are made absolute, so `"my_job"` and `"/my_job"` are equivalent.
    pub fn with_endpoints(mut self, endpoints: ReMeEndpoints) -> Self {
        self.endpoints = endpoints.normalized();
        self
    }

    /// The endpoint paths currently in use.
    pub fn endpoints(&self) -> &ReMeEndpoints {
        &self.endpoints
    }

    /// The counters fed by the instrumented transport, when the client was built by
    /// one of the `reqwest`-backed constructors.
    pub fn metrics(&self) -> Option<&MemMetrics> {
        self.metrics.as_deref()
    }

    /// The configured base URL.
    pub fn api_base_url(&self) -> &str {
        &self.api_base_url
    }

    fn headers() -> Vec<(String, String)> {
        vec![("Content-Type".to_string(), "application/json".to_string())]
    }

    /// POSTs `body` to `endpoint`, tagging a non-2xx failure with the Java vendor and
    /// operation names (`ReMeClient#executePost`).
    async fn post(&self, operation: &'static str, endpoint: &str, body: &str) -> Result<String> {
        let url = format!("{}{}", self.api_base_url, endpoint);
        let raw = self
            .transport
            .post_json(&url, &Self::headers(), body)
            .await
            .map_err(|err| err.with_context("ReMe", operation))?;
        // Java returns an empty response object for null/blank bodies.
        if raw.trim().is_empty() {
            return Ok("{}".to_string());
        }
        Ok(raw)
    }

    /// Adds memories: `POST {summary endpoint}` (default `/summary_personal_memory`).
    pub async fn add(&self, request: &ReMeAddRequest) -> Result<ReMeAddResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self
            .post("summary_personal_memory", &self.endpoints.summary, &body)
            .await?;
        Ok(parse_or_default::<ReMeAddResponse>(&raw)?)
    }

    /// Searches memories: `POST {retrieve endpoint}` (default `/retrieve_personal_memory`).
    pub async fn search(&self, request: &ReMeSearchRequest) -> Result<ReMeSearchResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self
            .post("retrieve_personal_memory", &self.endpoints.retrieve, &body)
            .await?;
        Ok(parse_or_default::<ReMeSearchResponse>(&raw)?)
    }
}

/// Parses a response body, falling back to `Default` for the literal `{}` (matching
/// Java's `fromJson("{}", responseType)` behaviour).
fn parse_or_default<T>(raw: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    if raw.trim() == "{}" {
        return Ok(T::default());
    }
    serde_json::from_str(raw).map_err(MemError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::MockTransport;

    #[test]
    fn blank_base_url_is_rejected_at_construction() {
        let transport = Arc::new(MockTransport::new());
        for blank in ["", "   ", " \u{0009} "] {
            let err = ReMeClient::with_transport(transport.clone(), blank).unwrap_err();
            assert!(matches!(&err, MemError::Config(_)));
            assert!(err.to_string().contains("apiBaseUrl is required"));
        }
    }

    #[test]
    fn new_rejects_blank_base_url() {
        assert!(matches!(ReMeClient::new(""), Err(MemError::Config(_))));
        assert!(matches!(ReMeClient::new("   "), Err(MemError::Config(_))));
        assert!(ReMeClient::new("https://reme.test").is_ok());
    }

    #[test]
    fn base_url_is_trimmed_and_trailing_slash_stripped() {
        let client =
            ReMeClient::with_transport(Arc::new(MockTransport::new()), "  https://reme.test/  ")
                .unwrap();
        assert_eq!(client.api_base_url(), "https://reme.test");
    }

    #[test]
    fn default_endpoints_use_the_legacy_paths() {
        let client =
            ReMeClient::with_transport(Arc::new(MockTransport::new()), "https://reme.test")
                .unwrap();
        assert_eq!(client.endpoints().summary, SUMMARY_ENDPOINT);
        assert_eq!(client.endpoints().retrieve, RETRIEVE_ENDPOINT);
    }

    #[tokio::test]
    async fn job_endpoints_route_both_operations_to_the_job_path() {
        let mock = MockTransport::new();
        mock.push_body("{}");
        mock.push_body("{}");
        let client = ReMeClient::with_transport(Arc::new(mock.clone()), "https://reme.test")
            .unwrap()
            .with_endpoints(ReMeEndpoints::for_job("jobs/demo").unwrap());

        client
            .add(&ReMeAddRequest::new("ws", Vec::new()))
            .await
            .unwrap();
        client.search(&ReMeSearchRequest::new()).await.unwrap();

        let urls: Vec<String> = mock.requests().into_iter().map(|r| r.0).collect();
        assert_eq!(
            urls,
            vec![
                "https://reme.test/jobs/demo".to_string(),
                "https://reme.test/jobs/demo".to_string()
            ]
        );
    }

    #[test]
    fn endpoint_overrides_are_made_absolute() {
        let client =
            ReMeClient::with_transport(Arc::new(MockTransport::new()), "https://reme.test")
                .unwrap()
                .with_endpoints(ReMeEndpoints {
                    summary: "add".to_string(),
                    retrieve: "/retrieve".to_string(),
                });
        assert_eq!(client.endpoints().summary, "/add");
        assert_eq!(client.endpoints().retrieve, "/retrieve");
    }

    #[test]
    fn blank_job_name_is_rejected() {
        assert!(matches!(
            ReMeEndpoints::for_job("   "),
            Err(MemError::Config(_))
        ));
        assert!(matches!(
            ReMeEndpoints::for_job("/"),
            Err(MemError::Config(_))
        ));
    }

    #[tokio::test]
    async fn add_posts_to_summary_endpoint() {
        let mock = MockTransport::new();
        mock.push_body("{}");
        let client =
            ReMeClient::with_transport(Arc::new(mock.clone()), "https://reme.test").unwrap();
        let response = client
            .add(&ReMeAddRequest::new("ws", Vec::new()))
            .await
            .unwrap();
        assert!(response.metadata.is_none());
        let request = &mock.requests()[0];
        assert_eq!(request.0, format!("https://reme.test{SUMMARY_ENDPOINT}"));
        assert_eq!(
            request.1,
            vec![("Content-Type".to_string(), "application/json".to_string())]
        );
        assert_eq!(request.2, "{\"workspace_id\":\"ws\",\"trajectories\":[]}");
    }

    #[tokio::test]
    async fn blank_response_body_falls_back_to_default() {
        let mock = MockTransport::new();
        mock.push_body("   ");
        let client = ReMeClient::with_transport(Arc::new(mock), "https://reme.test").unwrap();
        let response = client.search(&ReMeSearchRequest::new()).await.unwrap();
        assert!(response.answer.is_none());
        assert!(response.memories().is_empty());
    }

    #[test]
    fn default_construction_installs_metrics_and_policy_is_configurable() {
        let client = ReMeClient::new("https://reme.test").unwrap();
        assert!(client.metrics().is_some());
        let client = ReMeClient::with_timeout_and_policy(
            "https://reme.test",
            Duration::from_secs(5),
            RetryPolicy::new(2, Duration::from_millis(10), Duration::from_millis(20), 2.0),
        )
        .unwrap();
        assert!(client.metrics().is_some());
        assert!(
            ReMeClient::with_transport(Arc::new(MockTransport::new()), "https://reme.test")
                .unwrap()
                .metrics()
                .is_none()
        );
    }

    /// MEM-02: the ReMe client uses Java's endpoint-derived operation names
    /// (`ReMeClient#add` / `#search`).
    #[tokio::test]
    async fn http_failures_name_the_vendor_and_the_operation() {
        let mock = MockTransport::new();
        mock.push_response(Err(MemError::http_status(401, "unauthorized")));
        let client =
            ReMeClient::with_transport(Arc::new(mock.clone()), "https://reme.test").unwrap();
        let err = client
            .add(&ReMeAddRequest::new("ws", Vec::new()))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "ReMe API summary_personal_memory failed with status 401: unauthorized"
        );

        mock.push_response(Err(MemError::http_status(503, "busy")));
        let err = client.search(&ReMeSearchRequest::new()).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "ReMe API retrieve_personal_memory failed with status 503: busy"
        );
    }

    /// MEM-06: a non-2xx `add` surfaces the structured status/body (not only the
    /// rendered message) so a caller can branch on the code.
    #[tokio::test]
    async fn add_propagates_http_status_and_body() {
        let mock = MockTransport::new();
        mock.push_response(Err(MemError::http_status(500, "boom")));
        let client =
            ReMeClient::with_transport(Arc::new(mock.clone()), "https://reme.test").unwrap();
        let err = client
            .add(&ReMeAddRequest::new("ws", Vec::new()))
            .await
            .unwrap_err();
        match err {
            MemError::HttpStatus {
                vendor,
                operation,
                status,
                ref body,
            } => {
                assert_eq!(
                    (vendor, operation, status),
                    ("ReMe", "summary_personal_memory", 500)
                );
                assert_eq!(body, "boom");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    /// MEM-06: a malformed body is reported as [`MemError::Json`] (Java surfaces the
    /// Jackson failure raised by `executePost`'s `fromJson`).
    #[tokio::test]
    async fn invalid_json_surfaces_as_a_json_error() {
        let mock = MockTransport::new();
        mock.push_body("not json");
        mock.push_body("not json either");
        let client = ReMeClient::with_transport(Arc::new(mock), "https://reme.test").unwrap();
        assert!(matches!(
            client
                .add(&ReMeAddRequest::new("ws", Vec::new()))
                .await
                .unwrap_err(),
            MemError::Json(_)
        ));
        assert!(matches!(
            client.search(&ReMeSearchRequest::new()).await.unwrap_err(),
            MemError::Json(_)
        ));
    }
}
