//! AgentCard discovery from Nacos, mirroring
//! `io.agentscope.core.nacos.a2a.discovery.NacosAgentCardResolver`.
//!
//! The Java resolver is blocking (the Nacos client call is blocking). Our
//! [`NacosAiService`] is async, so the resolver trait here is `async_trait`; a
//! synchronous adapter can wrap it with `block_on` where needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::converter::convert_to_a2a_agent_card;
use super::model::A2aAgentCard;
use crate::service::{AgentCardListener, NacosAiService};

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
pub struct NacosAgentCardResolver {
    service: Arc<dyn NacosAiService>,
    caches: Arc<Mutex<HashMap<String, A2aAgentCard>>>,
}

impl NacosAgentCardResolver {
    pub fn new(service: Arc<dyn NacosAiService>) -> Self {
        Self {
            service,
            caches: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl AgentCardResolver for NacosAgentCardResolver {
    async fn get_agent_card(&self, agent_name: &str) -> Result<A2aAgentCard, String> {
        if let Some(cached) = self.caches.lock().unwrap().get(agent_name) {
            return Ok(cached.clone());
        }

        // Listener keeps the shared cache fresh on Nacos push events.
        let listener_caches = self.caches.clone();
        let listener: AgentCardListener = Arc::new(move |name, nacos_card| {
            let card = convert_to_a2a_agent_card(nacos_card);
            listener_caches.lock().unwrap().insert(name.to_string(), card);
        });

        let fetched = self
            .service
            .subscribe_agent_card(agent_name, listener)
            .await
            .map_err(|e| e.to_string())?;
        let card = convert_to_a2a_agent_card(&fetched);

        // Merge: the listener may have written a newer value during the call.
        let mut guard = self.caches.lock().unwrap();
        let chosen = guard.get(agent_name).cloned().unwrap_or(card);
        guard.insert(agent_name.to_string(), chosen.clone());
        Ok(chosen)
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
        assert_eq!(first.get_agent_card("remote").await.unwrap().url, "http://r:9000");

        // Nacos push updates the subscribed resolver's cache.
        let mut updated = NacosAgentCard::new("remote", "http://r:9000");
        updated.description = Some("updated".into());
        svc.fire_agent_card_update("remote", updated);
        assert_eq!(
            first.get_agent_card("remote").await.unwrap().description.as_deref(),
            Some("updated")
        );
    }

    #[tokio::test]
    async fn missing_card_is_error() {
        let svc = Arc::new(InMemoryNacosAiService::new());
        let resolver = NacosAgentCardResolver::new(svc);
        assert!(resolver.get_agent_card("nope").await.is_err());
    }
}
