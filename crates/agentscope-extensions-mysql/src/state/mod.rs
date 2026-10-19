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
//! over a `state_key:_hash` companion row (mirroring `HASH_KEY_SUFFIX`), preserving the
//! cross-language semantics without inventing a parallel trait.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::state::AgentStateStore;

use crate::error::{MysqlError, MysqlResult};

/// Suffix for hash (version) storage keys, mirroring `HASH_KEY_SUFFIX`.
pub const HASH_KEY_SUFFIX: &str = ":_hash";

/// `item_index` value for single state values.
pub const SINGLE_STATE_INDEX: i64 = 0;

/// Default database name, mirroring `DEFAULT_DATABASE_NAME`.
pub const DEFAULT_DATABASE_NAME: &str = "agentscope";

/// Default table name, mirroring `DEFAULT_TABLE_NAME`.
pub const DEFAULT_TABLE_NAME: &str = "agentscope_sessions";

/// Low-level SQL surface used by [`MysqlAgentStateStore`].
///
/// Single state is stored as a full replacement at `item_index = 0`; list state appends
/// only the new trailing items (`PUT` from `from_index`) and removes any stale trailing
/// rows beyond the new length.
pub trait StateBackend: Send + Sync {
    /// Ensures the schema (database + table) exists.
    fn ensure_schema(&self) -> MysqlResult<()>;

    /// Full-replacement save of a single value at `item_index = 0`.
    fn save_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<()>;

    /// Reads the single value at `item_index = 0`.
    fn get_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Option<String>>;

    /// Appends list values starting at `from_index`, then trims any rows at or beyond the
    /// new total length (so shrinking a list removes stale rows).
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

    /// Reads the current version for `state_key` (0 when absent). Mirrors `readVersion`.
    fn read_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<i64> {
        match self.get_single(user_id, session_id, &format!("{state_key}{HASH_KEY_SUFFIX}"))? {
            Some(json) => Ok(json.trim().parse::<i64>().unwrap_or(0)),
            None => Ok(0),
        }
    }

    /// Writes the current version for `state_key`. Mirrors the version column `save`.
    fn write_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        version: i64,
    ) -> MysqlResult<()> {
        self.save_single(
            user_id,
            session_id,
            &format!("{state_key}{HASH_KEY_SUFFIX}"),
            &version.to_string(),
        )
    }
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
        validate_identifier(&database_name, "Database name")?;
        validate_identifier(&table_name, "Table name")?;

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

    /// Full-replacement save plus version bump; returns the new version.
    ///
    /// Mirrors Java `AgentStateStore.saveIfVersion` with an `UNVERSIONED` expectation:
    /// the value is written unconditionally and the resulting version is read back.
    pub async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: Option<i64>,
    ) -> MysqlResult<i64> {
        require_session_id(session_id)?;
        match expected_version {
            Some(expected) => {
                let current = self
                    .backend
                    .read_version(user_id, session_id, key)?;
                if current != expected {
                    return Err(MysqlError::InvalidArgument(format!(
                        "version conflict for key '{key}': expected {expected}, found {current}"
                    )));
                }
                self.save(user_id, session_id, key, value).await;
                let next = current + 1;
                self.backend
                    .write_version(user_id, session_id, key, next)?;
                Ok(next)
            }
            None => {
                self.save(user_id, session_id, key, value).await;
                let next = self.backend.read_version(user_id, session_id, key)? + 1;
                self.backend
                    .write_version(user_id, session_id, key, next)?;
                Ok(next)
            }
        }
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
            let _ = self.backend.save_single(user_id, session_id, key, &json);
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
        let encoded: Vec<String> = values
            .iter()
            .filter_map(|v| serde_json::to_string(v).ok())
            .collect();
        // Incremental append: only write items beyond the current length, then trim.
        let from_index = self
            .backend
            .list_len(user_id, session_id, key)
            .unwrap_or(0);
        let _ = self
            .backend
            .save_list(user_id, session_id, key, &encoded, from_index);
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
        let json = self
            .backend
            .get_single(user_id, session_id, key)
            .ok()??;
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

/// Pattern for validating database and table names, mirroring `IDENTIFIER_PATTERN`.
fn validate_identifier(name: &str, label: &str) -> MysqlResult<()> {
    const MAX_IDENTIFIER_LENGTH: usize = 64;
    let mut chars = name.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !first_ok || !rest_ok {
        return Err(MysqlError::InvalidArgument(format!(
            "{label} contains invalid characters: {name:?}"
        )));
    }
    if name.len() > MAX_IDENTIFIER_LENGTH {
        return Err(MysqlError::InvalidArgument(format!(
            "{label} exceeds {MAX_IDENTIFIER_LENGTH} characters: {name:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub mod testing;

#[cfg(test)]
mod tests {
    use super::testing::InMemoryStateBackend;
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
        assert_eq!(
            s.get_list(Some("u"), "sess", "msgs").await,
            vec![json!(9)]
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
}
