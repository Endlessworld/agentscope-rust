//! HTTP client for the ReMe Memory API, mirroring `ReMeClient`.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{MemError, Result};
use crate::http::{HttpTransport, ReqwestTransport};
use crate::reme::dto::{ReMeAddRequest, ReMeAddResponse, ReMeSearchRequest, ReMeSearchResponse};

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
    pub fn new(api_base_url: impl Into<String>) -> Result<Self> {
        Self::with_timeout(api_base_url, Duration::from_secs(60))
    }

    /// Builds a client with an explicit timeout.
    pub fn with_timeout(api_base_url: impl Into<String>, timeout: Duration) -> Result<Self> {
        let transport = Arc::new(ReqwestTransport::new(timeout)?);
        Ok(Self::with_transport(transport, api_base_url))
    }

    /// Builds a client on top of an injected transport (used by tests).
    pub fn with_transport(
        transport: Arc<dyn HttpTransport>,
        api_base_url: impl Into<String>,
    ) -> Self {
        let api_base_url = api_base_url.into();
        let api_base_url = api_base_url.strip_suffix('/').unwrap_or(&api_base_url).to_string();
        Self { transport, api_base_url }
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
        let raw = self.transport.post_json(&url, &Self::headers(), body).await?;
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
