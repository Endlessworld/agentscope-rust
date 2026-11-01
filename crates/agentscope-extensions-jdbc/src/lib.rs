//! agentscope-extensions-jdbc — multi-database JDBC dialect abstraction for
//! AgentScope Rust, mirroring `io.agentscope.extensions.jdbc`.
//!
//! One aggregated [`dialect::AbstractJdbcDialect`] serves four vendor dialects — MySQL,
//! PostgreSQL, H2-style ANSI and SQLite — so every component below is database-agnostic and free
//! of inline SQL.
//!
//! # Execution backends
//!
//! * [`backend::RusqliteExecutor`] — SQLite (including in-memory), always built, fully offline;
//!   mirrors the Java H2/SQLite test backends.
//! * `backend::driver::PostgresExecutor` — real PostgreSQL over `tokio-postgres`, opt-in via the
//!   `postgres-executor` feature (`backend::driver::connect_postgres`). It implements
//!   `in_transaction` atomically and `open_native_lock_session` (`pg_try_advisory_lock`).
//! * MySQL — [`backend::driver::MysqlSqlClient`] over `mysql_async`, opt-in via the
//!   `mysql-executor` feature (`backend::driver::connect_mysql`,
//!   `backend::driver::mysql_executor_from_env`). Same threading model as PostgreSQL; every
//!   native-lock lease opens its own connection and speaks `GET_LOCK` / `RELEASE_LOCK` /
//!   `IS_USED_LOCK`.
//!
//! An executor written outside this crate **must** override:
//! * [`backend::JdbcExecutor::in_transaction`] — the default is non-transactional, and
//!   [`skill`] writes two tables inside one transaction;
//! * [`backend::JdbcExecutor::open_native_lock_session`] — server-scoped advisory locks
//!   (`GET_LOCK` / `pg_try_advisory_lock`) need a dedicated session per lease, which the shared
//!   handle cannot express.
//!
//! # Architecture
//!
//! Four layers, top-down:
//!
//! 1. **Facade** — [`JdbcDistributedStore`] auto-detects the dialect and assembles all
//!    four components.
//! 2. **Component layer** (database-agnostic) — [`JdbcStore`], [`JdbcAgentStateStore`],
//!    [`JdbcSnapshotSpec`], [`JdbcSandboxExecutionGuard`],
//!    [`JdbcAgentSkillRepository`]. Zero inline SQL — all SQL sourced from
//!    [`dialect::BoundSql`] returned by dialect methods.
//! 3. **Dialect layer** — [`dialect::AbstractJdbcDialect`] implements all table-domain
//!    traits (`StoreDialect`, `SessionStateDialect`, `SnapshotDialect`, `SkillDialect`,
//!    `SkillResourcesDialect`). Lock strategies are **separate** types
//!    ([`sandbox::TableLockStrategy`], [`dialect::MysqlNativeLockStrategy`],
//!    [`dialect::PostgresNativeLockStrategy`]) because advisory locks are session-scoped and a
//!    dialect carries no connection.
//! 4. **Vendor layer** — one type per database ([`dialect::vendor`]), overriding only DDL
//!    and divergent SQL syntax.
//!
//! # Reused harness/core traits
//!
//! Per `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.1, this crate **implements** existing
//! traits rather than defining parallel ones:
//! * [`agentscope_core::state::AgentStateStore`]
//! * [`agentscope_harness::filesystem::remote::BaseStore`]
//! * [`agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec`] / `RemoteSnapshotClient`
//! * [`agentscope_harness::agent::sandbox::SandboxExecutionGuard`]
//!
//! `put_if_version` CAS semantics, `StoreItem.version` meaning, and snapshot-id validation
//! are all harness-defined.
//!
//! # Execution backend
//!
//! Java components take a `javax.sql.DataSource`. Rust has no universal JDBC surface, so
//! components depend on the [`backend::JdbcExecutor`] trait. Three backends ship here —
//! [`backend::RusqliteExecutor`] (always) plus `backend::driver::PostgresExecutor`
//! (`postgres-executor`) and `backend::driver::MysqlExecutor` (`mysql-executor`); see
//! "Execution backends" above for what an out-of-crate executor owes.

pub mod backend;
pub mod dialect;
mod distributed_store;
pub mod sandbox;
mod schema_bootstrap;
pub mod skill;
pub mod snapshot;
pub mod state;
pub mod store;

pub use backend::{
    ColumnValue, DbError, JdbcError, JdbcExecutor, JdbcResult, JdbcSession, RusqliteExecutor,
    SharedExecutor,
};
pub use backend::driver::{DriverExecutor, SqlClient};
#[cfg(feature = "postgres-executor")]
pub use backend::driver::{
    connect_postgres, postgres_executor_from_env, PostgresExecutor, POSTGRES_URL_ENV,
};
#[cfg(feature = "mysql-executor")]
pub use backend::driver::{
    connect_mysql, mysql_executor_from_env, MysqlExecutor, MYSQL_URL_ENV,
};
pub use distributed_store::JdbcDistributedStore;
pub use sandbox::{
    JdbcSandboxExecutionGuard, JdbcSandboxLease, NativeLockFlavor, NativeLockGuard,
    NativeLockGuardLease, ProviderBackedGuard, TableLockStrategy,
};
pub use skill::{JdbcAgentSkillRepository, SkillRepositoryError, DEFAULT_NAMESPACE};
pub use snapshot::{JdbcRemoteSnapshotClient, JdbcSnapshotSpec};
pub use state::{JdbcAgentStateStore, VersionedState, UNVERSIONED};
pub use store::JdbcStore;
// Re-export the reused harness/core traits for ergonomic use by callers.
pub use agentscope_core::state::AgentStateStore;
pub use agentscope_harness::filesystem::{BaseStore, StoreItem, StoreValue};

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
///
/// Used as the `updated_at` / `acquired_at` column value across all components.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A monotonic counter used to disambiguate same-millisecond lock acquisitions in tests.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns a monotonically increasing sequence number. Useful for ordering-agnostic
/// assertions in tests.
pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_millis_is_positive() {
        assert!(now_millis() > 0);
    }

    #[test]
    fn next_seq_is_monotonic() {
        assert!(next_seq() < next_seq());
    }
}
