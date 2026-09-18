//! Aggregate dialect implementing all table-domain traits plus the lock strategy,
//! mirroring `AbstractJdbcDialect`.
//!
//! Holds a unified table prefix and per-table name overrides. Vendor dialects extend
//! this type and override only the methods where their SQL diverges from the ANSI
//! baseline. Table-name resolution priority: per-table override > prefix + base.

use super::store_dialect::StoreDialect;
use super::session_state_dialect::SessionStateDialect;
use super::snapshot_dialect::SnapshotDialect;

/// Default table prefix applied to every table name.
pub const DEFAULT_TABLE_PREFIX: &str = "agentscope_";

/// Which database a dialect targets. Used for dialect auto-detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DialectKind {
    /// Generic/portable ANSI dialect (H2 and other JDBC databases).
    Ansi,
    /// MySQL / MariaDB.
    Mysql,
    /// PostgreSQL.
    Postgres,
    /// SQLite.
    Sqlite,
}

/// Validates a SQL identifier (table names and prefixes flow into DDL verbatim, so
/// this is the SQL-injection guard). Same pattern as the Java
/// `AbstractJdbcDialectBuilder.VALID_IDENTIFIER`.
pub fn validate_identifier(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    let mut chars = value.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "{field} must start with a letter or underscore: {value}"
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "{field} must contain only alphanumeric characters and underscores: {value}"
        ));
    }
    Ok(())
}

/// Aggregate dialect: implements [`StoreDialect`] + [`SessionStateDialect`] +
/// [`SnapshotDialect`], and provides table-name resolution plus lock strategy hooks.
///
/// The trait methods themselves resolve the final, prefixed table names; vendors
/// override only the DDL/UPSERT methods where their syntax diverges.
pub trait AbstractJdbcDialect: StoreDialect + SessionStateDialect + SnapshotDialect + Send + Sync {
    /// The dialect kind, used for auto-detection and reporting.
    fn kind(&self) -> DialectKind;

    /// The unified table prefix (default `agentscope_`).
    ///
    /// The per-table prefixes inherited from the table-domain traits default to the same
    /// value; this accessor exists for the lock table name (which has no table-domain trait).
    fn table_prefix(&self) -> &str {
        DEFAULT_TABLE_PREFIX
    }

    /// Java equivalent: `AbstractJdbcDialect.lockTableName()` = `tablePrefix + "distributed_locks"`.
    /// DDL statement for the distributed-locks table (used by the portable
    /// table-based lock). Idempotent.
    fn lock_create_table_ddl(&self) -> String {
        format!(
            "CREATE TABLE IF NOT EXISTS {} (lock_name VARCHAR(255) PRIMARY KEY, acquired_at BIGINT NOT NULL)",
            self.resolved_lock_table_name()
        )
    }

    /// Resolved lock table name: `{prefix}distributed_locks` (mirrors Java).
    fn resolved_lock_table_name(&self) -> String {
        format!("{}distributed_locks", self.table_prefix())
    }

    /// Whether this dialect supports native advisory locks (MySQL `GET_LOCK`,
    /// PostgreSQL `pg_try_advisory_lock`). The portable table lock is the default.
    fn supports_native_locks(&self) -> bool {
        false
    }
}
