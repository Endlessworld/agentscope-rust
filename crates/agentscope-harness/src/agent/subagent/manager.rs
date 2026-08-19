//! Pure agent factory and invoker, mirroring `DefaultAgentManager`. Knows how to
//! create agents from registered factories and invoke them with a prompt.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::{CallableAgent, RuntimeContext};
use agentscope_core::error::Result;
use agentscope_core::message::Msg;

use super::{SubagentDeclaration, SubagentEntry, SubagentFactory, SubagentMode};

/// Registry of subagent factories keyed by agent id, mirroring `DefaultAgentManager`.
pub struct DefaultAgentManager {
    factories: Mutex<BTreeMap<String, Arc<dyn SubagentFactory>>>,
    declarations: Mutex<BTreeMap<String, SubagentDeclaration>>,
}

impl DefaultAgentManager {
    /// Builds a manager from subagent entries (factories plus optional declarations).
    pub fn new(entries: &[SubagentEntry]) -> Self {
        let manager = Self {
            factories: Mutex::new(BTreeMap::new()),
            declarations: Mutex::new(BTreeMap::new()),
        };
        manager.refresh_entries(entries);
        manager
    }

    /// Replaces the current set of entries with a new snapshot.
    pub fn refresh_entries(&self, entries: &[SubagentEntry]) {
        let mut factories = BTreeMap::new();
        let mut declarations = BTreeMap::new();
        for e in entries {
            factories.insert(e.name.clone(), e.factory.clone());
            if let Some(decl) = &e.declaration {
                declarations.insert(e.name.clone(), decl.clone());
            }
        }
        *self.factories.lock().unwrap() = factories;
        *self.declarations.lock().unwrap() = declarations;
    }

    /// Atomic alias of `refresh_entries` used by dynamic middlewares.
    pub fn replace_agents(&self, entries: &[SubagentEntry]) {
        self.refresh_entries(entries);
    }

    /// Race-safe lookup-and-create. Returns `None` when no factory is registered
    /// or the declaration is PRIMARY-only (cannot be spawned as a subagent).
    pub fn create_agent_if_present(
        &self,
        agent_id: &str,
        parent_rc: &RuntimeContext,
    ) -> Option<Arc<dyn CallableAgent>> {
        if agent_id.is_empty() {
            return None;
        }
        let factory = self.factories.lock().unwrap().get(agent_id).cloned()?;
        if self.is_primary_only(agent_id) {
            return None;
        }
        Some(factory.create(parent_rc))
    }

    /// Whether `agent_id` is registered with a PRIMARY-only declaration.
    pub fn is_primary_only(&self, agent_id: &str) -> bool {
        self.declarations
            .lock()
            .unwrap()
            .get(agent_id)
            .map(|d| d.mode == SubagentMode::Primary)
            .unwrap_or(false)
    }

    /// Whether a factory is registered for the given agent id.
    pub fn has_agent(&self, agent_id: &str) -> bool {
        self.factories.lock().unwrap().contains_key(agent_id)
    }

    /// Optional declaration metadata for the given agent id (e.g. remote URL).
    pub fn get_declaration(&self, agent_id: &str) -> Option<SubagentDeclaration> {
        self.declarations.lock().unwrap().get(agent_id).cloned()
    }

    /// Returns the factory registered for the given agent id, if any.
    pub fn get_factory(&self, agent_id: &str) -> Option<Arc<dyn SubagentFactory>> {
        self.factories.lock().unwrap().get(agent_id).cloned()
    }

    /// Invokes an agent with a user prompt, returning the reply.
    pub async fn invoke_agent(
        &self,
        agent: &Arc<dyn CallableAgent>,
        prompt: &str,
    ) -> Result<Msg> {
        agent.call(vec![Msg::user(prompt)]).await
    }
}
