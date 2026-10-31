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

/// `CREATE DATABASE` statement, mirroring the DDL Java's `createDatabaseIfNotExist` issues.
///
/// `IF NOT EXISTS` keeps it idempotent, so `create_if_not_exist` can be requested on an
/// already-provisioned server.
pub fn create_database_sql(database_name: &str) -> String {
    format!(
        "CREATE DATABASE IF NOT EXISTS `{database_name}` \
         DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"
    )
}

/// `CREATE TABLE` statement, mirroring Java's `createTableIfNotExist`.
///
/// `session_id` / `state_key` are case-sensitive identifiers, so they pin `utf8mb4_bin`:
/// the table default collation is case-insensitive, which would silently merge session ids
/// or state keys differing only in letter case. `version` is declared here with `DEFAULT 0`
/// exactly like Java; every write path sets the version explicitly (`1` on insert), and the
/// migration path ([`add_version_column_sql`]) backfills pre-existing rows with `1`, because
/// `0` is the "row absent" sentinel `read_version` reports.
pub fn create_sessions_table_sql(database_name: &str, table_name: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS `{database_name}`.`{table_name}` (\
         session_id VARCHAR(255) COLLATE utf8mb4_bin NOT NULL, \
         state_key VARCHAR(255) COLLATE utf8mb4_bin NOT NULL, \
         item_index INT NOT NULL DEFAULT 0, \
         state_data LONGTEXT NOT NULL, \
         version BIGINT NOT NULL DEFAULT 0, \
         created_at DATETIME DEFAULT CURRENT_TIMESTAMP, \
         updated_at DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, \
         PRIMARY KEY (session_id, state_key, item_index)\
         ) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"
    )
}

/// `ALTER TABLE ... ADD COLUMN version` migration, mirroring Java's `ensureVersionColumn`.
///
/// `DEFAULT 1` (not `0`): the ALTER backfills every pre-existing row with the default, and
/// `0` is the sentinel [`StateBackend::read_version`] reports for "row absent". Backfilling
/// `0` would make those rows look absent to `save_if_version(.., Some(0))`, which takes the
/// INSERT branch and hits a duplicate key — a phantom CAS conflict. (The `version = 0`
/// fallback in [`MysqlAgentStateStore::save_if_version`] heals such rows, but not
/// backfilling `0` in the first place is still the right migration.)
pub fn add_version_column_sql(database_name: &str, table_name: &str) -> String {
    format!(
        "ALTER TABLE `{database_name}`.`{table_name}` \
         ADD COLUMN version BIGINT NOT NULL DEFAULT 1"
    )
}

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
    /// Executes a DDL statement (schema bootstrap or the `version` column migration).
    ///
    /// Mirrors the `Connection.prepareStatement(ddl).execute()` calls Java issues in
    /// `createDatabaseIfNotExist`, `createTableIfNotExist` and `ensureVersionColumn`. The
    /// statement text is produced by [`create_database_sql`],
    /// [`create_sessions_table_sql`] and [`add_version_column_sql`], so the crate — not the
    /// adapter — owns the schema shape.
    fn execute_ddl(&self, sql: &str) -> MysqlResult<()>;

    /// Whether `database_name` exists. Mirrors Java `verifyDatabaseExists`, which probes
    /// `INFORMATION_SCHEMA.SCHEMATA`.
    fn database_exists(&self, database_name: &str) -> MysqlResult<bool>;

    /// Whether `table_name` exists inside `database_name`. Mirrors Java
    /// `verifyTableExists`, which probes `INFORMATION_SCHEMA.TABLES`.
    fn table_exists(&self, database_name: &str, table_name: &str) -> MysqlResult<bool>;

    /// Column names of `table_name`, in schema order. Mirrors the
    /// `INFORMATION_SCHEMA.COLUMNS` count probe in Java `ensureVersionColumn`.
    ///
    /// An unknown table yields an empty vector (the caller has already verified existence).
    fn table_columns(&self, database_name: &str, table_name: &str) -> MysqlResult<Vec<String>>;

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
    ///
    /// Java's no-arg constructor is the opposite (verify-only, `createIfNotExist = false`);
    /// this convenience keeps the historical Rust default of auto-creation but no longer
    /// swallows a failing bootstrap — schema errors are returned to the caller.
    pub fn new(backend: Arc<dyn StateBackend>) -> MysqlResult<Self> {
        Self::with_config(backend, None, None, true)
    }

    /// Creates a store with explicit database/table names and auto-creation semantics.
    ///
    /// Mirrors Java `MysqlAgentStateStore(DataSource, databaseName, tableName,
    /// createIfNotExist)`:
    ///
    /// * `create_if_not_exist == true` — creates the database and table (both
    ///   `IF NOT EXISTS`, so it is idempotent) before returning.
    /// * `create_if_not_exist == false` — verifies the database exists, then the table;
    ///   a missing one is reported as [`MysqlError::Schema`] carrying the Java message
    ///   ("Database does not exist: …" / "Table does not exist: …").
    ///
    /// Both branches then run the `version` column migration
    /// ([`ensure_version_column`]), which is what upgrades databases created by the
    /// pre-versioning schema.
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
        store.bootstrap_schema()?;
        Ok(store)
    }

    /// Creates the schema, or verifies it, then runs the `version` column migration.
    ///
    /// Ordering follows Java's constructor: `createDatabaseIfNotExist` →
    /// `createTableIfNotExist` **or** `verifyDatabaseExists` → `verifyTableExists`, and in
    /// both cases `ensureVersionColumn` last.
    fn bootstrap_schema(&self) -> MysqlResult<()> {
        let database_name = &self.database_name;
        let table_name = &self.table_name;
        if self.create_if_not_exist {
            self.backend.execute_ddl(&create_database_sql(database_name))?;
            self.backend
                .execute_ddl(&create_sessions_table_sql(database_name, table_name))?;
        } else {
            if !self.backend.database_exists(database_name)? {
                return Err(MysqlError::Schema(format!(
                    "Database does not exist: {database_name}. \
                     Use MysqlAgentStateStore::with_config(.., true) to auto-create."
                )));
            }
            if !self.backend.table_exists(database_name, table_name)? {
                return Err(MysqlError::Schema(format!(
                    "Table does not exist: {database_name}.{table_name}. \
                     Use MysqlAgentStateStore::with_config(.., true) to auto-create."
                )));
            }
        }
        ensure_version_column(self.backend.as_ref(), database_name, table_name)
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
    /// * `Some(0)` — create-if-absent: an atomic `INSERT`; a duplicate key falls back to
    ///   `UPDATE ... WHERE version = 0`, healing a row that already exists with
    ///   `version = 0` (the pre-versioning DDL default). A row that is already versioned
    ///   (`>= 1`) is still a conflict.
    /// * `Some(n)` — an atomic `UPDATE ... WHERE version = n`; a version mismatch (or a
    ///   missing row) is a conflict.
    ///
    /// Every branch is one or two backend statements, each of them a single conditional
    /// statement, so two concurrent writers cannot both pass the same expectation.
    pub async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: Option<i64>,
    ) -> MysqlResult<i64> {
        require_session_id(session_id)?;
        validate_state_key(key)?;
        let json = serde_json::to_string(&value)?;
        match expected_version {
            None => self
                .backend
                .save_single_versioned(user_id, session_id, key, &json),
            Some(0) => {
                let inserted = self
                    .backend
                    .insert_single_if_absent(user_id, session_id, key, &json)?;
                match inserted {
                    CasOutcome::Applied(version) => Ok(version),
                    // Java `saveIfVersion` falls back to `updateIfVersion(.., 0L)` when
                    // `insertIfAbsent` reports UNVERSIONED: a row can already exist with
                    // `version = 0` (the pre-versioning DDL default), and without the
                    // fallback that key could never be created through `Some(0)` again.
                    // The fallback is still a single conditional statement, so the CAS
                    // stays atomic.
                    CasOutcome::Conflict => {
                        self.update_zero_fallback(user_id, session_id, key, &json)
                    }
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

    /// `updateIfVersion(.., 0)` fallback for `save_if_version(.., Some(0))`, mirroring Java
    /// `saveIfVersion`: after the atomic `INSERT` hit a duplicate key, try to take over a
    /// row that is still unversioned (`version = 0`, the DDL default of pre-versioning
    /// tables). A row already at `version >= 1` remains a conflict, so create-if-absent is
    /// still a real CAS.
    fn update_zero_fallback(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        json: &str,
    ) -> MysqlResult<i64> {
        match self
            .backend
            .update_single_if_version(user_id, session_id, key, json, 0)?
        {
            CasOutcome::Applied(version) => Ok(version),
            CasOutcome::Conflict => Err(self.conflict(user_id, session_id, key, 0)),
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
        validate_state_key(key)?;
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
        validate_state_key(key)?;
        self.backend.read_version(user_id, session_id, key)
    }

    /// Truncates the session table, mirroring Java `truncateAllSessions`.
    ///
    /// MySQL treats `TRUNCATE TABLE` as DDL (implicit commit, not rollbackable, requires
    /// the `DROP` privilege), so it is issued directly rather than through a write path.
    /// The reported count is always `0`, the value the driver returns for `TRUNCATE` —
    /// exactly as in Java. The deprecated Java `clearAllSessions()` (`DELETE FROM`, row
    /// count) is intentionally not ported: Java itself points callers at this method.
    pub fn truncate_all_sessions(&self) -> MysqlResult<u64> {
        self.backend.execute_ddl(&format!(
            "TRUNCATE TABLE `{}`.`{}`",
            self.database_name, self.table_name
        ))?;
        Ok(0)
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
        if !key_access_is_valid(session_id, key) {
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
        if !key_access_is_valid(session_id, key) {
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
        if !key_access_is_valid(session_id, key) {
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
        if !key_access_is_valid(session_id, key) {
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

/// Adds the `version` column when the table predates it, mirroring Java
/// `ensureVersionColumn`.
///
/// Idempotent: the `INFORMATION_SCHEMA.COLUMNS` probe short-circuits when the column is
/// already there, so calling it on every construction costs one query and never fails with
/// MySQL's `ER_DUP_FIELDNAME` on a repeated `ALTER TABLE`.
fn ensure_version_column(
    backend: &dyn StateBackend,
    database_name: &str,
    table_name: &str,
) -> MysqlResult<()> {
    let columns = backend.table_columns(database_name, table_name)?;
    if columns.iter().any(|c| c.eq_ignore_ascii_case("version")) {
        return Ok(());
    }
    backend.execute_ddl(&add_version_column_sql(database_name, table_name))
}

/// Applies Java's `validateSessionId` + `validateStateKey` pair at the infallible
/// [`AgentStateStore`] entry points (`save` / `get` / `get_list` / `get_versioned`), which
/// must swallow errors by contract.
///
/// Java calls `validateStateKey` in every one of `save` / `get` / `getList` / `exists` /
/// `delete`; the Rust trait only passes a key to the first three (the session-wide
/// `exists` / `delete` have no key parameter). Without the guard a key longer than
/// `VARCHAR(255)` reaches MySQL, where non-strict mode truncates it and two distinct keys
/// sharing a 255-character prefix silently collide.
fn key_access_is_valid(session_id: &str, key: &str) -> bool {
    match require_session_id(session_id).and_then(|_| validate_state_key(key)) {
        Ok(()) => true,
        Err(err) => {
            tracing::debug!("rejected state access with an invalid identifier: {err}");
            false
        }
    }
}

fn require_session_id(session_id: &str) -> MysqlResult<String> {
    if session_id.trim().is_empty() {
        return Err(MysqlError::BlankSessionId);
    }
    require_max_length(session_id, "sessionId")?;
    Ok(session_id.to_string())
}

/// Maximum length of a session id or state key, matching both the Java
/// `validateSessionId` / `validateStateKey` limit and the `VARCHAR(255)` columns.
///
/// Without it a longer key reaches MySQL as a silently truncated value (non-strict mode)
/// or an opaque `Data too long` error (strict mode), and two keys sharing a 255-character
/// prefix would collide.
pub const MAX_STATE_IDENTIFIER_LENGTH: usize = 255;

/// Validates a state key, mirroring Java `validateStateKey`: non-blank, at most
/// [`MAX_STATE_IDENTIFIER_LENGTH`] characters.
pub fn validate_state_key(state_key: &str) -> MysqlResult<()> {
    if state_key.trim().is_empty() {
        return Err(MysqlError::InvalidArgument(
            "stateKey must not be blank".to_string(),
        ));
    }
    require_max_length(state_key, "stateKey")
}

fn require_max_length(value: &str, label: &str) -> MysqlResult<()> {
    if value.chars().count() > MAX_STATE_IDENTIFIER_LENGTH {
        return Err(MysqlError::InvalidArgument(format!(
            "{label} must be at most {MAX_STATE_IDENTIFIER_LENGTH} characters"
        )));
    }
    Ok(())
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
            .expect("in-memory schema bootstrap")
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
        (
            MysqlAgentStateStore::new(backend.clone()).expect("in-memory schema bootstrap"),
            backend,
        )
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

    /// Java `saveIfVersionZeroFallsBackToUpdateOnDuplicateKey`: the atomic INSERT conflicts
    /// because the row exists, but the row is still at `version = 0` (pre-versioning DDL
    /// default), so `save_if_version(.., Some(0))` must fall back to
    /// `UPDATE ... WHERE version = 0` and heal it instead of failing forever.
    #[tokio::test]
    async fn save_if_version_zero_heals_legacy_row_with_version_zero() {
        let (s, backend) = store_with_backend();
        backend.seed_single(Some("u"), "sess", "k", r#"{"legacy":true}"#, 0);
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 0);

        let version = s
            .save_if_version(Some("u"), "sess", "k", json!({"fresh": true}), Some(0))
            .await
            .expect("version = 0 row must be healed by the UPDATE fallback");

        assert_eq!(version, 1);
        assert_eq!(
            s.get(Some("u"), "sess", "k").await,
            Some(json!({"fresh": true}))
        );
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 1);
    }

    /// Java `saveIfVersionZeroConflictWhenRowAlreadyVersioned`: the fallback only matches
    /// `version = 0`, so a row that has been written at least once stays a conflict and is
    /// left untouched.
    #[tokio::test]
    async fn save_if_version_zero_conflicts_when_row_already_versioned() {
        let (s, backend) = store_with_backend();
        backend.seed_single(Some("u"), "sess", "k", r#"{"old":true}"#, 5);

        let err = s
            .save_if_version(Some("u"), "sess", "k", json!({"new": true}), Some(0))
            .await
            .expect_err("a row at version 5 must reject create-if-absent");
        assert!(matches!(err, MysqlError::InvalidArgument(_)), "{err:?}");
        assert!(
            err.to_string().contains("expected 0, found 5"),
            "conflict message should report the stored version: {err}"
        );
        assert_eq!(s.get(Some("u"), "sess", "k").await, Some(json!({"old": true})));
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 5);
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
        let s = MysqlAgentStateStore::new(backend).expect("in-memory schema bootstrap");
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

    // ---------------------------------------------------------------------------------
    // P1-6: constructor schema verification + `version` column migration
    // (Java `MysqlAgentStateStore` ctor / `verifyDatabaseExists` / `verifyTableExists` /
    //  `ensureVersionColumn`)
    // ---------------------------------------------------------------------------------

    /// `create_if_not_exist == true` provisions the database and the table, and the table
    /// carries every column the store reads/writes, both `utf8mb4_bin` key columns included.
    #[test]
    fn auto_create_provisions_database_and_table() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let s = MysqlAgentStateStore::with_config(backend.clone(), None, None, true)
            .expect("bootstrap");
        assert_eq!(s.database_name(), DEFAULT_DATABASE_NAME);
        assert_eq!(s.table_name(), DEFAULT_TABLE_NAME);
        assert!(backend.has_database(DEFAULT_DATABASE_NAME));
        assert!(backend.has_table(DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME));

        let columns = backend.columns(DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME);
        for expected in [
            "session_id",
            "state_key",
            "item_index",
            "state_data",
            "version",
            "created_at",
            "updated_at",
        ] {
            assert!(
                columns.iter().any(|c| c == expected),
                "missing column {expected:?} in {columns:?}"
            );
        }
        // Java: `CREATE DATABASE IF NOT EXISTS` then `CREATE TABLE IF NOT EXISTS`.
        let ddl = backend.ddl_log();
        assert_eq!(ddl.len(), 2, "one CREATE DATABASE + one CREATE TABLE: {ddl:?}");
        assert_eq!(ddl[0], create_database_sql(DEFAULT_DATABASE_NAME));
        assert_eq!(
            ddl[1],
            create_sessions_table_sql(DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME)
        );
    }

    /// Java pins `utf8mb4_bin` on both key columns; the case-insensitive table default would
    /// merge `S1`/`s1` and `K`/`k` into one row.
    #[test]
    fn create_table_ddl_pins_binary_collation_on_key_columns() {
        let ddl = create_sessions_table_sql("agentscope", "agentscope_sessions");
        assert!(ddl
            .starts_with("CREATE TABLE IF NOT EXISTS `agentscope`.`agentscope_sessions` ("));
        assert!(ddl.contains("session_id VARCHAR(255) COLLATE utf8mb4_bin NOT NULL"));
        assert!(ddl.contains("state_key VARCHAR(255) COLLATE utf8mb4_bin NOT NULL"));
        assert!(ddl.contains("version BIGINT NOT NULL DEFAULT 0"));
        assert!(ddl.contains("PRIMARY KEY (session_id, state_key, item_index)"));
        assert!(ddl.contains("state_data LONGTEXT NOT NULL"));
    }

    /// `create_if_not_exist == false` verifies the database first (Java
    /// `verifyDatabaseExists`) and reports the Java message when it is missing.
    #[test]
    fn missing_database_is_reported_when_not_auto_creating() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let err = MysqlAgentStateStore::with_config(backend.clone(), None, None, false)
            .expect_err("empty server has no database");
        assert!(matches!(err, MysqlError::Schema(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("Database does not exist: agentscope"),
            "{err}"
        );
        assert!(
            backend.ddl_log().is_empty(),
            "verification must not create anything"
        );
    }

    /// The database may exist while the table does not — Java's second check.
    #[test]
    fn missing_table_is_reported_when_not_auto_creating() {
        let backend = Arc::new(InMemoryStateBackend::default());
        backend.create_database(DEFAULT_DATABASE_NAME);
        let err = MysqlAgentStateStore::with_config(backend.clone(), None, None, false)
            .expect_err("database without table");
        assert!(matches!(err, MysqlError::Schema(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("Table does not exist: agentscope.agentscope_sessions"),
            "{err}"
        );
        assert!(backend.ddl_log().is_empty());
    }

    /// A table created before versioning gets the `ALTER TABLE ... DEFAULT 1` migration on
    /// construction, even with `create_if_not_exist == false` (Java runs
    /// `ensureVersionColumn` in both branches).
    #[test]
    fn legacy_table_gets_the_version_column_migration() {
        let backend = Arc::new(InMemoryStateBackend::default());
        backend.create_legacy_table(DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME);
        assert!(!backend
            .columns(DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME)
            .iter()
            .any(|c| c == "version"));

        let _s = MysqlAgentStateStore::with_config(backend.clone(), None, None, false)
            .expect("existing schema is upgraded");

        assert!(backend
            .columns(DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME)
            .iter()
            .any(|c| c == "version"));
        assert_eq!(
            backend.ddl_log(),
            vec![add_version_column_sql(
                DEFAULT_DATABASE_NAME,
                DEFAULT_TABLE_NAME
            )],
            "exactly one ALTER, with DEFAULT 1 (0 is the 'row absent' sentinel)"
        );
        assert!(add_version_column_sql("agentscope", "agentscope_sessions")
            .ends_with("ADD COLUMN version BIGINT NOT NULL DEFAULT 1"));
    }

    /// The migration is idempotent: a second construction finds `version` and issues no DDL
    /// (MySQL would otherwise fail the repeated `ALTER TABLE` with ER_DUP_FIELDNAME).
    #[test]
    fn version_column_migration_is_idempotent() {
        let backend = Arc::new(InMemoryStateBackend::default());
        backend.create_legacy_table(DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME);
        MysqlAgentStateStore::with_config(backend.clone(), None, None, false)
            .expect("first upgrade");
        let after_first = backend.ddl_log().len();
        assert_eq!(after_first, 1);

        MysqlAgentStateStore::with_config(backend.clone(), None, None, false)
            .expect("second construction");
        assert_eq!(backend.ddl_log().len(), after_first, "no repeated ALTER");
    }

    /// A table that already has `version` is left untouched, and `auto_create` on an
    /// existing table is a pair of idempotent `IF NOT EXISTS` statements.
    #[test]
    fn existing_version_column_is_left_alone() {
        let backend = Arc::new(InMemoryStateBackend::default());
        MysqlAgentStateStore::with_config(backend.clone(), None, None, true)
            .expect("bootstrap");
        let after_create = backend.ddl_log().len();
        assert_eq!(after_create, 2);

        MysqlAgentStateStore::with_config(backend.clone(), None, None, false)
            .expect("verify existing schema");
        assert_eq!(
            backend.ddl_log().len(),
            after_create,
            "nothing to migrate, nothing to create"
        );
    }

    /// A failing DDL must surface instead of leaving a half-provisioned store behind
    /// (the previous implementation discarded the `ensure_schema` result).
    #[test]
    fn ddl_failure_propagates_when_auto_creating() {
        let backend = Arc::new(InMemoryStateBackend::default());
        backend.fail_ddl(true);
        let err = MysqlAgentStateStore::with_config(backend.clone(), None, None, true)
            .expect_err("CREATE DATABASE failed");
        assert!(matches!(err, MysqlError::Sql(_)), "{err:?}");
        assert!(!backend.has_database(DEFAULT_DATABASE_NAME));
    }

    /// Custom names flow into the DDL verbatim and are still validated as identifiers.
    #[test]
    fn custom_names_flow_into_the_provisioning_ddl() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let s = MysqlAgentStateStore::with_config(
            backend.clone(),
            Some("custom_db"),
            Some("custom_sessions"),
            true,
        )
        .expect("bootstrap");
        assert_eq!(s.database_name(), "custom_db");
        assert_eq!(s.table_name(), "custom_sessions");
        assert!(backend.has_table("custom_db", "custom_sessions"));
        assert_eq!(
            backend.ddl_log(),
            vec![
                create_database_sql("custom_db"),
                create_sessions_table_sql("custom_db", "custom_sessions")
            ]
        );
    }

    /// Mirrors Java `saveIfVersionUnconditionalAbsentKeyReturnsZero`: reading the version of
    /// a key that was never written reports `0`, and an unconditional save then starts at 1.
    #[test]
    fn absent_key_reports_version_zero() {
        let s = store();
        assert_eq!(s.read_version(Some("u"), "sess", "never").unwrap(), 0);
    }

    /// Java `validateStateKey`: blank keys and keys longer than 255 characters are rejected
    /// before they reach the `VARCHAR(255)` column, where they would be truncated or
    /// collide with a sibling key sharing the prefix.
    #[tokio::test]
    async fn state_key_validation_mirrors_java() {
        let s = store();
        let blank = s
            .save_if_version(Some("u"), "sess", "  ", json!(1), None)
            .await;
        assert!(
            matches!(blank, Err(MysqlError::InvalidArgument(_))),
            "unexpected: {blank:?}"
        );

        let long = "k".repeat(MAX_STATE_IDENTIFIER_LENGTH + 1);
        assert!(matches!(
            s.read_version(Some("u"), "sess", &long),
            Err(MysqlError::InvalidArgument(_))
        ));

        let at_limit = "k".repeat(MAX_STATE_IDENTIFIER_LENGTH);
        assert_eq!(s.read_version(Some("u"), "sess", &at_limit).unwrap(), 0);
        assert!(validate_state_key("ok").is_ok());
    }

    /// Java `save` / `get` / `getList` all call `validateStateKey` before touching the
    /// backend. The Rust trait entry points are infallible, so an invalid key must drop the
    /// access instead of writing or reading a value that MySQL would truncate to 255 chars.
    #[tokio::test]
    async fn infallible_entries_reject_invalid_state_keys() {
        let (s, backend) = store_with_backend();
        let long = "k".repeat(MAX_STATE_IDENTIFIER_LENGTH + 1);

        // `save` must not reach the backend at all.
        s.save(Some("u"), "sess", &long, json!(1)).await;
        assert_eq!(backend.get_single(Some("u"), "sess", &long).unwrap(), None);

        // A row that already exists under such a key (written before this guard) must not be
        // readable through the truncated key either.
        backend.seed_single(Some("u"), "sess", &long, r#"{"secret":1}"#, 1);
        assert_eq!(s.get(Some("u"), "sess", &long).await, None);
        assert!(s.get_list(Some("u"), "sess", &long).await.is_empty());

        // Blank keys are rejected the same way (Java: "stateKey must not be blank").
        s.save(Some("u"), "sess", "   ", json!(3)).await;
        assert_eq!(s.get(Some("u"), "sess", "   ").await, None);

        // Boundary: a key at the limit is still accepted.
        let at_limit = "k".repeat(MAX_STATE_IDENTIFIER_LENGTH);
        s.save(Some("u"), "sess", &at_limit, json!(2)).await;
        assert_eq!(s.get(Some("u"), "sess", &at_limit).await, Some(json!(2)));
    }

    /// `truncate_all_sessions` issues the Java `TRUNCATE TABLE` statement, and the rows are
    /// gone afterwards (the deprecated `clearAllSessions()` is not ported).
    #[tokio::test]
    async fn truncate_all_sessions_clears_rows() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let s = MysqlAgentStateStore::new(backend.clone()).expect("bootstrap");
        s.save_if_version(Some("u"), "sess", "k", json!(1), None)
            .await
            .expect("save");
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 1);

        assert_eq!(s.truncate_all_sessions().expect("truncate"), 0);
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 0);
        assert!(backend
            .ddl_log()
            .iter()
            .any(|sql| sql == "TRUNCATE TABLE `agentscope`.`agentscope_sessions`"));
    }

    /// Java `saveIfVersionUnconditionalPropagatesReadVersionException`: a backend failure on
    /// the CAS path must surface, never be reported as "unversioned" or a silent success.
    #[tokio::test]
    async fn cas_backend_errors_propagate() {
        let backend = Arc::new(InMemoryStateBackend::default());
        let s = MysqlAgentStateStore::new(backend.clone()).expect("bootstrap");

        backend.fail_cas(true);
        let create = s
            .save_if_version(Some("u"), "sess", "k", json!(1), Some(0))
            .await;
        assert!(matches!(create, Err(MysqlError::Sql(_))), "{create:?}");
        assert_eq!(s.read_version(Some("u"), "sess", "k").unwrap(), 0);

        backend.fail_cas(false);
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", json!(1), Some(0))
                .await
                .expect("create after recovery"),
            1
        );
        backend.fail_cas(true);
        let update = s
            .save_if_version(Some("u"), "sess", "k", json!(2), Some(1))
            .await;
        assert!(matches!(update, Err(MysqlError::Sql(_))), "{update:?}");
    }

    /// Java `createTablePrimaryKeyFitsInnoDbUtf8mb4Limit`: the composite key must stay under
    /// InnoDB's 3072-byte limit for `utf8mb4` ((512 + 255) * 4 = 3068 bytes).
    #[test]
    fn create_table_primary_key_fits_inno_db_limit() {
        let ddl = create_sessions_table_sql(DEFAULT_DATABASE_NAME, DEFAULT_TABLE_NAME);
        let bytes = (512 + 255) * 4;
        assert!(bytes <= 3072, "{bytes} bytes exceeds the index limit");
        assert!(ddl.contains("session_id VARCHAR(255)"), "{ddl}");
        assert!(ddl.contains("state_key VARCHAR(255)"), "{ddl}");
        assert!(
            ddl.contains("PRIMARY KEY (session_id, state_key, item_index)"),
            "{ddl}"
        );
    }
}
