//! PostgreSQL-backed [`AgentStateStore`] implementation.
//!
//! Mirrors the Java `PostgresAgentStateStore`:
//! * session state stored in `(session_id, state_key, item_index)` rows;
//! * single values at `item_index = 0`, list items at `0..n`;
//! * a `":_hash"` companion row for list change detection;
//! * versioned CAS via `save_if_version` (version starts at 1, `0` = unversioned);
//! * every [`AgentStateStore`] method is `async` but the SQL seam is synchronous, so each one
//!   hops onto the blocking pool ([`crate::blocking`]) before touching the client — calling
//!   `PgSimpleClient` directly from an async context would `block_on` inside a runtime and
//!   panic.

use std::sync::Arc;

use agentscope_core::state::{list_hash, AgentStateStore};
use async_trait::async_trait;
use serde_json::Value;

use crate::client::{Cell, PostgresClient, QueryOutcome, SqlStatement, SqlValue};
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
///
/// `create_if_not_exist` defaults to `true`, matching the Java
/// `PostgresAgentStateStore.Builder` (`private boolean createIfNotExist = true;`), so a bare
/// `PostgresAgentStateStore::builder(client).build()` provisions the schema and table. Use
/// [`PostgresAgentStateStoreBuilder::create_if_not_exist(false)`] for the verifying, no-DDL
/// path — that is what the Java `new PostgresAgentStateStore(dataSource)` constructor does.
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
            // Java `Builder.createIfNotExist` defaults to `true`; keep the same default so a
            // straight port of `PostgresAgentStateStore.builder(ds).build()` keeps working.
            create_if_not_exist: true,
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

    /// Runs a blocking store operation on the blocking pool.
    ///
    /// [`AgentStateStore`] is async but [`PostgresClient`] is synchronous, and `PgSimpleClient`
    /// drives the latter with its own runtime + `block_on` — which panics when the calling
    /// thread is already inside a Tokio runtime. Every async method therefore hops onto the
    /// blocking pool first, exactly as documented in [`crate::blocking`].
    async fn run_blocking<T, F>(&self, op: F) -> PgResult<T>
    where
        F: FnOnce(Self) -> PgResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.clone();
        match tokio::task::spawn_blocking(move || op(store)).await {
            Ok(result) => result,
            Err(e) => Err(PostgresError::Query(format!(
                "blocking task join failed: {e}"
            ))),
        }
    }

    /// Session existence probe (sync; backs the async [`AgentStateStore::exists`]).
    fn session_exists_sync(&self, slot: &str) -> PgResult<bool> {
        let sql = format!(
            "SELECT 1 FROM {} WHERE session_id = $1 LIMIT 1",
            self.full_table
        );
        match self.client.query(&sql, &[SqlValue::text(slot)])? {
            QueryOutcome::Rows(rows) => Ok(!rows.is_empty()),
            QueryOutcome::Affected(n) => Ok(n > 0),
        }
    }

    /// Session delete (sync; backs the async [`AgentStateStore::delete`]).
    fn delete_session_sync(&self, slot: &str) -> PgResult<()> {
        let sql = format!("DELETE FROM {} WHERE session_id = $1", self.full_table);
        self.client.query(&sql, &[SqlValue::text(slot)])?;
        Ok(())
    }

    /// Session-id listing for one user segment (sync; backs
    /// [`AgentStateStore::list_session_ids`]).
    fn list_session_ids_sync(&self, user_segment: &str) -> PgResult<Vec<String>> {
        let prefix = format!("{user_segment}:");
        let pattern = format!("{prefix}%");
        let sql = format!(
            "SELECT DISTINCT session_id FROM {} WHERE session_id LIKE $1 ORDER BY session_id",
            self.full_table
        );
        let QueryOutcome::Rows(rows) = self.client.query(&sql, &[SqlValue::text(pattern)])? else {
            return Ok(Vec::new());
        };
        let mut ids: Vec<String> = rows
            .into_iter()
            .filter_map(|r| r.first().and_then(Cell::as_str).map(str::to_string))
            .filter_map(|slot| slot.strip_prefix(&prefix).map(str::to_string))
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    fn slot_id(user_id: Option<&str>, session_id: &str) -> PgResult<String> {
        if session_id.trim().is_empty() {
            return Err(PostgresError::Query("sessionId must not be blank".into()));
        }
        // Path separators are allowed: the slot id is only ever bound as a prepared-statement
        // parameter here, never used as a filesystem path, and sandbox stores legitimately
        // generate slash-separated slot ids (`sandbox/session/<id>`); rejecting them silently
        // dropped all sandbox resume state (Java 7d10efc3b, #3231). Only the blank check and
        // the bounded length remain.
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
            QueryOutcome::Rows(rows) => Ok(rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_str)
                .map(str::to_string)),
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
            QueryOutcome::Rows(rows) => Ok(rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_i64)
                .unwrap_or(0)),
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
            // `sessions_ddl` declares `updated_at TIMESTAMP`; binding the millisecond value
            // as `BIGINT` failed at the driver's Bind stage (`cannot convert between i64 and
            // timestamp`), and `save` only logs such errors (the table stayed empty).
            SqlValue::Timestamp(now),
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
                SqlValue::text(&slot),
                SqlValue::text(key),
                SqlValue::int(SINGLE_STATE_INDEX),
                SqlValue::text(&json),
                SqlValue::Timestamp(now),
            ];
            match self.client.query(&sql, &params)? {
                QueryOutcome::Affected(1) => return Ok(1),
                // The row already exists. If its stored version is still `0` (e.g. back-filled
                // by an older ALTER TABLE that used DEFAULT 0), that satisfies the CAS the
                // caller asked for: fall through to the generic branch, which bumps 0 -> 1.
                // A concurrent writer that already moved it past 0 matches nothing there and
                // correctly reports a conflict. Mirrors Java `PostgresAgentStateStore`
                // and its regression test
                // `saveIfVersionZeroFallsBackToCasUpdateForBackfilledRow`.
                _ => {}
            }
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
            SqlValue::Timestamp(now),
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
    ///
    /// Both write shapes are multi-statement (delete + N inserts + hash row), so they go
    /// through [`PostgresClient::execute_in_transaction`]: Java wraps the same work in
    /// `executeInWriteTransaction`, and a half-applied rewrite would leave the list and its
    /// `":_hash"` companion row out of sync forever.
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
            let mut statements = vec![Self::delete_list_items_stmt(&self.full_table, slot, key)];
            statements.extend(Self::insert_items_stmts(
                &self.full_table,
                slot,
                key,
                values,
                0,
            )?);
            statements.push(Self::save_hash_stmt(
                &self.full_table,
                slot,
                &hash_key,
                &current_hash,
            ));
            self.client.execute_in_transaction(&statements)?;
        } else if values.len() > existing_count {
            let mut statements = Self::insert_items_stmts(
                &self.full_table,
                slot,
                key,
                &values[existing_count..],
                existing_count,
            )?;
            statements.push(Self::save_hash_stmt(
                &self.full_table,
                slot,
                &hash_key,
                &current_hash,
            ));
            self.client.execute_in_transaction(&statements)?;
        }
        Ok(())
    }

    fn delete_list_items_stmt(full_table: &str, slot: &str, key: &str) -> SqlStatement {
        SqlStatement::new(
            format!("DELETE FROM {full_table} WHERE session_id = $1 AND state_key = $2"),
            vec![SqlValue::text(slot), SqlValue::text(key)],
        )
    }

    fn insert_items_stmts(
        full_table: &str,
        slot: &str,
        key: &str,
        items: &[Value],
        start_index: usize,
    ) -> PgResult<Vec<SqlStatement>> {
        let sql = format!(
            "INSERT INTO {full_table} (session_id, state_key, item_index, state_data) \
             VALUES ($1, $2, $3, $4)"
        );
        let mut statements = Vec::with_capacity(items.len());
        for (offset, item) in items.iter().enumerate() {
            let json = serde_json::to_string(item)
                .map_err(|e| PostgresError::Encode(format!("list item encode failed: {e}")))?;
            statements.push(SqlStatement::new(
                sql.clone(),
                vec![
                    SqlValue::text(slot),
                    SqlValue::text(key),
                    SqlValue::int((start_index + offset) as i32),
                    SqlValue::text(json),
                ],
            ));
        }
        Ok(statements)
    }

    fn save_hash_stmt(full_table: &str, slot: &str, hash_key: &str, hash: &str) -> SqlStatement {
        let now = chrono::Utc::now().timestamp_millis();
        let sql = format!(
            "INSERT INTO {full_table} (session_id, state_key, item_index, state_data, updated_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (session_id, state_key, item_index) DO UPDATE SET \
                 state_data = EXCLUDED.state_data, \
                 updated_at = EXCLUDED.updated_at"
        );
        SqlStatement::new(
            sql,
            vec![
                SqlValue::text(slot),
                SqlValue::text(hash_key),
                SqlValue::int(SINGLE_STATE_INDEX),
                SqlValue::text(hash),
                SqlValue::Timestamp(now),
            ],
        )
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
        let user = user_id.map(str::to_string);
        let session = session_id.to_string();
        let key = key.to_string();
        let _ = self
            .run_blocking(move |store| {
                store.save_versioned(user.as_deref(), &session, &key, &value)
            })
            .await;
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
        let key = key.to_string();
        let values = values.to_vec();
        let _ = self
            .run_blocking(move |store| store.save_list_sync(&slot, &key, &values))
            .await;
    }

    async fn get(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Option<Value> {
        let slot = Self::slot_id(user_id, session_id).ok()?;
        if Self::validate_state_key(key).is_err() {
            return None;
        }
        let key = key.to_string();
        let json = self
            .run_blocking(move |store| store.get_sync(&slot, &key, SINGLE_STATE_INDEX))
            .await
            .ok()??;
        serde_json::from_str(&json).ok()
    }

    async fn get_list(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Vec<Value> {
        let Ok(slot) = Self::slot_id(user_id, session_id) else {
            return Vec::new();
        };
        if Self::validate_state_key(key).is_err() {
            return Vec::new();
        }
        let key = key.to_string();
        self.run_blocking(move |store| store.get_list_rows_sync(&slot, &key))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect()
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        let Ok(slot) = Self::slot_id(user_id, session_id) else {
            return false;
        };
        self.run_blocking(move |store| store.session_exists_sync(&slot))
            .await
            .unwrap_or(false)
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        let Ok(slot) = Self::slot_id(user_id, session_id) else {
            return;
        };
        let _ = self
            .run_blocking(move |store| store.delete_session_sync(&slot))
            .await;
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let user_segment = match user_id {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => ANON_USER.to_string(),
        };
        self.run_blocking(move |store| store.list_session_ids_sync(&user_segment))
            .await
            .unwrap_or_default()
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
        let unversioned = || agentscope_core::state::VersionedState {
            value: None,
            version: agentscope_core::state::UNVERSIONED,
        };
        let Ok(slot) = Self::slot_id(user_id, session_id) else {
            return unversioned();
        };
        if Self::validate_state_key(key).is_err() {
            return unversioned();
        }
        let key = key.to_string();
        let loaded = self
            .run_blocking(move |store| {
                let value = store.get_sync(&slot, &key, SINGLE_STATE_INDEX)?;
                let version = store.read_version_sync(&slot, &key)?;
                Ok((value, version))
            })
            .await;
        match loaded {
            Ok((Some(json), version)) => agentscope_core::state::VersionedState {
                value: serde_json::from_str(&json).ok(),
                version,
            },
            Ok((None, version)) => agentscope_core::state::VersionedState {
                value: None,
                version,
            },
            Err(_) => unversioned(),
        }
    }

    async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
        expected_version: i64,
    ) -> i64 {
        let user = user_id.map(str::to_string);
        let session = session_id.to_string();
        let key = key.to_string();
        match self
            .run_blocking(move |store| {
                store.save_if_version(
                    user.as_deref(),
                    &session,
                    &key,
                    &value,
                    expected_version,
                )
            })
            .await
        {
            Ok(v) => v,
            Err(_) => agentscope_core::state::UNVERSIONED,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Offline unit tests mirroring `PostgresAgentStateStoreTest` (JUnit5 + Mockito + H2 on
    //! the Java side). The crate's [`MemoryClient`] plays the role of H2: it recognises the
    //! exact statement shapes emitted by this module and simulates `ON CONFLICT`, conditional
    //! CAS, `LIKE` and the `INFORMATION_SCHEMA` existence checks.

    use super::*;
    use crate::memory::MemoryClient;
    use crate::sql::{MAX_IDENTIFIER_LENGTH, SCHEMA_EXISTS_QUERY, TABLE_EXISTS_QUERY};
    use agentscope_core::state::list_hash::compute_hash;
    use agentscope_core::state::AgentStateStore;
    use serde_json::json;

    fn client() -> Arc<MemoryClient> {
        Arc::new(MemoryClient::new())
    }

    /// Builds a store over the default schema/table, auto-creating them (the Java
    /// `new PostgresAgentStateStore(dataSource, true)`).
    fn store(client: &Arc<MemoryClient>) -> PostgresAgentStateStore {
        PostgresAgentStateStore::create_if_not_exist(client.clone()).unwrap()
    }

    fn slot(session: &str) -> String {
        format!("{ANON_USER}:{session}")
    }

    // ---- builder / schema verification -------------------------------------

    // Java: PostgresAgentStateStoreTest#builderWithDefaultsCreatesStore, #constructorWithAutoCreate
    #[test]
    fn builder_create_if_not_exist_provisions_schema_and_table() {
        let c = client();
        let s = store(&c);
        assert_eq!(s.schema_name(), DEFAULT_SCHEMA_NAME);
        assert_eq!(s.table_name(), DEFAULT_TABLE_NAME);
        assert!(c.has_schema(DEFAULT_SCHEMA_NAME));
        assert!(c.has_table(DEFAULT_TABLE_NAME));
        let ddl = c.ddl_statements();
        assert!(ddl.iter().any(|s| s.contains("CREATE TABLE IF NOT EXISTS")));
        // Java ensureVersionColumn(): the back-fill ALTER runs on every build, and uses
        // DEFAULT 1 because 0 means "row absent" to the CAS path.
        assert!(ddl
            .iter()
            .any(|s| s.contains("ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 1")));
    }

    // Java: PostgresAgentStateStoreTest#builderWithCustomNames
    #[test]
    fn builder_uses_custom_schema_and_table() {
        let c = client();
        let s = PostgresAgentStateStore::builder(c.clone())
            .schema("custom_schema")
            .unwrap()
            .table("custom_sessions")
            .unwrap()
            .create_if_not_exist(true)
            .build()
            .unwrap();
        assert_eq!(s.schema_name(), "custom_schema");
        assert_eq!(s.table_name(), "custom_sessions");
        assert!(c.has_schema("custom_schema"));
        assert!(c.has_table("custom_sessions"));
    }

    // Java: PostgresAgentStateStoreTest#rejectsInvalidSchemaName
    #[test]
    fn builder_rejects_invalid_schema_name() {
        let c = client();
        assert!(PostgresAgentStateStore::builder(c.clone())
            .schema("1bad")
            .is_err());
        assert!(PostgresAgentStateStore::builder(c.clone())
            .schema("bad-name")
            .is_err());
        assert!(PostgresAgentStateStore::builder(c).schema("   ").is_err());
    }

    // Java: PostgresAgentStateStoreTest#rejectsInvalidTableName
    #[test]
    fn builder_rejects_invalid_table_name() {
        let c = client();
        assert!(PostgresAgentStateStore::builder(c.clone())
            .table("bad name")
            .is_err());
        assert!(PostgresAgentStateStore::builder(c).table("").is_err());
    }

    // Java: PostgresAgentStateStoreTest#rejectsTooLongIdentifier
    #[test]
    fn builder_rejects_too_long_identifier() {
        let c = client();
        let ok = "a".repeat(MAX_IDENTIFIER_LENGTH);
        let too_long = "a".repeat(MAX_IDENTIFIER_LENGTH + 1);
        assert!(PostgresAgentStateStore::builder(c.clone())
            .table(ok)
            .is_ok());
        assert!(PostgresAgentStateStore::builder(c).table(too_long).is_err());
    }

    // Java: PostgresAgentStateStoreTest#constructorDefaultsDoNotAutoCreate,
    //       #verifySchemaMissingThrows
    //
    // NOTE: Java has two entry points with *different* defaults — the constructor
    // `new PostgresAgentStateStore(ds)` is `createIfNotExist = false` (this test), while
    // `builder(ds).build()` is `true` (see `builder_defaults_to_create_if_not_exist`).
    // The explicit `create_if_not_exist(false)` reproduces the constructor's default.
    #[test]
    fn build_without_auto_create_fails_when_schema_missing() {
        let c = client();
        let err = PostgresAgentStateStore::builder(c)
            .create_if_not_exist(false)
            .build()
            .unwrap_err();
        assert!(matches!(err, PostgresError::Schema(_)));
    }

    // Java: PostgresAgentStateStoreTest#verifyTableMissingThrows
    #[test]
    fn build_without_auto_create_fails_when_table_missing() {
        let c = client();
        c.execute_batch("CREATE SCHEMA IF NOT EXISTS \"agentscope\"")
            .unwrap();
        let err = PostgresAgentStateStore::builder(c)
            .create_if_not_exist(false)
            .build()
            .unwrap_err();
        assert!(matches!(err, PostgresError::Schema(_)));
    }

    // Java: PostgresAgentStateStoreTest#verifySchemaSqlExceptionThrows (Rust: the schema/
    // table existence probes are the two INFORMATION_SCHEMA queries in `sql.rs`).
    #[test]
    fn build_succeeds_when_schema_and_table_already_exist() {
        let c = client();
        let _ = store(&c);
        let s = PostgresAgentStateStore::builder(c)
            .create_if_not_exist(false)
            .build()
            .unwrap();
        assert_eq!(s.table_name(), DEFAULT_TABLE_NAME);
        assert!(SCHEMA_EXISTS_QUERY.contains("INFORMATION_SCHEMA.SCHEMATA"));
        assert!(TABLE_EXISTS_QUERY.contains("INFORMATION_SCHEMA.TABLES"));
    }

    // Java: PostgresAgentStateStoreTest#builderWithDefaultsCreatesStore
    // (`private boolean createIfNotExist = true;` in Java's Builder): a bare
    // `builder(client).build()` must provision the schema + table instead of failing with
    // `PostgresError::Schema` when the database is empty.
    #[test]
    fn builder_defaults_to_create_if_not_exist() {
        let c = client();
        assert!(!c.has_schema(DEFAULT_SCHEMA_NAME));
        let s = PostgresAgentStateStore::builder(c.clone()).build().unwrap();
        assert_eq!(s.schema_name(), DEFAULT_SCHEMA_NAME);
        assert_eq!(s.table_name(), DEFAULT_TABLE_NAME);
        assert!(c.has_schema(DEFAULT_SCHEMA_NAME));
        assert!(c.has_table(DEFAULT_TABLE_NAME));
        assert!(c
            .ddl_statements()
            .iter()
            .any(|s| s.contains("CREATE TABLE IF NOT EXISTS")));
    }

    // Java: PostgresAgentStateStoreTest#builderWithDefaultsCreatesStore (custom schema,
    // default auto-create): only the names are overridden, so the schema must still be
    // provisioned without an explicit `create_if_not_exist(true)`.
    #[test]
    fn builder_default_auto_create_applies_to_custom_schema() {
        let c = client();
        let s = PostgresAgentStateStore::builder(c.clone())
            .schema("custom_schema")
            .unwrap()
            .table("custom_sessions")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(s.schema_name(), "custom_schema");
        assert!(c.has_schema("custom_schema"));
        assert!(c.has_table("custom_sessions"));
    }

    // ---- single-value operations -------------------------------------------

    // Java: PostgresAgentStateStoreTest#saveSingleState, #getSingleState
    #[tokio::test]
    async fn save_and_get_single_state_roundtrip() {
        let c = client();
        let s = store(&c);
        let v = json!({"count": 7, "name": "x"});
        assert_eq!(s.save_versioned(None, "s1", "k", &v).unwrap(), 1);
        assert_eq!(s.get(None, "s1", "k").await, Some(v.clone()));
        assert_eq!(s.save_versioned(None, "s1", "k", &v).unwrap(), 2);
    }

    // Java: PostgresAgentStateStoreTest#getSingleStateReturnsEmptyWhenMissing
    #[tokio::test]
    async fn get_returns_none_when_missing() {
        let c = client();
        let s = store(&c);
        assert!(s.get(None, "s1", "k").await.is_none());
        assert!(s.get(Some("u1"), "s1", "k").await.is_none());
    }

    // Regression for #3231 (Java PostgresAgentStateStoreTest#acceptsSandboxShapedSessionIds):
    // sandbox-shaped slot ids containing path separators persist and reload.
    #[tokio::test]
    async fn save_and_get_roundtrip_for_sandbox_shaped_session_id() {
        let c = client();
        let s = store(&c);
        let flat = "sandbox/session/01M31AQP7A24X0Y4A12RKADD5P";
        let nested = "sandbox/user/agent-7/01M31AQP7A24X0Y4A12RKADD5P";
        let v = json!({"resumed": true});

        assert_eq!(s.save_versioned(None, flat, "k", &v).unwrap(), 1);
        assert_eq!(s.get(None, flat, "k").await, Some(v.clone()));
        assert!(s.exists(None, flat).await);

        s.save(None, nested, "k", json!(2)).await;
        assert_eq!(s.get(None, nested, "k").await, Some(json!(2)));
        assert_eq!(
            s.list_session_ids(None).await,
            vec![flat.to_string(), nested.to_string()]
        );

        s.delete(None, flat).await;
        assert!(s.get(None, flat, "k").await.is_none());
        assert_eq!(s.get(None, nested, "k").await, Some(json!(2)));
    }

    // Java: PostgresAgentStateStoreTest#existsTrue, #existsFalse
    #[tokio::test]
    async fn exists_tracks_session_presence() {
        let c = client();
        let s = store(&c);
        assert!(!s.exists(None, "s1").await);
        s.save(None, "s1", "k", json!(1)).await;
        assert!(s.exists(None, "s1").await);
    }

    // Java: PostgresAgentStateStoreTest#deleteSession
    #[tokio::test]
    async fn delete_removes_all_rows_for_session() {
        let c = client();
        let s = store(&c);
        s.save(None, "s1", "k1", json!(1)).await;
        s.save(None, "s1", "k2", json!(2)).await;
        s.save(None, "s2", "k1", json!(3)).await;
        assert_eq!(c.session_row_count(), 3);
        s.delete(None, "s1").await;
        assert!(!s.exists(None, "s1").await);
        assert!(s.exists(None, "s2").await);
        assert_eq!(c.session_row_count(), 1);
    }

    // Java: PostgresAgentStateStoreTest#listSessionIds
    #[tokio::test]
    async fn list_session_ids_scopes_by_user() {
        let c = client();
        let s = store(&c);
        s.save(Some("u1"), "s1", "k", json!(1)).await;
        s.save(Some("u1"), "s2", "k", json!(1)).await;
        s.save(Some("u2"), "s9", "k", json!(1)).await;
        assert_eq!(s.list_session_ids(Some("u1")).await, vec!["s1", "s2"]);
        assert_eq!(s.list_session_ids(Some("u2")).await, vec!["s9"]);
    }

    // Java: PostgresAgentStateStoreTest#listSessionIdsForAnonymousUser, #listSessionIdsForBlankUser
    #[tokio::test]
    async fn list_session_ids_treats_none_and_blank_as_anonymous() {
        let c = client();
        let s = store(&c);
        s.save(None, "anon1", "k", json!(1)).await;
        s.save(Some("   "), "anon2", "k", json!(1)).await;
        assert_eq!(s.list_session_ids(None).await, vec!["anon1", "anon2"]);
        assert_eq!(s.list_session_ids(Some("  ")).await, vec!["anon1", "anon2"]);
        assert!(s.list_session_ids(Some("u1")).await.is_empty());
    }

    // ---- identifier validation ---------------------------------------------

    // Java: PostgresAgentStateStoreTest#acceptsSandboxShapedSessionIds, #rejectsTooLongSessionId
    #[test]
    fn slot_id_accepts_path_separators_and_rejects_overlong_ids() {
        // Regression for #3231: slash-separated sandbox slot ids are opaque bind values.
        assert_eq!(
            PostgresAgentStateStore::slot_id(None, "sandbox/session/01M31AQP7A24X0Y4A12RKADD5P")
                .unwrap(),
            format!("{ANON_USER}:sandbox/session/01M31AQP7A24X0Y4A12RKADD5P")
        );
        assert_eq!(
            PostgresAgentStateStore::slot_id(None, "a\\b").unwrap(),
            format!("{ANON_USER}:a\\b")
        );
        let long = "s".repeat(MAX_SESSION_ID_LENGTH + 1);
        assert!(PostgresAgentStateStore::slot_id(None, &long).is_err());
        assert_eq!(slot("ok"), format!("{ANON_USER}:ok"));
        assert_eq!(
            PostgresAgentStateStore::slot_id(Some(""), "ok").unwrap(),
            format!("{ANON_USER}:ok")
        );
        assert_eq!(
            PostgresAgentStateStore::slot_id(Some("u1"), "ok").unwrap(),
            "u1:ok"
        );
    }

    // Java: PostgresAgentStateStoreTest#rejectsNullSessionId, #rejectsBlankSessionId,
    //       #validateSessionIdRejectsNullOrEmpty
    #[test]
    fn slot_id_rejects_blank_session_id() {
        assert!(PostgresAgentStateStore::slot_id(None, "").is_err());
        assert!(PostgresAgentStateStore::slot_id(None, "   ").is_err());
        assert!(PostgresAgentStateStore::slot_id(Some("u1"), "").is_err());
    }

    // Java: PostgresAgentStateStoreTest#rejectsNullStateKey, #rejectsEmptyStateKey, #rejectsTooLongStateKey
    #[test]
    fn state_key_validation_boundaries() {
        assert!(PostgresAgentStateStore::validate_state_key("").is_err());
        assert!(PostgresAgentStateStore::validate_state_key("   ").is_err());
        assert!(
            PostgresAgentStateStore::validate_state_key(&"k".repeat(MAX_SESSION_ID_LENGTH)).is_ok()
        );
        assert!(PostgresAgentStateStore::validate_state_key(
            &"k".repeat(MAX_SESSION_ID_LENGTH + 1)
        )
        .is_err());
    }

    // Java: PostgresAgentStateStoreTest#saveSingleStateThrowsWhenSqlFails (Rust: the
    // `AgentStateStore` trait returns `()`, so invalid identifiers are swallowed).
    #[tokio::test]
    async fn invalid_identifiers_are_ignored_by_trait_methods() {
        let c = client();
        let s = store(&c);
        s.save(None, "   ", "k", json!(1)).await;
        assert_eq!(c.session_row_count(), 0);
        assert!(s.get(None, "   ", "k").await.is_none());
        assert!(s.get(None, "s1", "  ").await.is_none());
        assert!(s.get_list(None, "s1", "").await.is_empty());
    }

    // ---- versioned CAS ------------------------------------------------------

    // Java: PostgresAgentStateStoreTest#saveIfVersionUnconditionalAbsentKeyReturnsZero
    #[tokio::test]
    async fn version_of_is_zero_for_absent_key() {
        let c = client();
        let s = store(&c);
        assert_eq!(s.version_of(None, "s", "k").unwrap(), 0);
    }

    // Java: PostgresAgentStateStoreTest#saveIfVersion* (version starts at 1; 0 = create-only;
    // stale version fails; UNVERSIONED = unconditional overwrite)
    #[tokio::test]
    async fn save_if_version_cas_three_branches() {
        let c = client();
        let s = store(&c);
        let v1 = json!({"n": 1});
        let v2 = json!({"n": 2});

        // create-only insert (expected 0) on an absent key succeeds with version 1.
        assert_eq!(s.save_if_version(None, "s", "k", &v1, 0).unwrap(), 1);
        assert_eq!(s.version_of(None, "s", "k").unwrap(), 1);

        // create-only insert against an existing key: duplicate -> CAS failure.
        assert_eq!(
            s.save_if_version(None, "s", "k", &v2, 0).unwrap(),
            UNVERSIONED
        );
        assert_eq!(s.get(None, "s", "k").await, Some(v1.clone()));

        // matching version succeeds and bumps to version 2.
        assert_eq!(s.save_if_version(None, "s", "k", &v2, 1).unwrap(), 2);
        assert_eq!(s.version_of(None, "s", "k").unwrap(), 2);

        // stale versions fail without mutating the row.
        assert_eq!(
            s.save_if_version(None, "s", "k", &v1, 1).unwrap(),
            UNVERSIONED
        );
        assert_eq!(
            s.save_if_version(None, "s", "k", &v1, 99).unwrap(),
            UNVERSIONED
        );
        assert_eq!(s.get(None, "s", "k").await, Some(v2.clone()));

        // UNVERSIONED behaves as an unconditional save.
        assert_eq!(
            s.save_if_version(None, "s", "k", &v1, UNVERSIONED).unwrap(),
            3
        );
        assert_eq!(s.get(None, "s", "k").await, Some(v1));
    }

    // Java: PostgresAgentStateStoreTest#saveIfVersionZeroFallsBackToCasUpdateForBackfilledRow
    //
    // A row back-filled by an older `ALTER TABLE ... DEFAULT 0` migration sits at version 0.
    // `save_if_version(..., 0)` must not report a phantom conflict; it bumps 0 -> 1.
    #[tokio::test]
    async fn save_if_version_zero_falls_back_to_cas_for_backfilled_row() {
        let c = client();
        let s = store(&c);
        c.seed_session_row(&slot("s"), "k", SINGLE_STATE_INDEX, r#"{"n":0}"#, 0);

        let written = json!({"n": 1});
        assert_eq!(s.save_if_version(None, "s", "k", &written, 0).unwrap(), 1);
        assert_eq!(s.version_of(None, "s", "k").unwrap(), 1);
        assert_eq!(s.get(None, "s", "k").await, Some(written));

        // Once past 0, another create-only write is a genuine conflict.
        assert_eq!(
            s.save_if_version(None, "s", "k", &json!({"n": 2}), 0)
                .unwrap(),
            UNVERSIONED
        );
    }

    // ---- list hashing (`:_hash` companion row, core `list_hash`) -----------

    // Java: PostgresAgentStateStoreTest#getList
    #[tokio::test]
    async fn get_list_returns_items_in_index_order() {
        let c = client();
        let s = store(&c);
        s.save_list(None, "s1", "items", &[json!("a"), json!("b"), json!("c")])
            .await;
        assert_eq!(
            s.get_list(None, "s1", "items").await,
            vec![json!("a"), json!("b"), json!("c")]
        );
        assert!(s.get_list(None, "s1", "missing").await.is_empty());
    }

    // Java: PostgresAgentStateStoreTest#saveListAppendsNewItems (also covers the
    // `state_key + ":_hash"` companion row).
    #[tokio::test]
    async fn save_list_writes_hash_companion_row() {
        let c = client();
        let s = store(&c);
        let values = vec![json!({"i": 1}), json!({"i": 2})];
        s.save_list(None, "s1", "items", &values).await;
        assert_eq!(s.list_count_sync(&slot("s1"), "items").unwrap(), 2);
        let stored = s
            .get_sync(
                &slot("s1"),
                &format!("items{HASH_KEY_SUFFIX}"),
                SINGLE_STATE_INDEX,
            )
            .unwrap();
        let expected = compute_hash(&values);
        assert_eq!(stored.as_deref(), Some(expected.as_str()));
        // 2 items + 1 hash companion row.
        assert_eq!(c.session_row_count(), 3);
    }

    // Java: PostgresAgentStateStoreTest#saveListAppendsNewItems
    #[tokio::test]
    async fn save_list_appends_without_rewriting_when_prefix_unchanged() {
        let c = client();
        let s = store(&c);
        let (a, b, d) = (json!("a"), json!("b"), json!("c"));
        s.save_list(None, "s1", "items", &[a.clone(), b.clone()])
            .await;
        assert_eq!(c.session_row_count(), 3);
        s.save_list(None, "s1", "items", &[a.clone(), b.clone(), d.clone()])
            .await;
        // One extra item row only: the previously-stored prefix was reused.
        assert_eq!(c.session_row_count(), 4);
        assert_eq!(s.get_list(None, "s1", "items").await, vec![a, b, d]);
    }

    // Java: PostgresAgentStateStoreTest#saveListSkipsWhenHashMatchesAndSizeUnchanged
    #[tokio::test]
    async fn save_list_skips_when_hash_matches_and_size_unchanged() {
        let c = client();
        let s = store(&c);
        let values = vec![json!(1), json!(2)];
        s.save_list(None, "s1", "items", &values).await;
        s.save_list(None, "s1", "items", &values).await;
        assert_eq!(c.session_row_count(), 3);
        assert_eq!(s.get_list(None, "s1", "items").await, values);
    }

    // Java: PostgresAgentStateStoreTest#saveListFullRewriteWhenHashChanged
    #[tokio::test]
    async fn save_list_rewrites_when_prefix_modified() {
        let c = client();
        let s = store(&c);
        s.save_list(None, "s1", "items", &[json!("a"), json!("b")])
            .await;
        s.save_list(None, "s1", "items", &[json!("x"), json!("b")])
            .await;
        assert_eq!(
            s.get_list(None, "s1", "items").await,
            vec![json!("x"), json!("b")]
        );
        // Full rewrite leaves exactly 2 items + 1 hash row.
        assert_eq!(c.session_row_count(), 3);
    }

    // Java: PostgresAgentStateStoreTest#saveListFullRewriteWhenHashChanged (shrink branch)
    #[tokio::test]
    async fn save_list_rewrites_when_list_shrinks() {
        let c = client();
        let s = store(&c);
        s.save_list(None, "s1", "items", &[json!(1), json!(2), json!(3)])
            .await;
        assert_eq!(c.session_row_count(), 4);
        s.save_list(None, "s1", "items", &[json!(1)]).await;
        assert_eq!(s.get_list(None, "s1", "items").await, vec![json!(1)]);
        assert_eq!(c.session_row_count(), 2);
    }

    // Java: PostgresAgentStateStoreTest#saveListHashNotFound
    #[tokio::test]
    async fn save_list_rewrites_when_hash_missing_but_data_present() {
        let c = client();
        let s = store(&c);
        s.save_list(None, "s1", "items", &[json!(1), json!(2)])
            .await;
        // Simulate legacy rows written before the ":_hash" companion row existed.
        c.query(
            &format!(
                "DELETE FROM {} WHERE session_id = $1 AND state_key = $2",
                s.full_table
            ),
            &[
                SqlValue::text(slot("s1")),
                SqlValue::text(format!("items{HASH_KEY_SUFFIX}")),
            ],
        )
        .unwrap();
        assert_eq!(c.session_row_count(), 2);
        s.save_list(None, "s1", "items", &[json!(1), json!(2)])
            .await;
        assert_eq!(c.session_row_count(), 3);
        assert_eq!(
            s.get_list(None, "s1", "items").await,
            vec![json!(1), json!(2)]
        );
    }

    // Java: PostgresAgentStateStoreTest#saveListEmptyDoesNothing
    #[tokio::test]
    async fn save_list_empty_is_noop() {
        let c = client();
        let s = store(&c);
        s.save_list(None, "s1", "items", &[]).await;
        assert_eq!(c.session_row_count(), 0);
    }

    // Java: PostgresAgentStateStore#executeInWriteTransaction (see also
    // `PostgresAgentStateStoreTest#executeInWriteTransactionRollsBackOnException`)
    //
    // `saveList`'s full-rewrite path is delete + N inserts + hash row. Java wraps it in
    // `executeInWriteTransaction`; without a transaction a mid-way failure leaves the list
    // truncated (or half rewritten) *and* the ":_hash" row stale, which the incremental
    // append logic can never repair.
    #[tokio::test]
    async fn save_list_rolls_back_when_a_statement_fails_midway() {
        let c = client();
        let s = store(&c);
        s.save_list(None, "s1", "items", &[json!(1), json!(2)])
            .await;
        assert_eq!(c.session_row_count(), 3);

        // Full rewrite sequence: #1 get_sync, #2 list_count, #3 DELETE, #4/#5/#6 the three
        // item INSERTs — fail the *third* insert (= the 6th query) so the DELETE and two
        // inserts are already applied when the transaction aborts.
        let before = c.query_count();
        c.fail_query_after(5);
        let err = s
            .save_list_sync(&slot("s1"), "items", &[json!(9), json!(8), json!(7)])
            .unwrap_err();
        assert!(matches!(err, PostgresError::Query(_)), "got {err:?}");
        assert_eq!(
            c.query_count() - before,
            6,
            "the failure must land on the third item insert, not earlier"
        );

        // Nothing was applied: same rows, same content, same hash row.
        assert_eq!(c.session_row_count(), 3);
        assert_eq!(
            s.get_list(None, "s1", "items").await,
            vec![json!(1), json!(2)]
        );
        assert_eq!(
            s.get_sync(
                &slot("s1"),
                &format!("items{HASH_KEY_SUFFIX}"),
                SINGLE_STATE_INDEX
            )
            .unwrap()
            .as_deref(),
            Some(compute_hash(&[json!(1), json!(2)]).as_str())
        );

        // The store is still usable afterwards (no poisoned session).
        s.save_list(None, "s1", "items", &[json!(1), json!(2), json!(3)])
            .await;
        assert_eq!(
            s.get_list(None, "s1", "items").await,
            vec![json!(1), json!(2), json!(3)]
        );
    }

    // Java: PostgresAgentStateStore#executeInWriteTransaction — the append-only path
    // (N inserts + hash row) is atomic too: a failure must not bump the hash row past the
    // rows that were actually written.
    #[tokio::test]
    async fn save_list_append_rolls_back_when_the_hash_write_fails() {
        let c = client();
        let s = store(&c);
        s.save_list(None, "s1", "items", &[json!(1)]).await;
        assert_eq!(c.session_row_count(), 2);

        // Append sequence: #1 get_sync, #2 list_count, #3 INSERT new item, #4 hash upsert
        // (fails) — so the new item row has already been written when the abort happens.
        let before = c.query_count();
        c.fail_query_after(3);
        assert!(s
            .save_list_sync(&slot("s1"), "items", &[json!(1), json!(2)])
            .is_err());
        assert_eq!(
            c.query_count() - before,
            4,
            "the failure must land on the hash upsert"
        );

        assert_eq!(c.session_row_count(), 2);
        assert_eq!(s.get_list(None, "s1", "items").await, vec![json!(1)]);
        assert_eq!(
            s.get_sync(
                &slot("s1"),
                &format!("items{HASH_KEY_SUFFIX}"),
                SINGLE_STATE_INDEX
            )
            .unwrap()
            .as_deref(),
            Some(compute_hash(&[json!(1)]).as_str())
        );
    }

    // Explicit failure rather than a silent non-atomic write: a client that cannot run a
    // write transaction makes the list save fail with `DriverUnavailable` (Java always has
    // a `Connection`, so this branch is Rust-specific).
    #[tokio::test]
    async fn save_list_requires_transaction_support() {
        /// Wraps a working client but does **not** implement `execute_in_transaction`.
        #[derive(Debug)]
        struct NoTransactionClient(Arc<MemoryClient>);

        impl PostgresClient for NoTransactionClient {
            fn query(&self, sql: &str, params: &[SqlValue]) -> PgResult<QueryOutcome> {
                self.0.query(sql, params)
            }
            fn execute_batch(&self, sql: &str) -> PgResult<()> {
                self.0.execute_batch(sql)
            }
        }

        let inner = client();
        let s = PostgresAgentStateStore::builder(Arc::new(NoTransactionClient(inner.clone())))
            .create_if_not_exist(true)
            .build()
            .unwrap();
        let err = s
            .save_list_sync(&slot("s1"), "items", &[json!(1)])
            .unwrap_err();
        assert!(
            matches!(err, PostgresError::DriverUnavailable(_)),
            "got {err:?}"
        );
        // Nothing was written.
        assert_eq!(inner.session_row_count(), 0);
    }

    // ---- truncate -----------------------------------------------------------

    // Java: PostgresAgentStateStoreTest#truncateAllSessionsTruncatesTable
    #[tokio::test]
    async fn truncate_all_sessions_clears_table() {
        let c = client();
        let s = store(&c);
        s.save(None, "s1", "k", json!(1)).await;
        s.save(None, "s2", "k", json!(2)).await;
        assert!(s.exists(None, "s1").await);
        s.truncate_all_sessions().unwrap();
        assert!(!s.exists(None, "s1").await);
        assert!(!s.exists(None, "s2").await);
        assert_eq!(c.session_row_count(), 0);
    }

    /// A client that fails every non-DDL statement, used to check error propagation the way
    /// the Java tests use a throwing mock.
    #[derive(Debug)]
    struct FailingClient;

    impl PostgresClient for FailingClient {
        fn query(&self, _sql: &str, _params: &[SqlValue]) -> PgResult<QueryOutcome> {
            Err(PostgresError::Query("boom".into()))
        }
        fn execute_batch(&self, _sql: &str) -> PgResult<()> {
            Err(PostgresError::Query("boom".into()))
        }
    }

    /// A client whose batch (DDL) calls succeed but whose statements all fail.
    #[derive(Debug, Default)]
    struct BatchOkQueryFailClient;

    impl PostgresClient for BatchOkQueryFailClient {
        fn query(&self, _sql: &str, _params: &[SqlValue]) -> PgResult<QueryOutcome> {
            Err(PostgresError::Query("boom".into()))
        }
        fn execute_batch(&self, _sql: &str) -> PgResult<()> {
            Ok(())
        }
    }

    /// A client that succeeds all provisioning DDL but whose `TRUNCATE` batch fails.
    #[derive(Debug, Default)]
    struct TruncateFailClient;

    impl PostgresClient for TruncateFailClient {
        fn query(&self, _sql: &str, _params: &[SqlValue]) -> PgResult<QueryOutcome> {
            Err(PostgresError::Query("boom".into()))
        }
        fn execute_batch(&self, sql: &str) -> PgResult<()> {
            if sql.to_uppercase().contains("TRUNCATE") {
                Err(PostgresError::Query("boom".into()))
            } else {
                Ok(())
            }
        }
    }

    // Java: PostgresAgentStateStoreTest#truncateAllSessionsThrowsWhenSqlFails
    #[test]
    fn truncate_propagates_sql_errors() {
        let s = PostgresAgentStateStore::builder(Arc::new(TruncateFailClient))
            .create_if_not_exist(true)
            .build()
            .unwrap();
        assert!(matches!(
            s.truncate_all_sessions().unwrap_err(),
            PostgresError::Query(_)
        ));
    }

    // Java: PostgresAgentStateStoreTest#createSchemaFailureThrows, #verifySchemaSqlExceptionThrows
    #[test]
    fn build_propagates_batch_failures() {
        let err = PostgresAgentStateStore::builder(Arc::new(FailingClient))
            .create_if_not_exist(true)
            .build()
            .unwrap_err();
        assert!(matches!(err, PostgresError::Query(_)));
    }

    // Java: PostgresAgentStateStoreTest#saveSingleStateThrowsWhenSqlFails,
    //       #saveListThrowsWhenSqlFails, #getThrowsWhenQueryFails
    #[tokio::test]
    async fn query_failures_surface_through_inherent_methods() {
        let s = PostgresAgentStateStore::builder(Arc::new(BatchOkQueryFailClient))
            .create_if_not_exist(true)
            .build()
            .unwrap();
        assert!(s.save_versioned(None, "s1", "k", &json!(1)).is_err());
        assert!(s.get_sync(&slot("s1"), "k", SINGLE_STATE_INDEX).is_err());
        assert!(s.get_list_rows_sync(&slot("s1"), "k").is_err());
        assert!(s.version_of(None, "s1", "k").is_err());
        assert!(s.save_if_version(None, "s1", "k", &json!(1), 0).is_err());
    }

    // Java: PostgresAgentStateStoreTest#getThrowsWhenJsonInvalid
    #[tokio::test]
    async fn get_returns_none_for_invalid_json_payload() {
        let c = client();
        let s = store(&c);
        c.query(
            &format!(
                "INSERT INTO {} (session_id, state_key, item_index, state_data, version, updated_at) \
                 VALUES ($1, $2, $3, $4, 1, $5)",
                s.full_table
            ),
            &[
                SqlValue::text(slot("s1")),
                SqlValue::text("k"),
                SqlValue::int(SINGLE_STATE_INDEX),
                SqlValue::text("not json"),
                SqlValue::BigInt(0),
            ],
        )
        .unwrap();
        assert!(s.get(None, "s1", "k").await.is_none());
    }

    /// Wraps [`MemoryClient`] but fails if a query runs on the thread polling the async test.
    ///
    /// The SQL seam ([`PostgresClient`]) is synchronous and the real driver
    /// (`PgSimpleClient`) calls `block_on` internally, which panics inside a runtime. Every
    /// `AgentStateStore` method is therefore contractually required to go through
    /// [`PostgresAgentStateStore::run_blocking`]; this client enforces that contract by
    /// rejecting the runtime's own polling thread (blocking-pool threads are fine).
    #[derive(Debug)]
    struct OffRuntimeClient {
        runtime_thread: std::thread::ThreadId,
    }

    impl OffRuntimeClient {
        fn assert_off_runtime(&self, what: &str) {
            assert!(
                std::thread::current().id() != self.runtime_thread,
                "synchronous {what} ran on the Tokio runtime thread"
            );
        }
    }

    impl PostgresClient for OffRuntimeClient {
        fn query(&self, sql: &str, _params: &[SqlValue]) -> PgResult<QueryOutcome> {
            self.assert_off_runtime(&format!("SQL ({sql})"));
            if sql.to_lowercase().contains("information_schema") {
                return Ok(QueryOutcome::Rows(vec![Vec::new()]));
            }
            Ok(QueryOutcome::Rows(Vec::new()))
        }

        fn execute_batch(&self, _sql: &str) -> PgResult<()> {
            self.assert_off_runtime("DDL");
            Ok(())
        }
    }

    #[tokio::test]
    async fn async_trait_methods_never_touch_the_client_on_the_runtime_thread() {
        let client: Arc<dyn PostgresClient> = Arc::new(OffRuntimeClient {
            runtime_thread: std::thread::current().id(),
        });
        // `build()` runs the INFORMATION_SCHEMA checks synchronously, so it must be called
        // off-runtime too — the same rule the async methods implement via `run_blocking`.
        let s = tokio::task::spawn_blocking(move || {
            PostgresAgentStateStore::builder(client)
                .create_if_not_exist(false)
                .build()
                .unwrap()
        })
        .await
        .unwrap();

        assert_eq!(s.get(None, "s1", "k").await, None);
        assert!(s.get_list(None, "s1", "k").await.is_empty());
        assert!(!s.exists(None, "s1").await);
        assert!(s.list_session_ids(None).await.is_empty());
        let _ = s.delete(None, "s1").await;
    }

    // ---- bound-literal types -------------------------------------------------------

    /// Wraps [`MemoryClient`] and keeps every `(sql, params)` pair it was handed.
    ///
    /// The simulator rewrites `created_at`/`updated_at` from its own clock, so it accepts
    /// **any** value in those slots and cannot notice a wrong bind type; recording the pair
    /// lets a test assert the literal type the real driver would receive.
    #[derive(Debug)]
    struct RecordingClient {
        inner: MemoryClient,
        statements: std::sync::Mutex<Vec<(String, Vec<SqlValue>)>>,
    }

    impl RecordingClient {
        fn new() -> Self {
            Self {
                inner: MemoryClient::new(),
                statements: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn captured(&self) -> Vec<(String, Vec<SqlValue>)> {
            self.statements.lock().unwrap().clone()
        }
    }

    impl PostgresClient for RecordingClient {
        fn query(&self, sql: &str, params: &[SqlValue]) -> PgResult<QueryOutcome> {
            self.statements
                .lock()
                .unwrap()
                .push((sql.to_string(), params.to_vec()));
            self.inner.query(sql, params)
        }

        fn execute_batch(&self, sql: &str) -> PgResult<()> {
            self.inner.execute_batch(sql)
        }
    }

    /// Regression for the `updated_at` **bind type** (offline, no server needed).
    ///
    /// `sessions_ddl` declares `updated_at TIMESTAMP`, but the save paths used to bind the
    /// millisecond value as `SqlValue::BigInt`. `tokio-postgres` rejects that at the Bind
    /// stage (`cannot convert between i64 and timestamp`) and `save` only logs the failure, so
    /// the table silently stayed empty; the live suite caught it as a `None`/`Some` round-trip
    /// mismatch. `MemoryClient` accepts any value in the timestamp slot, so this asserts the
    /// literal type on all three save paths instead of relying on the live database.
    #[test]
    fn every_save_path_binds_updated_at_as_a_timestamp() {
        let client = Arc::new(RecordingClient::new());
        let store = PostgresAgentStateStore::create_if_not_exist(
            client.clone() as Arc<dyn PostgresClient>
        )
        .unwrap();

        // 1. Unconditional upsert (`save` / `save_versioned`).
        store.save_versioned(Some("u"), "s1", "k", &json!(1)).unwrap();
        // 2. Create-if-absent (`save_if_version(..., 0)`): INSERT ... DO NOTHING.
        store.save_if_version(Some("u"), "s1", "k2", &json!(2), 0).unwrap();
        // 3. CAS update of an existing row: UPDATE ... updated_at = $3.
        store.save_if_version(Some("u"), "s1", "k", &json!(3), 1).unwrap();

        let captured = client.captured();
        let mut checked: Vec<String> = Vec::new();
        for (sql, params) in &captured {
            let upper = sql.to_ascii_uppercase();
            let slot = if upper.starts_with("INSERT INTO") && upper.contains("UPDATED_AT") {
                // INSERT ... VALUES ($1, $2, $3, $4, 1, $5)
                4
            } else if upper.starts_with("UPDATE") && upper.contains("UPDATED_AT") {
                // UPDATE ... SET state_data = $1, version = $2, updated_at = $3
                2
            } else {
                continue;
            };
            match params.get(slot) {
                Some(SqlValue::Timestamp(_)) => {
                    checked.push(upper.split_whitespace().next().unwrap().to_string())
                }
                Some(other) => panic!(
                    "updated_at must bind SqlValue::Timestamp, got {other:?} at ${} of: {sql}",
                    slot + 1
                ),
                None => panic!("missing updated_at parameter ${} of: {sql}", slot + 1),
            }
        }
        assert_eq!(
            checked,
            vec!["INSERT".to_string(), "INSERT".to_string(), "UPDATE".to_string()],
            "all three save paths must bind updated_at: {captured:#?}"
        );
    }
}
