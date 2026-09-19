//! Table-domain dialect interface for the snapshots table, mirroring `SnapshotDialect`.
//!
//! Stores binary workspace tar archives keyed by `snapshot_id`.

use super::abstract_dialect::TableNameConfigurable;
use super::bound_sql::{BoundSql, SqlParam};

/// ANSI baseline SQL for the snapshots table.
pub trait SnapshotDialect: TableNameConfigurable + Send + Sync {
    /// Base table name (without prefix).
    fn snapshot_table_name(&self) -> &str {
        "snapshots"
    }

    /// Table-name prefix (default `agentscope_`).
    fn snapshot_table_prefix(&self) -> String {
        self.table_config().prefix
    }

    /// Optional full-table-name override (takes priority over prefix + base).
    fn snapshot_table_name_override(&self) -> Option<String> {
        self.table_config().snapshot_table_name
    }

    /// Physical table name used by every business SQL statement in this trait:
    /// per-table override > prefix + base (mirrors Java's final `snapshotTableName()`).
    fn resolved_snapshot_table_name(&self) -> String {
        match self.snapshot_table_name_override() {
            Some(name) => name,
            None => format!(
                "{}{}",
                self.snapshot_table_prefix(),
                self.snapshot_table_name()
            ),
        }
    }

    /// DDL statements (one or more) to create the snapshots table. Must be idempotent.
    fn snapshot_create_table_ddls(&self) -> Vec<String>;

    /// Idempotent UPSERT for uploading a snapshot; the data param is bound as a BLOB.
    fn snapshot_upsert(&self, snapshot_id: &str, data: &[u8]) -> BoundSql;

    /// Single-snapshot SELECT. Projection: `data`.
    fn snapshot_select(&self, snapshot_id: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT data FROM {} WHERE snapshot_id = ?",
                self.resolved_snapshot_table_name()
            ),
            vec![SqlParam::Text(snapshot_id.to_string())],
        )
    }

    /// Snapshot existence probe. Projection: `1`.
    fn snapshot_exists(&self, snapshot_id: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT 1 FROM {} WHERE snapshot_id = ? LIMIT 1",
                self.resolved_snapshot_table_name()
            ),
            vec![SqlParam::Text(snapshot_id.to_string())],
        )
    }
}
