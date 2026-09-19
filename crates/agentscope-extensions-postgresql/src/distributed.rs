//! PostgreSQL-backed [`DistributedStore`] composer.
//!
//! Mirrors the Java `PostgresDistributedStore`: a single factory that wires the
//! PostgreSQL agent-state store, KV store, snapshot spec, and sandbox execution guard
//! together.
//!
//! **Harness note:** the harness `DistributedStore` currently only carries
//! `agent_state_store` + `isolation_scope`. The additional components (base store,
//! snapshot spec, guard) are exposed directly on this composer for callers to place
//! where needed, mirroring the Java aggregation without changing harness types. See the
//! module gap tracker §4.2.

use std::sync::Arc;

use agentscope_harness::agent::distributed_store::DistributedStore;

use crate::client::PostgresClient;
use crate::error::{PgResult, PostgresError};
use crate::guard::PostgresSandboxExecutionGuard;
use crate::snapshot::{PostgresRemoteSnapshotClient, PostgresSnapshotSpec};
use crate::state::PostgresAgentStateStore;
use crate::store::PostgresStore;

/// PostgreSQL-backed distributed store composer.
#[derive(Clone)]
pub struct PostgresDistributedStore {
    client: Arc<dyn PostgresClient>,
}

impl std::fmt::Debug for PostgresDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresDistributedStore").finish_non_exhaustive()
    }
}

impl PostgresDistributedStore {
    /// Creates a composer over a SQL client.
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self { client }
    }

    /// Creates a composer (factory alias, mirrors Java `PostgresDistributedStore.create`).
    pub fn create(client: Arc<dyn PostgresClient>) -> Self {
        Self::new(client)
    }

    /// The shared SQL client.
    pub fn client(&self) -> &Arc<dyn PostgresClient> {
        &self.client
    }

    /// Builds the agent-state store with schema/table auto-creation.
    pub fn agent_state_store(&self) -> PgResult<PostgresAgentStateStore> {
        PostgresAgentStateStore::create_if_not_exist(Arc::clone(&self.client))
    }

    /// Builds the KV [`BaseStore`](agentscope_harness::filesystem::BaseStore) with
    /// schema initialization enabled.
    pub fn base_store(&self) -> PgResult<PostgresStore> {
        PostgresStore::with_schema_init(Arc::clone(&self.client))
    }

    /// Builds the snapshot client (table auto-created).
    pub fn snapshot_client(&self) -> PgResult<PostgresRemoteSnapshotClient> {
        PostgresRemoteSnapshotClient::new(Arc::clone(&self.client), true)
            .map_err(|e| PostgresError::Schema(e.to_string()))
    }

    /// Builds the snapshot spec (table auto-created).
    pub fn snapshot_spec(&self) -> PgResult<PostgresSnapshotSpec> {
        PostgresSnapshotSpec::new(Arc::clone(&self.client))
            .map_err(|e| PostgresError::Schema(e.to_string()))
    }

    /// Builds the sandbox execution guard with default settings.
    pub fn sandbox_execution_guard(&self) -> PostgresSandboxExecutionGuard {
        PostgresSandboxExecutionGuard::new(Arc::clone(&self.client))
    }

    /// Builds a harness [`DistributedStore`] carrying the agent-state store.
    pub fn distributed_store(&self) -> PgResult<DistributedStore> {
        let state: Arc<dyn agentscope_core::state::AgentStateStore> =
            Arc::new(self.agent_state_store()?);
        Ok(DistributedStore::builder()
            .agent_state_store(state)
            .build())
    }
}

#[cfg(test)]
mod tests {
    //! Offline unit tests mirroring `PostgresDistributedStoreTest` (JUnit5 + Mockito). The
    //! composer is a pure factory over a shared `Arc<dyn PostgresClient>`, so the Java
    //! `assertInstanceOf` assertions become Rust type checks on the returned components.

    use super::*;
    use agentscope_core::state::AgentStateStore;
    use agentscope_harness::agent::isolation_scope::IsolationScope;
    use agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec;
    use agentscope_harness::filesystem::BaseStore;
    use crate::memory::MemoryClient;
    use serde_json::json;

    fn composer() -> (Arc<MemoryClient>, PostgresDistributedStore) {
        let c = Arc::new(MemoryClient::new());
        let as_client: Arc<dyn PostgresClient> = c.clone();
        let store = PostgresDistributedStore::create(as_client);
        (c, store)
    }

    // Java: PostgresDistributedStoreTest#createReturnsStore (the composer is a factory over
    // the shared client; sharing is observable through the underlying table).
    #[tokio::test]
    async fn create_returns_composer_sharing_the_client() {
        let (c, store) = composer();
        let state = store.agent_state_store().unwrap();
        state.save(None, "s1", "k", json!(1)).await;
        // The store wrote through the same client whose tables `c` observes.
        assert_eq!(c.session_row_count(), 1);
        assert_eq!(format!("{store:?}"), "PostgresDistributedStore { .. }");
    }

    // Java: PostgresDistributedStoreTest#agentStateStoreReturnsPostgresAgentStateStore
    #[test]
    fn agent_state_store_factory_provisions_sessions_table() {
        let (c, store) = composer();
        let state: PostgresAgentStateStore = store.agent_state_store().unwrap();
        assert_eq!(state.table_name(), crate::state::DEFAULT_TABLE_NAME);
        assert!(c.has_table(crate::state::DEFAULT_TABLE_NAME));
    }

    // Java: PostgresDistributedStoreTest#baseStoreReturnsPostgresBaseStore
    #[test]
    fn base_store_factory_provisions_store_table() {
        let (c, store) = composer();
        let base: PostgresStore = store.base_store().unwrap();
        assert_eq!(base.table_name(), crate::store::DEFAULT_TABLE_NAME);
        assert!(c.has_table(crate::store::DEFAULT_TABLE_NAME));
    }

    // Java: PostgresDistributedStoreTest#sandboxSnapshotSpecReturnsPostgresSnapshotSpec
    #[test]
    fn snapshot_factories_return_pg_types() {
        let (c, store) = composer();
        let spec: PostgresSnapshotSpec = store.snapshot_spec().unwrap();
        assert!(spec.build("snap-1").is_ok());
        let client: PostgresRemoteSnapshotClient = store.snapshot_client().unwrap();
        assert_eq!(client.table_name(), crate::snapshot::DEFAULT_TABLE);
        assert!(c.has_table(crate::snapshot::DEFAULT_TABLE));
    }

    // Java: PostgresDistributedStoreTest#sandboxExecutionGuardReturnsPostgresSandboxExecutionGuard
    #[test]
    fn sandbox_execution_guard_factory_returns_pg_guard() {
        let (_c, store) = composer();
        let guard: PostgresSandboxExecutionGuard = store.sandbox_execution_guard();
        let key = agentscope_harness::agent::sandbox::SandboxIsolationKey::new("session", "s1");
        let _lease = guard.try_enter(&key).unwrap();
    }

    // Java: PostgresDistributedStoreTest#createRejectsNullDataSource (Rust: a null client is
    // not representable; the analogue is the composer's projection contract below).

    // The composer's `distributed_store()` exposes only the agent-state store + isolation
    // scope (the harness `DistributedStore` has no base-store / snapshot / guard fields).
    #[test]
    fn distributed_store_projects_agent_state_store_only() {
        let (c, store) = composer();
        let ds: DistributedStore = store.distributed_store().unwrap();
        assert!(ds.agent_state_store.is_some());
        assert_eq!(ds.isolation_scope, IsolationScope::default());
        assert!(c.has_table(crate::state::DEFAULT_TABLE_NAME));
    }

    // The projected store is functional end-to-end through the harness trait object.
    #[tokio::test]
    async fn distributed_store_state_is_usable() {
        let (_c, store) = composer();
        let ds = store.distributed_store().unwrap();
        let state = ds.agent_state_store.unwrap();
        state.save(None, "s1", "k", json!({"n": 1})).await;
        assert_eq!(state.get(None, "s1", "k").await, Some(json!({"n": 1})));
        assert!(state.exists(None, "s1").await);
    }

    // All components share a single client/underlying tables (the Java composer holds one
    // `DataSource` and derives every component from it).
    #[tokio::test]
    async fn all_components_share_the_same_backend() {
        let (c, store) = composer();
        let state = store.agent_state_store().unwrap();
        let base = store.base_store().unwrap();
        state.save(None, "s1", "k", json!(1)).await;
        base.put(&["filesystem".to_string()], "kv", json!({"a": 1}).as_object().unwrap().clone());
        assert_eq!(c.session_row_count(), 1);
        assert_eq!(c.store_row_count(), 1);
        assert!(base.get(&["filesystem".to_string()], "kv").is_some());
    }
}
