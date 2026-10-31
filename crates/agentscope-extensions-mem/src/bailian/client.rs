//! HTTP client for the Bailian Memory API, mirroring `BailianMemoryClient`.

use std::sync::Arc;
use std::time::Duration;

use crate::bailian::dto::{
    BailianAddRequest, BailianAddResponse, BailianSearchRequest, BailianSearchResponse,
};
use crate::error::{MemError, Result};
use crate::http::{HttpTransport, ReqwestTransport};
use crate::retry::{RetryPolicy, RetryTransport};
use crate::telemetry::{InstrumentedTransport, MemMetrics};

/// Default base URL (`https://dashscope.aliyuncs.com`).
pub const DEFAULT_BASE_URL: &str = "https://dashscope.aliyuncs.com";
/// Add-memory endpoint.
pub const ADD_MEMORY_ENDPOINT: &str = "/api/v2/apps/memory/add";
/// Search-memory endpoint.
pub const SEARCH_MEMORY_ENDPOINT: &str = "/api/v2/apps/memory/memory_nodes/search";

/// HTTP client for the Bailian Long Term Memory service.
///
/// Authentication uses `Authorization: Bearer <apiKey>` (Java hard requirement).
pub struct BailianMemoryClient {
    transport: Arc<dyn HttpTransport>,
    api_base_url: String,
    api_key: String,
    /// Present for the `reqwest`-backed constructors, so a host can scrape counters.
    metrics: Option<Arc<MemMetrics>>,
}

impl std::fmt::Debug for BailianMemoryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BailianMemoryClient")
            .field("api_base_url", &self.api_base_url)
            .finish_non_exhaustive()
    }
}

impl BailianMemoryClient {
    /// Builds a client; rejects a blank API key like Java does.
    pub fn new(api_key: impl Into<String>, api_base_url: Option<String>) -> Result<Self> {
        Self::with_timeout(api_key, api_base_url, Duration::from_secs(60))
    }

    /// Builds a client with an explicit timeout.
    pub fn with_timeout(
        api_key: impl Into<String>,
        api_base_url: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        Self::with_timeout_and_policy(api_key, api_base_url, timeout, RetryPolicy::default())
    }

    /// Builds a client with an explicit timeout and retry policy.
    ///
    /// The transport chain is `Retry(Instrumented(Reqwest))`; counters are exposed
    /// through [`Self::metrics`]. The default policy is a single attempt (Java parity).
    pub fn with_timeout_and_policy(
        api_key: impl Into<String>,
        api_base_url: Option<String>,
        timeout: Duration,
        policy: RetryPolicy,
    ) -> Result<Self> {
        let metrics = MemMetrics::shared();
        let instrumented: Arc<dyn HttpTransport> = Arc::new(InstrumentedTransport::new(
            Arc::new(ReqwestTransport::new(timeout)?),
            Arc::clone(&metrics),
        ));
        let transport: Arc<dyn HttpTransport> = Arc::new(RetryTransport::new(instrumented, policy));
        let mut client = Self::with_transport(
            transport,
            api_key,
            api_base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        )?;
        client.metrics = Some(metrics);
        Ok(client)
    }

    /// Builds a client on top of an injected transport (used by tests).
    pub fn with_transport(
        transport: Arc<dyn HttpTransport>,
        api_key: impl Into<String>,
        api_base_url: impl Into<String>,
    ) -> Result<Self> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(MemError::Config("apiKey cannot be null or blank".into()));
        }
        let api_base_url = api_base_url.into();
        // Java `BailianMemoryClient` builder rejects a null / blank base URL.
        if api_base_url.trim().is_empty() {
            return Err(MemError::Config(
                "apiBaseUrl cannot be null or blank".into(),
            ));
        }
        let api_base_url = api_base_url
            .strip_suffix('/')
            .unwrap_or(&api_base_url)
            .to_string();
        Ok(Self {
            transport,
            api_base_url,
            api_key,
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

    fn headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
    }

    /// POSTs `body` to `endpoint`, tagging a non-2xx failure with the Java vendor and
    /// operation names (`BailianMemoryClient#executePost`).
    async fn post(&self, operation: &'static str, endpoint: &str, body: &str) -> Result<String> {
        let url = format!("{}{}", self.api_base_url, endpoint);
        let raw = self
            .transport
            .post_json(&url, &self.headers(), body)
            .await
            .map_err(|err| err.with_context("Bailian", operation))?;
        if raw.trim().is_empty() {
            return Err(MemError::Transport(
                "Bailian API returned empty response".into(),
            ));
        }
        Ok(raw)
    }

    /// Adds memories: `POST /api/v2/apps/memory/add`.
    pub async fn add(&self, request: &BailianAddRequest) -> Result<BailianAddResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self.post("add memory", ADD_MEMORY_ENDPOINT, &body).await?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Searches memories: `POST /api/v2/apps/memory/memory_nodes/search`.
    pub async fn search(&self, request: &BailianSearchRequest) -> Result<BailianSearchResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self.post("search memory", SEARCH_MEMORY_ENDPOINT, &body).await?;
        Ok(serde_json::from_str(&raw)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::MockTransport;

    #[test]
    fn default_construction_defaults_the_base_url_and_installs_metrics() {
        let client = BailianMemoryClient::new("key", None).unwrap();
        assert_eq!(client.api_base_url(), DEFAULT_BASE_URL);
        let metrics = client.metrics().expect("instrumented transport installed");
        assert_eq!(metrics.snapshot().requests, 0);
    }

    #[test]
    fn injected_transports_are_not_instrumented() {
        let client = BailianMemoryClient::with_transport(
            Arc::new(MockTransport::new()),
            "key",
            DEFAULT_BASE_URL,
        )
        .unwrap();
        assert!(client.metrics().is_none());
    }

    #[test]
    fn policy_constructor_is_equivalent_to_the_default_one() {
        let client = BailianMemoryClient::with_timeout_and_policy(
            "key",
            Some("https://bailian.test/".into()),
            Duration::from_secs(5),
            RetryPolicy::new(3, Duration::from_millis(10), Duration::from_millis(50), 2.0),
        )
        .unwrap();
        assert_eq!(client.api_base_url(), "https://bailian.test");
        assert!(client.metrics().is_some());
    }

    /// MEM-02: the Bailian client uses Java's `"add memory"` / `"search memory"`
    /// operation names (`BailianMemoryClient#executePost`).
    #[tokio::test]
    async fn http_failures_name_the_vendor_and_the_operation() {
        let mock = MockTransport::new();
        mock.push_response(Err(MemError::http_status(403, "forbidden")));
        let client = BailianMemoryClient::with_transport(
            Arc::new(mock.clone()),
            "key",
            DEFAULT_BASE_URL,
        )
        .unwrap();
        let err = client
            .add(&crate::bailian::dto::BailianAddRequest::new())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Bailian API add memory failed with status 403: forbidden"
        );

        mock.push_response(Err(MemError::http_status(429, "slow down")));
        let err = client
            .search(&crate::bailian::dto::BailianSearchRequest::new())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Bailian API search memory failed with status 429: slow down"
        );
    }
}
