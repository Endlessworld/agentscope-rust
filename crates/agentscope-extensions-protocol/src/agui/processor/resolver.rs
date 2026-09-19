//! Agent resolution abstraction, mirroring `AgentResolver`.

use std::sync::Arc;

use agentscope_core::RuntimeContext;

use crate::agui::adapter::AguiStreamableAgent;
use crate::agui::error::AguiError;

/// Resolves agents from various sources.
///
/// The abstraction allows different strategies: a plain registry lookup, or a session-aware
/// resolver that creates and caches a per-thread agent instance.
pub trait AgentResolver: Send + Sync {
    /// Resolves an agent by id and thread id.
    ///
    /// # Errors
    ///
    /// Returns [`AguiError::AgentNotFound`] when no agent is registered under `agent_id`.
    fn resolve_agent(
        &self,
        agent_id: &str,
        thread_id: &str,
    ) -> Result<Arc<dyn AguiStreamableAgent>, AguiError>;

    /// Resolves an agent by id, thread id and user id.
    ///
    /// The default implementation ignores `user_id`. Session-aware resolvers should override this
    /// so tenants sharing a thread id do not also share an agent instance.
    fn resolve_agent_for_user(
        &self,
        agent_id: &str,
        thread_id: &str,
        _user_id: Option<&str>,
    ) -> Result<Arc<dyn AguiStreamableAgent>, AguiError> {
        self.resolve_agent(agent_id, thread_id)
    }

    /// Whether a thread already has memory/conversation history.
    ///
    /// This decides whether frontend-provided history is used or server-side memory is trusted.
    fn has_memory(&self, runtime_context: &RuntimeContext) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::registry::AguiAgentRegistry;
    use agentscope_core::{Agent, AgentEvent, Msg, StreamOptions, Toolkit};
    use std::sync::Mutex;

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

    /// A resolver backing `resolve_agent` with the AG-UI agent registry.
    struct RegistryResolver {
        registry: AguiAgentRegistry,
        threads_with_memory: Mutex<Vec<String>>,
    }

    impl AgentResolver for RegistryResolver {
        fn resolve_agent(
            &self,
            agent_id: &str,
            _thread_id: &str,
        ) -> Result<Arc<dyn AguiStreamableAgent>, AguiError> {
            self.registry
                .get_agent(agent_id)
                .ok_or_else(|| AguiError::AgentNotFound(agent_id.to_string()))
        }

        fn has_memory(&self, runtime_context: &RuntimeContext) -> bool {
            runtime_context
                .session_id()
                .is_some_and(|id| self.threads_with_memory.lock().unwrap().contains(&id.to_string()))
        }
    }

    #[test]
    fn a_registry_resolver_finds_registered_agents() {
        let registry = AguiAgentRegistry::new();
        registry.register("assistant", Arc::new(StubAgent::new("assistant")));
        let resolver = RegistryResolver {
            registry,
            threads_with_memory: Mutex::new(Vec::new()),
        };
        let agent = resolver.resolve_agent("assistant", "t").unwrap();
        assert_eq!(agent.agent_id(), "assistant");
        // The user-aware variant falls back to the two-argument method.
        assert_eq!(
            resolver
                .resolve_agent_for_user("assistant", "t", Some("u"))
                .unwrap()
                .agent_id(),
            "assistant"
        );
    }

    #[test]
    fn a_missing_agent_yields_agent_not_found() {
        let resolver = RegistryResolver {
            registry: AguiAgentRegistry::new(),
            threads_with_memory: Mutex::new(Vec::new()),
        };
        let error = match resolver.resolve_agent("nope", "t") {
            Ok(_) => panic!("expected AgentNotFound for an unregistered agent"),
            Err(error) => error,
        };
        assert_eq!(error, AguiError::AgentNotFound("nope".to_string()));
        assert_eq!(error.to_string(), "Agent not found: nope");
    }

    #[test]
    fn has_memory_is_driven_by_the_runtime_context_session() {
        let resolver = RegistryResolver {
            registry: AguiAgentRegistry::new(),
            threads_with_memory: Mutex::new(vec!["t1".to_string()]),
        };
        assert!(resolver.has_memory(&RuntimeContext::default().with_session_id("t1")));
        assert!(!resolver.has_memory(&RuntimeContext::default().with_session_id("t2")));
        assert!(!resolver.has_memory(&RuntimeContext::default()));
    }
}
