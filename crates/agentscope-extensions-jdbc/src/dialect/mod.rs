//! Aggregated multi-database JDBC dialect abstraction, mirroring
//! `io.agentscope.extensions.jdbc.dialect`.
//!
//! Four layers, top-down:
//! 1. **Facade** — [`crate::JdbcDistributedStore`] auto-detects the dialect and
//!    assembles all components.
//! 2. **Component layer** — [`crate::JdbcStore`], [`crate::JdbcAgentStateStore`],
//!    [`crate::JdbcSnapshotSpec`], [`crate::JdbcSandboxExecutionGuard`]. Zero inline
//!    SQL — all SQL sourced from [`BoundSql`] returned by dialect methods.
//! 3. **Dialect layer** — [`AbstractJdbcDialect`] implements all table-domain traits
//!    and the [`SandboxLockStrategy`] contract. Holds a unified table prefix +
//!    per-table overrides.
//! 4. **Vendor layer** — one type per database ([`vendor::MysqlDialect`], etc.),
//!    overriding only DDL and divergent SQL syntax.

mod abstract_dialect;
mod bound_sql;
mod detection;
mod lock_strategy;
mod native_lock;
mod session_state_dialect;
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
pub use lock_strategy::{LockLease, LockOutcome, SandboxLockStrategy};
pub use native_lock::{
    normalize_lock_name, postgres_lock_key, MysqlNativeLockStrategy, NativeLockError,
    NativeLockSession, PostgresNativeLockStrategy, MYSQL_GET_LOCK_SQL, MYSQL_MAX_LOCK_NAME_LENGTH,
    MYSQL_RELEASE_LOCK_SQL, NATIVE_LOCK_POLL_INTERVAL, PG_ADVISORY_UNLOCK_SQL,
    PG_TRY_ADVISORY_LOCK_SQL,
};
pub use session_state_dialect::SessionStateDialect;
pub use snapshot_dialect::SnapshotDialect;
pub use store_dialect::StoreDialect;
pub use table_schema::{parse_create_table_columns, validate_table_schema};
pub use vendor::{AnsiDialect, MysqlDialect, PostgresDialect, SqliteDialect};
