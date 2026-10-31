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

use agentscope_core::state::list_hash::{
    compute_hash as core_compute_hash, needs_full_rewrite as core_needs_full_rewrite,
};
use agentscope_core::state::AgentStateStore;
use async_trait::async_trait;
use serde_json::Value;

use crate::backend::{ColumnValue, JdbcError, JdbcResult, JdbcSession, SharedExecutor};
use crate::dialect::{
    validate_table_schema, AbstractJdbcDialect, BoundSql, SessionStateDialect, SqlParam,
};

/// The separator between the user segment and session id inside a `slot_id`.
const SLOT_SEPARATOR: char = ':';
/// Suffix appended to a list key to derive its stored hash row's key.
const HASH_KEY_SUFFIX: &str = ":_hash";
/// The single-value index (mirrors Java `SINGLE_STATE_INDEX`).
const SINGLE_STATE_INDEX: i64 = 0;
/// The anonymous user namespace, mirroring core's `ANON_USER`.
const ANON_USER: &str = "__anon__";

// `VersionedState` / `UNVERSIONED` have exactly **one** definition in the workspace: the
// core `agentscope_core::state` pair (adjudication A2 lifted the versioning surface onto the
// [`AgentStateStore`] trait). They are re-exported here — rather than redefined — so the
// pre-existing public paths (`agentscope_extensions_jdbc::{VersionedState, UNVERSIONED}` and
// `agentscope_extensions_jdbc::state::{...}`) keep working while callers can move values
// between this crate and core without a conversion.
pub use agentscope_core::state::{VersionedState, UNVERSIONED};

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
    pub fn with_schema(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
    ) -> JdbcResult<Self> {
        Self::new(executor, dialect, true)
    }

    /// Runs the dialect's idempotent DDL for the session-state table, then validates
    /// the resulting schema (Java `TableSchemaValidator`).
    pub fn create_table_if_not_exist(&self) -> JdbcResult<()> {
        let ddls = self.dialect.session_state_create_table_ddls();
        crate::schema_bootstrap::run_ddl(&*self.executor, self.dialect.as_ref(), &ddls)?;
        validate_table_schema(
            &*self.executor,
            self.dialect.as_ref(),
            &self.dialect.resolved_session_state_table_name(),
            &ddls,
        )?;
        Ok(())
    }

    /// Verifies the session-state table exists (used when auto-creation is disabled).
    ///
    /// Mirrors the Java `verifyTableExists`, which throws when the existence probe
    /// returns no rows. The probe is a `SELECT 1 ... WHERE <name match>` query, so an
    /// empty result — not a SQL error — signals a missing table.
    pub fn verify_table_exists(&self) -> JdbcResult<()> {
        let table = self.dialect.resolved_session_state_table_name();
        let bound = self.dialect.session_state_check_table_exists(&table);
        let rows = self.executor.query(&bound).map_err(|e| {
            JdbcError::Database(format!(
                "table does not exist: {table}. use create_if_not_exist=true to auto-create. ({e})"
            ))
        })?;
        if rows.is_empty() {
            return Err(JdbcError::Database(format!(
                "table does not exist: {table}. use create_if_not_exist=true to auto-create."
            )));
        }
        Ok(())
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

    /// Rejects negative expected versions other than [`UNVERSIONED`], mirroring the Java
    /// `JdbcAgentStateStore.saveIfVersion` guard (`expectedVersion != UNVERSIONED &&
    /// expectedVersion < 0` → `IllegalArgumentException`).
    fn validate_expected_version(expected_version: i64) -> JdbcResult<()> {
        if expected_version != UNVERSIONED && expected_version < 0 {
            return Err(JdbcError::Database(format!(
                "expectedVersion must be >= 0 or UNVERSIONED: {expected_version}"
            )));
        }
        Ok(())
    }

    fn json_of(value: &Value) -> JdbcResult<String> {
        serde_json::to_string(value)
            .map_err(|e| JdbcError::Database(format!("serialize state: {e}")))
    }

    /// Reads the stored hash for a list key, if any.
    ///
    /// Runs on `session` so that it can take part in the same transaction as the write it
    /// guards (see [`Self::save_list_impl`]).
    fn stored_hash(
        &self,
        session: &mut dyn JdbcSession,
        slot_id: &str,
        hash_key: &str,
    ) -> JdbcResult<Option<String>> {
        let bound = self
            .dialect
            .session_state_select(slot_id, hash_key, SINGLE_STATE_INDEX as i32);
        let rows = session.query(&bound)?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_text().map(|s| s.to_string())))
    }

    /// Reads the number of stored items for a list key (`MAX(item_index) + 1`).
    fn list_count(
        &self,
        session: &mut dyn JdbcSession,
        slot_id: &str,
        key: &str,
    ) -> JdbcResult<usize> {
        let bound = self.dialect.session_state_select_max_index(slot_id, key);
        let rows = session.query(&bound)?;
        let max = rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_int());
        Ok(max.map(|m| (m + 1) as usize).unwrap_or(0))
    }

    /// Reads the stored version for a single key without deserializing `state_data`.
    ///
    /// Mirrors the Java `readVersion`: reading the version must never touch the payload
    /// (the `State` marker type has no concrete counterpart to deserialize into), so only
    /// the `version` column of the dialect's versioned projection is consumed.
    fn read_version_at(&self, slot_id: &str, key: &str) -> JdbcResult<i64> {
        let bound = self
            .dialect
            .session_state_select(slot_id, key, SINGLE_STATE_INDEX as i32);
        Ok(version_from_rows(self.executor.query(&bound)?))
    }

    /// [`Self::read_version_at`] on an already-open session, so the read can share a
    /// transaction with the write whose version it reports (see
    /// [`Self::upsert_then_read_version`]).
    fn read_version_in(
        &self,
        session: &mut dyn JdbcSession,
        slot_id: &str,
        key: &str,
    ) -> JdbcResult<i64> {
        let bound = self
            .dialect
            .session_state_select(slot_id, key, SINGLE_STATE_INDEX as i32);
        Ok(version_from_rows(session.query(&bound)?))
    }

    /// Whether this backend supports optimistic concurrency, mirroring the Java
    /// `supportsVersioning()`. Always `true` for JDBC.
    ///
    /// Core's `AgentStateStore` trait exposes no versioning surface, so the versioned API
    /// below is reachable only as inherent methods (see §3.2 / §4.2).
    pub fn supports_versioning(&self) -> bool {
        true
    }

    /// Reads the stored version for `(user_id, session_id, key)`, or `0` when absent
    /// (mirrors the Java `readVersion`).
    pub fn read_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> JdbcResult<i64> {
        Self::validate_session(session_id)?;
        Self::validate_key(key)?;
        let slot = Self::slot_id(user_id, session_id);
        self.read_version_at(&slot, key)
    }

    /// Loads a single state value together with its store version, mirroring the Java
    /// `getVersioned`. An absent key reports `version == 0` on this versioning backend.
    pub fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> JdbcResult<VersionedState> {
        Self::validate_session(session_id)?;
        Self::validate_key(key)?;
        let slot = Self::slot_id(user_id, session_id);
        let bound = self
            .dialect
            .session_state_select(&slot, key, SINGLE_STATE_INDEX as i32);
        let rows = self.executor.query(&bound)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(VersionedState {
                value: None,
                version: 0,
            });
        };
        let mut columns = row.into_iter();
        let json = columns.next().and_then(|c| c.as_text().map(str::to_string));
        let version = columns.next().and_then(|c| c.as_int()).unwrap_or(0);
        let value = match json {
            Some(j) => Some(
                serde_json::from_str(&j)
                    .map_err(|e| JdbcError::Database(format!("deserialize state: {e}")))?,
            ),
            None => None,
        };
        Ok(VersionedState { value, version })
    }

    /// Compare-and-swap write of a single state value, mirroring the Java `saveIfVersion`.
    ///
    /// * `expected_version == 0` — create-if-absent (returns [`UNVERSIONED`] when the key
    ///   already exists).
    /// * `expected_version == `[`UNVERSIONED`] — unconditional overwrite (same as `save`).
    /// * otherwise — writes only when the stored version equals `expected_version`.
    ///
    /// Returns the new version on success, or [`UNVERSIONED`] on conflict (nothing written).
    /// Any other negative `expected_version` is rejected up front (mirrors the Java
    /// `IllegalArgumentException`): such a token can never match a stored version, so treating
    /// it as a conflict would report "another writer got there first" instead of "the caller
    /// passed a nonsensical token".
    pub fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
        expected_version: i64,
    ) -> JdbcResult<i64> {
        Self::validate_session(session_id)?;
        Self::validate_key(key)?;
        Self::validate_expected_version(expected_version)?;
        let slot = Self::slot_id(user_id, session_id);
        let json = Self::json_of(&value)?;

        if expected_version == UNVERSIONED {
            return self.upsert_returning_version(&slot, key, &json);
        }

        // Every branch below is a single statement, so each is already atomic and the Java
        // `executeInWriteTransaction` wrapper has no counterpart here.
        if expected_version == 0 {
            let bound = self.dialect.session_state_insert_if_absent(
                &slot,
                key,
                SINGLE_STATE_INDEX as i32,
                &json,
            );
            return match self.executor.execute(&bound) {
                Ok(1) => Ok(1),
                // The key already exists. `session_state_insert` (list elements) omits the
                // `version` column, so it lands on the DDL default `0`. Such a row can never
                // be create-if-absent written (PK conflict every time) and version `0` is
                // also never taken by the CAS path — the key would be stuck forever.
                // Self-heal: retry as a CAS at version `0`.
                Ok(_) => self.cas_zero_version(&slot, key, &json),
                Err(e) if crate::backend::is_unique_violation(&e) => {
                    self.cas_zero_version(&slot, key, &json)
                }
                Err(e) => Err(e),
            };
        }

        let bound = self.dialect.session_state_update_if_version(
            &slot,
            key,
            SINGLE_STATE_INDEX as i32,
            &json,
            expected_version,
        );
        match self.executor.execute(&bound) {
            Ok(1) => Ok(expected_version + 1),
            Ok(_) => Ok(UNVERSIONED),
            Err(e) => Err(e),
        }
    }

    /// Retries a failed create-if-absent as a CAS at version `0`, unblocking rows that sit on
    /// the DDL default.
    ///
    /// The `WHERE version = 0` guard is what keeps this a genuine CAS rather than an
    /// unconditional overwrite: a row at any other version reports `0` affected rows and the
    /// caller still sees [`UNVERSIONED`].
    fn cas_zero_version(&self, slot_id: &str, key: &str, json: &str) -> JdbcResult<i64> {
        let bound = self.dialect.session_state_update_if_version(
            slot_id,
            key,
            SINGLE_STATE_INDEX as i32,
            json,
            0,
        );
        match self.executor.execute(&bound) {
            Ok(1) => Ok(1),
            Ok(_) => Ok(UNVERSIONED),
            Err(e) => Err(e),
        }
    }

    fn save_single(&self, slot_id: &str, key: &str, json: &str) -> JdbcResult<()> {
        let bound =
            self.dialect
                .session_state_upsert(slot_id, key, SINGLE_STATE_INDEX as i32, json);
        self.executor.execute(&bound)?;
        Ok(())
    }

    /// UPSERTs a single row and returns the version that this very write produced.
    ///
    /// Preferred path: one statement (`INSERT ... ON CONFLICT DO UPDATE ... RETURNING
    /// version`), so no concurrent writer can interleave between the write and the read.
    ///
    /// Fallback path: dialects without `RETURNING` (MySQL's `ON DUPLICATE KEY UPDATE`,
    /// H2's `MERGE`) run the UPSERT then a separate `SELECT version`.
    ///
    /// Those two statements share **one transaction** (mirroring Java's
    /// `executeInWriteTransaction`): the UPSERT holds the row lock until commit, so a
    /// concurrent writer cannot commit in between and the version handed back is the one this
    /// write produced. Without the transaction the follow-up `SELECT` could report *another*
    /// writer's version — a CAS token this call never wrote.
    fn upsert_returning_version(&self, slot_id: &str, key: &str, json: &str) -> JdbcResult<i64> {
        let Some(bound) = self.dialect.session_state_upsert_returning_version(
            slot_id,
            key,
            SINGLE_STATE_INDEX as i32,
            json,
        ) else {
            return self.upsert_then_read_version(slot_id, key, json);
        };
        let rows = self.executor.query(&bound)?;
        let version = rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_int());
        match version {
            Some(v) => Ok(v),
            // A dialect advertising a returning UPSERT must hand back exactly one row; if it
            // does not, fall back to a plain read instead of reporting a bogus version (and
            // without a second write: the UPSERT above already landed).
            None => self.read_version_at(slot_id, key),
        }
    }

    /// UPSERT followed by `SELECT version` inside one transaction — the no-`RETURNING`
    /// equivalent of [`Self::upsert_returning_version`]'s single statement.
    fn upsert_then_read_version(&self, slot_id: &str, key: &str, json: &str) -> JdbcResult<i64> {
        let mut version = 0i64;
        self.executor
            .in_transaction(&mut |session: &mut dyn JdbcSession| -> JdbcResult<()> {
                let bound = self.dialect.session_state_upsert(
                    slot_id,
                    key,
                    SINGLE_STATE_INDEX as i32,
                    json,
                );
                session.execute(&bound)?;
                version = self.read_version_in(session, slot_id, key)?;
                Ok(())
            })?;
        Ok(version)
    }

    /// Saves a list using hash-based change detection, mirroring Java semantics.
    ///
    /// The whole read-modify-write — read `:_hash`, read `MAX(item_index)`, then insert the
    /// tail / rewrite the list — runs inside **one** [`JdbcExecutor::in_transaction`] call,
    /// mirroring Java's `executeInWriteTransaction`. Without that envelope a second writer
    /// (another process, another pool connection) can commit between our reads and our
    /// writes: our `existing` count is then stale, the tail INSERT hits the
    /// `(session_id, state_key, item_index)` primary key, and the whole batch is rolled back
    /// — an append silently lost. With the envelope the reads and writes are serialized
    /// against every other writer of the same rows, so the index we append at is the index
    /// that is actually committed.
    fn save_list_impl(&self, slot_id: &str, key: &str, values: &[Value]) -> JdbcResult<()> {
        if values.is_empty() {
            return Ok(());
        }
        let hash_key = format!("{key}{HASH_KEY_SUFFIX}");
        let current_hash = core_compute_hash(values);
        // Serialize before opening the transaction so a bad value cannot leave one behind.
        let payloads = values.iter().map(Self::json_of).collect::<JdbcResult<Vec<_>>>()?;

        self.executor
            .in_transaction(&mut |session: &mut dyn JdbcSession| -> JdbcResult<()> {
                let stored = self.stored_hash(session, slot_id, &hash_key)?;
                let existing = self.list_count(session, slot_id, key)?;

                // Reuse core's list-hash change detection (mirrors Java
                // `ListHashUtil.needsFullRewrite`) rather than inventing a parallel heuristic.
                // `needs_full_rewrite` hashes the stored prefix of the incoming list and
                // compares it against the persisted hash.
                let needs_full_rewrite =
                    core_needs_full_rewrite(values, stored.as_deref(), existing);

                if needs_full_rewrite {
                    session.execute(&self.dialect.session_state_delete_by_key(slot_id, key))?;
                    for (index, payload) in payloads.iter().enumerate() {
                        session.execute(&self.dialect.session_state_insert(
                            slot_id,
                            key,
                            index as i32,
                            payload,
                        ))?;
                    }
                } else if values.len() > existing {
                    // Incremental append: only the tail beyond the stored prefix is inserted.
                    for (offset, payload) in payloads[existing..].iter().enumerate() {
                        session.execute(&self.dialect.session_state_insert(
                            slot_id,
                            key,
                            (existing + offset) as i32,
                            payload,
                        ))?;
                    }
                } else {
                    // Nothing changed: an identical list is a no-op (no hash rewrite either).
                    return Ok(());
                }

                // The tail inserts and the hash upsert use different SQL templates
                // (INSERT vs UPSERT); both run in the same transaction as the reads above,
                // so a duplicate-PK abort can no longer leave the list half written.
                session.execute(&self.dialect.session_state_upsert(
                    slot_id,
                    &hash_key,
                    SINGLE_STATE_INDEX as i32,
                    &current_hash,
                ))?;
                Ok(())
            })
    }
}

#[async_trait]
impl AgentStateStore for JdbcAgentStateStore {
    async fn save(&self, user_id: Option<&str>, session_id: &str, key: &str, value: Value) {
        if Self::validate_session(session_id).is_err() || Self::validate_key(key).is_err() {
            return;
        }
        let slot = Self::slot_id(user_id, session_id);
        let Ok(json) = Self::json_of(&value) else {
            return;
        };
        if let Err(e) = self.save_single(&slot, key, &json) {
            tracing::warn!("[jdbc-state] save failed for {key}: {e}");
        }
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[Value],
    ) {
        if Self::validate_session(session_id).is_err() || Self::validate_key(key).is_err() {
            return;
        }
        let slot = Self::slot_id(user_id, session_id);
        if let Err(e) = self.save_list_impl(&slot, key, values) {
            // The read-modify-write runs in one transaction, so a failure here means the whole
            // append was rolled back — the stored list is still at its previous length, never
            // half written. (A caller-visible failure channel is JDBC-12, blocked on core.)
            tracing::warn!(
                "[jdbc-state] save_list failed for {key}; the whole append was rolled back: {e}"
            );
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
        let pattern = like_prefix_pattern(&prefix, self.dialect.session_state_like_escape_char());
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

    fn supports_versioning(&self) -> bool {
        true
    }

    async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> agentscope_core::state::VersionedState {
        // The inherent method already returns the core type (single definition, re-exported
        // at the top of this module), so no conversion is needed here.
        match JdbcAgentStateStore::get_versioned(self, user_id, session_id, key) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("[jdbc-state] get_versioned failed for {key}: {e}");
                VersionedState {
                    value: None,
                    version: UNVERSIONED,
                }
            }
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
        match JdbcAgentStateStore::save_if_version(
            self,
            user_id,
            session_id,
            key,
            value,
            expected_version,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("[jdbc-state] save_if_version failed for {key}: {e}");
                UNVERSIONED
            }
        }
    }
}

/// Extracts the `version` column (second column of the versioned projection) from a
/// session-state select result. An absent row reports `0`, mirroring Java's `readVersion`.
fn version_from_rows(rows: Vec<Vec<ColumnValue>>) -> i64 {
    rows.into_iter()
        .next()
        .and_then(|r| r.into_iter().nth(1))
        .and_then(|c| c.as_int())
        .unwrap_or(0)
}

/// Builds a `LIKE` prefix pattern matching exactly `prefix` followed by anything,
/// escaping every `_`, `%` and the dialect's escape char so the user segment is literal.
fn like_prefix_pattern(prefix: &str, escape: char) -> String {
    let mut out = String::with_capacity(prefix.len() + 1);
    for ch in prefix.chars() {
        if ch == escape || ch == '%' || ch == '_' {
            out.push(escape);
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
