//! HTTP client for the ReMe Memory API, mirroring `ReMeClient`.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{MemError, Result};
use crate::http::{HttpTransport, ReqwestTransport};
use crate::reme::dto::{ReMeAddRequest, ReMeAddResponse, ReMeSearchRequest, ReMeSearchResponse};
use crate::retry::{RetryPolicy, RetryTransport};
use crate::telemetry::{InstrumentedTransport, MemMetrics};

/// Add-memory endpoint.
pub const SUMMARY_ENDPOINT: &str = "/summary_personal_memory";
/// Search-memory endpoint.
pub const RETRIEVE_ENDPOINT: &str = "/retrieve_personal_memory";

/// HTTP client for the ReMe service.
///
/// Java's `ReMeClient` sends no authentication headers; only `Content-Type`.
pub struct ReMeClient {
    transport: Arc<dyn HttpTransport>,
    api_base_url: String,
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
            metrics: None,
        })
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

    async fn post(&self, endpoint: &str, body: &str) -> Result<String> {
        let url = format!("{}{}", self.api_base_url, endpoint);
        let raw = self
            .transport
            .post_json(&url, &Self::headers(), body)
            .await?;
        // Java returns an empty response object for null/blank bodies.
        if raw.trim().is_empty() {
            return Ok("{}".to_string());
        }
        Ok(raw)
    }

    /// Adds memories: `POST /summary_personal_memory`.
    pub async fn add(&self, request: &ReMeAddRequest) -> Result<ReMeAddResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self.post(SUMMARY_ENDPOINT, &body).await?;
        Ok(parse_or_default::<ReMeAddResponse>(&raw)?)
    }

    /// Searches memories: `POST /retrieve_personal_memory`.
    pub async fn search(&self, request: &ReMeSearchRequest) -> Result<ReMeSearchResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self.post(RETRIEVE_ENDPOINT, &body).await?;
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
        assert!(ReMeClient::with_transport(Arc::new(MockTransport::new()), "https://reme.test")
            .unwrap()
            .metrics()
            .is_none());
    }
}
