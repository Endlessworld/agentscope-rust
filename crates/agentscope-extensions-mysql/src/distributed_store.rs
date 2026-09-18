//! MySQL-backed [`DistributedStore`](agentscope_harness::agent::distributed_store::DistributedStore)
//! bundle, mirroring `io.agentscope.extensions.mysql.MysqlDistributedStore`.
//!
//! Assembles the four MySQL-backed components into a single configuration object:
//!
//! * [`MysqlAgentStateStore`] — agent session state.
//! * [`MysqlStore`] — workspace filesystem KV (a `BaseStore`).
//! * [`MysqlSnapshotSpec`] — sandbox snapshots as BLOBs.
//! * [`MysqlSandboxExecutionGuard`] — distributed lock via MySQL `GET_LOCK()`.
//!
//! ## Harness alignment (EXTENSIONS_GAP_TRACKER.md §2.1 / §2.2)
//! The harness `DistributedStore` currently exposes only `agent_state_store` +
//! `isolation_scope`, whereas the Java class also carries `base_store`,
//! `sandbox_snapshot_spec` and `sandbox_execution_guard`. Per §2.2 this is an open item
//! for the aggregator. To stay within this crate and avoid inventing parallel harness
//! types, `MysqlDistributedStore` keeps the full four-component bundle here and exposes
//! [`MysqlDistributedStore::agent_state_store`] (usable directly with the harness type),
//! plus accessors for the remaining components that the harness does not yet model.

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;
use agentscope_harness::agent::distributed_store::DistributedStore;
use agentscope_harness::agent::isolation_scope::IsolationScope;

use crate::guard::{LockBackend, MysqlSandboxExecutionGuard};
use crate::snapshot::{MysqlSnapshotSpec, SnapshotBackend};
use crate::state::{MysqlAgentStateStore, StateBackend};
use crate::store::{MysqlStore, SqlBackend};

/// MySQL/JDBC-backed distributed store bundle, mirroring `MysqlDistributedStore`.
///
/// Build one via [`MysqlDistributedStore::create`] (auto-creates schema) or
/// [`MysqlDistributedStore::builder`] for full control.
#[derive(Clone)]
pub struct MysqlDistributedStore {
    agent_state_store: Arc<MysqlAgentStateStore>,
    base_store: Arc<MysqlStore>,
    snapshot_spec: Arc<MysqlSnapshotSpec>,
    execution_guard: Arc<MysqlSandboxExecutionGuard>,
    isolation_scope: IsolationScope,
}

impl std::fmt::Debug for MysqlDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlDistributedStore")
            .field("isolation_scope", &self.isolation_scope)
            .finish_non_exhaustive()
    }
}

impl MysqlDistributedStore {
    /// Creates a MySQL distributed store with schema auto-creation, mirroring
    /// `MysqlDistributedStore.create(DataSource)`.
    pub fn create(
        state_backend: Arc<dyn StateBackend>,
        sql_backend: Arc<dyn SqlBackend>,
        snapshot_backend: Arc<dyn SnapshotBackend>,
        lock_backend: Arc<dyn LockBackend>,
    ) -> Self {
        MysqlDistributedStoreBuilder::new(
            state_backend,
            sql_backend,
            snapshot_backend,
            lock_backend,
        )
        .build()
    }

    /// Creates a builder for full control over components.
    pub fn builder(
        state_backend: Arc<dyn StateBackend>,
        sql_backend: Arc<dyn SqlBackend>,
        snapshot_backend: Arc<dyn SnapshotBackend>,
        lock_backend: Arc<dyn LockBackend>,
    ) -> MysqlDistributedStoreBuilder {
        MysqlDistributedStoreBuilder::new(
            state_backend,
            sql_backend,
            snapshot_backend,
            lock_backend,
        )
    }

    /// The agent session-state store.
    pub fn agent_state_store(&self) -> Arc<MysqlAgentStateStore> {
        Arc::clone(&self.agent_state_store)
    }

    /// The workspace filesystem KV store (`BaseStore`).
    pub fn base_store(&self) -> Arc<MysqlStore> {
        Arc::clone(&self.base_store)
    }

    /// The sandbox snapshot spec.
    pub fn sandbox_snapshot_spec(&self) -> Arc<MysqlSnapshotSpec> {
        Arc::clone(&self.snapshot_spec)
    }

    /// The sandbox execution guard.
    pub fn sandbox_execution_guard(&self) -> Arc<MysqlSandboxExecutionGuard> {
        Arc::clone(&self.execution_guard)
    }

    /// The configured isolation scope.
    pub fn isolation_scope(&self) -> IsolationScope {
        self.isolation_scope
    }

    /// Builds the harness [`DistributedStore`] wired with this bundle's agent state store
    /// and isolation scope (the subset the harness currently models — see §2.2).
    pub fn to_harness_distributed_store(&self) -> DistributedStore {
        DistributedStore::builder()
            .agent_state_store(self.agent_state_store.clone() as Arc<dyn AgentStateStore>)
            .isolation_scope(self.isolation_scope)
            .build()
    }
}

/// Builder for [`MysqlDistributedStore`], mirroring `MysqlDistributedStore` construction.
pub struct MysqlDistributedStoreBuilder {
    state_backend: Arc<dyn StateBackend>,
    sql_backend: Arc<dyn SqlBackend>,
    snapshot_backend: Arc<dyn SnapshotBackend>,
    lock_backend: Arc<dyn LockBackend>,
    store_table_name: String,
    snapshot_table_name: String,
    lock_key_prefix: String,
    isolation_scope: IsolationScope,
    initialize_schema: bool,
}

impl MysqlDistributedStoreBuilder {
    fn new(
        state_backend: Arc<dyn StateBackend>,
        sql_backend: Arc<dyn SqlBackend>,
        snapshot_backend: Arc<dyn SnapshotBackend>,
        lock_backend: Arc<dyn LockBackend>,
    ) -> Self {
        Self {
            state_backend,
            sql_backend,
            snapshot_backend,
            lock_backend,
            store_table_name: crate::store::DEFAULT_STORE_TABLE.to_string(),
            snapshot_table_name: crate::snapshot::DEFAULT_SNAPSHOT_TABLE.to_string(),
            lock_key_prefix: crate::guard::DEFAULT_KEY_PREFIX.to_string(),
            isolation_scope: IsolationScope::User,
            initialize_schema: true,
        }
    }

    /// Overrides the workspace KV table name.
    pub fn store_table_name(mut self, name: impl Into<String>) -> Self {
        self.store_table_name = name.into();
        self
    }

    /// Overrides the snapshot table name.
    pub fn snapshot_table_name(mut self, name: impl Into<String>) -> Self {
        self.snapshot_table_name = name.into();
        self
    }

    /// Overrides the sandbox lock-name prefix.
    pub fn lock_key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.lock_key_prefix = prefix.into();
        self
    }

    /// Sets the isolation scope (default: [`IsolationScope::User`]).
    pub fn isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = scope;
        self
    }

    /// When `true` (default), bootstraps all schemas on build.
    pub fn initialize_schema(mut self, initialize: bool) -> Self {
        self.initialize_schema = initialize;
        self
    }

    /// Builds the bundle.
    pub fn build(self) -> MysqlDistributedStore {
        let agent_state_store = if self.initialize_schema {
            MysqlAgentStateStore::with_config(self.state_backend.clone(), None, None, true)
                .unwrap_or_else(|_| MysqlAgentStateStore::with_config(
                    self.state_backend.clone(),
                    None,
                    None,
                    false,
                ).expect("state store construction"))
        } else {
            MysqlAgentStateStore::with_config(self.state_backend.clone(), None, None, false)
                .expect("state store construction")
        };

        let base_store = MysqlStore::builder(self.sql_backend.clone())
            .table_name(self.store_table_name.clone())
            .initialize_schema(self.initialize_schema)
            .build();

        let snapshot_spec = MysqlSnapshotSpec::with_table(
            self.snapshot_backend.clone(),
            self.snapshot_table_name.clone(),
        );

        let execution_guard = MysqlSandboxExecutionGuard::builder(self.lock_backend.clone())
            .key_prefix(self.lock_key_prefix.clone())
            .build();

        MysqlDistributedStore {
            agent_state_store: Arc::new(agent_state_store),
            base_store: Arc::new(base_store),
            snapshot_spec: Arc::new(snapshot_spec),
            execution_guard: Arc::new(execution_guard),
            isolation_scope: self.isolation_scope,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::filesystem::remote::store::BaseStore;
    use agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec as _;
    use crate::guard::testing::InMemoryLockBackend;
    use crate::snapshot::testing::InMemorySnapshotBackend;
    use crate::state::testing::InMemoryStateBackend;
    use crate::store::testing::InMemorySqlBackend;
    use async_trait::async_trait;

    fn bundle() -> MysqlDistributedStore {
        MysqlDistributedStore::create(
            Arc::new(InMemoryStateBackend::default()),
            Arc::new(InMemorySqlBackend::default()),
            Arc::new(InMemorySnapshotBackend::default()),
            Arc::new(InMemoryLockBackend::default()),
        )
    }

    #[tokio::test]
    async fn bundle_wires_all_components() {
        let b = bundle();
        let state = b.agent_state_store();
        state.save(Some("u"), "s", "k", serde_json::json!(1)).await;
        assert_eq!(
            state.get(Some("u"), "s", "k").await,
            Some(serde_json::json!(1))
        );

        let store = b.base_store();
        let mut v = agentscope_harness::filesystem::remote::store::StoreValue::new();
        v.insert("x".into(), serde_json::json!(2));
        store.put(&["ns".to_string()], "k", v);
        assert!(store.get(&["ns".to_string()], "k").is_some());

        let spec = b.sandbox_snapshot_spec();
        let snap = spec.build("snap1").expect("build");
        snap.persist(b"bytes").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());

        let guard = b.sandbox_execution_guard();
        let key = agentscope_harness::agent::sandbox::SandboxIsolationKey::new("user", "u");
        let _lease = guard.try_enter(&key).expect("lock");
        assert!(guard.is_held(&key));
    }

    #[test]
    fn to_harness_distributed_store_carries_state_store() {
        let b = bundle();
        let ds = b.to_harness_distributed_store();
        assert!(ds.agent_state_store.is_some());
        assert_eq!(ds.isolation_scope, IsolationScope::User);
    }

    #[test]
    fn custom_table_and_prefix_are_applied() {
        let b = MysqlDistributedStore::builder(
            Arc::new(InMemoryStateBackend::default()),
            Arc::new(InMemorySqlBackend::default()),
            Arc::new(InMemorySnapshotBackend::default()),
            Arc::new(InMemoryLockBackend::default()),
        )
        .store_table_name("custom_store")
        .snapshot_table_name("custom_snaps")
        .lock_key_prefix("app:lock:")
        .isolation_scope(IsolationScope::Session)
        .build();
        assert_eq!(b.isolation_scope(), IsolationScope::Session);
        let guard = b.sandbox_execution_guard();
        let key = agentscope_harness::agent::sandbox::SandboxIsolationKey::new("User", "u");
        assert_eq!(guard.compose_lock_name(&key), "app:lock:user:u");
    }

    #[allow(dead_code)]
    fn _assert_object_safe_usage(_store: Arc<dyn AgentStateStore>) {}

    #[allow(dead_code)]
    #[async_trait]
    trait _Unused {}
}
