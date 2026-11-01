//! Aggregated multi-database JDBC dialect abstraction, mirroring
//! `io.agentscope.extensions.jdbc.dialect`.
//!
//! Four layers, top-down:
//! 1. **Facade** — [`crate::JdbcDistributedStore`] auto-detects the dialect and
//!    assembles all components.
//! 2. **Component layer** — [`crate::JdbcStore`], [`crate::JdbcAgentStateStore`],
//!    [`crate::JdbcSnapshotSpec`], [`crate::JdbcSandboxExecutionGuard`]. Zero inline
//!    SQL — all SQL sourced from [`BoundSql`] returned by dialect methods.
//! 3. **Dialect layer** — [`AbstractJdbcDialect`] aggregates the table-domain traits
//!    ([`StoreDialect`], [`SessionStateDialect`], [`SnapshotDialect`], [`SkillDialect`],
//!    [`SkillResourcesDialect`]) and holds a unified table prefix + per-table overrides.
//!    The lock contract is **not** part of it: [`SandboxLockStrategy`] implementations are
//!    separate types ([`crate::sandbox::TableLockStrategy`], [`MysqlNativeLockStrategy`],
//!    [`PostgresNativeLockStrategy`]) because a dialect carries no connection and advisory
//!    locks are session-scoped (see the `native_lock` module docs).
//! 4. **Vendor layer** — one type per database ([`vendor::MysqlDialect`], etc.),
//!    overriding only DDL and divergent SQL syntax.

mod abstract_dialect;
mod bound_sql;
mod detection;
mod lock_strategy;
mod native_lock;
mod session_state_dialect;
mod skill_dialect;
mod snapshot_dialect;
mod store_dialect;
mod table_schema;
pub mod vendor;

pub use abstract_dialect::{
    validate_identifier, AbstractJdbcDialect, DialectKind, TableNameConfig, TableNameConfigurable,
    TableNames, DEFAULT_TABLE_PREFIX,
};
pub use bound_sql::{BoundSql, SqlParam};
pub use detection::{detect_dialect, JdbcDialectBuilder};
pub use lock_strategy::{
    next_lock_poll_interval, LockLease, LockOutcome, SandboxLockStrategy, DEFAULT_LOCK_POLL_INTERVAL,
    MAX_LOCK_POLL_INTERVAL,
};
pub use native_lock::{
    normalize_lock_name, postgres_lock_key, ExecutorNativeLockProvider, MysqlNativeLockStrategy,
    NativeLockError, NativeLockSession, NativeLockSessionFuture, NativeLockSessionProvider,
    PostgresNativeLockStrategy, SessionScopedLockSession, MYSQL_GET_LOCK_SQL,
    MYSQL_MAX_LOCK_NAME_LENGTH, MYSQL_RELEASE_LOCK_SQL, NATIVE_LOCK_POLL_INTERVAL,
    PG_ADVISORY_UNLOCK_SQL, PG_TRY_ADVISORY_LOCK_SQL,
};
pub use session_state_dialect::SessionStateDialect;
pub use skill_dialect::{SkillDialect, SkillResourcesDialect};
pub use snapshot_dialect::SnapshotDialect;
pub use store_dialect::StoreDialect;
pub use table_schema::{parse_create_table_columns, validate_table_schema};
pub use vendor::{AnsiDialect, MysqlDialect, PostgresDialect, SqliteDialect};
