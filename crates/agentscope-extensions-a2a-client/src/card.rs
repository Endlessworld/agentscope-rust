//! Agent card resolvers, mirroring `io.agentscope.core.a2a.agent.card`.
//!
//! * [`AgentCardResolver`] - the abstraction used by the A2A client to obtain the
//!   discovery document of a remote agent.
//! * [`FixedAgentCardResolver`] - always returns one statically configured card.
//! * [`WellKnownAgentCardResolver`] - fetches the card over HTTP from the well-known URI.
//!
//! Remote fetches are performed through the injectable [`CardFetcher`] trait so that the
//! resolvers stay unit-testable without network access.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::A2aError;
use crate::model::{AgentCard, WELL_KNOWN_AGENT_CARD_PATH};

/// Resolves the [`AgentCard`] of a remote agent by name.
///
/// Mirrors Java `io.agentscope.core.a2a.agent.card.AgentCardResolver`.
pub trait AgentCardResolver: Send + Sync {
    /// Produces the agent card for `agent_name`.
    fn get_agent_card(
        &self,
        agent_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<AgentCard, A2aError>> + Send + '_>>;
}

/// Always returns the same, statically configured card.
///
/// Mirrors Java `FixedAgentCardResolver`.
#[derive(Debug, Clone)]
pub struct FixedAgentCardResolver {
    agent_card: AgentCard,
}

impl FixedAgentCardResolver {
    /// Wraps a fixed card.
    pub fn new(agent_card: AgentCard) -> Self {
        Self { agent_card }
    }

    /// The configured card.
    pub fn agent_card(&self) -> &AgentCard {
        &self.agent_card
    }

    /// Returns the Java-style fluent builder.
    pub fn builder() -> FixedAgentCardResolverBuilder {
        FixedAgentCardResolverBuilder::default()
    }
}

/// Builder for [`FixedAgentCardResolver`], mirroring Java `FixedAgentCardProducerBuilder`.
#[derive(Debug, Default, Clone)]
pub struct FixedAgentCardResolverBuilder {
    agent_card: Option<AgentCard>,
}

impl FixedAgentCardResolverBuilder {
    pub fn agent_card(mut self, agent_card: AgentCard) -> Self {
        self.agent_card = Some(agent_card);
        self
    }

    pub fn build(self) -> FixedAgentCardResolver {
        FixedAgentCardResolver::new(
            self.agent_card
                .expect("FixedAgentCardResolver requires an agent card"),
        )
    }
}

impl AgentCardResolver for FixedAgentCardResolver {
    fn get_agent_card(
        &self,
        _agent_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<AgentCard, A2aError>> + Send + '_>> {
        let card = self.agent_card.clone();
        Box::pin(async move { Ok(card) })
    }
}

/// Transport-agnostic HTTP GET used to retrieve a well-known agent card.
///
/// This is the injection seam that keeps [`WellKnownAgentCardResolver`] offline-testable:
/// production code uses [`ReqwestCardFetcher`], tests use a stub.
pub trait CardFetcher: Send + Sync {
    fn get_json(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, A2aError>> + Send + '_>>;
}

/// [`CardFetcher`] backed by `reqwest`.
#[derive(Debug, Clone, Default)]
pub struct ReqwestCardFetcher {
    client: reqwest::Client,
}

impl ReqwestCardFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reuses an existing `reqwest` client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl CardFetcher for ReqwestCardFetcher {
    fn get_json(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, A2aError>> + Send + '_>> {
        let client = self.client.clone();
        let url = url.to_string();
        let headers = headers.clone();
        Box::pin(async move {
            let mut request = client.get(&url);
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let response = request.send().await.map_err(A2aError::transport)?;
            let status = response.status();
            if !status.is_success() {
                return Err(A2aError::Http {
                    status: status.as_u16(),
                    message: format!("agent card request to {url} failed"),
                });
            }
            response
                .json::<serde_json::Value>()
                .await
                .map_err(A2aError::transport)
        })
    }
}

/// Fetches an agent card from the A2A well-known URI.
///
/// Mirrors Java `WellKnownAgentCardResolver`, which delegates to
/// `A2A.getAgentCard(baseUrl, relativeCardPath, authHeaders)`. The well-known URI is the
/// concatenation of `base_url` and `relative_card_path`.
#[derive(Clone)]
pub struct WellKnownAgentCardResolver {
    base_url: String,
    relative_card_path: String,
    auth_headers: BTreeMap<String, String>,
    fetcher: Arc<dyn CardFetcher>,
}

impl std::fmt::Debug for WellKnownAgentCardResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WellKnownAgentCardResolver")
            .field("base_url", &self.base_url)
            .field("relative_card_path", &self.relative_card_path)
            .field(
                "auth_headers",
                &self.auth_headers.keys().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl WellKnownAgentCardResolver {
    /// The well-known URI computed from `base_url` + `relative_card_path`.
    pub fn well_known_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = self.relative_card_path.trim_start_matches('/');
        format!("{base}/{path}")
    }

    /// Convenience constructor using the default relative path and a `reqwest` fetcher.
    pub fn for_base_url(base_url: impl Into<String>) -> Self {
        WellKnownAgentCardResolverBuilder::default()
            .base_url(base_url)
            .build()
    }

    /// Returns the Java-style fluent builder.
    pub fn builder() -> WellKnownAgentCardResolverBuilder {
        WellKnownAgentCardResolverBuilder::default()
    }
}

impl AgentCardResolver for WellKnownAgentCardResolver {
    fn get_agent_card(
        &self,
        _agent_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<AgentCard, A2aError>> + Send + '_>> {
        Box::pin(async move {
            let url = self.well_known_url();
            let value = self.fetcher.get_json(&url, &self.auth_headers).await?;
            let card: AgentCard = serde_json::from_value(value).map_err(A2aError::json)?;
            Ok(card)
        })
    }
}

/// Builder for [`WellKnownAgentCardResolver`].
///
/// Defaults mirror Java: `relativeCardPath = "/.well-known/agent-card.json"`,
/// empty auth headers and a `reqwest` fetcher.
pub struct WellKnownAgentCardResolverBuilder {
    base_url: String,
    relative_card_path: String,
    auth_headers: BTreeMap<String, String>,
    fetcher: Option<Arc<dyn CardFetcher>>,
}

impl Default for WellKnownAgentCardResolverBuilder {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            relative_card_path: WELL_KNOWN_AGENT_CARD_PATH.to_string(),
            auth_headers: BTreeMap::new(),
            fetcher: None,
        }
    }
}

impl WellKnownAgentCardResolverBuilder {
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn relative_card_path(mut self, relative_card_path: impl Into<String>) -> Self {
        self.relative_card_path = relative_card_path.into();
        self
    }

    pub fn auth_headers(mut self, auth_headers: BTreeMap<String, String>) -> Self {
        self.auth_headers = auth_headers;
        self
    }

    /// Injects a custom HTTP transport (used by tests).
    pub fn fetcher(mut self, fetcher: Arc<dyn CardFetcher>) -> Self {
        self.fetcher = Some(fetcher);
        self
    }

    pub fn build(self) -> WellKnownAgentCardResolver {
        WellKnownAgentCardResolver {
            base_url: self.base_url,
            relative_card_path: self.relative_card_path,
            auth_headers: self.auth_headers,
            fetcher: self
                .fetcher
                .unwrap_or_else(|| Arc::new(ReqwestCardFetcher::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_resolver_returns_card_for_any_name() {
        let card = AgentCard::new("fixed", "http://example.com");
        let resolver = FixedAgentCardResolver::builder()
            .agent_card(card.clone())
            .build();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let fetched = rt.block_on(resolver.get_agent_card("whoever")).unwrap();
        assert_eq!(fetched, card);
    }

    #[test]
    fn well_known_url_joins_base_and_relative_path() {
        let resolver = WellKnownAgentCardResolver::builder()
            .base_url("http://localhost:8080/")
            .build();
        assert_eq!(
            resolver.well_known_url(),
            "http://localhost:8080/.well-known/agent-card.json"
        );
    }

    #[test]
    fn well_known_resolver_uses_injected_fetcher() {
        struct Stub;
        impl CardFetcher for Stub {
            fn get_json(
                &self,
                url: &str,
                _headers: &BTreeMap<String, String>,
            ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, A2aError>> + Send + '_>>
            {
                assert_eq!(url, "http://host:9000/.well-known/agent-card.json");
                Box::pin(async move {
                    Ok(serde_json::json!({
                        "name": "remote",
                        "url": "http://host:9000",
                        "capabilities": {},
                        "defaultInputModes": ["text"],
                        "defaultOutputModes": ["text"],
                        "skills": []
                    }))
                })
            }
        }

        let resolver = WellKnownAgentCardResolver::builder()
            .base_url("http://host:9000")
            .fetcher(Arc::new(Stub))
            .build();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let card = rt.block_on(resolver.get_agent_card("remote")).unwrap();
        assert_eq!(card.name, "remote");
    }
}
