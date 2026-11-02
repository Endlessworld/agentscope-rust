//! AgentCard discovery from Nacos, mirroring
//! `io.agentscope.core.nacos.a2a.discovery.NacosAgentCardResolver`.
//!
//! The Java resolver is blocking (the Nacos client call is blocking). Our
//! [`NacosAiService`] is async, so the resolver trait here is `async_trait`; a
//! synchronous adapter can wrap it with `block_on` where needed.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope_extensions_a2a_client::error::A2aError;
use agentscope_extensions_a2a_client::model::AgentCard;
use async_trait::async_trait;

use super::converter::convert_to_a2a_agent_card;
use super::model::A2aAgentCard;
use crate::service::{AgentCardListener, AiServiceError, NacosAiService};

/// A resolved AgentCard plus the moment it was fetched.
#[derive(Debug, Clone)]
struct CachedCard {
    card: A2aAgentCard,
    fetched_at: Instant,
}

/// Resolves AgentCards by agent name, mirroring
/// `io.agentscope.core.a2a.agent.card.AgentCardResolver`.
#[async_trait]
pub trait AgentCardResolver: Send + Sync {
    /// Returns the AgentCard for `agent_name`.
    async fn get_agent_card(&self, agent_name: &str) -> Result<A2aAgentCard, String>;
}

/// An [`AgentCardResolver`] backed by a Nacos A2A registry, mirroring
/// `NacosAgentCardResolver`.
///
/// Cards are cached after the first (subscribing) load; the subscription
/// updates the cache in the background via the supplied callback.
///
/// ## TTL (§3.3 P2)
///
/// Java's resolver caches forever and relies on the Nacos push channel. The Rust port can be
/// handed a service whose push channel is polling-based (see
/// [`crate::http::NacosHttpAiService`]), so a stale entry must be able to expire:
/// [`with_ttl`](Self::with_ttl) bounds how long a cached card is reused, and
/// [`invalidate`](Self::invalidate) drops an entry on demand. The default (`None`) keeps the
/// unbounded behaviour Java has.
pub struct NacosAgentCardResolver {
    service: Arc<dyn NacosAiService>,
    caches: Arc<Mutex<HashMap<String, CachedCard>>>,
    ttl: Option<Duration>,
}

impl NacosAgentCardResolver {
    pub fn new(service: Arc<dyn NacosAiService>) -> Self {
        Self {
            service,
            caches: Arc::new(Mutex::new(HashMap::new())),
            ttl: None,
        }
    }

    /// Makes cached cards expire after `ttl` (a zero TTL always re-fetches).
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Drops a cached card so the next resolution subscribes again.
    pub fn invalidate(&self, agent_name: &str) {
        self.caches.lock().unwrap().remove(agent_name);
    }

    /// Cached card when present and still fresh.
    fn cached(&self, agent_name: &str) -> Option<A2aAgentCard> {
        let guard = self.caches.lock().unwrap();
        let entry = guard.get(agent_name)?;
        match self.ttl {
            Some(ttl) if entry.fetched_at.elapsed() >= ttl => None,
            _ => Some(entry.card.clone()),
        }
    }
}

impl NacosAgentCardResolver {
    /// Resolves a card, preserving the Nacos error kind.
    ///
    /// The local [`AgentCardResolver`] trait reports failures as `String` (it predates the
    /// protocol trait); the protocol adapter needs the classification to report `NOT_FOUND` as
    /// [`A2aError::CardUnavailable`] instead of a transport failure (NACOS-02).
    pub async fn resolve_detailed(&self, agent_name: &str) -> Result<A2aAgentCard, AiServiceError> {
        if let Some(cached) = self.cached(agent_name) {
            return Ok(cached);
        }
        // An expired entry must not survive into the merge below, otherwise the stale card
        // would win over the freshly fetched one (`with_ttl(Duration::ZERO)` always refetches).
        self.caches.lock().unwrap().remove(agent_name);

        // Listener keeps the shared cache fresh on Nacos push events.
        let listener_caches = self.caches.clone();
        let listener: AgentCardListener = Arc::new(move |name, nacos_card| {
            let card = convert_to_a2a_agent_card(nacos_card);
            listener_caches.lock().unwrap().insert(
                name.to_string(),
                CachedCard {
                    card,
                    fetched_at: Instant::now(),
                },
            );
        });

        let fetched = self
            .service
            .subscribe_agent_card(agent_name, listener)
            .await?;
        let card = convert_to_a2a_agent_card(&fetched);

        // Merge: the listener may have written a newer value during the call.
        let mut guard = self.caches.lock().unwrap();
        let chosen = guard
            .get(agent_name)
            .map(|entry| entry.card.clone())
            .unwrap_or(card);
        guard.insert(
            agent_name.to_string(),
            CachedCard {
                card: chosen.clone(),
                fetched_at: Instant::now(),
            },
        );
        Ok(chosen)
    }
}

#[async_trait]
impl AgentCardResolver for NacosAgentCardResolver {
    async fn get_agent_card(&self, agent_name: &str) -> Result<A2aAgentCard, String> {
        self.resolve_detailed(agent_name)
            .await
            .map_err(|error| error.to_string())
    }
}

// ===========================================================================
// Protocol-side interop (NACOS-02)
// ===========================================================================

/// The resolver name reported to the A2A client (Java `getClass().getSimpleName()`).
const PROTOCOL_RESOLVER_NAME: &str = "NacosAgentCardResolver";

/// Implements the **protocol** card resolver
/// (`agentscope_extensions_a2a_client::card::AgentCardResolver`), so a Nacos-discovered card can
/// be consumed by [`A2aClient`](agentscope_extensions_a2a_client::client::A2aClient).
///
/// Mirrors Java `NacosAgentCardResolver implements
/// io.agentscope.core.a2a.agent.card.AgentCardResolver`. The existing cache / TTL / push
/// bookkeeping is reused; only the error shape differs (the protocol trait wants
/// [`A2aError`], and a Nacos `NOT_FOUND` must become [`A2aError::CardUnavailable`] so the client
/// fails fast with the agent name and this resolver's name).
impl agentscope_extensions_a2a_client::card::AgentCardResolver for NacosAgentCardResolver {
    fn get_agent_card(
        &self,
        agent_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<AgentCard, A2aError>> + Send + '_>> {
        // The trait ties the returned future to `&self` only, so the borrowed name is
        // copied before the async block captures it.
        let agent_name = agent_name.to_string();
        Box::pin(async move {
            let card = self
                .resolve_detailed(&agent_name)
                .await
                .map_err(|error| to_protocol_error(PROTOCOL_RESOLVER_NAME, &agent_name, error))?;
            Ok(card.inner)
        })
    }

    fn resolver_name(&self) -> &'static str {
        PROTOCOL_RESOLVER_NAME
    }
}

/// Maps a Nacos discovery failure onto the protocol [`A2aError`].
fn to_protocol_error(resolver: &str, agent_name: &str, error: AiServiceError) -> A2aError {
    if error.is_not_found() {
        return A2aError::CardUnavailable {
            agent_name: agent_name.to_string(),
            resolver: resolver.to_string(),
            detail: error.to_string(),
        };
    }
    match error {
        AiServiceError::Http { status, message } => A2aError::Http { status, message },
        other => A2aError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::model::NacosAgentCard;
    use crate::service::InMemoryNacosAiService;

    #[tokio::test]
    async fn resolves_and_caches_agent_card() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9000"));
        let resolver = NacosAgentCardResolver::new(svc.clone());

        let card = resolver.get_agent_card("remote").await.unwrap();
        assert_eq!(card.name, "remote");
        assert_eq!(card.url, "http://r:9000");

        // Second call is served from cache (no extra subscription work).
        let cached = resolver.get_agent_card("remote").await.unwrap();
        assert_eq!(cached, card);
    }

    #[tokio::test]
    async fn caches_are_per_resolver_instance() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9000"));
        let first = NacosAgentCardResolver::new(svc.clone());
        assert_eq!(
            first.get_agent_card("remote").await.unwrap().url,
            "http://r:9000"
        );

        // Nacos push updates the subscribed resolver's cache.
        let mut updated = NacosAgentCard::new("remote", "http://r:9000");
        updated.description = Some("updated".into());
        svc.fire_agent_card_update("remote", updated);
        assert_eq!(
            first
                .get_agent_card("remote")
                .await
                .unwrap()
                .description
                .as_deref(),
            Some("updated")
        );
    }

    #[tokio::test]
    async fn missing_card_is_error() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let resolver = NacosAgentCardResolver::new(svc);
        assert!(resolver.get_agent_card("nope").await.is_err());
    }

    /// §3.3 P2: without a TTL a cached card is reused forever (Java behaviour).
    #[tokio::test]
    async fn a_cached_card_never_expires_without_a_ttl() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9000"));
        let resolver = NacosAgentCardResolver::new(svc.clone());
        assert_eq!(
            resolver.get_agent_card("remote").await.unwrap().url,
            "http://r:9000"
        );

        // The card changes *without* a push reaching the subscriber.
        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9999"));
        assert_eq!(
            resolver.get_agent_card("remote").await.unwrap().url,
            "http://r:9000"
        );
    }

    /// §3.3 P2: a zero TTL always resolves through the service again.
    #[tokio::test]
    async fn a_zero_ttl_refetches_the_card() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9000"));
        let resolver = NacosAgentCardResolver::new(svc.clone()).with_ttl(Duration::ZERO);
        assert_eq!(
            resolver.get_agent_card("remote").await.unwrap().url,
            "http://r:9000"
        );

        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9999"));
        assert_eq!(
            resolver.get_agent_card("remote").await.unwrap().url,
            "http://r:9999"
        );
    }

    /// `invalidate` drops a cached entry even when no TTL is configured.
    #[tokio::test]
    async fn invalidate_drops_a_cached_card() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9000"));
        let resolver = NacosAgentCardResolver::new(svc.clone());
        assert_eq!(
            resolver.get_agent_card("remote").await.unwrap().url,
            "http://r:9000"
        );

        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9999"));
        resolver.invalidate("remote");
        assert_eq!(
            resolver.get_agent_card("remote").await.unwrap().url,
            "http://r:9999"
        );
    }

    /// NACOS-02: the protocol resolver feeds an A2A client and reports its own name.
    #[tokio::test]
    async fn the_protocol_resolver_feeds_the_a2a_client() {
        use agentscope_extensions_a2a_client::card::AgentCardResolver as ProtocolResolver;
        use agentscope_extensions_a2a_client::client::A2aClient;

        let svc = Arc::new(InMemoryNacosAiService::new());
        svc.set_agent_card("remote", NacosAgentCard::new("remote", "http://r:9000"));
        let resolver: Arc<dyn ProtocolResolver> = Arc::new(NacosAgentCardResolver::new(svc));

        assert_eq!(resolver.resolver_name(), "NacosAgentCardResolver");

        let client = A2aClient::new("remote", resolver);
        let card = client.agent_card().await.unwrap();
        assert_eq!(card.name, "remote");
        assert_eq!(card.url, "http://r:9000");
    }

    /// NACOS-02: a Nacos `NOT_FOUND` surfaces as `CardUnavailable` naming this resolver.
    #[tokio::test]
    async fn a_missing_card_is_reported_as_card_unavailable() {
        use agentscope_extensions_a2a_client::card::AgentCardResolver as ProtocolResolver;
        use agentscope_extensions_a2a_client::client::A2aClient;

        let svc = Arc::new(InMemoryNacosAiService::new());
        let resolver: Arc<dyn ProtocolResolver> = Arc::new(NacosAgentCardResolver::new(svc));
        let client = A2aClient::new("ghost", resolver);

        let error = client.agent_card().await.unwrap_err();
        match error {
            A2aError::CardUnavailable {
                agent_name,
                resolver,
                detail,
            } => {
                assert_eq!(agent_name, "ghost");
                assert_eq!(resolver, "NacosAgentCardResolver");
                assert!(detail.contains("NacosAgentCardResolver"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
