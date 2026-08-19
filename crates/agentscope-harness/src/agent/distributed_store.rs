//! One-stop configuration for distributed/persistent storage components, mirroring
//! `DistributedStore`. Auto-wires the agent state store and isolation scope.
use std::sync::Arc;

use agentscope_core::state::AgentStateStore;

use super::isolation_scope::IsolationScope;

/// Configuration bundle for distributed and persistent storage.
///
/// Instead of configuring `AgentStateStore` and isolation individually, pass a single
/// `DistributedStore` to `HarnessAgent::builder().distributed_store(...)`.
#[derive(Clone)]
pub struct DistributedStore {
    pub agent_state_store: Option<Arc<dyn AgentStateStore>>,
    pub isolation_scope: IsolationScope,
}

impl DistributedStore {
    pub fn builder() -> DistributedStoreBuilder {
        DistributedStoreBuilder::default()
    }
}

/// Builder for composing a `DistributedStore`.
#[derive(Default)]
pub struct DistributedStoreBuilder {
    agent_state_store: Option<Arc<dyn AgentStateStore>>,
    isolation_scope: IsolationScope,
}

impl DistributedStoreBuilder {
    pub fn agent_state_store(mut self, store: Arc<dyn AgentStateStore>) -> Self {
        self.agent_state_store = Some(store);
        self
    }

    pub fn isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = scope;
        self
    }

    pub fn build(self) -> DistributedStore {
        DistributedStore {
            agent_state_store: self.agent_state_store,
            isolation_scope: self.isolation_scope,
        }
    }
}
