//! A minimal, driver-agnostic SQL seam.
//!
//! The Java repositories depend on `javax.sql.DataSource` / JDBC. Rust has no single
//! equivalent, and the crate must build & test **without a live database**, so this module
//! defines the smallest seam that carries the behaviour the repositories need:
//!
//! * parameterized statements (no string interpolation of user data);
//! * multi-statement transactions with an explicit `commit` / `rollback`;
//! * result rows with typed column access.
//!
//! [`InMemorySqlDatabase`] is a dependency-free implementation used by the unit tests and
//! by embedders that want a scratch skill store. A real MySQL / PostgreSQL driver can be
//! layered on behind a feature flag (see tracker §4.2) without touching repository logic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::SkillRepositoryError;

/// A single bound SQL parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    /// SQL `NULL`.
    Null,
    /// Integer value (maps to `BIGINT` / `BIGSERIAL` ids).
    Int(i64),
    /// Text value (`VARCHAR` / `TEXT` / `LONGTEXT` / `metadata_json`).
    Text(String),
}

impl SqlParam {
    /// Materialises a text value (`NULL` when `None`).
    pub fn text(v: Option<impl Into<String>>) -> Self {
        match v {
            Some(s) => SqlParam::Text(s.into()),
            None => SqlParam::Null,
        }
    }
}

/// A column value read back from a result row.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    /// SQL `NULL`.
    Null,
    /// Integer value.
    Int(i64),
    /// Text value.
    Text(String),
}

impl SqlValue {
    /// Returns the text representation, or `None` for `NULL`.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            SqlValue::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Returns the integer value, coercing text digits when necessary.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            SqlValue::Int(i) => Some(*i),
            SqlValue::Text(s) => s.parse().ok(),
            SqlValue::Null => None,
        }
    }
}

/// A result row addressed by column name.
#[derive(Debug, Clone, Default)]
pub struct SqlRow {
    columns: HashMap<String, SqlValue>,
}

impl SqlRow {
    /// Builds a row from `(column, value)` pairs.
    pub fn new<I, K>(cells: I) -> Self
    where
        I: IntoIterator<Item = (K, SqlValue)>,
        K: Into<String>,
    {
        let mut columns = HashMap::new();
        for (k, v) in cells {
            columns.insert(k.into(), v);
        }
        Self { columns }
    }

    /// Fetches a column value, or [`SqlValue::Null`] when absent.
    pub fn get(&self, column: &str) -> SqlValue {
        self.columns.get(column).cloned().unwrap_or(SqlValue::Null)
    }

    /// Fetches a text column, or `None` when `NULL`/absent.
    pub fn get_text(&self, column: &str) -> Option<String> {
        self.get(column).as_text().map(|s| s.to_string())
    }

    /// Fetches an integer column, or `None` when `NULL`/absent.
    pub fn get_int(&self, column: &str) -> Option<i64> {
        self.get(column).as_int()
    }
}

/// A connection capable of running parameterized statements.
///
/// Implementations are expected to hold the transaction state that
/// [`Self::set_auto_commit`] toggles; results are materialised eagerly so no cursor
/// lifetime crosses the trait boundary.
pub trait SqlConnection: Send {
    /// Runs a statement and returns the affected row count.
    fn execute(&mut self, sql: &str, params: &[SqlParam]) -> Result<usize, SkillRepositoryError>;

    /// Runs a query and returns all rows.
    fn query(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<SqlRow>, SkillRepositoryError>;

    /// Runs an insert and returns the generated primary key.
    fn insert_returning_id(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<i64, SkillRepositoryError>;

    /// Runs the same statement once per parameter set and returns the total affected rows.
    ///
    /// Mirrors JDBC `addBatch` / `executeBatch`: drivers that support batching override this to
    /// send the whole slice in one round trip. The default implementation (used by
    /// [`InMemoryConnection`]) just loops over [`Self::execute`], so behaviour is identical and
    /// only the number of round trips differs. The PostgreSQL repository chunks resource rows by
    /// `RESOURCE_BATCH_SIZE` so a skill with thousands of files never exceeds the driver's
    /// bind-parameter limit.
    fn execute_batch(
        &mut self,
        sql: &str,
        params: &[Vec<SqlParam>],
    ) -> Result<usize, SkillRepositoryError> {
        let mut affected = 0;
        for row in params {
            affected += self.execute(sql, row)?;
        }
        Ok(affected)
    }

    /// Toggles auto-commit (transaction boundaries).
    fn set_auto_commit(&mut self, enabled: bool) -> Result<(), SkillRepositoryError>;

    /// Commits the current transaction.
    fn commit(&mut self) -> Result<(), SkillRepositoryError>;

    /// Rolls back the current transaction.
    fn rollback(&mut self) -> Result<(), SkillRepositoryError>;

    /// Runs `work` inside a transaction: auto-commit off, `commit` on success, `rollback` on
    /// failure, auto-commit restored either way.
    ///
    /// This is the seam-level equivalent of the Java `setAutoCommit(false)` + `commit` /
    /// `rollback` pattern (`executeInWriteTransaction`) that both SQL repositories wrap around
    /// multi-statement writes. Without it a failure halfway through a write leaves orphaned rows
    /// behind - e.g. the resource rows survive when the skill row delete fails.
    fn in_transaction<T>(
        &mut self,
        work: impl FnOnce(&mut Self) -> Result<T, SkillRepositoryError>,
    ) -> Result<T, SkillRepositoryError>
    where
        Self: Sized,
    {
        self.set_auto_commit(false)?;
        match work(self) {
            Ok(value) => match self.commit() {
                Ok(()) => {
                    self.set_auto_commit(true)?;
                    Ok(value)
                }
                Err(e) => {
                    let _ = self.rollback();
                    let _ = self.set_auto_commit(true);
                    Err(e)
                }
            },
            Err(e) => {
                let _ = self.rollback();
                let _ = self.set_auto_commit(true);
                Err(e)
            }
        }
    }
}

/// A source of connections, mirroring `javax.sql.DataSource`.
pub trait SqlDatabase: Send + Sync {
    /// The boxed connection type handed out by this database.
    type Connection: SqlConnection;

    /// Obtains a fresh connection.
    fn connection(&self) -> Result<Self::Connection, SkillRepositoryError>;
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

/// An in-memory table: an auto-increment counter plus stored rows.
#[derive(Debug, Default, Clone)]
struct InMemoryTable {
    next_id: i64,
    rows: Vec<HashMap<String, SqlValue>>,
}

/// In-memory database state shared across connections.
#[derive(Debug, Default)]
struct InMemoryState {
    /// Table name -> table.
    tables: HashMap<String, InMemoryTable>,
    /// Table name -> declared `metadata_json` column type (absence = no such column).
    metadata_columns: HashMap<String, String>,
    /// Databases / schemas registered through `CREATE DATABASE` / `CREATE SCHEMA`.
    schemas: std::collections::HashSet<String>,
}

/// A dependency-free [`SqlDatabase`] backed by process memory.
///
/// The implementation understands just enough SQL for the skill repositories: `CREATE TABLE`,
/// `SELECT ... FROM`, `INSERT`, `DELETE`, and the `INFORMATION_SCHEMA` existence probes. It is
/// **not** a general SQL engine — it is a deterministic test double and scratch store.
#[derive(Clone)]
pub struct InMemorySqlDatabase {
    state: Arc<Mutex<InMemoryState>>,
}

impl Default for InMemorySqlDatabase {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySqlDatabase {
    /// Creates an empty in-memory database.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryState::default())),
        }
    }

    /// Pre-registers a table with an explicit id column and optional `metadata_json` support.
    /// Mostly used to emulate a legacy schema lacking `metadata_json`.
    pub fn register_table(&self, table: &str, has_metadata: bool) {
        let mut st = self.state.lock().unwrap();
        st.tables.entry(table.to_string()).or_default();
        if has_metadata {
            // MySQL does not inspect the type; PostgreSQL accepts `text`.
            st.metadata_columns
                .insert(table.to_string(), "text".to_string());
        }
    }

    /// Returns `true` when the database/schema was created (or a table made it implicit).
    pub fn schema_exists(&self, schema: &str) -> bool {
        let st = self.state.lock().unwrap();
        st.schemas.contains(schema)
    }

    /// The declared `metadata_json` column type of a table, if the column exists.
    pub fn metadata_column_type(&self, table: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .metadata_columns
            .get(table)
            .cloned()
    }

    /// Returns `true` when the table exists.
    pub fn table_exists(&self, table: &str) -> bool {
        self.state.lock().unwrap().tables.contains_key(table)
    }

    /// Returns the stored rows of a table (test inspection helper).
    pub fn rows(&self, table: &str) -> Vec<HashMap<String, SqlValue>> {
        self.state
            .lock()
            .unwrap()
            .tables
            .get(table)
            .map(|t| t.rows.clone())
            .unwrap_or_default()
    }

    // P2-10: `has_metadata_column()` was removed - it had no callers anywhere in the workspace and
    // is exactly `metadata_column_type(table).is_some()`; keeping both invites drift.
}

impl SqlDatabase for InMemorySqlDatabase {
    type Connection = InMemoryConnection;

    fn connection(&self) -> Result<Self::Connection, SkillRepositoryError> {
        Ok(InMemoryConnection {
            state: self.state.clone(),
            auto_commit: true,
            staged: Vec::new(),
        })
    }
}

/// Connection over [`InMemorySqlDatabase`]. Buffers writes until `commit` when auto-commit
/// is off, giving real transaction semantics for the `force=false` pre-check + rollback path.
pub struct InMemoryConnection {
    state: Arc<Mutex<InMemoryState>>,
    auto_commit: bool,
    staged: Vec<StagedWrite>,
}

enum StagedWrite {
    Insert {
        table: String,
        row: HashMap<String, SqlValue>,
        id: i64,
    },
    Delete {
        table: String,
        column: String,
        value: SqlValue,
    },
}

impl InMemoryConnection {
    fn parse_table_token(sql: &str) -> String {
        // Handles `FROM "schema"."table"`, `FROM db.table`, `FROM table`,
        // `CREATE TABLE [IF NOT EXISTS] ...`, `CREATE [DATABASE|SCHEMA] [IF NOT EXISTS] ...`
        // and `INSERT INTO ...`. The token keeps its dotted prefix / quotes.
        let lower = sql.to_ascii_lowercase();
        let skip_if_not_exists = |after: usize, lower: &str| {
            let tail = &lower[after..];
            match tail
                .strip_prefix("if not exists ")
                .or_else(|| tail.strip_prefix("if not exists"))
            {
                Some(stripped) => after + (tail.len() - stripped.len()),
                None => after,
            }
        };
        let marker = if let Some(i) = lower.find(" from ") {
            i + " from ".len()
        } else if let Some(i) = lower.find("create database ") {
            skip_if_not_exists(i + "create database ".len(), &lower)
        } else if let Some(i) = lower.find("create table ") {
            skip_if_not_exists(i + "create table ".len(), &lower)
        } else if let Some(i) = lower.find("create schema ") {
            skip_if_not_exists(i + "create schema ".len(), &lower)
        } else if let Some(i) = lower.find("insert into ") {
            i + "insert into ".len()
        } else {
            0
        };
        let rest = &sql[marker..];
        rest.chars()
            .take_while(|c| !c.is_whitespace() && *c != '(' && *c != ';')
            .collect()
    }

    fn parse_table(sql: &str) -> String {
        // Keep only the last dotted component, stripping quotes.
        let token = Self::parse_table_token(sql);
        token
            .rsplit('.')
            .next()
            .unwrap_or(&token)
            .trim_matches('"')
            .trim_matches('`')
            .to_string()
    }

    /// The schema prefix of the target token (`db.table` -> `db`), when present.
    fn parse_schema(token: &str) -> Option<String> {
        let (schema, _) = token.rsplit_once('.')?;
        let schema = schema.trim_matches('"').trim_matches('`').to_string();
        if schema.is_empty() {
            None
        } else {
            Some(schema)
        }
    }

    /// The declared type of a `metadata_json` column in a `CREATE TABLE` statement.
    fn declared_metadata_type(sql: &str) -> Option<String> {
        let lower = sql.to_ascii_lowercase();
        let idx = lower.find("metadata_json")?;
        let after = lower[idx + "metadata_json".len()..].trim_start();
        if after.starts_with("character varying") {
            return Some("character varying".to_string());
        }
        let ty: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        if ty.is_empty() {
            None
        } else {
            Some(ty)
        }
    }

    fn parse_insert_table(sql: &str) -> String {
        let lower = sql.to_ascii_lowercase();
        let marker = "insert into ";
        let start = sql[lower.find(marker).map(|i| i + marker.len()).unwrap_or(0)..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '(')
            .collect::<String>();
        start
            .rsplit('.')
            .next()
            .unwrap_or(&start)
            .trim_matches('"')
            .to_string()
    }

    fn parse_insert_columns(sql: &str) -> Vec<String> {
        let open = match sql.find('(') {
            Some(i) => i,
            None => return Vec::new(),
        };
        let close = match sql[open..].find(')') {
            Some(i) => open + i,
            None => return Vec::new(),
        };
        sql[open + 1..close]
            .split(',')
            .map(|c| c.trim().trim_matches('"').to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .collect()
    }

    fn apply_write(&mut self, write: StagedWrite) {
        let mut st = self.state.lock().unwrap();
        match write {
            StagedWrite::Insert { table, row, id } => {
                let entry = st.tables.entry(table).or_default();
                entry.next_id = entry.next_id.max(id);
                entry.rows.push(row);
            }
            StagedWrite::Delete {
                table,
                column,
                value,
            } => {
                if let Some(entry) = st.tables.get_mut(&table) {
                    entry.rows.retain(|r| r.get(&column) != Some(&value));
                }
            }
        }
    }
}

impl SqlConnection for InMemoryConnection {
    fn execute(&mut self, sql: &str, params: &[SqlParam]) -> Result<usize, SkillRepositoryError> {
        let trimmed = sql.trim_start().to_ascii_lowercase();
        if trimmed.starts_with("create database") || trimmed.starts_with("create schema") {
            let schema = Self::parse_table(sql);
            self.state.lock().unwrap().schemas.insert(schema);
            return Ok(0);
        }
        if trimmed.starts_with("create table") {
            let token = Self::parse_table_token(sql);
            let table = Self::parse_table(sql);
            let data_type = Self::declared_metadata_type(sql);
            let mut st = self.state.lock().unwrap();
            if let Some(schema) = Self::parse_schema(&token) {
                st.schemas.insert(schema);
            }
            st.tables.entry(table.clone()).or_default();
            // Absence of `metadata_json` in the DDL means the column is absent. Record the
            // declared type so the PostgreSQL `DATA_TYPE` probe can reject `jsonb`.
            if let Some(data_type) = data_type {
                st.metadata_columns.insert(table, data_type);
            }
            return Ok(0);
        }
        if trimmed.starts_with("delete") {
            let table = Self::parse_table(sql);
            if params.is_empty() {
                // `DELETE FROM table` (clear-all).
                let mut st = self.state.lock().unwrap();
                let n = st.tables.get(&table).map(|t| t.rows.len()).unwrap_or(0);
                if let Some(entry) = st.tables.get_mut(&table) {
                    entry.rows.clear();
                }
                return Ok(n);
            }
            let column = sql
                .to_ascii_lowercase()
                .split("where")
                .nth(1)
                .and_then(|w| w.split('=').next())
                .map(|c| c.trim().trim_matches('"').to_ascii_lowercase())
                .unwrap_or_else(|| "name".to_string());
            let value = match params.first() {
                Some(SqlParam::Text(s)) => SqlValue::Text(s.clone()),
                Some(SqlParam::Int(i)) => SqlValue::Int(*i),
                _ => SqlValue::Null,
            };
            let write = StagedWrite::Delete {
                table: table.clone(),
                column,
                value,
            };
            let n = self
                .state
                .lock()
                .unwrap()
                .tables
                .get(&table)
                .map(|t| t.rows.len())
                .unwrap_or(0);
            if self.auto_commit {
                self.apply_write(write);
            } else {
                self.staged.push(write);
            }
            return Ok(n);
        }
        if trimmed.starts_with("insert") {
            // Plain INSERT executed via `executeUpdate` (e.g. resource rows with an explicit
            // id column). Mirrors the `insert_returning_id` row-construction logic.
            let table = Self::parse_insert_table(sql);
            let columns = Self::parse_insert_columns(sql);
            let mut row: HashMap<String, SqlValue> = HashMap::new();
            for (i, col) in columns.iter().enumerate() {
                let value = match params.get(i) {
                    Some(SqlParam::Text(s)) => SqlValue::Text(s.clone()),
                    Some(SqlParam::Int(v)) => SqlValue::Int(*v),
                    _ => SqlValue::Null,
                };
                row.insert(col.clone(), value);
            }
            // Assign a generated id when the statement did not supply one.
            let explicit_id = row.get("id").and_then(|v| v.as_int());
            let id = match explicit_id {
                Some(id) => id,
                None => {
                    let mut st = self.state.lock().unwrap();
                    let entry = st.tables.entry(table.clone()).or_default();
                    entry.next_id += 1;
                    entry.next_id
                }
            };
            row.entry("id".to_string()).or_insert(SqlValue::Int(id));
            let write = StagedWrite::Insert { table, row, id };
            if self.auto_commit {
                self.apply_write(write);
            } else {
                self.staged.push(write);
            }
            return Ok(1);
        }
        Ok(0)
    }

    fn query(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<SqlRow>, SkillRepositoryError> {
        let lower = sql.to_ascii_lowercase();
        // Column probe for metadata_json support. Must be matched before the generic
        // table-existence probe because the SQL also contains `INFORMATION_SCHEMA`.
        // Java: `... WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ? LIMIT 1`
        // -> the *second* text param is the table name.
        if lower.contains("information_schema.columns") {
            let table = params
                .iter()
                .filter_map(|p| match p {
                    SqlParam::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .nth(1);
            let data_type =
                table.and_then(|t| self.state.lock().unwrap().metadata_columns.get(&t).cloned());
            return Ok(match data_type {
                Some(data_type) => {
                    vec![SqlRow::new([("DATA_TYPE", SqlValue::Text(data_type))])]
                }
                None => Vec::new(),
            });
        }
        // INFORMATION_SCHEMA table/schema existence probes.
        if lower.contains("information_schema") {
            // `SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE ... TABLE_NAME = ?`
            // (MySQL, 2nd param) or `SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA
            // WHERE SCHEMA_NAME = ?` (1st/only param). Use the last text param, which is
            // the table name in the TABLES probe.
            let texts: Vec<String> = params
                .iter()
                .filter_map(|p| match p {
                    SqlParam::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            let st = self.state.lock().unwrap();
            let exists = if lower.contains("schemata") {
                // Schema probe. The scratch store has a single implicit default schema, so any
                // registered table vouches for it (keeps the legacy single-schema tests valid).
                texts.iter().any(|t| st.schemas.contains(t)) || !st.tables.is_empty()
            } else {
                texts
                    .iter()
                    .any(|t| st.tables.contains_key(t) || st.metadata_columns.contains_key(t))
            };
            return Ok(if exists {
                vec![SqlRow::new([("TABLE_NAME", SqlValue::Text("1".into()))])]
            } else {
                Vec::new()
            });
        }

        let table = Self::parse_table(sql);
        let st = self.state.lock().unwrap();
        let Some(entry) = st.tables.get(&table) else {
            return Ok(Vec::new());
        };
        let filter_col = lower
            .split("where")
            .nth(1)
            .and_then(|w| w.split('=').next())
            .map(|c| c.trim().trim_matches('"').to_ascii_lowercase());
        let filter_val = params.first().map(|p| match p {
            SqlParam::Text(s) => SqlValue::Text(s.clone()),
            SqlParam::Int(i) => SqlValue::Int(*i),
            SqlParam::Null => SqlValue::Null,
        });
        let mut out = Vec::new();
        for row in &entry.rows {
            if let Some(col) = &filter_col {
                if let Some(val) = &filter_val {
                    if row.get(col) != Some(val) {
                        continue;
                    }
                }
            }
            // Materialise as a row honouring the projection (all stored columns).
            let mut projected = row.clone();
            projected.entry("id".to_string()).or_insert(SqlValue::Null);
            out.push(SqlRow::new(projected));
        }
        if let Some(tail) = lower.split("order by").nth(1) {
            let desc = tail.contains(" desc");
            let column = tail
                .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ')')
                .find(|s| !s.is_empty())
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            if !column.is_empty() {
                out.sort_by(|a, b| {
                    let order = match (a.get(&column), b.get(&column)) {
                        (SqlValue::Text(x), SqlValue::Text(y)) => x.cmp(&y),
                        (SqlValue::Int(x), SqlValue::Int(y)) => x.cmp(&y),
                        (SqlValue::Null, SqlValue::Null) => std::cmp::Ordering::Equal,
                        (SqlValue::Null, _) => std::cmp::Ordering::Less,
                        (_, SqlValue::Null) => std::cmp::Ordering::Greater,
                        _ => std::cmp::Ordering::Equal,
                    };
                    if desc {
                        order.reverse()
                    } else {
                        order
                    }
                });
            }
        }
        Ok(out)
    }

    fn insert_returning_id(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<i64, SkillRepositoryError> {
        let table = Self::parse_insert_table(sql);
        let columns = Self::parse_insert_columns(sql);
        let mut row: HashMap<String, SqlValue> = HashMap::new();
        for (i, col) in columns.iter().enumerate() {
            let value = match params.get(i) {
                Some(SqlParam::Text(s)) => SqlValue::Text(s.clone()),
                Some(SqlParam::Int(v)) => SqlValue::Int(*v),
                _ => SqlValue::Null,
            };
            row.insert(col.clone(), value);
        }
        let id = {
            let mut st = self.state.lock().unwrap();
            let entry = st.tables.entry(table.clone()).or_default();
            let id = match row.get("id").and_then(|value| value.as_int()) {
                Some(id) => id,
                None => {
                    entry.next_id += 1;
                    entry.next_id
                }
            };
            entry.next_id = entry.next_id.max(id);
            id
        };
        row.entry("id".to_string()).or_insert(SqlValue::Int(id));
        let write = StagedWrite::Insert { table, row, id };
        if self.auto_commit {
            self.apply_write(write);
        } else {
            self.staged.push(write);
        }
        Ok(id)
    }

    fn set_auto_commit(&mut self, enabled: bool) -> Result<(), SkillRepositoryError> {
        self.auto_commit = enabled;
        Ok(())
    }

    fn commit(&mut self) -> Result<(), SkillRepositoryError> {
        for write in std::mem::take(&mut self.staged) {
            self.apply_write(write);
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), SkillRepositoryError> {
        self.staged.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_insert_query_roundtrip() {
        let db = InMemorySqlDatabase::new();
        let mut conn = db.connection().unwrap();
        conn.execute(
            "CREATE TABLE agentscope_skills (id BIGINT, name VARCHAR(255), metadata_json LONGTEXT)",
            &[],
        )
        .unwrap();
        assert!(db.table_exists("agentscope_skills"));
        let id = conn
            .insert_returning_id(
                "INSERT INTO agentscope_skills (name, metadata_json) VALUES (?, ?)",
                &[SqlParam::Text("a".into()), SqlParam::Null],
            )
            .unwrap();
        assert_eq!(id, 1);
        let rows = conn
            .query(
                "SELECT id, name FROM agentscope_skills WHERE name = ?",
                &[SqlParam::Text("a".into())],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_text("name").as_deref(), Some("a"));
    }

    #[test]
    fn transaction_rollback_discards_staged_writes() {
        let db = InMemorySqlDatabase::new();
        let mut conn = db.connection().unwrap();
        conn.execute("CREATE TABLE t (id BIGINT, name VARCHAR(255))", &[])
            .unwrap();
        conn.set_auto_commit(false).unwrap();
        conn.insert_returning_id(
            "INSERT INTO t (name) VALUES (?)",
            &[SqlParam::Text("x".into())],
        )
        .unwrap();
        conn.rollback().unwrap();
        conn.set_auto_commit(true).unwrap();
        assert!(db.rows("t").is_empty());
    }

    #[test]
    fn transaction_commit_applies_staged_writes() {
        let db = InMemorySqlDatabase::new();
        let mut conn = db.connection().unwrap();
        conn.execute("CREATE TABLE t (id BIGINT, name VARCHAR(255))", &[])
            .unwrap();
        conn.set_auto_commit(false).unwrap();
        conn.insert_returning_id(
            "INSERT INTO t (name) VALUES (?)",
            &[SqlParam::Text("x".into())],
        )
        .unwrap();
        conn.commit().unwrap();
        conn.set_auto_commit(true).unwrap();
        assert_eq!(db.rows("t").len(), 1);
    }

    #[test]
    fn in_transaction_rolls_back_when_the_work_fails() {
        let db = InMemorySqlDatabase::new();
        let mut conn = db.connection().unwrap();
        conn.execute("CREATE TABLE t (id BIGINT, name VARCHAR(255))", &[])
            .unwrap();
        let err = conn
            .in_transaction(|conn| -> Result<(), SkillRepositoryError> {
                conn.insert_returning_id(
                    "INSERT INTO t (name) VALUES (?)",
                    &[SqlParam::Text("x".into())],
                )?;
                Err(SkillRepositoryError::Database("boom".into()))
            })
            .unwrap_err();
        assert!(matches!(err, SkillRepositoryError::Database(_)));
        // The staged insert was rolled back and auto-commit is back on.
        assert!(db.rows("t").is_empty());
        conn.insert_returning_id(
            "INSERT INTO t (name) VALUES (?)",
            &[SqlParam::Text("y".into())],
        )
        .unwrap();
        assert_eq!(db.rows("t").len(), 1);
    }

    #[test]
    fn execute_batch_default_runs_every_parameter_set() {
        let db = InMemorySqlDatabase::new();
        let mut conn = db.connection().unwrap();
        conn.execute("CREATE TABLE t (id BIGINT, name VARCHAR(255))", &[])
            .unwrap();
        let affected = conn
            .execute_batch(
                "INSERT INTO t (name) VALUES (?)",
                &[
                    vec![SqlParam::Text("a".into())],
                    vec![SqlParam::Text("b".into())],
                ],
            )
            .unwrap();
        assert_eq!(affected, 2);
        assert_eq!(db.rows("t").len(), 2);
    }

    /// `SqlParam::text(None)` is SQL `NULL`; `Some` is bound as text (no interpolation).
    #[test]
    fn text_parameter_materialises_null_for_none() {
        assert_eq!(SqlParam::text(None::<String>), SqlParam::Null);
        assert_eq!(SqlParam::text(Some("x")), SqlParam::Text("x".to_string()));
    }

    /// `SqlValue` coercions are the contract the repositories rely on when reading columns.
    #[test]
    fn sql_value_accessors_coerce_and_reject() {
        assert_eq!(SqlValue::Text("42".into()).as_int(), Some(42));
        assert_eq!(SqlValue::Int(7).as_text(), None);
        assert_eq!(SqlValue::Null.as_text(), None);
        assert_eq!(SqlValue::Null.as_int(), None);
        assert_eq!(SqlValue::Text("not-a-number".into()).as_int(), None);
    }

    /// An absent column reads as `NULL` instead of panicking (metadata_json columns of legacy
    /// schemas are simply not selected).
    #[test]
    fn missing_columns_read_as_null() {
        let row = SqlRow::new([("name", SqlValue::Text("a".into()))]);
        assert_eq!(row.get("absent"), SqlValue::Null);
        assert_eq!(row.get_text("absent"), None);
        assert_eq!(row.get_int("name"), None);
        assert_eq!(row.get_text("name").as_deref(), Some("a"));
    }

    /// `execute` reports the affected row count and honours an explicit primary key.
    #[test]
    fn execute_reports_affected_rows_and_keeps_explicit_ids() {
        let db = InMemorySqlDatabase::new();
        let mut conn = db.connection().unwrap();
        conn.execute("CREATE TABLE t (id BIGINT, name VARCHAR(255))", &[])
            .unwrap();
        conn.insert_returning_id(
            "INSERT INTO t (id, name) VALUES (?, ?)",
            &[SqlParam::Int(42), SqlParam::Text("a".into())],
        )
        .unwrap();
        let rows = conn.query("SELECT id, name FROM t WHERE id = ?", &[SqlParam::Int(42)])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_int("id"), Some(42));
        let deleted = conn
            .execute("DELETE FROM t WHERE name = ?", &[SqlParam::Text("a".into())])
            .unwrap();
        assert_eq!(deleted, 1);
        assert!(db.rows("t").is_empty());
    }

    /// `commit` / `rollback` outside a transaction are no-ops, and toggling auto-commit twice
    /// does not open two nested transactions (JDBC `setAutoCommit` is idempotent).
    #[test]
    fn transaction_controls_are_idempotent() {
        let db = InMemorySqlDatabase::new();
        let mut conn = db.connection().unwrap();
        conn.execute("CREATE TABLE t (id BIGINT, name VARCHAR(255))", &[])
            .unwrap();
        conn.commit().unwrap();
        conn.rollback().unwrap();
        conn.set_auto_commit(false).unwrap();
        conn.set_auto_commit(false).unwrap();
        conn.insert_returning_id(
            "INSERT INTO t (name) VALUES (?)",
            &[SqlParam::Text("x".into())],
        )
        .unwrap();
        conn.commit().unwrap();
        conn.set_auto_commit(true).unwrap();
        assert_eq!(db.rows("t").len(), 1);
    }

    /// An empty batch is a no-op (the PostgreSQL repository skips it, but the seam must agree).
    #[test]
    fn empty_batch_is_a_noop() {
        let db = InMemorySqlDatabase::new();
        let mut conn = db.connection().unwrap();
        conn.execute("CREATE TABLE t (id BIGINT, name VARCHAR(255))", &[])
            .unwrap();
        assert_eq!(conn.execute_batch("INSERT INTO t (name) VALUES (?)", &[]).unwrap(), 0);
        assert!(db.rows("t").is_empty());
    }
}

/// Fault-injection / traffic-recording wrappers over [`InMemorySqlDatabase`], used by the
/// repository tests to exercise the failure and batching paths of the SQL seam.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Delegates to [`InMemorySqlDatabase`] while recording / failing statements.
    ///
    /// * `needle` + `skip`: fails every executed statement whose SQL contains `needle`, after
    ///   letting the first `skip` matches through (counted per connection);
    /// * `batches`: the size of every [`SqlConnection::execute_batch`] call;
    /// * `batch_shortfall`: rows the fake under-reports from every batch, so the "driver lost
    ///   rows" branch stays reachable.
    pub(crate) struct InterceptingSqlDatabase {
        inner: InMemorySqlDatabase,
        needle: Option<String>,
        skip: usize,
        batch_shortfall: usize,
        failures: Arc<AtomicUsize>,
        batches: Arc<Mutex<Vec<usize>>>,
    }

    impl InterceptingSqlDatabase {
        /// A pass-through database that only records batch sizes.
        pub(crate) fn recording() -> Self {
            Self::with_fault(None, 0)
        }

        /// A database that fails the statement matching `needle` after `skip` matches.
        pub(crate) fn failing(needle: &str, skip: usize) -> Self {
            Self::with_fault(Some(needle.to_string()), skip)
        }

        /// Makes every [`SqlConnection::execute_batch`] report `rows` fewer affected rows.
        pub(crate) fn with_batch_shortfall(mut self, rows: usize) -> Self {
            self.batch_shortfall = rows;
            self
        }

        fn with_fault(needle: Option<String>, skip: usize) -> Self {
            Self {
                inner: InMemorySqlDatabase::new(),
                needle,
                skip,
                batch_shortfall: 0,
                failures: Arc::new(AtomicUsize::new(0)),
                batches: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// The wrapped store, for row-level assertions.
        pub(crate) fn inner(&self) -> &InMemorySqlDatabase {
            &self.inner
        }

        /// How many statements the injected fault rejected.
        pub(crate) fn failures(&self) -> usize {
            self.failures.load(Ordering::Relaxed)
        }

        /// The size of every batch handed to `execute_batch`, in call order.
        pub(crate) fn batch_sizes(&self) -> Vec<usize> {
            self.batches.lock().unwrap().clone()
        }
    }

    impl SqlDatabase for InterceptingSqlDatabase {
        type Connection = InterceptingConnection;

        fn connection(&self) -> Result<Self::Connection, SkillRepositoryError> {
            Ok(InterceptingConnection {
                inner: self.inner.connection()?,
                needle: self.needle.clone(),
                skip: self.skip,
                matched: 0,
                batch_shortfall: self.batch_shortfall,
                failures: self.failures.clone(),
                batches: self.batches.clone(),
            })
        }
    }

    /// Connection handed out by [`InterceptingSqlDatabase`].
    pub(crate) struct InterceptingConnection {
        inner: InMemoryConnection,
        needle: Option<String>,
        skip: usize,
        matched: usize,
        batch_shortfall: usize,
        failures: Arc<AtomicUsize>,
        batches: Arc<Mutex<Vec<usize>>>,
    }

    impl InterceptingConnection {
        fn guard(&mut self, sql: &str) -> Result<(), SkillRepositoryError> {
            let Some(needle) = self.needle.as_deref() else {
                return Ok(());
            };
            if !sql.contains(needle) {
                return Ok(());
            }
            self.matched += 1;
            if self.matched > self.skip {
                self.failures.fetch_add(1, Ordering::Relaxed);
                return Err(SkillRepositoryError::Database(format!(
                    "injected failure on: {needle}"
                )));
            }
            Ok(())
        }
    }

    impl SqlConnection for InterceptingConnection {
        fn execute(
            &mut self,
            sql: &str,
            params: &[SqlParam],
        ) -> Result<usize, SkillRepositoryError> {
            self.guard(sql)?;
            self.inner.execute(sql, params)
        }

        fn query(
            &mut self,
            sql: &str,
            params: &[SqlParam],
        ) -> Result<Vec<SqlRow>, SkillRepositoryError> {
            self.inner.query(sql, params)
        }

        fn insert_returning_id(
            &mut self,
            sql: &str,
            params: &[SqlParam],
        ) -> Result<i64, SkillRepositoryError> {
            self.guard(sql)?;
            self.inner.insert_returning_id(sql, params)
        }

        fn execute_batch(
            &mut self,
            sql: &str,
            params: &[Vec<SqlParam>],
        ) -> Result<usize, SkillRepositoryError> {
            self.batches.lock().unwrap().push(params.len());
            self.guard(sql)?;
            let affected = self.inner.execute_batch(sql, params)?;
            Ok(affected.saturating_sub(self.batch_shortfall))
        }

        fn set_auto_commit(&mut self, enabled: bool) -> Result<(), SkillRepositoryError> {
            self.inner.set_auto_commit(enabled)
        }

        fn commit(&mut self) -> Result<(), SkillRepositoryError> {
            self.inner.commit()
        }

        fn rollback(&mut self) -> Result<(), SkillRepositoryError> {
            self.inner.rollback()
        }
    }
}
