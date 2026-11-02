//! Real bridge implementation (`feature = "jdbc-bridge"`).
//!
//! [`MysqlJdbcStoreBuilder`] is the convenience constructor for the **jdbc** façade: it
//! encapsulates `connect_mysql` + [`MysqlDialect`] +
//! `JdbcDistributedStore::create_with_dialect_and_schema` and (by default) swaps the portable
//! table lock for the native `GET_LOCK` guard — which is what Java does by default, because
//! its `MysqlDialect` *is* the lock strategy there.

use std::sync::Arc;

use agentscope_extensions_jdbc::dialect::vendor::MysqlDialect;
use agentscope_extensions_jdbc::dialect::AbstractJdbcDialect;
use agentscope_extensions_jdbc::sandbox::NativeLockFlavor;
use agentscope_extensions_jdbc::{
    connect_mysql, JdbcAgentStateStore, JdbcDistributedStore, JdbcError, JdbcSandboxExecutionGuard,
    JdbcSnapshotSpec, JdbcStore, SharedExecutor,
};
use agentscope_harness::agent::distributed_store::DistributedStore;
use agentscope_harness::agent::isolation_scope::IsolationScope;

use crate::error::{MysqlError, MysqlResult};

/// A JDBC-façade distributed store connected to MySQL.
///
/// Wraps `agentscope_extensions_jdbc::JdbcDistributedStore` so callers get the same four
/// components the jdbc crate exposes (agent state, KV base store, snapshot spec, execution
/// guard) without assembling an executor + dialect by hand.
pub struct MysqlJdbcStore {
    inner: JdbcDistributedStore,
}

impl std::fmt::Debug for MysqlJdbcStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlJdbcStore")
            .field("kind", &self.inner.dialect().kind())
            .finish_non_exhaustive()
    }
}

impl MysqlJdbcStore {
    /// Connects with schema auto-creation, `IsolationScope::User` and the native lock guard.
    pub fn connect(url: impl Into<String>) -> MysqlResult<Self> {
        MysqlJdbcStoreBuilder::new(url).build()
    }

    /// The underlying jdbc façade (for anything this wrapper does not surface).
    pub fn jdbc_store(&self) -> &JdbcDistributedStore {
        &self.inner
    }

    /// Consumes the wrapper and hands back the jdbc façade.
    pub fn into_jdbc_store(self) -> JdbcDistributedStore {
        self.inner
    }

    /// The shared `JdbcExecutor` (the MySQL driver connection).
    pub fn executor(&self) -> SharedExecutor {
        self.inner.executor()
    }

    /// The agent session-state store.
    pub fn agent_state_store(&self) -> Arc<JdbcAgentStateStore> {
        self.inner.agent_state_store()
    }

    /// The workspace KV store (`BaseStore`).
    pub fn base_store(&self) -> Arc<JdbcStore> {
        self.inner.base_store()
    }

    /// The sandbox snapshot spec.
    pub fn sandbox_snapshot_spec(&self) -> Arc<JdbcSnapshotSpec> {
        self.inner.sandbox_snapshot_spec()
    }

    /// The sandbox execution guard.
    pub fn sandbox_execution_guard(&self) -> Arc<JdbcSandboxExecutionGuard> {
        self.inner.sandbox_execution_guard()
    }

    /// Projects every component into the harness [`DistributedStore`] bundle.
    pub fn to_harness_distributed_store(&self) -> DistributedStore {
        self.inner.to_harness_store()
    }
}

/// Builder for [`MysqlJdbcStore`], the equivalent of hand-assembling
/// `JdbcDistributedStore::create_with_dialect_and_schema(Arc::new(connect_mysql(url)), MysqlDialect)`.
#[derive(Debug, Clone)]
pub struct MysqlJdbcStoreBuilder {
    url: String,
    initialize_schema: bool,
    native_lock_guard: bool,
    isolation_scope: IsolationScope,
}

impl MysqlJdbcStoreBuilder {
    /// Starts from a MySQL URL (`mysql://user:password@host:port/database`).
    ///
    /// Defaults: schema auto-creation **on**, native `GET_LOCK` guard **on** (Java's
    /// `MysqlDialect.tryEnter` is the default lock path there), `IsolationScope::User`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            initialize_schema: true,
            native_lock_guard: true,
            isolation_scope: IsolationScope::User,
        }
    }

    /// Whether to run the dialect's `CREATE TABLE IF NOT EXISTS` DDL (default `true`).
    ///
    /// With `false` the existing schema is verified instead — a missing table fails
    /// construction rather than being created behind the operator's back.
    pub fn initialize_schema(mut self, initialize: bool) -> Self {
        self.initialize_schema = initialize;
        self
    }

    /// Whether the sandbox guard uses MySQL `GET_LOCK` (default `true`).
    ///
    /// `false` keeps the portable table lock, which is slower but needs no extra connection
    /// per lease.
    pub fn native_lock_guard(mut self, native: bool) -> Self {
        self.native_lock_guard = native;
        self
    }

    /// Overrides the isolation scope carried into the harness bundle (default
    /// `IsolationScope::User`).
    pub fn isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = scope;
        self
    }

    /// The configured URL (kept for diagnostics; it may carry a password, so callers should
    /// think twice before logging it).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Connects and assembles the store.
    ///
    /// Every failure mode surfaces here: an empty/invalid URL, an unreachable server, or a
    /// refused schema bootstrap. The wrapper never hands back a store whose first statement
    /// would fail.
    pub fn build(self) -> MysqlResult<MysqlJdbcStore> {
        let executor: SharedExecutor = Arc::new(connect_mysql(&self.url).map_err(bridge_error)?);
        let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(MysqlDialect::new());
        let mut store = JdbcDistributedStore::create_with_dialect_and_schema(
            executor,
            dialect,
            self.initialize_schema,
        )
        .map_err(bridge_error)?;
        if self.native_lock_guard {
            store = store.with_native_lock_guard(NativeLockFlavor::Mysql);
        }
        Ok(MysqlJdbcStore {
            inner: store.with_isolation_scope(self.isolation_scope),
        })
    }
}

/// Maps the jdbc error onto this crate's error.
///
/// "No URL / invalid URL / server unreachable" are all reported at construction, so they map
/// to [`MysqlError::DriverUnavailable`] (the same bucket `MysqlAsyncBackend::connect` uses);
/// statement-level and schema failures become [`MysqlError::Sql`].
fn bridge_error(error: JdbcError) -> MysqlError {
    match error {
        JdbcError::Configuration(message) => MysqlError::DriverUnavailable(message),
        JdbcError::Database(message) => MysqlError::Sql(message),
        JdbcError::Unsupported(message) => {
            MysqlError::Sql(format!("unsupported operation: {message}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_defaults_match_the_java_lock_path() {
        let builder = MysqlJdbcStoreBuilder::new("mysql://root@127.0.0.1:3306/agentscope");
        assert_eq!(builder.url(), "mysql://root@127.0.0.1:3306/agentscope");
        assert!(builder.initialize_schema, "schema bootstrap on by default");
        assert!(
            builder.native_lock_guard,
            "Java's MySQL lock path is GET_LOCK"
        );
        assert_eq!(builder.isolation_scope, IsolationScope::User);
    }

    #[test]
    fn builder_knobs_are_independent() {
        let builder = MysqlJdbcStoreBuilder::new("mysql://h/db")
            .initialize_schema(false)
            .native_lock_guard(false)
            .isolation_scope(IsolationScope::Session);
        assert!(!builder.initialize_schema);
        assert!(!builder.native_lock_guard);
        assert_eq!(builder.isolation_scope, IsolationScope::Session);
    }

    #[test]
    fn empty_url_reports_driver_unavailable() {
        // No server is contacted: `DriverExecutor::connect` rejects a blank configuration
        // before spawning the driver thread.
        let error = MysqlJdbcStore::connect("   ")
            .expect_err("a blank URL must not be dialled as localhost");
        assert!(
            matches!(error, MysqlError::DriverUnavailable(_)),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[test]
    fn malformed_url_reports_driver_unavailable() {
        // `Opts::from_url` rejects this before any socket is opened.
        let error = MysqlJdbcStore::connect("not-a-mysql-url")
            .expect_err("a malformed URL must not be treated as a server");
        assert!(
            matches!(error, MysqlError::DriverUnavailable(_)),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains("mysql"), "{error}");
    }
}
