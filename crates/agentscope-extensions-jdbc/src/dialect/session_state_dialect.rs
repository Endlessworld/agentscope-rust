//! Table-domain dialect interface for the session-state table, mirroring
//! `SessionStateDialect`.
//!
//! Stores agent session state as JSON rows keyed by
//! `(session_id, state_key, item_index)`. The `session_id` column encodes
//! `user_segment:session_id` (mirroring Java, which reuses the `session_id` column for the
//! composed slot). Method names are prefixed with `session_state` to avoid collisions in
//! the aggregate dialect.

use super::abstract_dialect::TableNameConfigurable;
use super::bound_sql::{BoundSql, SqlParam};

/// ANSI baseline SQL for the session-state table.
pub trait SessionStateDialect: TableNameConfigurable + Send + Sync {
    /// Base table name (without prefix).
    fn session_state_table_name(&self) -> &str {
        "sessions"
    }

    /// Table-name prefix (default `agentscope_`).
    fn session_state_table_prefix(&self) -> String {
        self.table_config().prefix
    }

    /// Optional full-table-name override (takes priority over prefix + base).
    fn session_state_table_name_override(&self) -> Option<String> {
        self.table_config().session_state_table_name
    }

    /// Physical table name used by every business SQL statement in this trait:
    /// per-table override > prefix + base (mirrors Java's final `sessionStateTableName()`).
    fn resolved_session_state_table_name(&self) -> String {
        match self.session_state_table_name_override() {
            Some(name) => name,
            None => format!(
                "{}{}",
                self.session_state_table_prefix(),
                self.session_state_table_name()
            ),
        }
    }

    /// DDL statements (one or more) to create the session-state table. Must be idempotent.
    fn session_state_create_table_ddls(&self) -> Vec<String>;

    /// The `LIKE` escape character used by [`Self::session_state_list_session_ids`].
    fn session_state_like_escape_char(&self) -> char {
        '!'
    }

    /// Table-existence probe (used when schema auto-creation is disabled).
    ///
    /// **Fail-closed sentinel.** The default returns **no rows even for an existing table**, so
    /// [`crate::state::JdbcAgentStateStore::verify_table_exists`] — which treats an empty result
    /// as "missing" — reports the table as missing instead of silently accepting it. Every
    /// vendor dialect (ANSI/H2, MySQL, PostgreSQL, SQLite) overrides this with a real
    /// `information_schema` / `sqlite_master` probe; a new vendor that forgets to override
    /// therefore fails loudly at start-up rather than quietly believing a missing table exists.
    fn session_state_check_table_exists(&self, table_name: &str) -> BoundSql {
        BoundSql::new(
            format!("SELECT 1 FROM {table_name} WHERE 1 = 0"),
            Vec::new(),
        )
    }

    /// UPSERT a single state row (last-write-wins).
    ///
    /// Insert semantics: every vendor override binds `version = 1` explicitly, so a fresh
    /// row starts at `1` — the DDL's `version ... DEFAULT 0` never applies to this
    /// statement (only [`Self::session_state_insert`], which omits the column, gets `0`).
    fn session_state_upsert(&self, slot_id: &str, key: &str, index: i32, json: &str) -> BoundSql;

    /// UPSERT a single state row and return the resulting version in the same round trip.
    ///
    /// Used by `save_if_version(UNVERSIONED)`: the write and the version read must be one
    /// statement, otherwise a concurrent writer can slip in between the write and a
    /// follow-up `SELECT version` and the caller would observe *that* writer's version.
    ///
    /// Returns `None` when the dialect cannot express a returning upsert — MySQL has no
    /// `RETURNING`, and H2's `MERGE` has no portable returning form. The caller then falls
    /// back to an UPSERT followed by a separate `SELECT version` **inside one transaction**
    /// (`JdbcAgentStateStore::upsert_then_read_version`), so the write still holds the row
    /// lock until the version has been read and the caller never observes another writer's
    /// version.
    fn session_state_upsert_returning_version(
        &self,
        _slot_id: &str,
        _key: &str,
        _index: i32,
        _json: &str,
    ) -> Option<BoundSql> {
        None
    }

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

    /// INSERT one list item at a given index (used in batched appends). Mirrors the Java
    /// `sessionStateInsert` — four params, `version` defaults to 1 in the schema.
    fn session_state_insert(&self, slot_id: &str, key: &str, index: i32, json: &str) -> BoundSql {
        BoundSql::new(
            format!(
                "INSERT INTO {} (session_id, state_key, item_index, state_data) VALUES (?, ?, ?, ?)",
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

    /// Compare-and-swap UPDATE guarded by `version == expected_version`; writes
    /// `version = expected_version + 1` directly (mirrors Java
    /// `sessionStateUpdateIfVersion`).
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
                "UPDATE {} SET state_data = ?, version = ? \
                 WHERE session_id = ? AND state_key = ? AND item_index = ? AND version = ?",
                self.resolved_session_state_table_name()
            ),
            vec![
                SqlParam::Text(json.to_string()),
                SqlParam::Int(expected_version + 1),
                SqlParam::Text(slot_id.to_string()),
                SqlParam::Text(key.to_string()),
                SqlParam::Int(index as i64),
                SqlParam::Int(expected_version),
            ],
        )
    }

    /// SELECT a single row. Projection: `(state_data, version)` (mirrors Java
    /// `sessionStateSelectVersioned`).
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
