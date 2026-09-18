//! Table-domain dialect interface for the KV store table, mirroring `StoreDialect`.
//!
//! Defines the create-schema DDL (varies per database, overridden by vendors) and the
//! ANSI-standard business SQL (default methods returning [`BoundSql`]). The aggregate
//! [`crate::dialect::AbstractJdbcDialect`] implements this together with the other
//! table-domain traits; vendor dialects override only what differs.

use super::bound_sql::{BoundSql, SqlParam};

/// Canonical ANSI baseline SQL for the KV store table.
pub trait StoreDialect: Send + Sync {
    /// Base table name (without prefix).
    fn store_table_name(&self) -> &str {
        "store"
    }

    /// Table-name prefix (default `agentscope_`).
    fn store_table_prefix(&self) -> &str {
        super::abstract_dialect::DEFAULT_TABLE_PREFIX
    }

    /// Optional full-table-name override (takes priority over prefix + base).
    fn store_table_name_override(&self) -> Option<&str> {
        None
    }

    /// Physical table name used by every business SQL statement in this trait:
    /// per-table override > prefix + base.
    ///
    /// Java achieves the same via `AbstractJdbcDialect`'s `final storeTableName()`
    /// override, which the interface default methods dispatch to. Rust trait default
    /// methods cannot dispatch to a subtrait, so the resolution lives here and the
    /// aggregate trait simply inherits it.
    fn resolved_store_table_name(&self) -> String {
        match self.store_table_name_override() {
            Some(name) => name.to_string(),
            None => format!("{}{}", self.store_table_prefix(), self.store_table_name()),
        }
    }

    /// DDL statements (one or more) to create the KV store table. Must be idempotent.
    fn store_create_table_ddls(&self) -> Vec<String>;

    /// UPSERT: insert a new row with `version = 1` or update an existing row `version + 1`.
    fn store_upsert(
        &self,
        namespace_path: &str,
        key: &str,
        json: &str,
        timestamp: i64,
    ) -> BoundSql;

    /// INSERT for create-only writes (`version` starts at 1). PK violation means the row
    /// already exists — the CAS path uses this to detect "key already present".
    fn store_insert(
        &self,
        namespace_path: &str,
        key: &str,
        json: &str,
        timestamp: i64,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (namespace_path, item_key, value_json, version, updated_at) \
                 VALUES (?, ?, ?, 1, ?)",
                self.resolved_store_table_name()
            ),
            vec![
                SqlParam::Text(namespace_path.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Text(json.to_string()),
                SqlParam::Int(timestamp),
            ],
        )
    }

    /// Conditional UPDATE for the CAS path. Affected-row count `1` means success.
    fn store_cas_update(
        &self,
        json: &str,
        timestamp: i64,
        namespace_path: &str,
        key: &str,
        expected_version: i64,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "UPDATE {} SET value_json = ?, version = version + 1, updated_at = ? \
                 WHERE namespace_path = ? AND item_key = ? AND version = ?",
                self.resolved_store_table_name()
            ),
            vec![
                SqlParam::Text(json.to_string()),
                SqlParam::Int(timestamp),
                SqlParam::Text(namespace_path.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Int(expected_version),
            ],
        )
    }

    /// Single-item SELECT. Projection: `(value_json, version)`.
    fn store_select(&self, namespace_path: &str, key: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT value_json, version FROM {} WHERE namespace_path = ? AND item_key = ?",
                self.resolved_store_table_name()
            ),
            vec![
                SqlParam::Text(namespace_path.to_string()),
                SqlParam::Text(key.to_string()),
            ],
        )
    }

    /// Single-item DELETE.
    fn store_delete(&self, namespace_path: &str, key: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "DELETE FROM {} WHERE namespace_path = ? AND item_key = ?",
                self.resolved_store_table_name()
            ),
            vec![
                SqlParam::Text(namespace_path.to_string()),
                SqlParam::Text(key.to_string()),
            ],
        )
    }

    /// The `LIKE` escape character used by [`Self::store_search`].
    fn store_like_escape_char(&self) -> char {
        '\\'
    }

    /// Prefix search by `namespace_path LIKE 'prefix%'`, ordered by key with pagination.
    /// Projection: `(item_key, value_json, version)`.
    fn store_search(&self, namespace_prefix: &str, limit: usize, offset: usize) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT item_key, value_json, version FROM {} \
                 WHERE namespace_path LIKE ? ESCAPE '{}' ORDER BY item_key LIMIT ? OFFSET ?",
                self.resolved_store_table_name(),
                self.store_like_escape_char()
            ),
            vec![
                SqlParam::Text(namespace_prefix.to_string()),
                SqlParam::Int(limit as i64),
                SqlParam::Int(offset as i64),
            ],
        )
    }
}
