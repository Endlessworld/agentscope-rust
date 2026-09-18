//! Table-domain dialect interface for the session-state table, mirroring
//! `SessionStateDialect`.
//!
//! Stores agent session state as JSON rows keyed by
//! `(session_id, state_key, item_index)`. The `session_id` column encodes
//! `user_segment:session_id` (mirroring Java, which reuses the `session_id` column for the
//! composed slot). Method names are prefixed with `session_state` to avoid collisions in
//! the aggregate dialect.

use super::bound_sql::{BoundSql, SqlParam};

/// ANSI baseline SQL for the session-state table.
pub trait SessionStateDialect: Send + Sync {
    /// Base table name (without prefix).
    fn session_state_table_name(&self) -> &str {
        "sessions"
    }

    /// Table-name prefix (default `agentscope_`).
    fn session_state_table_prefix(&self) -> &str {
        super::abstract_dialect::DEFAULT_TABLE_PREFIX
    }

    /// Optional full-table-name override (takes priority over prefix + base).
    fn session_state_table_name_override(&self) -> Option<&str> {
        None
    }

    /// Physical table name used by every business SQL statement in this trait:
    /// per-table override > prefix + base (mirrors Java's final `sessionStateTableName()`).
    fn resolved_session_state_table_name(&self) -> String {
        match self.session_state_table_name_override() {
            Some(name) => name.to_string(),
            None => format!(
                "{}{}",
                self.session_state_table_prefix(),
                self.resolved_session_state_table_name()
            ),
        }
    }

    /// DDL statements (one or more) to create the session-state table. Must be idempotent.
    fn session_state_create_table_ddls(&self) -> Vec<String>;

    /// The `LIKE` escape character used by [`Self::session_state_list_session_ids`].
    fn session_state_like_escape_char(&self) -> char {
        '\\'
    }

    /// Table-existence probe (used when schema auto-creation is disabled).
    fn session_state_check_table_exists(&self, table_name: &str) -> BoundSql {
        BoundSql::new(
            format!("SELECT 1 FROM {table_name} WHERE 1 = 0"),
            Vec::new(),
        )
    }

    /// UPSERT a single state row (last-write-wins).
    fn session_state_upsert(
        &self,
        slot_id: &str,
        key: &str,
        index: i32,
        json: &str,
    ) -> BoundSql;

    /// INSERT a row, failing on a duplicate PK (used for create-only versioned writes).
    fn session_state_insert_if_absent(
        &self,
        slot_id: &str,
        key: &str,
        index: i32,
        json: &str,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (session_id, state_key, item_index, state_data, version) \
                 VALUES (?, ?, ?, ?, 1)",
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Int(index as i64),
                SqlParam::Text(json.to_string()),
            ],
        )
    }

    /// Conditional UPDATE for a versioned single write (`version + 1`).
    fn session_state_update_if_version(
        &self,
        slot_id: &str,
        key: &str,
        index: i32,
        json: &str,
        expected_version: i64,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "UPDATE {} SET state_data = ?, version = version + 1 \
                 WHERE session_id = ? AND state_key = ? AND item_index = ? AND version = ?",
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(json.to_string()),
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Int(index as i64),
                SqlParam::Int(expected_version),
            ],
        )
    }

    /// SELECT a single row. Projection: `(state_data, version)`.
    fn session_state_select(&self, slot_id: &str, key: &str, index: i32) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT state_data, version FROM {} \
                 WHERE session_id = ? AND state_key = ? AND item_index = ?",
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Int(index as i64),
            ],
        )
    }

    /// SELECT list rows ordered by `item_index`. Projection: `state_data`.
    fn session_state_select_list(&self, slot_id: &str, key: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT state_data FROM {} \
                 WHERE session_id = ? AND state_key = ? ORDER BY item_index",
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
            ],
        )
    }

    /// SELECT the maximum `item_index` for a list key. Projection: `MAX(item_index)`.
    fn session_state_select_max_index(&self, slot_id: &str, key: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT MAX(item_index) FROM {} WHERE session_id = ? AND state_key = ?",
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
            ],
        )
    }

    /// INSERT one list item at a given index (used in batched appends).
    fn session_state_insert(
        &self,
        slot_id: &str,
        key: &str,
        index: i32,
        json: &str,
    ) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (session_id, state_key, item_index, state_data, version) \
                 VALUES (?, ?, ?, ?, 1)",
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Int(index as i64),
                SqlParam::Text(json.to_string()),
            ],
        )
    }

    /// DELETE all rows for a list key (full rewrite).
    fn session_state_delete_by_key(&self, slot_id: &str, key: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "DELETE FROM {} WHERE session_id = ? AND state_key = ?",
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
            ],
        )
    }

    /// SELECT 1 if a session has any rows. Projection: `1`.
    fn session_state_exists(&self, slot_id: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT 1 FROM {} WHERE session_id = ? LIMIT 1",
                self.resolved_session_state_table_name()
            ),
            vec![SqlParam::Text(slot_id.to_string())],
        )
    }

    /// DELETE all rows for a session.
    fn session_state_delete_session(&self, slot_id: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "DELETE FROM {} WHERE session_id = ?",
                self.resolved_session_state_table_name()
            ),
            vec![SqlParam::Text(slot_id.to_string())],
        )
    }

    /// List distinct session ids matching a `LIKE` prefix pattern.
    /// Projection: `session_id`, ordered by `session_id` (mirrors Java).
    fn session_state_list_session_ids(&self, like_pattern: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "SELECT DISTINCT session_id FROM {} WHERE session_id LIKE ? ESCAPE '{}' \
                 ORDER BY session_id",
                self.resolved_session_state_table_name(),
                self.session_state_like_escape_char()
            ),
            vec![SqlParam::Text(like_pattern.to_string())],
        )
    }
}
