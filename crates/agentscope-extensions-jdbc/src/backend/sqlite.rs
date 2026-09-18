//! SQLite execution backend built on `rusqlite`, mirroring the role of the JDBC driver.
//!
//! This is the default concrete [`JdbcExecutor`], letting the JDBC components run against
//! a real SQL engine (in-memory or file-backed) without an external database service. It
//! is the Rust analogue of the Java H2/SQLite test backends.

use std::sync::{Arc, Mutex};

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::dialect::{BoundSql, SqlParam};
use super::{ColumnValue, JdbcError, JdbcExecutor, JdbcResult};

/// A [`JdbcExecutor`] backed by a single `rusqlite` connection guarded by a mutex.
///
/// `rusqlite::Connection` is not `Sync`, so the connection is wrapped in a `Mutex` to
/// satisfy [`JdbcExecutor`]'s `Send + Sync` bound. Callers that need concurrency should
/// use a connection pool at a higher layer.
pub struct RusqliteExecutor {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for RusqliteExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RusqliteExecutor").finish_non_exhaustive()
    }
}

impl RusqliteExecutor {
    /// Opens an in-memory SQLite database.
    pub fn in_memory() -> JdbcResult<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| JdbcError::Database(format!("open in-memory sqlite: {e}")))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Opens a file-backed SQLite database.
    pub fn open(path: &str) -> JdbcResult<Self> {
        let conn = Connection::open(path)
            .map_err(|e| JdbcError::Database(format!("open sqlite '{path}': {e}")))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Runs idempotent DDL statements (e.g. `CREATE TABLE IF NOT EXISTS`).
    pub fn execute_ddl(&self, statements: &[String]) -> JdbcResult<()> {
        let conn = self.lock();
        for ddl in statements {
            conn.execute_batch(ddl)
                .map_err(|e| JdbcError::Database(format!("DDL failed: {ddl}: {e}")))?;
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn to_rusqlite_value(param: &SqlParam) -> Value {
    match param {
        SqlParam::Text(s) => Value::Text(s.clone()),
        SqlParam::Int(i) => Value::Integer(*i),
        SqlParam::Blob(b) => Value::Blob(b.clone()),
        SqlParam::Null => Value::Null,
    }
}

fn rusqlite_value_to_column(value: rusqlite::types::ValueRef<'_>) -> ColumnValue {
    match value {
        rusqlite::types::ValueRef::Null => ColumnValue::Null,
        rusqlite::types::ValueRef::Integer(i) => ColumnValue::Int(i),
        rusqlite::types::ValueRef::Real(r) => ColumnValue::Text(r.to_string()),
        rusqlite::types::ValueRef::Text(t) => {
            ColumnValue::Text(String::from_utf8_lossy(t).into_owned())
        }
        rusqlite::types::ValueRef::Blob(b) => ColumnValue::Blob(b.to_vec()),
    }
}

impl JdbcExecutor for RusqliteExecutor {
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize> {
        let conn = self.lock();
        let params: Vec<Value> = bound.params.iter().map(to_rusqlite_value).collect();
        conn.execute(&bound.sql, rusqlite::params_from_iter(params))
            .map_err(|e| JdbcError::Database(format!("{}: {e}", bound.sql)))
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        let conn = self.lock();
        let params: Vec<Value> = bound.params.iter().map(to_rusqlite_value).collect();
        let mut stmt = conn
            .prepare(&bound.sql)
            .map_err(|e| JdbcError::Database(format!("prepare {}: {e}", bound.sql)))?;
        let column_count = stmt.column_count();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params))
            .map_err(|e| JdbcError::Database(format!("query {}: {e}", bound.sql)))?;
        let mut result = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| JdbcError::Database(format!("row {}: {e}", bound.sql)))?
        {
            let mut record = Vec::with_capacity(column_count);
            for i in 0..column_count {
                let value = row
                    .get_ref(i)
                    .map_err(|e| JdbcError::Database(format!("column {i}: {e}")))?;
                record.push(rusqlite_value_to_column(value));
            }
            result.push(record);
        }
        Ok(result)
    }

    fn execute_batch(&self, bounds: &[BoundSql]) -> JdbcResult<()> {
        let mut conn = self.lock();
        let tx = conn
            .transaction()
            .map_err(|e| JdbcError::Database(format!("begin transaction: {e}")))?;
        for bound in bounds {
            let params: Vec<Value> = bound.params.iter().map(to_rusqlite_value).collect();
            tx.execute(&bound.sql, rusqlite::params_from_iter(params))
                .map_err(|e| JdbcError::Database(format!("{}: {e}", bound.sql)))?;
        }
        tx.commit()
            .map_err(|e| JdbcError::Database(format!("commit: {e}")))?;
        Ok(())
    }

    fn execute_prepared_batch(&self, bounds: &[BoundSql]) -> JdbcResult<usize> {
        let mut conn = self.lock();
        let tx = conn
            .transaction()
            .map_err(|e| JdbcError::Database(format!("begin transaction: {e}")))?;
        let mut total = 0usize;
        {
            let mut stmt = tx
                .prepare_cached(bounds[0].sql.as_str())
                .map_err(|e| JdbcError::Database(format!("prepare batch: {e}")))?;
            for bound in bounds {
                let params: Vec<Value> = bound.params.iter().map(to_rusqlite_value).collect();
                total += stmt
                    .execute(rusqlite::params_from_iter(params))
                    .map_err(|e| JdbcError::Database(format!("batch {}: {e}", bound.sql)))?;
            }
        }
        tx.commit()
            .map_err(|e| JdbcError::Database(format!("commit batch: {e}")))?;
        Ok(total)
    }
}

/// A convenience constructor returning a shared in-memory executor.
pub fn in_memory_executor() -> JdbcResult<Arc<RusqliteExecutor>> {
    Ok(Arc::new(RusqliteExecutor::in_memory()?))
}

/// Executes a query expected to return at most one row, returning it as a single column
/// value. Used by the lock table's acquisition probe.
pub(crate) fn query_optional_int(
    executor: &dyn JdbcExecutor,
    bound: &BoundSql,
) -> JdbcResult<Option<i64>> {
    let rows = executor.query(bound)?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .and_then(|c| c.as_int()))
}
