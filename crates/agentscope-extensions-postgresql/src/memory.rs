//! An in-memory *simulator* of the subset of PostgreSQL SQL used by this crate.
//!
//! This client exists so the crate's stores can be unit-tested without a live
//! database. It recognises the exact statement shapes emitted by [`crate::store`],
//! [`crate::state`], and [`crate::snapshot`] (dispatched on a normalised form of the
//! SQL text) and simulates table state in `HashMap`s, including `ON CONFLICT`
//! upserts, conditional CAS updates, `LIKE ... ESCAPE '!'` prefix search, and
//! advisory locks.
//!
//! It is intentionally **not** a general SQL engine; it only implements the
//! behaviour the crate depends on.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::client::{Cell, PostgresClient, QueryOutcome, Row, SqlValue};
use crate::error::{PgResult, PostgresError};

/// A row in the session-state table.
#[derive(Debug, Clone)]
struct SessionRow {
    state_data: String,
    version: i64,
    created_at: i64,
    updated_at: i64,
}

/// A row in the KV store table.
#[derive(Debug, Clone)]
struct StoreRow {
    value_json: String,
    version: i64,
    updated_at: i64,
}

#[derive(Debug, Default)]
struct Tables {
    /// (session_id, state_key, item_index) -> row
    sessions: HashMap<(String, String, i32), SessionRow>,
    /// (namespace_path, item_key) -> row
    store: HashMap<(String, String), StoreRow>,
    /// snapshot_id -> data
    snapshots: HashMap<String, Vec<u8>>,
    /// Advisory locks held, keyed by lock name. Value is a token so the same
    /// connection can re-acquire.
    advisory_locks: HashMap<i64, String>,
    /// Schemas created via `CREATE SCHEMA IF NOT EXISTS "x"`.
    schemas: HashSet<String>,
    /// Unqualified names of tables created via `CREATE TABLE IF NOT EXISTS`.
    created_tables: HashSet<String>,
    /// Every DDL statement observed through `execute_batch`, in order.
    ddl_statements: Vec<String>,
}

/// In-memory [`PostgresClient`] used for tests.
#[derive(Debug, Clone, Default)]
pub struct MemoryClient {
    tables: Arc<Mutex<Tables>>,
    /// A unique token representing this client's "connection" for advisory locks.
    conn_token: String,
}

impl MemoryClient {
    pub fn new() -> Self {
        Self {
            tables: Arc::new(Mutex::new(Tables::default())),
            conn_token: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Creates a *separate* client over the *same* tables (simulating a second
    /// connection/JVM): useful for CAS and lock concurrency tests.
    pub fn fork(&self) -> Self {
        Self {
            tables: Arc::clone(&self.tables),
            conn_token: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Returns the number of rows in the session table.
    pub fn session_row_count(&self) -> usize {
        self.tables.lock().unwrap().sessions.len()
    }

    /// Returns the number of rows in the KV store table.
    pub fn store_row_count(&self) -> usize {
        self.tables.lock().unwrap().store.len()
    }

    /// Returns the number of snapshot rows currently stored.
    pub fn snapshot_row_count(&self) -> usize {
        self.tables.lock().unwrap().snapshots.len()
    }

    /// Returns the number of advisory locks currently held.
    pub fn advisory_lock_count(&self) -> usize {
        self.tables.lock().unwrap().advisory_locks.len()
    }

    /// Whether the given schema was created via `CREATE SCHEMA IF NOT EXISTS`.
    pub fn has_schema(&self, schema: &str) -> bool {
        self.tables.lock().unwrap().schemas.contains(schema)
    }

    /// Whether a table with the given unqualified name was created.
    pub fn has_table(&self, table: &str) -> bool {
        self.tables
            .lock()
            .unwrap()
            .created_tables
            .contains(&table.to_lowercase())
    }

    /// Whether the default session-state table was created via `CREATE TABLE IF NOT EXISTS`.
    pub fn session_table_initialized(&self) -> bool {
        self.has_table("agentscope_sessions")
    }

    /// Every DDL statement observed through [`PostgresClient::execute_batch`], in order.
    pub fn ddl_statements(&self) -> Vec<String> {
        self.tables.lock().unwrap().ddl_statements.clone()
    }

    fn normalize(sql: &str) -> String {
        // Collapse whitespace, uppercase, and drop identifier quotes so that
        // schema-qualified names (`"agentscope"."agentscope_sessions"`) normalise to
        // `AGENTSCOPE.AGENTSCOPE_SESSIONS` and can be matched by column signature.
        sql.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase()
            .replace('"', "")
    }

    fn param<'a>(params: &'a [SqlValue], idx: usize) -> &'a SqlValue {
        params.get(idx).unwrap_or(&SqlValue::Null)
    }

    fn text_of(params: &[SqlValue], idx: usize) -> String {
        match Self::param(params, idx) {
            SqlValue::Text(s) => s.clone(),
            other => format!("{other:?}"),
        }
    }

    fn i64_of(params: &[SqlValue], idx: usize) -> i64 {
        match Self::param(params, idx) {
            SqlValue::BigInt(v) => *v,
            SqlValue::Int(v) => *v as i64,
            _ => 0,
        }
    }

    fn i32_of(params: &[SqlValue], idx: usize) -> i32 {
        match Self::param(params, idx) {
            SqlValue::Int(v) => *v,
            SqlValue::BigInt(v) => *v as i32,
            _ => 0,
        }
    }

    fn like_escape_match(value: &str, pattern: &str) -> bool {
        // Pattern supports `!` escaping of `!`, `%`, `_`, and a trailing `%` wildcard.
        let pat: Vec<char> = pattern.chars().collect();
        let val: Vec<char> = value.chars().collect();
        Self::like_match_from(&pat, 0, &val, 0)
    }

    fn like_match_from(pat: &[char], mut pi: usize, val: &[char], mut vi: usize) -> bool {
        while pi < pat.len() {
            match pat[pi] {
                '!' => {
                    // Escaped literal next char.
                    pi += 1;
                    if pi >= pat.len() {
                        return false;
                    }
                    let lit = pat[pi];
                    if vi >= val.len() || val[vi] != lit {
                        return false;
                    }
                    vi += 1;
                    pi += 1;
                }
                '%' => {
                    // Match zero or more chars (greedy, backtrack).
                    let rest = &pat[pi + 1..];
                    for skip in vi..=val.len() {
                        if Self::like_match_from(rest, 0, val, skip) {
                            return true;
                        }
                    }
                    return false;
                }
                '_' => {
                    if vi >= val.len() {
                        return false;
                    }
                    vi += 1;
                    pi += 1;
                }
                c => {
                    if vi >= val.len() || val[vi] != c {
                        return false;
                    }
                    vi += 1;
                    pi += 1;
                }
            }
        }
        vi == val.len()
    }
}

impl PostgresClient for MemoryClient {
    fn query(&self, sql: &str, params: &[SqlValue]) -> PgResult<QueryOutcome> {
        let n = Self::normalize(sql);
        let mut t = self.tables.lock().unwrap();

        // ---- advisory locks ----
        if n.starts_with("SELECT PG_TRY_ADVISORY_LOCK") {
            let key = Self::i64_of(params, 0);
            match t.advisory_locks.get(&key) {
                None => {
                    t.advisory_locks.insert(key, self.conn_token.clone());
                    return Ok(QueryOutcome::Rows(vec![vec![Cell::Bool(true)]]));
                }
                Some(owner) if owner == &self.conn_token => {
                    return Ok(QueryOutcome::Rows(vec![vec![Cell::Bool(true)]]));
                }
                _ => return Ok(QueryOutcome::Rows(vec![vec![Cell::Bool(false)]])),
            }
        }
        if n.starts_with("SELECT PG_ADVISORY_UNLOCK") {
            let key = Self::i64_of(params, 0);
            match t.advisory_locks.get(&key).cloned() {
                Some(owner) if owner == self.conn_token => {
                    t.advisory_locks.remove(&key);
                    return Ok(QueryOutcome::Rows(vec![vec![Cell::Bool(true)]]));
                }
                _ => return Ok(QueryOutcome::Rows(vec![vec![Cell::Bool(false)]])),
            }
        }

        // ---- session state: upsert (save / save_hash) ----
        // Dispatched by the `session_id` column so schema qualification / custom table
        // names don't defeat recognition.
        if n.starts_with("INSERT INTO") && n.contains("SESSION_ID") {
            let session_id = Self::text_of(params, 0);
            let key = Self::text_of(params, 1);
            let index = Self::i32_of(params, 2);
            let data = Self::text_of(params, 3);
            let slot = (session_id, key, index);
            let now = chrono::Utc::now().timestamp_millis();
            if n.contains("DO NOTHING") {
                if t.sessions.contains_key(&slot) {
                    return Ok(QueryOutcome::Affected(0));
                }
                t.sessions.insert(
                    slot,
                    SessionRow { state_data: data, version: 1, created_at: now, updated_at: now },
                );
                return Ok(QueryOutcome::Affected(1));
            }
            // Regular upsert: version = existing + 1 (or 1 on insert).
            match t.sessions.get_mut(&slot) {
                Some(row) => {
                    row.state_data = data;
                    row.version += 1;
                    row.updated_at = now;
                }
                None => {
                    t.sessions.insert(
                        slot,
                        SessionRow { state_data: data, version: 1, created_at: now, updated_at: now },
                    );
                }
            }
            return Ok(QueryOutcome::Affected(1));
        }

        // ---- session state: conditional CAS update ----
        if n.starts_with("UPDATE") && n.contains("STATE_DATA") {
            // UPDATE <table> SET state_data = ?, version = ?, updated_at = ?
            //   WHERE session_id = ? AND state_key = ? AND item_index = ? AND version = ?
            let data = Self::text_of(params, 0);
            let new_version = Self::i64_of(params, 1);
            let session_id = Self::text_of(params, 3);
            let key = Self::text_of(params, 4);
            let index = Self::i32_of(params, 5);
            let expected = Self::i64_of(params, 6);
            let slot = (session_id, key, index);
            let now = chrono::Utc::now().timestamp_millis();
            match t.sessions.get_mut(&slot) {
                Some(row) if row.version == expected => {
                    row.state_data = data;
                    row.version = new_version;
                    row.updated_at = now;
                    return Ok(QueryOutcome::Affected(1));
                }
                _ => return Ok(QueryOutcome::Affected(0)),
            }
        }

        // ---- INFORMATION_SCHEMA.SCHEMATA existence check ----
        if n.starts_with("SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA") {
            let schema = Self::text_of(params, 0);
            return Ok(if t.schemas.contains(&schema) {
                QueryOutcome::Rows(vec![vec![Cell::Text(schema)]])
            } else {
                QueryOutcome::Rows(vec![])
            });
        }

        // ---- INFORMATION_SCHEMA.TABLES existence check ----
        if n.starts_with("SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES") {
            let schema = Self::text_of(params, 0);
            let table = Self::text_of(params, 1);
            let found = t.schemas.contains(&schema)
                && t.created_tables.contains(&table.to_lowercase());
            return Ok(if found {
                QueryOutcome::Rows(vec![vec![Cell::Text(table)]])
            } else {
                QueryOutcome::Rows(vec![])
            });
        }

        // ---- session state: SELECT single/target row ----
        if n.starts_with("SELECT") && n.contains("SESSION_ID") {
            if n.contains("MAX(ITEM_INDEX)") {
                let session_id = Self::text_of(params, 0);
                let key = Self::text_of(params, 1);
                let max = t
                    .sessions
                    .iter()
                    .filter(|((s, k, _), _)| s == &session_id && k == &key)
                    .map(|((_, _, i), _)| *i)
                    .max();
                return Ok(QueryOutcome::Rows(vec![vec![match max {
                    Some(i) => Cell::Int(i),
                    None => Cell::Null,
                }]]));
            }
            if n.contains("SELECT DISTINCT SESSION_ID") {
                let pattern = Self::text_of(params, 0);
                let mut ids: Vec<String> = t
                    .sessions
                    .keys()
                    .map(|(s, _, _)| s.clone())
                    .filter(|s| Self::like_escape_match(s, &pattern))
                    .collect();
                ids.sort();
                ids.dedup();
                return Ok(QueryOutcome::Rows(
                    ids.into_iter().map(|id| vec![Cell::Text(id)]).collect(),
                ));
            }
            if n.contains("SELECT 1") {
                let session_id = Self::text_of(params, 0);
                let exists = t.sessions.keys().any(|(s, _, _)| s == &session_id);
                return Ok(QueryOutcome::Rows(if exists {
                    vec![vec![Cell::Int(1)]]
                } else {
                    vec![]
                }));
            }

            // Point lookup: session_id = ?, [state_key = ?, item_index = ?]
            let session_id = Self::text_of(params, 0);
            let key = if params.len() > 1 { Some(Self::text_of(params, 1)) } else { None };
            let index = if params.len() > 2 { Some(Self::i32_of(params, 2)) } else { None };
            let want_version = n.contains("VERSION") && !n.contains("STATE_DATA");
            let want_data_only = n.contains("STATE_DATA") && !n.contains("VERSION");
            let mut rows: Vec<Row> = Vec::new();
            let mut matched: Vec<(&(String, String, i32), &SessionRow)> = t
                .sessions
                .iter()
                .filter(|((s, k, i), _)| {
                    s == &session_id
                        && key.as_ref().map(|kk| kk == k).unwrap_or(true)
                        && index.map(|ii| ii == *i).unwrap_or(true)
                })
                .collect();
            matched.sort_by_key(|((_, k, i), _)| (k.clone(), *i));
            for (_, row) in matched {
                if want_version {
                    rows.push(vec![Cell::BigInt(row.version)]);
                } else if want_data_only {
                    rows.push(vec![Cell::Text(row.state_data.clone())]);
                } else {
                    rows.push(vec![Cell::Text(row.state_data.clone()), Cell::BigInt(row.version)]);
                }
            }
            return Ok(QueryOutcome::Rows(rows));
        }

        // ---- session state: DELETE ----
        if n.starts_with("DELETE FROM") && n.contains("SESSION_ID") {
            let session_id = Self::text_of(params, 0);
            if n.contains("STATE_KEY") {
                let key = Self::text_of(params, 1);
                let before = t.sessions.len();
                t.sessions.retain(|(s, k, _), _| !(s == &session_id && k == &key));
                return Ok(QueryOutcome::Affected((before - t.sessions.len()) as u64));
            }
            let before = t.sessions.len();
            t.sessions.retain(|(s, _, _), _| s != &session_id);
            return Ok(QueryOutcome::Affected((before - t.sessions.len()) as u64));
        }

        // ---- session state: TRUNCATE ----
        if n.starts_with("TRUNCATE") && n.contains("AGENTSCOPE_SESSIONS") {
            t.sessions.clear();
            return Ok(QueryOutcome::Affected(0));
        }

        // ---- KV store: get ----
        if n.starts_with("SELECT VALUE_JSON, VERSION FROM")
            && (n.contains("AGENTSCOPE_STORE") || n.contains("NAMESPACE_PATH"))
        {
            let ns = Self::text_of(params, 0);
            let key = Self::text_of(params, 1);
            return Ok(match t.store.get(&(ns, key)) {
                Some(row) => QueryOutcome::Rows(vec![vec![
                    Cell::Text(row.value_json.clone()),
                    Cell::BigInt(row.version),
                ]]),
                None => QueryOutcome::Rows(vec![]),
            });
        }

        // ---- KV store: search (LIKE prefix) ----
        if n.starts_with("SELECT ITEM_KEY, VALUE_JSON, VERSION FROM") {
            let pattern = Self::text_of(params, 0);
            let limit = Self::i64_of(params, 1).max(0) as usize;
            let offset = Self::i64_of(params, 2).max(0) as usize;
            let mut hits: Vec<(String, String, i64)> = t
                .store
                .iter()
                .filter(|((ns, _), _)| Self::like_escape_match(ns, &pattern))
                .map(|((_, k), row)| (k.clone(), row.value_json.clone(), row.version))
                .collect();
            hits.sort_by(|a, b| a.0.cmp(&b.0));
            let rows = hits
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|(k, v, ver)| vec![Cell::Text(k), Cell::Text(v), Cell::BigInt(ver)])
                .collect();
            return Ok(QueryOutcome::Rows(rows));
        }

        // ---- KV store: upsert / insert ----
        if n.starts_with("INSERT INTO") && (n.contains("AGENTSCOPE_STORE") || n.contains("NAMESPACE_PATH")) {
            let ns = Self::text_of(params, 0);
            let key = Self::text_of(params, 1);
            let value = Self::text_of(params, 2);
            let now = Self::i64_of(params, 3);
            let slot = (ns, key);
            if n.contains("DO NOTHING") {
                if t.store.contains_key(&slot) {
                    return Ok(QueryOutcome::Affected(0));
                }
                t.store.insert(slot, StoreRow { value_json: value, version: 1, updated_at: now });
                return Ok(QueryOutcome::Affected(1));
            }
            if n.contains("ON CONFLICT") {
                match t.store.get_mut(&slot) {
                    Some(row) => {
                        row.value_json = value;
                        row.version += 1;
                        row.updated_at = now;
                    }
                    None => {
                        t.store.insert(slot, StoreRow { value_json: value, version: 1, updated_at: now });
                    }
                }
                return Ok(QueryOutcome::Affected(1));
            }
            // Plain insert (CAS expected_version == 0): fail on duplicate.
            if t.store.contains_key(&slot) {
                return Err(PostgresError::Query(
                    "duplicate key value violates unique constraint (SQLSTATE 23505)".into(),
                ));
            }
            t.store.insert(slot, StoreRow { value_json: value, version: 1, updated_at: now });
            return Ok(QueryOutcome::Affected(1));
        }

        // ---- KV store: CAS update ----
        if n.starts_with("UPDATE") && (n.contains("AGENTSCOPE_STORE") || n.contains("NAMESPACE_PATH")) {
            // UPDATE <t> SET value_json = ?, version = version + 1, updated_at = ?
            //   WHERE namespace_path = ? AND item_key = ? AND version = ?
            let value = Self::text_of(params, 0);
            let now = Self::i64_of(params, 1);
            let ns = Self::text_of(params, 2);
            let key = Self::text_of(params, 3);
            let expected = Self::i64_of(params, 4);
            match t.store.get_mut(&(ns, key)) {
                Some(row) if row.version == expected => {
                    row.value_json = value;
                    row.version += 1;
                    row.updated_at = now;
                    return Ok(QueryOutcome::Affected(1));
                }
                _ => return Ok(QueryOutcome::Affected(0)),
            }
        }

        // ---- KV store: delete ----
        if n.starts_with("DELETE FROM") && (n.contains("AGENTSCOPE_STORE") || n.contains("NAMESPACE_PATH")) {
            let ns = Self::text_of(params, 0);
            let key = Self::text_of(params, 1);
            let before = t.store.len();
            t.store.remove(&(ns, key));
            return Ok(QueryOutcome::Affected((before - t.store.len()) as u64));
        }

        // ---- snapshots: upsert ----
        // Dispatched by the `snapshot_id` column so custom table names are recognised.
        if n.starts_with("INSERT INTO") && n.contains("SNAPSHOT_ID") {
            let id = Self::text_of(params, 0);
            let data = match Self::param(params, 1) {
                SqlValue::Bytes(b) => b.clone(),
                other => format!("{other:?}").into_bytes(),
            };
            t.snapshots.insert(id, data);
            return Ok(QueryOutcome::Affected(1));
        }

        // ---- snapshots: download ----
        if n.starts_with("SELECT DATA FROM") && n.contains("SNAPSHOT_ID") {
            let id = Self::text_of(params, 0);
            return Ok(match t.snapshots.get(&id) {
                Some(data) => QueryOutcome::Rows(vec![vec![Cell::Bytes(data.clone())]]),
                None => QueryOutcome::Rows(vec![]),
            });
        }

        // ---- snapshots: exists ----
        if n.starts_with("SELECT 1 FROM") && n.contains("SNAPSHOT_ID") {
            let id = Self::text_of(params, 0);
            let exists = t.snapshots.contains_key(&id);
            return Ok(QueryOutcome::Rows(if exists {
                vec![vec![Cell::Int(1)]]
            } else {
                vec![]
            }));
        }

        Err(PostgresError::Query(format!("MemoryClient: unrecognised SQL: {sql}")))
    }

    fn execute_batch(&self, sql: &str) -> PgResult<()> {
        // DDL is simulated: schema/table creation statements are recorded so the
        // INFORMATION_SCHEMA checks above can observe them.
        let mut t = self.tables.lock().unwrap();
        for stmt in sql.split(';') {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            t.ddl_statements.push(trimmed.to_string());
            let upper = trimmed.to_uppercase();
            if upper.starts_with("CREATE SCHEMA IF NOT EXISTS") {
                if let Some(name) = Self::first_quoted(trimmed) {
                    t.schemas.insert(name);
                }
            } else if upper.starts_with("CREATE TABLE IF NOT EXISTS") {
                if let Some(qualified) = Self::ddl_target(trimmed) {
                    let schema = qualified
                        .find('.')
                        .map(|i| qualified[..i].trim().trim_matches('"').to_string());
                    let table = qualified
                        .rsplit('.')
                        .next()
                        .unwrap_or(&qualified)
                        .trim_matches('"')
                        .to_string();
                    if let Some(schema) = schema {
                        t.schemas.insert(schema);
                    }
                    t.created_tables.insert(table.to_lowercase());
                }
            } else if upper.starts_with("ALTER TABLE")
                && upper.contains("ADD COLUMN IF NOT EXISTS VERSION")
            {
                // Simulated no-op: exercises the ensure-version-column path.
            } else if upper.starts_with("TRUNCATE") && upper.contains("AGENTSCOPE_SESSIONS") {
                t.sessions.clear();
            }
        }
        Ok(())
    }
}

impl MemoryClient {
    /// Returns the first double-quoted token in a statement, if any.
    fn first_quoted(statement: &str) -> Option<String> {
        let start = statement.find('"')?;
        let rest = &statement[start + 1..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    }

    /// Extracts the target name of a `CREATE TABLE IF NOT EXISTS <target>` statement.
    fn ddl_target(statement: &str) -> Option<String> {
        let upper = statement.to_uppercase();
        let idx = upper.find("IF NOT EXISTS")? + "IF NOT EXISTS".len();
        let rest = statement[idx..].trim_start();
        let end = rest.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(rest.len());
        let target = rest[..end].trim();
        if target.is_empty() {
            None
        } else {
            Some(target.to_string())
        }
    }
}
