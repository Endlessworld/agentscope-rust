//! Test-only in-memory [`StateBackend`] implementation.
//!
//! Models the `(session_id, state_key, item_index)` primary key plus the `version` column of
//! the MySQL schema, so the incremental list semantics, the hash-based change detection and
//! the atomic CAS primitives of [`super::MysqlAgentStateStore`] can be exercised without a
//! live database. Compiled only under `cfg(test)`.
//!
//! The CAS primitives take the same mutex as the value maps, so each of them is a single
//! critical section — the in-memory stand-in for one conditional SQL statement.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use super::{CasOutcome, StateBackend};
use crate::error::{MysqlError, MysqlResult};

/// Anonymous-user sentinel, mirroring `agentscope_core::state::store`.
const ANON_USER: &str = "__anon__";

fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => ANON_USER.to_string(),
    }
}

/// Identity of a session row group.
type SessionKey = (String, String);

/// In-memory model of the MySQL schema: `database -> table -> columns`.
#[derive(Default)]
struct SchemaState {
    databases: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

/// In-memory model of `agentscope_sessions`.
#[derive(Default)]
pub struct InMemoryStateBackend {
    /// `(user, session) -> state_key -> (item_index -> json)`.
    rows: Mutex<HashMap<SessionKey, HashMap<String, BTreeMap<i64, String>>>>,
    /// `(user, session) -> state_key -> version` of the `item_index = 0` row.
    versions: Mutex<HashMap<SessionKey, HashMap<String, i64>>>,
    /// Number of `save_list` calls, so tests can assert that an unchanged list is skipped.
    list_writes: AtomicUsize,
    /// Modelled `INFORMATION_SCHEMA` (databases / tables / columns).
    schema: Mutex<SchemaState>,
    /// DDL statements accepted so far, in order.
    ddl_log: Mutex<Vec<String>>,
    /// When set, every `execute_ddl` fails (simulates a locked or read-only server).
    fail_ddl: AtomicBool,
}

impl InMemoryStateBackend {
    fn key(user_id: Option<&str>, session_id: &str) -> SessionKey {
        (normalize_user(user_id), session_id.to_string())
    }

    /// Number of `save_list` invocations (stays at 0 when the store skipped an unchanged
    /// list).
    pub fn list_write_count(&self) -> usize {
        self.list_writes.load(Ordering::SeqCst)
    }

    /// Makes every subsequent `execute_ddl` fail.
    pub fn fail_ddl(&self, fail: bool) {
        self.fail_ddl.store(fail, Ordering::SeqCst);
    }

    /// DDL statements accepted so far, in order.
    pub fn ddl_log(&self) -> Vec<String> {
        self.ddl_log.lock().unwrap().clone()
    }

    /// Pre-provisions a database without any table.
    pub fn create_database(&self, database_name: &str) {
        self.schema
            .lock()
            .unwrap()
            .databases
            .entry(database_name.to_string())
            .or_default();
    }

    /// Pre-provisions a table in the **pre-versioning** shape (no `version` column), the way
    /// a database created before the CAS work looks.
    pub fn create_legacy_table(&self, database_name: &str, table_name: &str) {
        let mut schema = self.schema.lock().unwrap();
        schema
            .databases
            .entry(database_name.to_string())
            .or_default()
            .insert(
                table_name.to_string(),
                vec![
                    "session_id".to_string(),
                    "state_key".to_string(),
                    "item_index".to_string(),
                    "state_data".to_string(),
                ],
            );
    }

    /// Whether the modelled database exists.
    pub fn has_database(&self, database_name: &str) -> bool {
        self.schema.lock().unwrap().databases.contains_key(database_name)
    }

    /// Whether the modelled table exists.
    pub fn has_table(&self, database_name: &str, table_name: &str) -> bool {
        self.schema
            .lock()
            .unwrap()
            .databases
            .get(database_name)
            .map(|tables| tables.contains_key(table_name))
            .unwrap_or(false)
    }

    /// Modelled columns of a table (`[]` when absent).
    pub fn columns(&self, database_name: &str, table_name: &str) -> Vec<String> {
        self.schema
            .lock()
            .unwrap()
            .databases
            .get(database_name)
            .and_then(|tables| tables.get(table_name))
            .cloned()
            .unwrap_or_default()
    }
}

/// All backtick-quoted identifiers of a statement, in order.
fn backticked(sql: &str) -> Vec<String> {
    sql.split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.to_string())
        .collect()
}

/// Top-level column names of a `CREATE TABLE` statement (constraints skipped).
fn parse_columns(ddl: &str) -> Vec<String> {
    let Some(open) = ddl.find('(') else {
        return Vec::new();
    };
    let mut depth = 0usize;
    let mut segment = String::new();
    let mut segments: Vec<String> = Vec::new();
    for ch in ddl[open..].chars() {
        match ch {
            '(' => {
                depth += 1;
                if depth > 1 {
                    segment.push(ch);
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                segment.push(ch);
            }
            ',' if depth == 1 => segments.push(std::mem::take(&mut segment)),
            _ => {
                if depth >= 1 {
                    segment.push(ch);
                }
            }
        }
    }
    segments.push(segment);
    segments
        .iter()
        .filter_map(|seg| {
            let seg = seg.trim();
            if seg.is_empty() {
                return None;
            }
            let upper = seg.to_ascii_uppercase();
            const KEYWORDS: [&str; 7] = [
                "PRIMARY", "UNIQUE", "KEY", "INDEX", "CONSTRAINT", "FOREIGN", "CHECK",
            ];
            if KEYWORDS.iter().any(|kw| upper.starts_with(kw)) {
                return None;
            }
            seg.split_whitespace().next().map(|s| s.to_string())
        })
        .collect()
}

impl StateBackend for InMemoryStateBackend {
    /// Applies the statement to the modelled schema, so `database_exists` /
    /// `table_exists` / `table_columns` reflect what the crate's DDL actually declared.
    fn execute_ddl(&self, sql: &str) -> MysqlResult<()> {
        if self.fail_ddl.load(Ordering::SeqCst) {
            return Err(MysqlError::Sql("access denied for DDL".to_string()));
        }
        let upper = sql.trim_start().to_ascii_uppercase();
        {
            let mut schema = self.schema.lock().unwrap();
            if upper.starts_with("CREATE DATABASE") {
                if let Some(db) = backticked(sql).first() {
                    schema.databases.entry(db.clone()).or_default();
                }
            } else if upper.starts_with("CREATE TABLE") {
                let names = backticked(sql);
                if names.len() < 2 {
                    return Err(MysqlError::Sql(format!("unparsable DDL: {sql}")));
                }
                let (db, table) = (names[0].clone(), names[1].clone());
                let tables = schema
                    .databases
                    .get_mut(&db)
                    .ok_or_else(|| MysqlError::Sql(format!("Unknown database '{db}'")))?;
                tables.entry(table).or_insert_with(|| parse_columns(sql));
            } else if upper.starts_with("ALTER TABLE") {
                let names = backticked(sql);
                if names.len() < 2 {
                    return Err(MysqlError::Sql(format!("unparsable DDL: {sql}")));
                }
                let column = sql
                    .split_once("ADD COLUMN")
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .ok_or_else(|| MysqlError::Sql(format!("unparsable ALTER: {sql}")))?
                    .to_string();
                let tables = schema
                    .databases
                    .get_mut(&names[0])
                    .ok_or_else(|| MysqlError::Sql(format!("Unknown database '{}'", names[0])))?;
                let columns = tables
                    .get_mut(&names[1])
                    .ok_or_else(|| MysqlError::Sql(format!("Unknown table '{}'", names[1])))?;
                if !columns.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
                    columns.push(column);
                }
            }
        }
        self.ddl_log.lock().unwrap().push(sql.to_string());
        Ok(())
    }

    fn database_exists(&self, database_name: &str) -> MysqlResult<bool> {
        Ok(self.schema.lock().unwrap().databases.contains_key(database_name))
    }

    fn table_exists(&self, database_name: &str, table_name: &str) -> MysqlResult<bool> {
        Ok(self.has_table(database_name, table_name))
    }

    fn table_columns(&self, database_name: &str, table_name: &str) -> MysqlResult<Vec<String>> {
        Ok(self.columns(database_name, table_name))
    }

    fn save_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<()> {
        let mut rows = self.rows.lock().unwrap();
        let session = rows.entry(Self::key(user_id, session_id)).or_default();
        session
            .entry(state_key.to_string())
            .or_default()
            .insert(super::SINGLE_STATE_INDEX, value_json.to_string());
        Ok(())
    }

    fn save_single_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<i64> {
        let mut rows = self.rows.lock().unwrap();
        let mut versions = self.versions.lock().unwrap();
        let key = Self::key(user_id, session_id);
        rows.entry(key.clone())
            .or_default()
            .entry(state_key.to_string())
            .or_default()
            .insert(super::SINGLE_STATE_INDEX, value_json.to_string());
        let version = versions
            .entry(key)
            .or_default()
            .entry(state_key.to_string())
            .or_insert(0);
        *version += 1;
        Ok(*version)
    }

    fn insert_single_if_absent(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<CasOutcome> {
        let key = Self::key(user_id, session_id);
        let mut rows = self.rows.lock().unwrap();
        let mut versions = self.versions.lock().unwrap();
        // A row may pre-date versioning (Java: `ALTER TABLE ... DEFAULT 0` back-fill), so the
        // primary-key presence — not the version map — decides whether the INSERT conflicts.
        let present = rows
            .get(&key)
            .and_then(|s| s.get(state_key))
            .map(|items| items.contains_key(&super::SINGLE_STATE_INDEX))
            .unwrap_or(false);
        if present {
            return Ok(CasOutcome::Conflict);
        }
        rows.entry(key.clone())
            .or_default()
            .entry(state_key.to_string())
            .or_default()
            .insert(super::SINGLE_STATE_INDEX, value_json.to_string());
        versions
            .entry(key)
            .or_default()
            .insert(state_key.to_string(), 1);
        Ok(CasOutcome::Applied(1))
    }

    fn update_single_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
        expected_version: i64,
    ) -> MysqlResult<CasOutcome> {
        let key = Self::key(user_id, session_id);
        let mut rows = self.rows.lock().unwrap();
        let mut versions = self.versions.lock().unwrap();
        let present = rows
            .get(&key)
            .and_then(|s| s.get(state_key))
            .map(|items| items.contains_key(&super::SINGLE_STATE_INDEX))
            .unwrap_or(false);
        let current = versions
            .get(&key)
            .and_then(|v| v.get(state_key))
            .copied()
            .unwrap_or(0);
        if !present || current != expected_version {
            return Ok(CasOutcome::Conflict);
        }
        rows.entry(key.clone())
            .or_default()
            .entry(state_key.to_string())
            .or_default()
            .insert(super::SINGLE_STATE_INDEX, value_json.to_string());
        let next = expected_version + 1;
        versions
            .entry(key)
            .or_default()
            .insert(state_key.to_string(), next);
        Ok(CasOutcome::Applied(next))
    }

    fn get_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Option<String>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .get(&Self::key(user_id, session_id))
            .and_then(|s| s.get(state_key))
            .and_then(|m| m.get(&super::SINGLE_STATE_INDEX))
            .cloned())
    }

    fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        values_json: &[String],
        from_index: usize,
    ) -> MysqlResult<()> {
        self.list_writes.fetch_add(1, Ordering::SeqCst);
        let mut rows = self.rows.lock().unwrap();
        let session = rows.entry(Self::key(user_id, session_id)).or_default();
        let items = session.entry(state_key.to_string()).or_default();
        // Overwrite any positions the caller asked for (from_index == 0 rewrites everything,
        // a higher value overwrites the tail), then trim stale trailing rows.
        let overwrite_upto = from_index.min(values_json.len());
        for (offset, json) in values_json.iter().enumerate() {
            if offset < overwrite_upto || offset >= from_index {
                items.insert(offset as i64, json.clone());
            }
        }
        // Trim stale trailing rows so shrinking removes them.
        let total = values_json.len() as i64;
        let stale: Vec<i64> = items.keys().copied().filter(|i| *i >= total).collect();
        for idx in stale {
            items.remove(&idx);
        }
        Ok(())
    }

    fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Vec<String>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .get(&Self::key(user_id, session_id))
            .and_then(|s| s.get(state_key))
            .map(|items| items.values().cloned().collect())
            .unwrap_or_default())
    }

    fn list_len(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<usize> {
        Ok(self.get_list(user_id, session_id, state_key)?.len())
    }

    fn exists(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<bool> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .get(&Self::key(user_id, session_id))
            .map(|s| !s.is_empty())
            .unwrap_or(false))
    }

    fn delete(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<()> {
        let key = Self::key(user_id, session_id);
        self.rows.lock().unwrap().remove(&key);
        self.versions.lock().unwrap().remove(&key);
        Ok(())
    }

    fn list_session_ids(&self, user_id: Option<&str>) -> MysqlResult<Vec<String>> {
        let user = normalize_user(user_id);
        let rows = self.rows.lock().unwrap();
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for ((u, s), session) in rows.iter() {
            if *u == user && !session.is_empty() {
                ids.insert(s.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }

    fn read_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<i64> {
        let versions = self.versions.lock().unwrap();
        Ok(versions
            .get(&Self::key(user_id, session_id))
            .and_then(|v| v.get(state_key))
            .copied()
            .unwrap_or(0))
    }
}

/// Wrapper whose [`StateBackend::read_version`] always reports `0` (a stale reader).
///
/// Used to prove that [`super::MysqlAgentStateStore::save_if_version`] delegates the CAS to
/// the backend primitive instead of reading the version first: with a read-then-compare
/// implementation every `Some(n > 0)` write would be rejected here, while a genuine CAS
/// still succeeds because only the atomic primitive decides.
pub struct StaleVersionBackend {
    inner: InMemoryStateBackend,
}

impl StaleVersionBackend {
    /// Wraps a backend.
    pub fn new(inner: InMemoryStateBackend) -> Self {
        Self { inner }
    }
}

impl StateBackend for StaleVersionBackend {
    fn execute_ddl(&self, sql: &str) -> MysqlResult<()> {
        self.inner.execute_ddl(sql)
    }

    fn database_exists(&self, database_name: &str) -> MysqlResult<bool> {
        self.inner.database_exists(database_name)
    }

    fn table_exists(&self, database_name: &str, table_name: &str) -> MysqlResult<bool> {
        self.inner.table_exists(database_name, table_name)
    }

    fn table_columns(&self, database_name: &str, table_name: &str) -> MysqlResult<Vec<String>> {
        self.inner.table_columns(database_name, table_name)
    }

    fn save_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<()> {
        self.inner
            .save_single(user_id, session_id, state_key, value_json)
    }

    fn save_single_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<i64> {
        self.inner
            .save_single_versioned(user_id, session_id, state_key, value_json)
    }

    fn insert_single_if_absent(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<CasOutcome> {
        self.inner
            .insert_single_if_absent(user_id, session_id, state_key, value_json)
    }

    fn update_single_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
        expected_version: i64,
    ) -> MysqlResult<CasOutcome> {
        self.inner.update_single_if_version(
            user_id,
            session_id,
            state_key,
            value_json,
            expected_version,
        )
    }

    fn get_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Option<String>> {
        self.inner.get_single(user_id, session_id, state_key)
    }

    fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        values_json: &[String],
        from_index: usize,
    ) -> MysqlResult<()> {
        self.inner
            .save_list(user_id, session_id, state_key, values_json, from_index)
    }

    fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Vec<String>> {
        self.inner.get_list(user_id, session_id, state_key)
    }

    fn list_len(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<usize> {
        self.inner.list_len(user_id, session_id, state_key)
    }

    fn exists(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<bool> {
        self.inner.exists(user_id, session_id)
    }

    fn delete(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<()> {
        self.inner.delete(user_id, session_id)
    }

    fn list_session_ids(&self, user_id: Option<&str>) -> MysqlResult<Vec<String>> {
        self.inner.list_session_ids(user_id)
    }

    /// Deliberately stale: reports every key as unversioned.
    fn read_version(
        &self,
        _user_id: Option<&str>,
        _session_id: &str,
        _state_key: &str,
    ) -> MysqlResult<i64> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anon_and_named_users_are_separated() {
        let b = InMemoryStateBackend::default();
        b.save_single(None, "s", "k", "1").unwrap();
        assert!(b.get_single(Some("u"), "s", "k").unwrap().is_none());
        assert_eq!(b.get_single(None, "s", "k").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn versioned_save_starts_at_one_and_bumps() {
        let b = InMemoryStateBackend::default();
        assert_eq!(b.save_single_versioned(None, "s", "k", "1").unwrap(), 1);
        assert_eq!(b.save_single_versioned(None, "s", "k", "2").unwrap(), 2);
        assert_eq!(b.read_version(None, "s", "k").unwrap(), 2);
    }

    #[test]
    fn unversioned_hash_row_does_not_touch_version() {
        let b = InMemoryStateBackend::default();
        b.save_single_versioned(None, "s", "k", "1").unwrap();
        b.save_single(None, "s", "k:_hash", "abc").unwrap();
        assert_eq!(b.read_version(None, "s", "k").unwrap(), 1);
        assert_eq!(b.read_version(None, "s", "k:_hash").unwrap(), 0);
    }

    #[test]
    fn create_if_absent_conflicts_when_row_exists() {
        let b = InMemoryStateBackend::default();
        assert_eq!(
            b.insert_single_if_absent(None, "s", "k", "1").unwrap(),
            CasOutcome::Applied(1)
        );
        assert_eq!(
            b.insert_single_if_absent(None, "s", "k", "2").unwrap(),
            CasOutcome::Conflict
        );
        assert_eq!(b.get_single(None, "s", "k").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn cas_update_requires_matching_version() {
        let b = InMemoryStateBackend::default();
        assert_eq!(
            b.update_single_if_version(None, "s", "k", "x", 0).unwrap(),
            CasOutcome::Conflict,
            "a missing row must not be created by CAS"
        );
        b.save_single_versioned(None, "s", "k", "1").unwrap();
        assert_eq!(
            b.update_single_if_version(None, "s", "k", "x", 99).unwrap(),
            CasOutcome::Conflict
        );
        assert_eq!(
            b.update_single_if_version(None, "s", "k", "2", 1).unwrap(),
            CasOutcome::Applied(2)
        );
    }

    #[test]
    fn save_list_from_zero_rewrites_every_slot() {
        let b = InMemoryStateBackend::default();
        b.save_list(None, "s", "l", &["1".into(), "2".into(), "3".into()], 3)
            .unwrap();
        b.save_list(None, "s", "l", &["9".into(), "8".into()], 0)
            .unwrap();
        assert_eq!(b.get_list(None, "s", "l").unwrap(), vec!["9", "8"]);
    }

    // ---- modelled schema (INFORMATION_SCHEMA stand-in) ----------------------------

    #[test]
    fn create_statements_update_the_modelled_schema() {
        let b = InMemoryStateBackend::default();
        assert!(!b.database_exists("db").unwrap());
        b.execute_ddl(&super::super::create_database_sql("db")).unwrap();
        assert!(b.database_exists("db").unwrap());
        assert!(!b.table_exists("db", "t").unwrap());

        b.execute_ddl(&super::super::create_sessions_table_sql("db", "t"))
            .unwrap();
        assert!(b.table_exists("db", "t").unwrap());
        assert_eq!(
            b.table_columns("db", "t").unwrap(),
            vec![
                "session_id",
                "state_key",
                "item_index",
                "state_data",
                "version",
                "created_at",
                "updated_at"
            ]
        );
    }

    /// `CREATE TABLE` into an unknown database must fail, exactly like MySQL — this is what
    /// proves the store issues `CREATE DATABASE` first.
    #[test]
    fn create_table_requires_the_database_first() {
        let b = InMemoryStateBackend::default();
        let err = b
            .execute_ddl(&super::super::create_sessions_table_sql("missing_db", "t"))
            .unwrap_err();
        assert!(err.to_string().contains("Unknown database"), "{err}");
        assert!(b.ddl_log().is_empty(), "failed DDL is not recorded");
    }

    #[test]
    fn alter_adds_the_version_column_once() {
        let b = InMemoryStateBackend::default();
        b.create_legacy_table("db", "t");
        assert!(!b.columns("db", "t").iter().any(|c| c == "version"));

        let sql = super::super::add_version_column_sql("db", "t");
        b.execute_ddl(&sql).unwrap();
        assert!(b.columns("db", "t").iter().any(|c| c == "version"));

        // Idempotence is the store's job (guarded by the column probe): the fake mirrors
        // MySQL and does not add a duplicate column either.
        b.execute_ddl(&sql).unwrap();
        assert_eq!(
            b.columns("db", "t").iter().filter(|c| *c == "version").count(),
            1
        );
        assert_eq!(b.ddl_log(), vec![sql.clone(), sql]);
    }

    #[test]
    fn alter_on_an_unknown_table_fails() {
        let b = InMemoryStateBackend::default();
        b.create_database("db");
        let err = b
            .execute_ddl(&super::super::add_version_column_sql("db", "nope"))
            .unwrap_err();
        assert!(err.to_string().contains("Unknown table"), "{err}");
    }
}
