//! Multi-database JDBC-backed [`DistributedStore`] facade with automatic dialect
//! detection, mirroring `JdbcDistributedStore`.
//!
//! Assembles the four JDBC components against a shared [`JdbcExecutor`] and dialect:
//! * [`JdbcAgentStateStore`] — agent session state
//! * [`JdbcStore`] — workspace filesystem KV
//! * [`JdbcSnapshotSpec`] — sandbox snapshots as BLOBs
//! * [`JdbcSandboxExecutionGuard`] — distributed lock via the dialect's lock strategy
//!
//! # Alignment with harness `DistributedStore`
//!
//! The harness [`DistributedStore`] currently exposes only `agent_state_store` and
//! `isolation_scope`; it has no fields for the base store / snapshot spec / execution
//! guard / message bus (see `EXTENSIONS_GAP_TRACKER.md` §2.2). This facade therefore keeps
//! the assembled components behind its own accessors (mirroring the Java facade's lazy
//! component getters) and exposes a [`JdbcDistributedStore::to_harness_store`] bridge that
//! projects the agent-state-store half into the harness type. When the harness type gains
//! the remaining fields, the bridge can populate them without changing this crate's
//! public surface.

use std::sync::Arc;

use agentscope_harness::agent::distributed_store::DistributedStore as HarnessDistributedStore;
use agentscope_harness::agent::isolation_scope::IsolationScope;

use crate::backend::{JdbcResult, SharedExecutor};
use crate::dialect::{detect_dialect, AbstractJdbcDialect};
use crate::sandbox::JdbcSandboxExecutionGuard;
use crate::snapshot::JdbcSnapshotSpec;
use crate::state::JdbcAgentStateStore;
use crate::store::JdbcStore;

/// Multi-database JDBC-backed distributed store facade.
pub struct JdbcDistributedStore {
    executor: SharedExecutor,
    dialect: Arc<dyn AbstractJdbcDialect>,
    agent_state_store: Arc<JdbcAgentStateStore>,
    base_store: Arc<JdbcStore>,
    snapshot_spec: Arc<JdbcSnapshotSpec>,
    execution_guard: Arc<JdbcSandboxExecutionGuard>,
    isolation_scope: IsolationScope,
}

impl std::fmt::Debug for JdbcDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JdbcDistributedStore")
            .field("kind", &self.dialect.kind())
            .finish_non_exhaustive()
    }
}

impl JdbcDistributedStore {
    /// Creates a store with auto-detected dialect (from a database product name) and
    /// automatic table creation.
    ///
    /// Detection matches the JDBC `DatabaseMetaData.getDatabaseProductName()` string; pass
    /// e.g. `"SQLite"`, `"MySQL"`, `"PostgreSQL"`, or `"H2"`.
    pub fn create(executor: SharedExecutor, product_name: &str) -> JdbcResult<Self> {
        Self::create_with_dialect(executor, detect_dialect(product_name))
    }

    /// Creates a store with an explicitly provided dialect.
    pub fn create_with_dialect(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
    ) -> JdbcResult<Self> {
        Self::create_with_dialect_and_schema(executor, dialect, true)
    }

    /// Creates a store with an explicit dialect and schema-creation control.
    pub fn create_with_dialect_and_schema(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
        initialize_schema: bool,
    ) -> JdbcResult<Self> {
        let agent_state_store = Arc::new(JdbcAgentStateStore::new(
            executor.clone(),
            dialect.clone(),
            initialize_schema,
        )?);
        let base_store = Arc::new(JdbcStore::new(
            executor.clone(),
            dialect.clone(),
            initialize_schema,
        )?);
        let snapshot_spec = Arc::new(JdbcSnapshotSpec::with_schema_control(
            executor.clone(),
            dialect.clone(),
            initialize_schema,
        )?);
        let execution_guard = Arc::new(JdbcSandboxExecutionGuard::table_lock(
            executor.clone(),
            dialect.clone(),
        )?);
        Ok(Self {
            executor,
            dialect,
            agent_state_store,
            base_store,
            snapshot_spec,
            execution_guard,
            isolation_scope: IsolationScope::default(),
        })
    }

    /// Overrides the isolation scope carried by the harness bridge.
    pub fn with_isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = scope;
        self
    }

    /// The agent state store component.
    pub fn agent_state_store(&self) -> Arc<JdbcAgentStateStore> {
        self.agent_state_store.clone()
    }

    /// The KV store component.
    pub fn base_store(&self) -> Arc<JdbcStore> {
        self.base_store.clone()
    }

    /// The sandbox snapshot spec component.
    pub fn sandbox_snapshot_spec(&self) -> Arc<JdbcSnapshotSpec> {
        self.snapshot_spec.clone()
    }

    /// The sandbox execution guard component.
    pub fn sandbox_execution_guard(&self) -> Arc<JdbcSandboxExecutionGuard> {
        self.execution_guard.clone()
    }

    /// The shared executor handle.
    pub fn executor(&self) -> SharedExecutor {
        self.executor.clone()
    }

    /// The detected/selected dialect.
    pub fn dialect(&self) -> Arc<dyn AbstractJdbcDialect> {
        self.dialect.clone()
    }

    /// Projects the agent-state-store half into the harness [`HarnessDistributedStore`].
    ///
    /// Used by `HarnessAgent::builder().distributed_store(...)`. The remaining JDBC
    /// components are reached via this facade's accessors until the harness type gains the
    /// corresponding fields.
    pub fn to_harness_store(&self) -> HarnessDistributedStore {
        HarnessDistributedStore {
            agent_state_store: Some(self.agent_state_store.clone() as Arc<dyn agentscope_core::state::AgentStateStore>),
            isolation_scope: self.isolation_scope.clone(),
        }
    }
}
