//! JDBC-backed [`AgentStateStore`], mirroring `JdbcAgentStateStore`.
//!
//! Database-agnostic session state store backed by [`SessionStateDialect`]. Implements
//! [`AgentStateStore`] with zero inline SQL — every statement is sourced from the dialect
//! via [`BoundSql`].
//!
//! State rows are keyed by a `slot_id` (encoding `user_segment:session_id`),
//! `state_key`, and `item_index` (0 for single values, 0..n for list elements). List
//! values additionally store a hash row under `<key>:\_hash` so appends can be detected
//! incrementally.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::state::AgentStateStore;
use agentscope_core::state::list_hash::{
    compute_hash as core_compute_hash, needs_full_rewrite as core_needs_full_rewrite,
};
use serde_json::Value;

use crate::backend::{JdbcError, JdbcResult, SharedExecutor};
use crate::dialect::{AbstractJdbcDialect, BoundSql, SessionStateDialect, SqlParam};

/// The separator between the user segment and session id inside a `slot_id`.
const SLOT_SEPARATOR: char = ':';
/// Suffix appended to a list key to derive its stored hash row's key.
const HASH_KEY_SUFFIX: &str = ":_hash";
/// The single-value index (mirrors Java `SINGLE_STATE_INDEX`).
const SINGLE_STATE_INDEX: i64 = 0;
/// The anonymous user namespace, mirroring core's `ANON_USER`.
const ANON_USER: &str = "__anon__";

/// JDBC-backed [`AgentStateStore`].
pub struct JdbcAgentStateStore {
    executor: SharedExecutor,
    dialect: Arc<dyn AbstractJdbcDialect>,
}

impl std::fmt::Debug for JdbcAgentStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JdbcAgentStateStore")
            .field("kind", &self.dialect.kind())
            .finish_non_exhaustive()
    }
}

impl JdbcAgentStateStore {
    /// Creates a store; when `create_if_not_exist` is false, verifies the table exists.
    pub fn new(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
        create_if_not_exist: bool,
    ) -> JdbcResult<Self> {
        let store = Self { executor, dialect };
        if create_if_not_exist {
            store.create_table_if_not_exist()?;
        } else {
            store.verify_table_exists()?;
        }
        Ok(store)
    }

    /// Creates a store with automatic schema creation.
    pub fn with_schema(executor: SharedExecutor, dialect: Arc<dyn AbstractJdbcDialect>) -> JdbcResult<Self> {
        Self::new(executor, dialect, true)
    }

    /// Runs the dialect's idempotent DDL for the session-state table.
    pub fn create_table_if_not_exist(&self) -> JdbcResult<()> {
        for ddl in self.dialect.session_state_create_table_ddls() {
            self.executor.execute(&BoundSql::no_params(ddl))?;
        }
        Ok(())
    }

    /// Verifies the session-state table exists (used when auto-creation is disabled).
    pub fn verify_table_exists(&self) -> JdbcResult<()> {
        let table = self.dialect.resolved_session_state_table_name();
        let bound = self.dialect.session_state_check_table_exists(&table);
        self.executor.query(&bound).map(|_| ()).map_err(|e| {
            JdbcError::Database(format!(
                "table does not exist: {table}. use create_if_not_exist=true to auto-create. ({e})"
            ))
        })
    }

    /// Encodes `(user_id, session_id)` into the `slot_id` column value.
    fn slot_id(user_id: Option<&str>, session_id: &str) -> String {
        let user = match user_id {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => ANON_USER.to_string(),
        };
        format!("{user}{SLOT_SEPARATOR}{session_id}")
    }

    fn validate_session(session_id: &str) -> JdbcResult<()> {
        if session_id.trim().is_empty() {
            return Err(JdbcError::Database("sessionId must not be blank".into()));
        }
        Ok(())
    }

    fn validate_key(key: &str) -> JdbcResult<()> {
        if key.trim().is_empty() {
            return Err(JdbcError::Database("key must not be blank".into()));
        }
        Ok(())
    }

    fn json_of(value: &Value) -> JdbcResult<String> {
        serde_json::to_string(value)
            .map_err(|e| JdbcError::Database(format!("serialize state: {e}")))
    }

    /// Reads the stored hash for a list key, if any.
    fn stored_hash(&self, slot_id: &str, hash_key: &str) -> JdbcResult<Option<String>> {
        let bound = self
            .dialect
            .session_state_select(slot_id, hash_key, SINGLE_STATE_INDEX as i32);
        let rows = self.executor.query(&bound)?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_text().map(|s| s.to_string())))
    }

    /// Reads the number of stored items for a list key (`MAX(item_index) + 1`).
    fn list_count(&self, slot_id: &str, key: &str) -> JdbcResult<usize> {
        let bound = self.dialect.session_state_select_max_index(slot_id, key);
        let rows = self.executor.query(&bound)?;
        let max = rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_int());
        Ok(max.map(|m| (m + 1) as usize).unwrap_or(0))
    }

    fn read_version(&self, slot_id: &str, key: &str) -> JdbcResult<i64> {
        let bound = self
            .dialect
            .session_state_select(slot_id, key, SINGLE_STATE_INDEX as i32);
        let rows = self.executor.query(&bound)?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().nth(1))
            .and_then(|c| c.as_int())
            .unwrap_or(0))
    }

    fn save_single(&self, slot_id: &str, key: &str, json: &str) -> JdbcResult<()> {
        let bound = self
            .dialect
            .session_state_upsert(slot_id, key, SINGLE_STATE_INDEX as i32, json);
        self.executor.execute(&bound)?;
        Ok(())
    }

    /// Saves a list using hash-based change detection, mirroring Java semantics.
    fn save_list_impl(
        &self,
        slot_id: &str,
        key: &str,
        values: &[Value],
    ) -> JdbcResult<()> {
        if values.is_empty() {
            return Ok(());
        }
        let hash_key = format!("{key}{HASH_KEY_SUFFIX}");
        let current_hash = core_compute_hash(values);
        let stored = self.stored_hash(slot_id, &hash_key)?;
        let existing = self.list_count(slot_id, key)?;

        // Reuse core's list-hash change detection (mirrors Java `ListHashUtil.needsFullRewrite`)
        // rather than inventing a parallel heuristic. `needs_full_rewrite` hashes the stored
        // prefix of the incoming list and compares it against the persisted hash.
        let needs_full_rewrite = core_needs_full_rewrite(values, stored.as_deref(), existing);

        if needs_full_rewrite {
            let mut batch = Vec::with_capacity(values.len() + 2);
            batch.push(self.dialect.session_state_delete_by_key(slot_id, key));
            for (index, value) in values.iter().enumerate() {
                batch.push(self.dialect.session_state_insert(
                    slot_id,
                    key,
                    index as i32,
                    &Self::json_of(value)?,
                ));
            }
            batch.push(self.dialect.session_state_upsert(
                slot_id,
                &hash_key,
                SINGLE_STATE_INDEX as i32,
                &current_hash,
            ));
            self.executor.execute_batch(&batch)?;
        } else if values.len() > existing {
            // Incremental append: only the tail beyond the stored prefix is inserted.
            let tail = &values[existing..];
            let mut batch = Vec::with_capacity(tail.len() + 1);
            for (offset, value) in tail.iter().enumerate() {
                batch.push(self.dialect.session_state_insert(
                    slot_id,
                    key,
                    (existing + offset) as i32,
                    &Self::json_of(value)?,
                ));
            }
            batch.push(self.dialect.session_state_upsert(
                slot_id,
                &hash_key,
                SINGLE_STATE_INDEX as i32,
                &current_hash,
            ));
            self.executor.execute_prepared_batch(&batch)?;
        }
        Ok(())
    }
}

#[async_trait]
impl AgentStateStore for JdbcAgentStateStore {
    async fn save(&self, user_id: Option<&str>, session_id: &str, key: &str, value: Value) {
        if Self::validate_session(session_id).is_err() || Self::validate_key(key).is_err() {
            return;
        }
        let slot = Self::slot_id(user_id, session_id);
        let Ok(json) = Self::json_of(&value) else { return };
        if let Err(e) = self.save_single(&slot, key, &json) {
            tracing::warn!("[jdbc-state] save failed for {key}: {e}");
        }
    }

    async fn save_list(&self, user_id: Option<&str>, session_id: &str, key: &str, values: &[Value]) {
        if Self::validate_session(session_id).is_err() || Self::validate_key(key).is_err() {
            return;
        }
        let slot = Self::slot_id(user_id, session_id);
        if let Err(e) = self.save_list_impl(&slot, key, values) {
            tracing::warn!("[jdbc-state] save_list failed for {key}: {e}");
        }
    }

    async fn get(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Option<Value> {
        if Self::validate_session(session_id).is_err() || Self::validate_key(key).is_err() {
            return None;
        }
        let slot = Self::slot_id(user_id, session_id);
        let bound = self
            .dialect
            .session_state_select(&slot, key, SINGLE_STATE_INDEX as i32);
        let rows = self.executor.query(&bound).ok()?;
        let json = rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_text().map(|s| s.to_string()))?;
        serde_json::from_str(&json).ok()
    }

    async fn get_list(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Vec<Value> {
        if Self::validate_session(session_id).is_err() || Self::validate_key(key).is_err() {
            return Vec::new();
        }
        let slot = Self::slot_id(user_id, session_id);
        let bound = self.dialect.session_state_select_list(&slot, key);
        let Ok(rows) = self.executor.query(&bound) else {
            return Vec::new();
        };
        rows.into_iter()
            .filter_map(|r| {
                r.into_iter()
                    .next()
                    .and_then(|c| c.as_text().map(|s| s.to_string()))
                    .and_then(|json| serde_json::from_str(&json).ok())
            })
            .collect()
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        if Self::validate_session(session_id).is_err() {
            return false;
        }
        let slot = Self::slot_id(user_id, session_id);
        let bound = self.dialect.session_state_exists(&slot);
        self.executor
            .query(&bound)
            .map(|rows| !rows.is_empty())
            .unwrap_or(false)
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if Self::validate_session(session_id).is_err() {
            return;
        }
        let slot = Self::slot_id(user_id, session_id);
        let bound = self.dialect.session_state_delete_session(&slot);
        if let Err(e) = self.executor.execute(&bound) {
            tracing::warn!("[jdbc-state] delete failed for session {session_id}: {e}");
        }
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let user_segment = match user_id {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => ANON_USER.to_string(),
        };
        let prefix = format!("{user_segment}{SLOT_SEPARATOR}");
        let pattern = like_prefix_pattern(&prefix);
        let bound = self.dialect.session_state_list_session_ids(&pattern);
        let Ok(rows) = self.executor.query(&bound) else {
            return Vec::new();
        };
        let mut out: Vec<String> = rows
            .into_iter()
            .filter_map(|r| {
                r.into_iter()
                    .next()
                    .and_then(|c| c.as_text().map(|s| s.to_string()))
            })
            .filter_map(|slot| slot.strip_prefix(&prefix).map(|s| s.to_string()))
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Builds a `LIKE` prefix pattern matching exactly `prefix` followed by anything,
/// escaping every `_`, `%` and the escape char so the user segment is literal.
fn like_prefix_pattern(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 1);
    for ch in prefix.chars() {
        if ch == '\\' || ch == '%' || ch == '_' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// Constructs a session-state select parameter for tests.
pub(crate) fn slot_param(slot: &str) -> SqlParam {
    SqlParam::Text(slot.to_string())
}
