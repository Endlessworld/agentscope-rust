//! MySQL-backed [`AgentStateStore`], mirroring
//! `io.agentscope.extensions.mysql.state.MysqlAgentStateStore`.
//!
//! Session state is stored in a table keyed by `(session_id, state_key, item_index)`:
//!
//! * **Single state** — stored as JSON at `item_index = 0`.
//! * **List state** — each item stored in its own row at `item_index = 0, 1, 2, ...`,
//!   giving true incremental list storage (only new items are inserted).
//!
//! Java's `AgentStateStore` additionally exposes `saveIfVersion` / `readVersion` for
//! optimistic concurrency. The Rust [`AgentStateStore`] trait does not model versions on
//! the session-state surface, so those helpers are provided here as inherent methods
//! backed by the `version` column of the `item_index = 0` row (see [`StateBackend`]).
//! That is the same layout Java uses, which keeps CAS a single conditional statement
//! instead of a read-then-write pair that two concurrent writers can both pass.
//!
//! List state keeps a `{key}:_hash` companion row holding the sampled hash of the last
//! written list (Java `ListHashUtil`), so the store can tell an append-only growth
//! (insert only the new tail) from a mutated or shrunk list (rewrite every row).

use std::sync::Arc;

use agentscope_core::state::list_hash;
use agentscope_core::state::AgentStateStore;
use async_trait::async_trait;

use crate::error::{MysqlError, MysqlResult};
use crate::identifier::validate_identifier;

/// Suffix for the list-hash companion key, mirroring `HASH_KEY_SUFFIX`.
///
/// Java uses it for **list change detection only** (`ListHashUtil`): `save(list)` persists
/// the sampled hash of the written list under `{key}:_hash` so the next write can tell an
/// append-only growth from a mutated prefix. It carries no version semantics — versions
/// live in the `version` column of the `item_index = 0` row.
pub const HASH_KEY_SUFFIX: &str = ":_hash";

/// `item_index` value for single state values.
pub const SINGLE_STATE_INDEX: i64 = 0;

/// Default database name, mirroring `DEFAULT_DATABASE_NAME`.
pub const DEFAULT_DATABASE_NAME: &str = "agentscope";

/// Default table name, mirroring `DEFAULT_TABLE_NAME`.
pub const DEFAULT_TABLE_NAME: &str = "agentscope_sessions";

/// Outcome of a version-guarded single-value write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasOutcome {
    /// The write was applied and produced this version (always `>= 1`).
    Applied(i64),
    /// The stored version did not match the expectation; nothing was written.
    Conflict,
}

/// Low-level SQL surface used by [`MysqlAgentStateStore`].
///
/// Single state is one row at `item_index = 0`; list state is one row per item
/// (`item_index = 0, 1, 2, ...`). Rows carry a `version` column, read through
/// [`StateBackend::read_version`].
///
/// # Atomicity contract
///
/// [`StateBackend::insert_single_if_absent`] and
/// [`StateBackend::update_single_if_version`] back
/// [`MysqlAgentStateStore::save_if_version`] and **must** be atomic — one statement, never
/// a read followed by a write. They mirror Java's `INSERT` / `UPDATE ... WHERE version = ?`
/// pair:
///
/// ```text
/// INSERT INTO <table> (session_id, state_key, item_index, state_data, version)
///      VALUES (?, ?, 0, ?, 1);                   -- duplicate key => Conflict
///
/// UPDATE <table> SET state_data = ?, version = ?
///  WHERE session_id = ? AND state_key = ? AND item_index = 0 AND version = ?;
///                                               -- 0 rows affected => Conflict
/// ```
pub trait StateBackend: Send + Sync {
    /// Ensures the schema (database + table, including the `version` column) exists.
    fn ensure_schema(&self) -> MysqlResult<()>;

    /// Unversioned upsert of a value at `item_index = 0`; `version` is left untouched.
    ///
    /// Used only for the list-hash companion row (Java `saveHash`), which has no version
    /// semantics. Session-state writes go through
    /// [`StateBackend::save_single_versioned`].
    fn save_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<()>;

    /// Upserts the single value at `item_index = 0` and returns the version produced by
    /// **this** write (`1` when the row is created).
    ///
    /// Mirrors Java `save`: `INSERT ... VALUES (..., 1) ON DUPLICATE KEY UPDATE
    /// state_data = VALUES(state_data), version = version + 1`.
    fn save_single_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<i64>;

    /// Atomically inserts the single value with `version = 1`, only when no row exists.
    ///
    /// A duplicate-key violation must surface as [`CasOutcome::Conflict`] (a failed CAS
    /// with `expected_version == 0`). Mirrors Java `insertIfAbsent`.
    fn insert_single_if_absent(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<CasOutcome>;

    /// Atomically replaces the single value iff the stored version equals
    /// `expected_version`, bumping it to `expected_version + 1`.
    ///
    /// A missing row or a version mismatch must surface as [`CasOutcome::Conflict`].
    /// Mirrors Java `updateIfVersion`.
    fn update_single_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
        expected_version: i64,
    ) -> MysqlResult<CasOutcome>;

    /// Reads the single value at `item_index = 0`.
    fn get_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Option<String>>;

    /// Writes `values_json` starting at `from_index`, then removes any rows at or beyond the
    /// new length (`values_json.len()`) so shrinking a list drops stale rows.
    ///
    /// `values_json` is always the **full** new list; `from_index` only tells the backend
    /// where the unchanged prefix ends, so `from_index == 0` is a full rewrite and a higher
    /// value lets an append touch just the tail.
    fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        values_json: &[String],
        from_index: usize,
    ) -> MysqlResult<()>;

    /// Reads all list values ordered by `item_index`.
    fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Vec<String>>;

    /// Returns the number of list items currently stored for the key.
    fn list_len(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<usize>;

    /// Whether the session has any persisted state.
    fn exists(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<bool>;

    /// Deletes all state for the session.
    fn delete(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<()>;

    /// Lists session ids under the user namespace.
    fn list_session_ids(&self, user_id: Option<&str>) -> MysqlResult<Vec<String>>;

    /// Reads the `version` column of the single row at `item_index = 0` (`0` when the row is
    /// absent). Mirrors Java `readVersion`, which reads the column without deserialising
    /// `state_data`.
    fn read_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<i64>;
}

/// MySQL database-backed [`AgentStateStore`].
#[derive(Clone)]
pub struct MysqlAgentStateStore {
    backend: Arc<dyn StateBackend>,
    database_name: String,
    table_name: String,
    create_if_not_exist: bool,
}

impl std::fmt::Debug for MysqlAgentStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlAgentStateStore")
            .field("database_name", &self.database_name)
            .field("table_name", &self.table_name)
            .field("create_if_not_exist", &self.create_if_not_exist)
            .finish()
    }
}

impl MysqlAgentStateStore {
    /// Creates a store with the default database and table, auto-creating the schema.
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        let store = Self {
            backend,
            database_name: DEFAULT_DATABASE_NAME.to_string(),
            table_name: DEFAULT_TABLE_NAME.to_string(),
            create_if_not_exist: true,
        };
        let _ = store.backend.ensure_schema();
        store
    }

    /// Creates a store with explicit database/table names and auto-creation semantics.
    pub fn with_config(
        backend: Arc<dyn StateBackend>,
        database_name: Option<&str>,
        table_name: Option<&str>,
        create_if_not_exist: bool,
    ) -> MysqlResult<Self> {
        let database_name = database_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_DATABASE_NAME)
            .trim()
            .to_string();
        let table_name = table_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_TABLE_NAME)
            .trim()
            .to_string();
        let database_name = validate_identifier(&database_name, "Database name")?;
        let table_name = validate_identifier(&table_name, "Table name")?;

        let store = Self {
            backend,
            database_name,
            table_name,
            create_if_not_exist,
        };
        if create_if_not_exist {
            store.backend.ensure_schema()?;
        }
        Ok(store)
    }

    /// Database name this store writes to.
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// Table name this store writes to.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Versioned save; returns the version this write produced.
    ///
    /// Mirrors Java `AgentStateStore.saveIfVersion`:
    ///
    /// * `expected_version == None` (Rust's `UNVERSIONED`) — unconditional upsert that
    ///   bumps the version (`1` on insert, `n + 1` on update).
    /// * `Some(0)` — create-if-absent: an atomic `INSERT`; an existing row is a conflict.
    /// * `Some(n)` — an atomic `UPDATE ... WHERE version = n`; a version mismatch (or a
    ///   missing row) is a conflict.
    ///
    /// Every branch is a single backend statement, so two concurrent writers cannot both
    /// pass the same expectation.
    pub async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: Option<i64>,
    ) -> MysqlResult<i64> {
        require_session_id(session_id)?;
        let json = serde_json::to_string(&value)?;
        match expected_version {
            None => self
                .backend
                .save_single_versioned(user_id, session_id, key, &json),
            Some(0) => {
                match self
                    .backend
                    .insert_single_if_absent(user_id, session_id, key, &json)?
                {
                    CasOutcome::Applied(version) => Ok(version),
                    CasOutcome::Conflict => Err(self.conflict(user_id, session_id, key, 0)),
                }
            }
            Some(expected) => {
                match self
                    .backend
                    .update_single_if_version(user_id, session_id, key, &json, expected)?
                {
                    CasOutcome::Applied(version) => Ok(version),
                    CasOutcome::Conflict => Err(self.conflict(user_id, session_id, key, expected)),
                }
            }
        }
    }

    /// Builds the version-conflict error. Reading the stored version is only used to make
    /// the message actionable; it never gates the write (the CAS already decided).
    fn conflict(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        expected: i64,
    ) -> MysqlError {
        let found = self
            .backend
            .read_version(user_id, session_id, key)
            .unwrap_or(0);
        MysqlError::InvalidArgument(format!(
            "version conflict for key '{key}': expected {expected}, found {found}"
        ))
    }

    /// Encodes list items for the backend.
    fn encode_values(values: &[serde_json::Value]) -> MysqlResult<Vec<String>> {
        Ok(values
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Hash-based incremental list save, mirroring Java `save(list)`:
    ///
    /// * hash changed, list shrank, or stored hash missing while rows exist → full rewrite;
    /// * pure append → insert only the tail;
    /// * nothing changed → skip entirely.
    ///
    /// The hash is `agentscope_core::state::list_hash` (the same helper JDBC and PostgreSQL
    /// use), stored in the `{key}:_hash` companion row.
    fn save_list_impl(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) -> MysqlResult<()> {
        let hash_key = format!("{key}{HASH_KEY_SUFFIX}");
        let current_hash = list_hash::compute_hash(values);
        let stored_hash = self.backend.get_single(user_id, session_id, &hash_key)?;
        let existing_count = self.backend.list_len(user_id, session_id, key)?;

        if list_hash::needs_full_rewrite(values, stored_hash.as_deref(), existing_count) {
            let encoded = Self::encode_values(values)?;
            self.backend
                .save_list(user_id, session_id, key, &encoded, 0)?;
            self.backend
                .save_single(user_id, session_id, &hash_key, &current_hash)?;
        } else if values.len() > existing_count {
            let encoded = Self::encode_values(values)?;
            self.backend
                .save_list(user_id, session_id, key, &encoded, existing_count)?;
            self.backend
                .save_single(user_id, session_id, &hash_key, &current_hash)?;
        }
        Ok(())
    }

    /// Reads the current version for `key`. Mirrors `readVersion`.
    pub fn read_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> MysqlResult<i64> {
        require_session_id(session_id)?;
        self.backend.read_version(user_id, session_id, key)
    }
}

#[async_trait]
impl AgentStateStore for MysqlAgentStateStore {
    async fn save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) {
        if require_session_id(session_id).is_err() {
            return;
        }
        if let Ok(json) = serde_json::to_string(&value) {
            let _ = self
                .backend
                .save_single_versioned(user_id, session_id, key, &json);
        }
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) {
        if require_session_id(session_id).is_err() {
            return;
        }
        // Java: `if (values.isEmpty()) return;` (MysqlAgentStateStore.java:571-573).
        // Without this guard an empty write reaches the backend with `from_index =
        // list_len(...)`, which trims every existing item — silently wiping the list.
        if values.is_empty() {
            return;
        }
        let _ = self.save_list_impl(user_id, session_id, key, values);
    }

    async fn get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Option<serde_json::Value> {
        if require_session_id(session_id).is_err() {
            return None;
        }
        let json = self.backend.get_single(user_id, session_id, key).ok()??;
        serde_json::from_str(&json).ok()
    }

    async fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Vec<serde_json::Value> {
        if require_session_id(session_id).is_err() {
            return Vec::new();
        }
        self.backend
            .get_list(user_id, session_id, key)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect()
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        if require_session_id(session_id).is_err() {
            return false;
        }
        self.backend.exists(user_id, session_id).unwrap_or(false)
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if require_session_id(session_id).is_err() {
            return;
        }
        let _ = self.backend.delete(user_id, session_id);
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        self.backend.list_session_ids(user_id).unwrap_or_default()
    }

    fn supports_versioning(&self) -> bool {
        true
    }

    async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> agentscope_core::state::VersionedState {
        if require_session_id(session_id).is_err() {
            return agentscope_core::state::VersionedState {
                value: None,
                version: agentscope_core::state::UNVERSIONED,
            };
        }
        let version = self
            .backend
            .read_version(user_id, session_id, key)
            .unwrap_or(agentscope_core::state::UNVERSIONED);
        agentscope_core::state::VersionedState {
            value: self.get(user_id, session_id, key).await,
            version,
        }
    }

    async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> i64 {
        let expected = if expected_version == agentscope_core::state::UNVERSIONED {
            None
        } else {
            Some(expected_version)
        };
        match MysqlAgentStateStore::save_if_version(self, user_id, session_id, key, value, expected)
            .await
        {
            Ok(v) => v,
            Err(_) => agentscope_core::state::UNVERSIONED,
        }
    }
}

fn require_session_id(session_id: &str) -> MysqlResult<String> {
    if session_id.trim().is_empty() {
        return Err(MysqlError::BlankSessionId);
    }
    Ok(session_id.to_string())
}

#[cfg(test)]
pub mod testing;

#[cfg(test)]
mod tests {
    use super::testing::{InMemoryStateBackend, StaleVersionBackend};
    use super::*;
    use serde_json::json;

    fn store() -> MysqlAgentStateStore {
        MysqlAgentStateStore::new(Arc::new(InMemoryStateBackend::default()))
    }

    #[tokio::test]
    async fn save_and_get_single() {
        let s = store();
        s.save(Some("u"), "sess", "k", json!({"a": 1})).await;
        assert_eq!(s.get(Some("u"), "sess", "k").await, Some(json!({"a": 1})));
    }

    #[tokio::test]
    async fn missing_returns_none() {
        let s = store();
        assert_eq!(s.get(Some("u"), "sess", "k").await, None);
    }

    #[tokio::test]
    async fn save_list_incremental_append() {
        let s = store();
        s.save_list(Some("u"), "sess", "msgs", &[json!(1), json!(2)])
            .await;
        s.save_list(Some("u"), "sess", "msgs", &[json!(1), json!(2), json!(3)])
            .await;
        assert_eq!(
            s.get_list(Some("u"), "sess", "msgs").await,
            vec![json!(1), json!(2), json!(3)]
        );
    }

    #[tokio::test]
    async fn save_list_shrink_trims() {
        let s = store();
        s.save_list(Some("u"), "sess", "msgs", &[json!(1), json!(2), json!(3)])
            .await;
        s.save_list(Some("u"), "sess", "msgs", &[json!(9)]).await;
        assert_eq!(s.get_list(Some("u"), "sess", "msgs").await, vec![json!(9)]);
    }

    // Java: MysqlAgentStateStore#saveList — `values.isEmpty()` returns early, so an
    // empty write is a no-op rather than a clear.
    #[tokio::test]
    async fn save_list_empty_is_a_no_op() {
        let s = store();
        s.save_list(Some("u"), "sess", "msgs", &[json!(1), json!(2)])
            .await;
        s.save_list(Some("u"), "sess", "msgs", &[]).await;
        assert_eq!(
            s.get_list(Some("u"), "sess", "msgs").await,
            vec![json!(1), json!(2)]
        );
    }

    #[tokio::test]
    async fn exists_and_delete() {
        let s = store();
        assert!(!s.exists(Some("u"), "sess").await);
        s.save(Some("u"), "sess", "k", json!(1)).await;
        assert!(s.exists(Some("u"), "sess").await);
        s.delete(Some("u"), "sess").await;
        assert!(!s.exists(Some("u"), "sess").await);
    }

    #[tokio::test]
    async fn list_session_ids_scopes_by_user() {
        let s = store();
        s.save(Some("u1"), "a", "k", json!(1)).await;
        s.save(Some("u1"), "b", "k", json!(1)).await;
        s.save(Some("u2"), "c", "k", json!(1)).await;
        let mut ids = s.list_session_ids(Some("u1")).await;
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn blank_session_id_is_noop() {
        let s = store();
        s.save(Some("u"), "  ", "k", json!(1)).await;
        assert!(!s.exists(Some("u"), "  ").await);
    }

    #[tokio::test]
    async fn save_if_version_unversioned_bumps() {
        let s = store();
        let v1 = s
            .save_if_version(Some("u"), "sess", "k", json!(1), None)
            .await
            .unwrap();
        let v2 = s
            .save_if_version(Some("u"), "sess", "k", json!(2), None)
            .await
            .unwrap();
        assert_eq!((v1, v2), (1, 2));
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 2);
    }

    #[tokio::test]
    async fn save_if_version_conflict_detected() {
        let s = store();
        s.save_if_version(Some("u"), "sess", "k", json!(1), None)
            .await
            .unwrap();
        let ok = s
            .save_if_version(Some("u"), "sess", "k", json!(2), Some(1))
            .await;
        assert!(ok.is_ok());
        let bad = s
            .save_if_version(Some("u"), "sess", "k", json!(3), Some(1))
            .await;
        assert!(bad.is_err());
    }

    #[test]
    fn identifier_validation() {
        assert!(validate_identifier("agentscope_sessions", "Table name").is_ok());
        assert!(validate_identifier("1bad", "Table name").is_err());
        assert!(validate_identifier("bad name", "Table name").is_err());
        assert!(validate_identifier(&"x".repeat(65), "Table name").is_err());
    }

    /// Store plus a handle on its backend, for white-box assertions on the raw rows.
    fn store_with_backend() -> (MysqlAgentStateStore, Arc<InMemoryStateBackend>) {
        let backend = Arc::new(InMemoryStateBackend::default());
        (MysqlAgentStateStore::new(backend.clone()), backend)
    }

    // ---- list change detection (Java `ListHashUtil`) ------------------------------

    /// Java `save(list)` compares the stored `:_hash` with the hash of the stored prefix, so
    /// mutating an element in place forces a full rewrite. The previous Rust implementation
    /// only appended from `list_len()`, leaving the mutated item stale.
    #[tokio::test]
    async fn save_list_detects_mutated_prefix() {
        let s = store();
        s.save_list(Some("u"), "sess", "msgs", &[json!(1), json!(2), json!(3)])
            .await;
        s.save_list(Some("u"), "sess", "msgs", &[json!(1), json!(9), json!(3)])
            .await;
        assert_eq!(
            s.get_list(Some("u"), "sess", "msgs").await,
            vec![json!(1), json!(9), json!(3)]
        );
    }

    #[tokio::test]
    async fn save_list_persists_core_list_hash_row() {
        let (s, backend) = store_with_backend();
        let values = vec![json!(1), json!(2)];
        s.save_list(Some("u"), "sess", "msgs", &values).await;
        let stored = backend
            .get_single(Some("u"), "sess", &format!("msgs{HASH_KEY_SUFFIX}"))
            .unwrap()
            .expect("hash companion row must exist after a list save");
        assert_eq!(stored, list_hash::compute_hash(&values));
    }

    #[tokio::test]
    async fn save_list_unchanged_skips_backend_write() {
        let (s, backend) = store_with_backend();
        let values = vec![json!(1), json!(2), json!(3)];
        s.save_list(Some("u"), "sess", "msgs", &values).await;
        assert_eq!(backend.list_write_count(), 1);
        s.save_list(Some("u"), "sess", "msgs", &values).await;
        assert_eq!(
            backend.list_write_count(),
            1,
            "an unchanged list must not be rewritten"
        );
    }

    // ---- version column / atomic CAS ---------------------------------------------

    /// Java `save` upserts with `version = version + 1`; the version lives in the row, not in
    /// the `:_hash` companion row.
    #[tokio::test]
    async fn save_upsert_bumps_version_column() {
        let s = store();
        s.save(Some("u"), "sess", "k", json!(1)).await;
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 1);
        s.save(Some("u"), "sess", "k", json!(2)).await;
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 2);
    }

    #[tokio::test]
    async fn get_versioned_returns_stored_version() {
        let s = store();
        s.save_if_version(Some("u"), "sess", "k", json!(5), None)
            .await
            .unwrap();
        let versioned = s.get_versioned(Some("u"), "sess", "k").await;
        assert_eq!(versioned.version, 1);
        assert_eq!(versioned.value, Some(json!(5)));
    }

    /// `expected_version == 0` is a real create-if-absent: the row must not be overwritten.
    #[tokio::test]
    async fn save_if_version_zero_creates_then_conflicts() {
        let s = store();
        let v = s
            .save_if_version(Some("u"), "sess", "k", json!(1), Some(0))
            .await
            .unwrap();
        assert_eq!(v, 1);
        assert!(s
            .save_if_version(Some("u"), "sess", "k", json!(2), Some(0))
            .await
            .is_err());
        assert_eq!(s.get(Some("u"), "sess", "k").await, Some(json!(1)));
    }

    #[tokio::test]
    async fn cas_conflict_does_not_write() {
        let s = store();
        s.save_if_version(Some("u"), "sess", "k", json!("a"), None)
            .await
            .unwrap();
        let err = s
            .save_if_version(Some("u"), "sess", "k", json!("b"), Some(7))
            .await;
        assert!(err.is_err());
        assert_eq!(s.get(Some("u"), "sess", "k").await, Some(json!("a")));
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 1);
    }

    /// The CAS must be decided by the backend primitive, never by a prior `read_version`
    /// (which is racy: two writers can both read the same version and both then write).
    #[tokio::test]
    async fn save_if_version_delegates_cas_instead_of_read_then_write() {
        let backend = Arc::new(StaleVersionBackend::new(InMemoryStateBackend::default()));
        let s = MysqlAgentStateStore::new(backend);
        s.save_if_version(Some("u"), "sess", "k", json!(1), None)
            .await
            .unwrap();
        // A read-then-compare implementation would see version 0 here and reject the write.
        let v = s
            .save_if_version(Some("u"), "sess", "k", json!(2), Some(1))
            .await
            .expect("atomic CAS must not consult the (stale) version reader");
        assert_eq!(v, 2);
        assert_eq!(s.get(Some("u"), "sess", "k").await, Some(json!(2)));
        // The stale reader is still stale — proof that the write never went through it.
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 0);
    }
}
