//! HTTP client for the Mem0 API, mirroring `Mem0Client`.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{MemError, Result};
use crate::http::{HttpTransport, ReqwestTransport};
use crate::mem0::dto::{
    Mem0AddRequest, Mem0AddResponse, Mem0ApiType, Mem0SearchRequest, Mem0SearchResponse,
    Mem0SearchResult,
};

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
        let transport = Arc::new(ReqwestTransport::new(timeout)?);
        Ok(Self::assemble(
            transport,
            api_base_url.into(),
            api_key,
            api_type,
        ))
    }

    /// Builds a client on top of an injected transport (used by tests).
    pub fn with_transport(
        transport: Arc<dyn HttpTransport>,
        api_base_url: impl Into<String>,
        api_key: Option<String>,
        api_type: Mem0ApiType,
    ) -> Self {
        Self::assemble(transport, api_base_url.into(), api_key, api_type)
    }

    fn assemble(
        transport: Arc<dyn HttpTransport>,
        api_base_url: String,
        api_key: Option<String>,
        api_type: Mem0ApiType,
    ) -> Self {
        // Java strips a single trailing slash from the base URL.
        let api_base_url = api_base_url
            .strip_suffix('/')
            .unwrap_or(&api_base_url)
            .to_string();
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
        Self {
            transport,
            api_base_url,
            api_key,
            api_type,
            add_endpoint,
            search_endpoint,
        }
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

    async fn post(&self, endpoint: &str, body: &str) -> Result<String> {
        let url = format!("{}{}", self.api_base_url, endpoint);
        self.transport.post_json(&url, &self.headers(), body).await
    }

    /// Adds memories: `POST /v1/memories/` (platform) or `POST /memories` (self-hosted).
    pub async fn add(&self, request: &Mem0AddRequest) -> Result<Mem0AddResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self.post(&self.add_endpoint, &body).await?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Searches memories.
    ///
    /// Supports both API response shapes, matching Java `Mem0Client.search`:
    /// a bare JSON array (v1.0) or an object with a `results` field (v1.1).
    pub async fn search(&self, request: &Mem0SearchRequest) -> Result<Mem0SearchResponse> {
        let body = serde_json::to_string(request)?;
        let raw = self.post(&self.search_endpoint, &body).await?;
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
    }

    #[test]
    fn strips_single_trailing_slash_from_base_url() {
        let transport = Arc::new(MockTransport::new());
        let client = client(transport, None, Mem0ApiType::Platform);
        assert_eq!(client.add_endpoint(), PLATFORM_MEMORIES_ENDPOINT);
        assert_eq!(client.search_endpoint(), PLATFORM_SEARCH_ENDPOINT);
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
}
