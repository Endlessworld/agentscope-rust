//! Agent registry, mirroring `AguiAgentRegistry`.
//!
//! Supports two registration styles, exactly as Java does:
//!
//! * singleton agents - one shared instance returned for every request;
//! * factory agents - a new instance created per request.
//!
//! Factories take precedence over singletons when both are registered under the same id.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::agui::adapter::AguiStreamableAgent;

/// Creates a fresh agent instance for a factory-registered id.
pub trait AgentFactory: Send + Sync {
    /// Creates a new agent instance.
    fn create(&self) -> Arc<dyn AguiStreamableAgent>;
}

impl<F> AgentFactory for F
where
    F: Fn() -> Arc<dyn AguiStreamableAgent> + Send + Sync,
{
    fn create(&self) -> Arc<dyn AguiStreamableAgent> {
        self()
    }
}

/// Manages agents accessible through the AG-UI protocol.
#[derive(Default)]
pub struct AguiAgentRegistry {
    singleton_agents: RwLock<BTreeMap<String, Arc<dyn AguiStreamableAgent>>>,
    agent_factories: RwLock<BTreeMap<String, Arc<dyn AgentFactory>>>,
}

impl fmt::Debug for AguiAgentRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AguiAgentRegistry")
            .field("singleton_agents", &self.singleton_agent_ids())
            .field("agent_factories", &self.factory_ids())
            .finish()
    }
}

impl AguiAgentRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a singleton agent under `agent_id`.
    ///
    /// # Panics
    ///
    /// Panics when `agent_id` is empty, mirroring Java's
    /// `IllegalArgumentException("Agent ID cannot be null or empty")`.
    pub fn register(&self, agent_id: &str, agent: Arc<dyn AguiStreamableAgent>) {
        assert!(!agent_id.is_empty(), "Agent ID cannot be null or empty");
        self.singleton_agents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_string(), agent);
    }

    /// Registers a factory creating a new agent instance per request.
    ///
    /// # Panics
    ///
    /// Panics when `agent_id` is empty, mirroring Java's `IllegalArgumentException`.
    pub fn register_factory(&self, agent_id: &str, factory: Arc<dyn AgentFactory>) {
        assert!(!agent_id.is_empty(), "Agent ID cannot be null or empty");
        self.agent_factories
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_string(), factory);
    }

    /// Registers a closure as a factory.
    pub fn register_factory_fn<F>(&self, agent_id: &str, factory: F)
    where
        F: Fn() -> Arc<dyn AguiStreamableAgent> + Send + Sync + 'static,
    {
        self.register_factory(agent_id, Arc::new(factory));
    }

    /// Resolves an agent by id: a factory-created instance first, then the singleton.
    pub fn get_agent(&self, agent_id: &str) -> Option<Arc<dyn AguiStreamableAgent>> {
        let factory = self
            .agent_factories
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .cloned();
        if let Some(factory) = factory {
            return Some(factory.create());
        }
        self.singleton_agents
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .cloned()
    }

    /// Whether an agent is registered under `agent_id` (singleton or factory).
    pub fn has_agent(&self, agent_id: &str) -> bool {
        self.agent_factories
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(agent_id)
            || self
                .singleton_agents
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(agent_id)
    }

    /// Unregisters an agent by id, returning whether anything was removed.
    pub fn unregister(&self, agent_id: &str) -> bool {
        let removed_factory = self
            .agent_factories
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id)
            .is_some();
        let removed_singleton = self
            .singleton_agents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id)
            .is_some();
        removed_factory || removed_singleton
    }

    /// Removes every registration.
    pub fn clear(&self) {
        self.agent_factories
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.singleton_agents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// The number of registered agents (factories plus singletons).
    pub fn size(&self) -> usize {
        self.agent_factories
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            + self
                .singleton_agents
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .len()
    }

    /// The ids registered as singletons, in stable order.
    pub fn singleton_agent_ids(&self) -> Vec<String> {
        self.singleton_agents
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// The ids registered as factories, in stable order.
    pub fn factory_ids(&self) -> Vec<String> {
        self.agent_factories
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{Agent, AgentEvent, Msg, StreamOptions, Toolkit};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubAgent {
        id: String,
        toolkit: Toolkit,
    }

    impl StubAgent {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                toolkit: Toolkit::new(),
            }
        }
    }

    impl Agent for StubAgent {
        fn agent_id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.id
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> agentscope_core::AgentState {
            agentscope_core::AgentState::default()
        }
        fn toolkit(&self) -> &Toolkit {
            &self.toolkit
        }
    }

    impl AguiStreamableAgent for StubAgent {
        fn stream_events(
            &self,
            _messages: Vec<Msg>,
            _options: StreamOptions,
        ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[test]
    fn a_singleton_is_returned_for_every_lookup() {
        let registry = AguiAgentRegistry::new();
        let agent: Arc<dyn AguiStreamableAgent> = Arc::new(StubAgent::new("assistant"));
        registry.register("assistant", Arc::clone(&agent));
        assert!(registry.has_agent("assistant"));
        assert_eq!(registry.size(), 1);
        let first = registry.get_agent("assistant").unwrap();
        let second = registry.get_agent("assistant").unwrap();
        // The same instance is handed out both times.
        assert!(Arc::ptr_eq(&agent, &first));
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(registry.singleton_agent_ids(), vec!["assistant"]);
    }

    #[test]
    fn a_factory_creates_a_fresh_agent_per_lookup() {
        let registry = AguiAgentRegistry::new();
        let created = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&created);
        registry.register_factory_fn("chat", move || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            Arc::new(StubAgent::new(&format!("chat-{n}")))
        });
        assert!(registry.has_agent("chat"));
        assert_eq!(registry.factory_ids(), vec!["chat"]);
        let first = registry.get_agent("chat").unwrap();
        let second = registry.get_agent("chat").unwrap();
        assert_eq!(first.agent_id(), "chat-0");
        assert_eq!(second.agent_id(), "chat-1");
        assert_eq!(created.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn factories_take_precedence_over_singletons() {
        let registry = AguiAgentRegistry::new();
        registry.register("both", Arc::new(StubAgent::new("singleton")));
        registry.register_factory_fn("both", || Arc::new(StubAgent::new("factory")));
        assert_eq!(registry.get_agent("both").unwrap().agent_id(), "factory");
        // Both registrations still count towards the size, mirroring Java.
        assert_eq!(registry.size(), 2);
    }

    #[test]
    fn unknown_ids_resolve_to_nothing() {
        let registry = AguiAgentRegistry::new();
        assert!(registry.get_agent("missing").is_none());
        assert!(!registry.has_agent("missing"));
        assert!(!registry.unregister("missing"));
    }

    #[test]
    fn unregister_removes_both_registration_styles() {
        let registry = AguiAgentRegistry::new();
        registry.register("a", Arc::new(StubAgent::new("a")));
        registry.register_factory_fn("b", || Arc::new(StubAgent::new("b")));
        assert_eq!(registry.size(), 2);
        assert!(registry.unregister("a"));
        assert!(registry.unregister("b"));
        assert_eq!(registry.size(), 0);
        assert!(!registry.has_agent("a"));
    }

    #[test]
    fn clear_empties_the_registry() {
        let registry = AguiAgentRegistry::new();
        registry.register("a", Arc::new(StubAgent::new("a")));
        registry.register_factory_fn("b", || Arc::new(StubAgent::new("b")));
        registry.clear();
        assert_eq!(registry.size(), 0);
        assert!(registry.singleton_agent_ids().is_empty());
        assert!(registry.factory_ids().is_empty());
    }

    #[test]
    fn an_empty_agent_id_is_rejected() {
        let registry = AguiAgentRegistry::new();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.register("", Arc::new(StubAgent::new("x")))
        }));
        assert!(panicked.is_err());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.register_factory_fn("", || Arc::new(StubAgent::new("x")))
        }));
        assert!(panicked.is_err());
    }

    #[test]
    fn debug_lists_the_registered_ids() {
        let registry = AguiAgentRegistry::new();
        registry.register("single", Arc::new(StubAgent::new("single")));
        registry.register_factory_fn("made", || Arc::new(StubAgent::new("made")));
        let rendered = format!("{registry:?}");
        assert!(rendered.contains("AguiAgentRegistry"));
        assert!(rendered.contains("single"));
        assert!(rendered.contains("made"));
    }
}
