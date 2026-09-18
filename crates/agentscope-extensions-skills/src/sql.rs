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
        self.columns
            .get(column)
            .cloned()
            .unwrap_or(SqlValue::Null)
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
    fn query(&mut self, sql: &str, params: &[SqlParam]) -> Result<Vec<SqlRow>, SkillRepositoryError>;

    /// Runs an insert and returns the generated primary key.
    fn insert_returning_id(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<i64, SkillRepositoryError>;

    /// Toggles auto-commit (transaction boundaries).
    fn set_auto_commit(&mut self, enabled: bool) -> Result<(), SkillRepositoryError>;

    /// Commits the current transaction.
    fn commit(&mut self) -> Result<(), SkillRepositoryError>;

    /// Rolls back the current transaction.
    fn rollback(&mut self) -> Result<(), SkillRepositoryError>;
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
    /// Table name -> whether a `metadata_json` column exists (drives compatibility probes).
    metadata_columns: HashMap<String, bool>,
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
        st.metadata_columns.insert(table.to_string(), has_metadata);
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

    /// Whether the named table is known to expose a `metadata_json` column.
    fn has_metadata_column(&self, table: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .metadata_columns
            .get(table)
            .copied()
            .unwrap_or(false)
    }
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
    Insert { table: String, row: HashMap<String, SqlValue>, id: i64 },
    Delete { table: String, column: String, value: SqlValue },
}

impl InMemoryConnection {
    fn parse_table(sql: &str) -> String {
        // Handles `FROM "schema"."table"`, `FROM db.table`, `FROM table`, and
        // `CREATE TABLE [IF NOT EXISTS] "schema"."table"` / `CREATE SCHEMA ...`.
        let lower = sql.to_ascii_lowercase();
        let marker = if let Some(i) = lower.find(" from ") {
            i + " from ".len()
        } else if let Some(i) = lower.find("create table ") {
            // Skip the optional `if not exists` clause.
            let after = i + "create table ".len();
            let tail = &lower[after..];
            match tail.strip_prefix("if not exists ").or_else(|| tail.strip_prefix("if not exists")) {
                Some(stripped) => after + (tail.len() - stripped.len()),
                None => after,
            }
        } else if let Some(i) = lower.find("create schema ") {
            let after = i + "create schema ".len();
            let tail = &lower[after..];
            match tail.strip_prefix("if not exists ").or_else(|| tail.strip_prefix("if not exists")) {
                Some(stripped) => after + (tail.len() - stripped.len()),
                None => after,
            }
        } else if let Some(i) = lower.find("insert into ") {
            i + "insert into ".len()
        } else {
            0
        };
        let rest = &sql[marker..];
        let token: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '(' && *c != ';')
            .collect();
        // Keep only the last dotted component, stripping quotes.
        token
            .rsplit('.')
            .next()
            .unwrap_or(&token)
            .trim_matches('"')
            .trim_matches('`')
            .to_string()
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
        if trimmed.starts_with("create table") {
            let table = Self::parse_table(sql);
            let maybe = sql.to_ascii_lowercase();
            let has_meta = maybe.contains("metadata_json");
            let mut st = self.state.lock().unwrap();
            st.tables.entry(table.clone()).or_default();
            if has_meta {
                st.metadata_columns.insert(table, true);
            } else {
                st.metadata_columns.entry(table).or_insert(false);
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
            let write = StagedWrite::Insert {
                table,
                row,
                id,
            };
            if self.auto_commit {
                self.apply_write(write);
            } else {
                self.staged.push(write);
            }
            return Ok(1);
        }
        Ok(0)
    }

    fn query(&mut self, sql: &str, params: &[SqlParam]) -> Result<Vec<SqlRow>, SkillRepositoryError> {
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
            let has = table
                .as_deref()
                .map(|t| self.state.lock().unwrap().metadata_columns.get(t).copied().unwrap_or(false))
                .unwrap_or(false);
            return Ok(if has {
                vec![SqlRow::new([("DATA_TYPE", SqlValue::Text("longtext".into()))])]
            } else {
                Vec::new()
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
            let exists = texts
                .iter()
                .any(|t| st.tables.contains_key(t) || st.metadata_columns.contains_key(t));
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
            entry.next_id += 1;
            entry.next_id
        };
        row.insert("id".to_string(), SqlValue::Int(id));
        let write = StagedWrite::Insert {
            table,
            row,
            id,
        };
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
}