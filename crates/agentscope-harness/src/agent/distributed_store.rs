//! One-stop configuration for distributed/persistent storage components, mirroring
//! `DistributedStore`. Auto-wires the agent state store and isolation scope.
use std::sync::Arc;

use agentscope_core::state::AgentStateStore;

use super::isolation_scope::IsolationScope;
use super::sandbox::{SandboxGuard, SandboxSnapshotSpec};
use crate::filesystem::remote::store::BaseStore;

/// Configuration bundle for distributed and persistent storage.
///
/// Instead of configuring `AgentStateStore` and isolation individually, pass a single
/// `DistributedStore` to `HarnessAgent::builder().distributed_store(...)`.
#[derive(Clone)]
pub struct DistributedStore {
    pub agent_state_store: Option<Arc<dyn AgentStateStore>>,
    pub isolation_scope: IsolationScope,
    /// Remote key/value store (Java `DistributedStore.baseStore()`).
    ///
    /// Storage backends (COS/OSS/JDBC/...) project their own store into this slot so
    /// host code can build a `RemoteFilesystem` without re-plumbing the client.
    pub base_store: Option<Arc<dyn BaseStore>>,
    /// Snapshot builder for sandbox state (Java `sandboxSnapshotSpec()`).
    pub sandbox_snapshot_spec: Option<Arc<dyn SandboxSnapshotSpec>>,
    /// Distributed sandbox execution guard (Java `sandboxExecutionGuard()`).
    ///
    /// The slot is `Arc<dyn SandboxGuard>`, so it may hold **either** a distributed guard
    /// (JDBC / MySQL / PostgreSQL / Redis / MongoDB — cross-process locks) **or** a
    /// process-local one (the harness default [`SandboxExecutionGuard`](super::SandboxExecutionGuard),
    /// plus the COS/OSS object-store guards that delegate to it). Hosts must not assume
    /// cross-process serialisation: check
    /// [`SandboxGuard::is_distributed`](super::SandboxGuard::is_distributed) first.
    pub sandbox_execution_guard: Option<Arc<dyn SandboxGuard>>,
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
    base_store: Option<Arc<dyn BaseStore>>,
    sandbox_snapshot_spec: Option<Arc<dyn SandboxSnapshotSpec>>,
    sandbox_execution_guard: Option<Arc<dyn SandboxGuard>>,
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

    pub fn base_store(mut self, store: Arc<dyn BaseStore>) -> Self {
        self.base_store = Some(store);
        self
    }

    pub fn sandbox_snapshot_spec(mut self, spec: Arc<dyn SandboxSnapshotSpec>) -> Self {
        self.sandbox_snapshot_spec = Some(spec);
        self
    }

    pub fn sandbox_execution_guard(mut self, guard: Arc<dyn SandboxGuard>) -> Self {
        self.sandbox_execution_guard = Some(guard);
        self
    }

    pub fn build(self) -> DistributedStore {
        DistributedStore {
            agent_state_store: self.agent_state_store,
            isolation_scope: self.isolation_scope,
            base_store: self.base_store,
            sandbox_snapshot_spec: self.sandbox_snapshot_spec,
            sandbox_execution_guard: self.sandbox_execution_guard,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::sandbox::SandboxExecutionGuard;

    /// The three extra slots mirror the Java `DistributedStore` composite
    /// (`baseStore` / `sandboxSnapshotSpec` / `sandboxExecutionGuard`); omitting them must
    /// stay valid so existing two-field call sites keep working.
    #[test]
    fn builder_defaults_to_none_for_the_composite_slots() {
        let store = DistributedStore::builder().build();
        assert!(store.agent_state_store.is_none());
        assert!(store.base_store.is_none());
        assert!(store.sandbox_snapshot_spec.is_none());
        assert!(store.sandbox_execution_guard.is_none());
    }

    #[tokio::test]
    async fn builder_accepts_a_distributed_guard_behind_the_trait_object() {
        let guard: Arc<dyn SandboxGuard> = Arc::new(SandboxExecutionGuard::new());
        let store = DistributedStore::builder()
            .sandbox_execution_guard(guard)
            .build();
        let guard = store
            .sandbox_execution_guard
            .expect("guard slot must be populated");
        let key = crate::agent::sandbox::SandboxIsolationKey::new("user", "u1");
        let lease = guard.try_enter(key.clone()).await;
        assert!(!lease.is_noop());
        assert!(guard.is_held(&key));
        guard.release(&key).await;
        assert!(!guard.is_held(&key));
        assert!(
            !guard.is_distributed(),
            "the in-process default must not claim cross-process serialisation"
        );
    }
}
