//! PostgreSQL-backed [`AgentStateStore`] implementation.
//!
//! Mirrors the Java `PostgresAgentStateStore`:
//! * session state stored in `(session_id, state_key, item_index)` rows;
//! * single values at `item_index = 0`, list items at `0..n`;
//! * a `":_hash"` companion row for list change detection;
//! * versioned CAS via `save_if_version` (version starts at 1, `0` = unversioned).

use std::sync::Arc;

use agentscope_core::state::{list_hash, AgentStateStore};
use async_trait::async_trait;
use serde_json::Value;

use crate::client::{Cell, PostgresClient, QueryOutcome, SqlValue};
use crate::error::{PgResult, PostgresError};
use crate::sql::{
    ensure_version_column_ddl, qualified_table, sessions_ddl, validate_identifier,
    SCHEMA_EXISTS_QUERY, TABLE_EXISTS_QUERY,
};

/// Default schema name, matching the Java `DEFAULT_SCHEMA_NAME`.
pub const DEFAULT_SCHEMA_NAME: &str = "agentscope";
/// Default session table name, matching the Java `DEFAULT_TABLE_NAME`.
pub const DEFAULT_TABLE_NAME: &str = "agentscope_sessions";
/// Suffix for the list-hash companion row.
pub const HASH_KEY_SUFFIX: &str = ":_hash";
/// `item_index` used for single values.
pub const SINGLE_STATE_INDEX: i32 = 0;
/// Sentinel returned by `save_if_version` when a CAS fails.
pub const UNVERSIONED: i64 = -1;
/// Maximum session-id / state-key length, matching the Java `validateSessionId`/`validateStateKey`
/// and the `VARCHAR(255)` columns.
pub const MAX_SESSION_ID_LENGTH: usize = 255;
/// Anonymous-user sentinel (matches the core `InMemoryAgentStateStore`).
pub const ANON_USER: &str = "__anon__";

/// Builder for [`PostgresAgentStateStore`].
#[derive(Debug, Clone)]
pub struct PostgresAgentStateStoreBuilder {
    client: Arc<dyn PostgresClient>,
    schema: String,
    table: String,
    create_if_not_exist: bool,
}

impl PostgresAgentStateStoreBuilder {
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self {
            client,
            schema: DEFAULT_SCHEMA_NAME.to_string(),
            table: DEFAULT_TABLE_NAME.to_string(),
            create_if_not_exist: false,
        }
    }

    pub fn schema(mut self, schema: impl Into<String>) -> PgResult<Self> {
        self.schema = validate_identifier(&schema.into(), "Schema name")?;
        Ok(self)
    }

    pub fn table(mut self, table: impl Into<String>) -> PgResult<Self> {
        self.table = validate_identifier(&table.into(), "Table name")?;
        Ok(self)
    }

    pub fn create_if_not_exist(mut self, create: bool) -> Self {
        self.create_if_not_exist = create;
        self
    }

    pub fn build(self) -> PgResult<PostgresAgentStateStore> {
        let full_table = qualified_table(Some(&self.schema), &self.table);
        if self.create_if_not_exist {
            let ddl = format!(
                "CREATE SCHEMA IF NOT EXISTS \"{}\";\n{}",
                self.schema,
                sessions_ddl(&full_table)
            );
            self.client.execute_batch(&ddl)?;
        } else {
            verify_schema_exists(self.client.as_ref(), &self.schema)?;
            verify_table_exists(self.client.as_ref(), &self.schema, &self.table)?;
        }
        // Back-fill the `version` column for tables created before versioning existed.
        self.client
            .execute_batch(&ensure_version_column_ddl(&full_table))?;
        Ok(PostgresAgentStateStore {
            client: self.client,
            schema: self.schema,
            table: self.table,
            full_table,
        })
    }
}

/// Verifies the schema exists, mirroring the Java `verifySchemaExists`.
///
/// Raises [`PostgresError::Schema`] when the schema is absent, and
/// [`PostgresError::Query`] when the check itself fails.
fn verify_schema_exists(client: &dyn PostgresClient, schema: &str) -> PgResult<()> {
    let params = [SqlValue::text(schema)];
    match client.query(SCHEMA_EXISTS_QUERY, &params) {
        Ok(QueryOutcome::Rows(rows)) if !rows.is_empty() => Ok(()),
        Ok(_) => Err(PostgresError::Schema(format!(
            "Schema does not exist: {schema}. Use create_if_not_exist(true) to auto-create."
        ))),
        Err(e) => Err(PostgresError::Query(format!(
            "Failed to check schema existence: {schema}: {e}"
        ))),
    }
}

/// Verifies the table exists, mirroring the Java `verifyTableExists`.
fn verify_table_exists(client: &dyn PostgresClient, schema: &str, table: &str) -> PgResult<()> {
    let params = [SqlValue::text(schema), SqlValue::text(table)];
    match client.query(TABLE_EXISTS_QUERY, &params) {
        Ok(QueryOutcome::Rows(rows)) if !rows.is_empty() => Ok(()),
        Ok(_) => Err(PostgresError::Schema(format!(
            "Table does not exist: {schema}.{table}. Use create_if_not_exist(true) to auto-create."
        ))),
        Err(e) => Err(PostgresError::Query(format!(
            "Failed to check table existence: {table}: {e}"
        ))),
    }
}

/// PostgreSQL-backed [`AgentStateStore`].
#[derive(Debug, Clone)]
pub struct PostgresAgentStateStore {
    client: Arc<dyn PostgresClient>,
    schema: String,
    table: String,
    full_table: String,
}

impl PostgresAgentStateStore {
    /// Creates a builder over the given client.
    pub fn builder(client: Arc<dyn PostgresClient>) -> PostgresAgentStateStoreBuilder {
        PostgresAgentStateStoreBuilder::new(client)
    }

    /// Creates a store with schema/table auto-creation (mirrors
    /// `new PostgresAgentStateStore(dataSource, true)`).
    pub fn create_if_not_exist(client: Arc<dyn PostgresClient>) -> PgResult<Self> {
        Self::builder(client).create_if_not_exist(true).build()
    }

    pub fn schema_name(&self) -> &str {
        &self.schema
    }

    pub fn table_name(&self) -> &str {
        &self.table
    }

    fn slot_id(user_id: Option<&str>, session_id: &str) -> PgResult<String> {
        if session_id.trim().is_empty() {
            return Err(PostgresError::Query("sessionId must not be blank".into()));
        }
        // Parity with the Java `validateSessionId`: no path separators, bounded length.
        if session_id.contains('/') || session_id.contains('\\') {
            return Err(PostgresError::Query(
                "AgentStateStore ID cannot contain path separators".into(),
            ));
        }
        if session_id.len() > MAX_SESSION_ID_LENGTH {
            return Err(PostgresError::Query(format!(
                "AgentStateStore ID cannot exceed {MAX_SESSION_ID_LENGTH} characters"
            )));
        }
        let user = match user_id {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => ANON_USER.to_string(),
        };
        Ok(format!("{user}:{session_id}"))
    }

    fn validate_state_key(key: &str) -> PgResult<()> {
        if key.trim().is_empty() {
            return Err(PostgresError::Query("state key must not be blank".into()));
        }
        if key.len() > MAX_SESSION_ID_LENGTH {
            return Err(PostgresError::Query(format!(
                "State key cannot exceed {MAX_SESSION_ID_LENGTH} characters"
            )));
        }
        Ok(())
    }

    /// Synchronous single-value read (returns the JSON payload).
    fn get_sync(&self, slot: &str, key: &str, item_index: i32) -> PgResult<Option<String>> {
        let sql = format!(
            "SELECT state_data FROM {} WHERE session_id = $1 AND state_key = $2 AND item_index = $3",
            self.full_table
        );
        let params = [
            SqlValue::text(slot),
            SqlValue::text(key),
            SqlValue::int(item_index),
        ];
        match self.client.query(&sql, &params)? {
            QueryOutcome::Rows(rows) => {
                Ok(rows.first().and_then(|r| r.first()).and_then(Cell::as_str).map(str::to_string))
            }
            QueryOutcome::Affected(_) => Ok(None),
        }
    }

    /// Synchronous list read (all rows for `key`, ordered by `item_index`).
    fn get_list_rows_sync(&self, slot: &str, key: &str) -> PgResult<Vec<String>> {
        let sql = format!(
            "SELECT state_data FROM {} WHERE session_id = $1 AND state_key = $2 ORDER BY item_index",
            self.full_table
        );
        let params = [SqlValue::text(slot), SqlValue::text(key)];
        match self.client.query(&sql, &params)? {
            QueryOutcome::Rows(rows) => Ok(rows
                .into_iter()
                .filter_map(|r| r.first().and_then(Cell::as_str).map(str::to_string))
                .collect()),
            QueryOutcome::Affected(_) => Ok(Vec::new()),
        }
    }

    fn read_version_sync(&self, slot: &str, key: &str) -> PgResult<i64> {
        let sql = format!(
            "SELECT version FROM {} WHERE session_id = $1 AND state_key = $2 AND item_index = $3",
            self.full_table
        );
        let params = [
            SqlValue::text(slot),
            SqlValue::text(key),
            SqlValue::int(SINGLE_STATE_INDEX),
        ];
        match self.client.query(&sql, &params)? {
            QueryOutcome::Rows(rows) => {
                Ok(rows.first().and_then(|r| r.first()).and_then(Cell::as_i64).unwrap_or(0))
            }
            QueryOutcome::Affected(_) => Ok(0),
        }
    }

    fn list_count_sync(&self, slot: &str, key: &str) -> PgResult<i32> {
        let sql = format!(
            "SELECT MAX(item_index) as max_index FROM {} WHERE session_id = $1 AND state_key = $2",
            self.full_table
        );
        let params = [SqlValue::text(slot), SqlValue::text(key)];
        match self.client.query(&sql, &params)? {
            QueryOutcome::Rows(rows) => match rows.first().and_then(|r| r.first()) {
                Some(cell) => Ok(cell.as_i32().map(|i| i + 1).unwrap_or(0)),
                None => Ok(0),
            },
            QueryOutcome::Affected(_) => Ok(0),
        }
    }

    /// Saves a single value (full replacement) and returns the new version.
    pub fn save_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &Value,
    ) -> PgResult<i64> {
        let slot = Self::slot_id(user_id, session_id)?;
        Self::validate_state_key(key)?;
        let json = serde_json::to_string(value)
            .map_err(|e| PostgresError::Encode(format!("state encode failed: {e}")))?;
        let now = chrono::Utc::now().timestamp_millis();
        let sql = format!(
            "INSERT INTO {} (session_id, state_key, item_index, state_data, version, updated_at) \
             VALUES ($1, $2, $3, $4, 1, $5) \
             ON CONFLICT (session_id, state_key, item_index) DO UPDATE SET \
                 state_data = EXCLUDED.state_data, \
                 version = {}.version + 1, \
                 updated_at = EXCLUDED.updated_at",
            self.full_table, self.full_table
        );
        let params = [
            SqlValue::text(slot.clone()),
            SqlValue::text(key),
            SqlValue::int(SINGLE_STATE_INDEX),
            SqlValue::text(json),
            SqlValue::BigInt(now),
        ];
        self.client.query(&sql, &params)?;
        self.read_version_sync(&slot, key)
    }

    /// CAS single-value save. Returns the new version, or [`UNVERSIONED`] on failure.
    /// `expected_version == UNVERSIONED` behaves as an unconditional save.
    pub fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &Value,
        expected_version: i64,
    ) -> PgResult<i64> {
        if expected_version == UNVERSIONED {
            return self.save_versioned(user_id, session_id, key, value);
        }
        let slot = Self::slot_id(user_id, session_id)?;
        Self::validate_state_key(key)?;
        let json = serde_json::to_string(value)
            .map_err(|e| PostgresError::Encode(format!("state encode failed: {e}")))?;
        let now = chrono::Utc::now().timestamp_millis();

        if expected_version == 0 {
            let sql = format!(
                "INSERT INTO {} (session_id, state_key, item_index, state_data, version, updated_at) \
                 VALUES ($1, $2, $3, $4, 1, $5) \
                 ON CONFLICT (session_id, state_key, item_index) DO NOTHING",
                self.full_table
            );
            let params = [
                SqlValue::text(slot),
                SqlValue::text(key),
                SqlValue::int(SINGLE_STATE_INDEX),
                SqlValue::text(json),
                SqlValue::BigInt(now),
            ];
            return match self.client.query(&sql, &params)? {
                QueryOutcome::Affected(1) => Ok(1),
                _ => Ok(UNVERSIONED),
            };
        }

        let new_version = expected_version + 1;
        let sql = format!(
            "UPDATE {} SET state_data = $1, version = $2, updated_at = $3 \
             WHERE session_id = $4 AND state_key = $5 AND item_index = $6 AND version = $7",
            self.full_table
        );
        let params = [
            SqlValue::text(json),
            SqlValue::BigInt(new_version),
            SqlValue::BigInt(now),
            SqlValue::text(slot),
            SqlValue::text(key),
            SqlValue::int(SINGLE_STATE_INDEX),
            SqlValue::BigInt(expected_version),
        ];
        match self.client.query(&sql, &params)? {
            QueryOutcome::Affected(1) => Ok(new_version),
            _ => Ok(UNVERSIONED),
        }
    }

    /// Reads the current version for a single key (0 when absent).
    pub fn version_of(&self, user_id: Option<&str>, session_id: &str, key: &str) -> PgResult<i64> {
        let slot = Self::slot_id(user_id, session_id)?;
        Self::validate_state_key(key)?;
        self.read_version_sync(&slot, key)
    }

    /// Synchronous list save with hash-based incremental append (mirrors Java's `save`).
    fn save_list_sync(&self, slot: &str, key: &str, values: &[Value]) -> PgResult<()> {
        if values.is_empty() {
            return Ok(());
        }
        let hash_key = format!("{key}{HASH_KEY_SUFFIX}");
        let current_hash = list_hash::compute_hash(values);
        let stored_hash = self.get_sync(slot, &hash_key, SINGLE_STATE_INDEX)?;
        let existing_count = self.list_count_sync(slot, key)? as usize;
        let needs_full_rewrite =
            list_hash::needs_full_rewrite(values, stored_hash.as_deref(), existing_count);

        if needs_full_rewrite {
            self.delete_list_items_sync(slot, key)?;
            self.insert_items_sync(slot, key, values, 0)?;
            self.save_hash_sync(slot, &hash_key, &current_hash)?;
        } else if values.len() > existing_count {
            let new_items = &values[existing_count..];
            self.insert_items_sync(slot, key, new_items, existing_count)?;
            self.save_hash_sync(slot, &hash_key, &current_hash)?;
        }
        Ok(())
    }

    fn delete_list_items_sync(&self, slot: &str, key: &str) -> PgResult<()> {
        let sql = format!(
            "DELETE FROM {} WHERE session_id = $1 AND state_key = $2",
            self.full_table
        );
        let params = [SqlValue::text(slot), SqlValue::text(key)];
        self.client.query(&sql, &params)?;
        Ok(())
    }

    fn insert_items_sync(
        &self,
        slot: &str,
        key: &str,
        items: &[Value],
        start_index: usize,
    ) -> PgResult<()> {
        let sql = format!(
            "INSERT INTO {} (session_id, state_key, item_index, state_data) VALUES ($1, $2, $3, $4)",
            self.full_table
        );
        for (offset, item) in items.iter().enumerate() {
            let json = serde_json::to_string(item)
                .map_err(|e| PostgresError::Encode(format!("list item encode failed: {e}")))?;
            let params = [
                SqlValue::text(slot),
                SqlValue::text(key),
                SqlValue::int((start_index + offset) as i32),
                SqlValue::text(json),
            ];
            self.client.query(&sql, &params)?;
        }
        Ok(())
    }

    fn save_hash_sync(&self, slot: &str, hash_key: &str, hash: &str) -> PgResult<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let sql = format!(
            "INSERT INTO {} (session_id, state_key, item_index, state_data, updated_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (session_id, state_key, item_index) DO UPDATE SET \
                 state_data = EXCLUDED.state_data, \
                 updated_at = EXCLUDED.updated_at",
            self.full_table
        );
        let params = [
            SqlValue::text(slot),
            SqlValue::text(hash_key),
            SqlValue::int(SINGLE_STATE_INDEX),
            SqlValue::text(hash),
            SqlValue::BigInt(now),
        ];
        self.client.query(&sql, &params)?;
        Ok(())
    }

    /// Truncates the whole session table (mirrors `truncateAllSessions`).
    pub fn truncate_all_sessions(&self) -> PgResult<()> {
        self.client
            .execute_batch(&format!("TRUNCATE TABLE {}", self.full_table))
    }
}

#[async_trait]
impl AgentStateStore for PostgresAgentStateStore {
    async fn save(&self, user_id: Option<&str>, session_id: &str, key: &str, value: Value) {
        let _ = self.save_versioned(user_id, session_id, key, &value);
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[Value],
    ) {
        let Ok(slot) = Self::slot_id(user_id, session_id) else {
            return;
        };
        if Self::validate_state_key(key).is_err() {
            return;
        }
        let _ = self.save_list_sync(&slot, key, values);
    }

    async fn get(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Option<Value> {
        let slot = Self::slot_id(user_id, session_id).ok()?;
        if Self::validate_state_key(key).is_err() {
            return None;
        }
        let json = self.get_sync(&slot, key, SINGLE_STATE_INDEX).ok()??;
        serde_json::from_str(&json).ok()
    }

    async fn get_list(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Vec<Value> {
        let Ok(slot) = Self::slot_id(user_id, session_id) else {
            return Vec::new();
        };
        if Self::validate_state_key(key).is_err() {
            return Vec::new();
        }
        self.get_list_rows_sync(&slot, key)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect()
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        let Ok(slot) = Self::slot_id(user_id, session_id) else {
            return false;
        };
        let sql = format!(
            "SELECT 1 FROM {} WHERE session_id = $1 LIMIT 1",
            self.full_table
        );
        let params = [SqlValue::text(slot)];
        matches!(
            self.client.query(&sql, &params),
            Ok(QueryOutcome::Rows(rows)) if !rows.is_empty()
        )
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        let Ok(slot) = Self::slot_id(user_id, session_id) else {
            return;
        };
        let sql = format!("DELETE FROM {} WHERE session_id = $1", self.full_table);
        let params = [SqlValue::text(slot)];
        let _ = self.client.query(&sql, &params);
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let user_segment = match user_id {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => ANON_USER.to_string(),
        };
        let prefix = format!("{user_segment}:");
        let pattern = format!("{prefix}%");
        let sql = format!(
            "SELECT DISTINCT session_id FROM {} WHERE session_id LIKE $1 ORDER BY session_id",
            self.full_table
        );
        let params = [SqlValue::text(pattern)];
        match self.client.query(&sql, &params) {
            Ok(QueryOutcome::Rows(rows)) => {
                let mut ids: Vec<String> = rows
                    .into_iter()
                    .filter_map(|r| r.first().and_then(Cell::as_str).map(str::to_string))
                    .filter_map(|slot| slot.strip_prefix(&prefix).map(str::to_string))
                    .collect();
                ids.sort();
                ids.dedup();
                ids
            }
            _ => Vec::new(),
        }
    }
}
