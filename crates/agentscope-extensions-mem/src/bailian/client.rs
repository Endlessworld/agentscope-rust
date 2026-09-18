//! HTTP client for the Bailian Memory API, mirroring `BailianMemoryClient`.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{MemError, Result};
use crate::http::{HttpTransport, ReqwestTransport};
use crate::bailian::dto::{
    BailianAddRequest, BailianAddResponse, BailianSearchRequest, BailianSearchResponse,
};

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
        let transport = Arc::new(ReqwestTransport::new(timeout)?);
        Self::with_transport(
            transport,
            api_key,
            api_base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        )
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
            return Err(MemError::Config("apiBaseUrl cannot be null or blank".into()));
        }
        let api_base_url = api_base_url.strip_suffix('/').unwrap_or(&api_base_url).to_string();
        Ok(Self { transport, api_base_url, api_key })
    }

    /// The configured base URL.
    pub fn api_base_url(&self) -> &str {
        &self.api_base_url
    }

    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("Authorization".to_string(), format!("Bearer {}", self.api_key)),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
    }

    async fn post(&self, endpoint: &str, body: &str) -> Result<String> {
        let url = format!("{}{}", self.api_base_url, endpoint);
        let raw = self.transport.post_json(&url, &self.headers(), body).await?;
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
        let raw = self.post(ADD_MEMORY_ENDPOINT, &body).await?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Searches memories: `POST /api/v2/apps/memory/memory_nodes/search`.
    pub async fn search(&self, request: &BailianSearchRequest) -> Result<BailianSearchResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self.post(SEARCH_MEMORY_ENDPOINT, &body).await?;
        Ok(serde_json::from_str(&raw)?)
    }
}
