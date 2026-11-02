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
//! The harness `DistributedStore` carries `agent_state_store` + `isolation_scope` plus the
//! composite slots `base_store`, `sandbox_snapshot_spec` and `sandbox_execution_guard`
//! (tracker §2.2, resolved 2026-10-28). [`MysqlDistributedStore::to_harness_distributed_store`]
//! therefore projects all four MySQL components: the agent state store, the KV base store,
//! the snapshot spec, and the distributed execution guard (through the harness
//! `SandboxGuard` trait implemented by [`MysqlSandboxExecutionGuard`]).

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;
use agentscope_harness::agent::distributed_store::DistributedStore;
use agentscope_harness::agent::isolation_scope::IsolationScope;

use crate::error::MysqlResult;
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

    /// Connects the whole bundle to `url` over one [`crate::MysqlAsyncBackend`].
    ///
    /// Equivalent to `MysqlDistributedStore::builder(backend.clone(), backend.clone(),
    /// backend.clone(), backend).try_build()`: the driver adapter implements all four seams,
    /// so the caller does not have to name them. Construction fails eagerly (schema bootstrap,
    /// unreachable server) instead of handing back a half-configured bundle.
    #[cfg(feature = "mysql_async")]
    pub fn connect(url: &str) -> MysqlResult<Self> {
        Self::connect_with_config(url, crate::MysqlAsyncConfig::default())
    }

    /// Connects the whole bundle to `url` with explicit table names.
    ///
    /// This is what makes [`crate::MysqlAsyncConfig`] authoritative: `state_table`,
    /// `store_table` and `snapshot_table` are handed to the components that actually emit
    /// the statements (and `database` to the session-state store), so the table a write
    /// lands in is the table that was configured — previously the config only reached the
    /// driver adapter's own bookkeeping while the components kept their defaults.
    #[cfg(feature = "mysql_async")]
    pub fn connect_with_config(url: &str, config: crate::MysqlAsyncConfig) -> MysqlResult<Self> {
        let backend = crate::MysqlAsyncBackend::connect_with_config(url, config.clone())?;
        let state: Arc<dyn StateBackend> = backend.clone();
        let sql: Arc<dyn SqlBackend> = backend.clone();
        let snapshot: Arc<dyn SnapshotBackend> = backend.clone();
        let lock: Arc<dyn LockBackend> = backend;
        Self::builder(state, sql, snapshot, lock)
            .state_database_name(config.database)
            .state_table_name(config.state_table)
            .store_table_name(config.store_table)
            .snapshot_table_name(config.snapshot_table)
            .try_build()
    }

    /// Without the `mysql_async` feature there is no driver to build the bundle over, so this
    /// reports [`MysqlError`](crate::MysqlError)`::DriverUnavailable` explicitly instead of
    /// pretending to have a working store.
    #[cfg(not(feature = "mysql_async"))]
    pub fn connect(url: &str) -> MysqlResult<Self> {
        Err(crate::MysqlError::DriverUnavailable(format!(
            "the `mysql_async` feature is disabled; rebuild with `--features mysql_async` \
             to connect to {url}"
        )))
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

    /// Builds the harness [`DistributedStore`] wired with this bundle's four components
    /// (agent state store, base store, snapshot spec, execution guard) and isolation scope.
    pub fn to_harness_distributed_store(&self) -> DistributedStore {
        let state: Arc<dyn AgentStateStore> = self.agent_state_store.clone();
        let base: Arc<dyn agentscope_harness::filesystem::remote::store::BaseStore> =
            self.base_store.clone();
        let snapshots: Arc<dyn agentscope_harness::agent::sandbox::SandboxSnapshotSpec> =
            self.snapshot_spec.clone();
        let guard: Arc<dyn agentscope_harness::agent::sandbox::SandboxGuard> =
            self.execution_guard.clone();
        DistributedStore::builder()
            .agent_state_store(state)
            .isolation_scope(self.isolation_scope)
            .base_store(base)
            .sandbox_snapshot_spec(snapshots)
            .sandbox_execution_guard(guard)
            .build()
    }
}

/// Builder for [`MysqlDistributedStore`], mirroring `MysqlDistributedStore` construction.
pub struct MysqlDistributedStoreBuilder {
    state_backend: Arc<dyn StateBackend>,
    sql_backend: Arc<dyn SqlBackend>,
    snapshot_backend: Arc<dyn SnapshotBackend>,
    lock_backend: Arc<dyn LockBackend>,
    state_database_name: String,
    state_table_name: String,
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
            state_database_name: crate::state::DEFAULT_DATABASE_NAME.to_string(),
            state_table_name: crate::state::DEFAULT_TABLE_NAME.to_string(),
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

    /// Overrides the session-state database (default:
    /// [`crate::DEFAULT_DATABASE_NAME`]).
    pub fn state_database_name(mut self, name: impl Into<String>) -> Self {
        self.state_database_name = name.into();
        self
    }

    /// Overrides the session-state table (default: [`crate::DEFAULT_TABLE_NAME`]).
    pub fn state_table_name(mut self, name: impl Into<String>) -> Self {
        self.state_table_name = name.into();
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

    /// Builds the bundle, panicking if construction fails.
    ///
    /// Prefer [`MysqlDistributedStoreBuilder::try_build`] when the configuration comes from
    /// untrusted input: an invalid table name is an error there, not a panic.
    pub fn build(self) -> MysqlDistributedStore {
        self.try_build()
            .expect("MysqlDistributedStore construction failed")
    }

    /// Builds the bundle, surfacing invalid configuration as an error.
    ///
    /// Table names are validated here (they end up in DDL/DML verbatim), so a name that is
    /// not a plain `[A-Za-z_][A-Za-z0-9_]*` identifier fails loudly instead of being
    /// concatenated into SQL. Keeps the previous best-effort behaviour for schema
    /// bootstrapping: when `initialize_schema` is set and creating the schema fails, the
    /// state store is rebuilt without auto-creation — but the downgrade is now logged and
    /// the second attempt's error is returned (never swallowed).
    pub fn try_build(self) -> MysqlResult<MysqlDistributedStore> {
        let database = Some(self.state_database_name.as_str());
        let table = Some(self.state_table_name.as_str());
        let agent_state_store = if self.initialize_schema {
            match MysqlAgentStateStore::with_config(
                self.state_backend.clone(),
                database,
                table,
                true,
            ) {
                Ok(store) => store,
                Err(bootstrap_error) => {
                    tracing::warn!(
                        error = %bootstrap_error,
                        "schema auto-creation failed; verifying the existing schema instead"
                    );
                    MysqlAgentStateStore::with_config(
                        self.state_backend.clone(),
                        database,
                        table,
                        false,
                    )?
                }
            }
        } else {
            MysqlAgentStateStore::with_config(self.state_backend.clone(), database, table, false)?
        };

        let base_store = MysqlStore::builder(self.sql_backend.clone())
            .table_name(self.store_table_name.clone())?
            .initialize_schema(self.initialize_schema)
            .build();

        let snapshot_spec = MysqlSnapshotSpec::with_table(
            self.snapshot_backend.clone(),
            self.snapshot_table_name.clone(),
        )?;

        let execution_guard = MysqlSandboxExecutionGuard::builder(self.lock_backend.clone())
            .key_prefix(self.lock_key_prefix.clone())
            .build();

        Ok(MysqlDistributedStore {
            agent_state_store: Arc::new(agent_state_store),
            base_store: Arc::new(base_store),
            snapshot_spec: Arc::new(snapshot_spec),
            execution_guard: Arc::new(execution_guard),
            isolation_scope: self.isolation_scope,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::testing::InMemoryLockBackend;
    use crate::snapshot::testing::InMemorySnapshotBackend;
    use crate::state::testing::InMemoryStateBackend;
    use crate::store::testing::InMemorySqlBackend;
    use agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec as _;
    use agentscope_harness::filesystem::remote::store::BaseStore;
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

    /// Tracker §2.2 resolved 2026-10-28: every MySQL component is projected into the
    /// harness bundle, and the guard is usable through the object-safe `SandboxGuard`.
    #[tokio::test]
    async fn to_harness_distributed_store_fills_every_composite_slot() {
        let b = bundle();
        let ds = b.to_harness_distributed_store();

        let store = ds.base_store.as_ref().expect("base_store projected");
        let mut v = agentscope_harness::filesystem::remote::store::StoreValue::new();
        v.insert("projected".into(), serde_json::json!(true));
        store.put(&["ns".to_string()], "k", v);
        assert!(store.get(&["ns".to_string()], "k").is_some());

        let spec = ds
            .sandbox_snapshot_spec
            .as_ref()
            .expect("sandbox_snapshot_spec projected");
        assert_eq!(spec.build("projected").expect("build").id(), "projected");

        let guard = ds
            .sandbox_execution_guard
            .as_ref()
            .expect("sandbox_execution_guard projected");
        let key = agentscope_harness::agent::sandbox::SandboxIsolationKey::new("user", "u");
        let lease = guard.try_enter(key.clone()).await.expect("lease");
        assert!(!lease.is_noop());
        assert!(guard.is_held(&key));
        guard.release(&key).await;
        assert!(!guard.is_held(&key));
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

    /// MYSQL-11: the configured table names must be the ones the components actually use.
    /// `MysqlAsyncConfig::store_table` / `snapshot_table` used to be validated and then
    /// ignored, while the components kept their own defaults (and live integration tests
    /// dropped the configured tables, so a mismatch looked like a passing test).
    #[test]
    fn configured_table_names_reach_the_components() {
        let snapshot_backend = Arc::new(InMemorySnapshotBackend::default());
        let b = MysqlDistributedStore::builder(
            Arc::new(InMemoryStateBackend::default()),
            Arc::new(InMemorySqlBackend::default()),
            snapshot_backend.clone(),
            Arc::new(InMemoryLockBackend::default()),
        )
        .state_database_name("agentscope-prod")
        .state_table_name("sessions-prod")
        .store_table_name("store-prod")
        .snapshot_table_name("snaps-prod")
        .build();

        assert_eq!(b.agent_state_store().database_name(), "agentscope-prod");
        assert_eq!(b.agent_state_store().table_name(), "sessions-prod");
        assert_eq!(b.base_store().table_name(), "store-prod");
        // The snapshot DDL is issued for that table and quotes it (it contains `-`).
        let ddl = snapshot_backend
            .ddl_log()
            .first()
            .cloned()
            .expect("the spec bootstraps its table");
        assert!(
            ddl.contains("`snaps-prod`"),
            "snapshot DDL must target the configured table: {ddl}"
        );
    }

    #[allow(dead_code)]
    fn _assert_object_safe_usage(_store: Arc<dyn AgentStateStore>) {}

    // Table names are spliced into DDL/DML verbatim, so an injected name must fail
    // construction instead of reaching a statement.
    #[test]
    fn try_build_rejects_unsafe_table_names() {
        let bad_store = MysqlDistributedStore::builder(
            Arc::new(InMemoryStateBackend::default()),
            Arc::new(InMemorySqlBackend::default()),
            Arc::new(InMemorySnapshotBackend::default()),
            Arc::new(InMemoryLockBackend::default()),
        )
        .store_table_name("agentscope_store; DROP TABLE users")
        .try_build();
        assert!(bad_store.is_err(), "store table name must be validated");

        let bad_snapshot = MysqlDistributedStore::builder(
            Arc::new(InMemoryStateBackend::default()),
            Arc::new(InMemorySqlBackend::default()),
            Arc::new(InMemorySnapshotBackend::default()),
            Arc::new(InMemoryLockBackend::default()),
        )
        .snapshot_table_name("snaps`--")
        .try_build();
        assert!(
            bad_snapshot.is_err(),
            "snapshot table name must be validated"
        );
    }

    #[test]
    fn try_build_accepts_safe_table_names() {
        let built = MysqlDistributedStore::builder(
            Arc::new(InMemoryStateBackend::default()),
            Arc::new(InMemorySqlBackend::default()),
            Arc::new(InMemorySnapshotBackend::default()),
            Arc::new(InMemoryLockBackend::default()),
        )
        .store_table_name("custom_store")
        .snapshot_table_name("custom_snaps")
        .try_build();
        assert!(built.is_ok(), "safe identifiers must build: {built:?}");
    }

    #[allow(dead_code)]
    #[async_trait]
    trait _Unused {}
}
