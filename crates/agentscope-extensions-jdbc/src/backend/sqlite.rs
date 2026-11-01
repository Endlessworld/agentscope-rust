//! SQLite execution backend built on `rusqlite`, mirroring the role of the JDBC driver.
//!
//! This is the default concrete [`JdbcExecutor`], letting the JDBC components run against
//! a real SQL engine (in-memory or file-backed) without an external database service. It
//! is the Rust analogue of the Java H2/SQLite test backends.

use std::sync::{Arc, Mutex};

use rusqlite::types::Value;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::{ColumnValue, DbError, JdbcError, JdbcExecutor, JdbcResult, JdbcSession};
use crate::dialect::{BoundSql, NativeLockSession, SqlParam};

/// How long a statement waits for another connection's write lock before giving up.
///
/// `rusqlite` defaults to no wait at all, so a second writer on the same database file gets
/// `SQLITE_BUSY` the instant the first one is mid-transaction. The JDBC components do
/// read-modify-write work inside [`JdbcExecutor::in_transaction`] (list appends, versioned
/// upserts), which only stays correct — and only succeeds at all — when the waiter blocks
/// until the writer commits.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Maps a `rusqlite` failure, keeping SQLite's **extended** result code.
///
/// SQLite reports no SQLSTATE and words every constraint failure as "constraint failed", so
/// the code is the only structured signal: `2067` (`SQLITE_CONSTRAINT_UNIQUE`) and `1555`
/// (`SQLITE_CONSTRAINT_PRIMARYKEY`) are duplicates, while `1299` (NOT NULL), `275` (CHECK)
/// and `787` (FOREIGN KEY) are not (see [`crate::backend::is_unique_violation`]).
fn sqlite_error(context: impl std::fmt::Display, error: rusqlite::Error) -> JdbcError {
    let mut db = DbError::new(format!("{context}: {error}"));
    if let rusqlite::Error::SqliteFailure(inner, _) = &error {
        db = db.with_vendor_code(i64::from(inner.extended_code));
    }
    JdbcError::Database(db)
}

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
            .map_err(|e| sqlite_error("open in-memory sqlite", e))?;
        Self::with_busy_timeout(conn)
    }

    /// Opens a file-backed SQLite database.
    pub fn open(path: &str) -> JdbcResult<Self> {
        let conn =
            Connection::open(path).map_err(|e| sqlite_error(format!("open sqlite '{path}'"), e))?;
        Self::with_busy_timeout(conn)
    }

    fn with_busy_timeout(conn: Connection) -> JdbcResult<Self> {
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| sqlite_error("set busy timeout", e))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Runs idempotent DDL statements (e.g. `CREATE TABLE IF NOT EXISTS`).
    pub fn execute_ddl(&self, statements: &[String]) -> JdbcResult<()> {
        let conn = self.lock();
        for ddl in statements {
            conn.execute_batch(ddl)
                .map_err(|e| sqlite_error(format!("DDL failed: {ddl}"), e))?;
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

fn execute_bound(conn: &Connection, bound: &BoundSql) -> JdbcResult<usize> {
    let params: Vec<Value> = bound.params.iter().map(to_rusqlite_value).collect();
    conn.execute(&bound.sql, rusqlite::params_from_iter(params))
        .map_err(|e| sqlite_error(&bound.sql, e))
}

fn query_bound(conn: &Connection, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
    let params: Vec<Value> = bound.params.iter().map(to_rusqlite_value).collect();
    let mut stmt = conn
        .prepare(&bound.sql)
        .map_err(|e| sqlite_error(format!("prepare {}", bound.sql), e))?;
    let column_count = stmt.column_count();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params))
        .map_err(|e| sqlite_error(format!("query {}", bound.sql), e))?;
    let mut result = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| sqlite_error(format!("row {}", bound.sql), e))?
    {
        let mut record = Vec::with_capacity(column_count);
        for i in 0..column_count {
            let value = row
                .get_ref(i)
                .map_err(|e| sqlite_error(format!("column {i}"), e))?;
            record.push(rusqlite_value_to_column(value));
        }
        result.push(record);
    }
    Ok(result)
}

/// Connection-scoped view over an open transaction, handed to
/// [`JdbcExecutor::in_transaction`] callbacks.
struct TxSession<'a, 'conn> {
    tx: &'a Transaction<'conn>,
}

impl JdbcSession for TxSession<'_, '_> {
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize> {
        execute_bound(self.tx, bound)
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        query_bound(self.tx, bound)
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
        execute_bound(&self.lock(), bound)
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        query_bound(&self.lock(), bound)
    }

    fn execute_batch(&self, bounds: &[BoundSql]) -> JdbcResult<()> {
        let mut conn = self.lock();
        let tx = conn
            .transaction()
            .map_err(|e| sqlite_error("begin transaction", e))?;
        for bound in bounds {
            let params: Vec<Value> = bound.params.iter().map(to_rusqlite_value).collect();
            tx.execute(&bound.sql, rusqlite::params_from_iter(params))
                .map_err(|e| sqlite_error(&bound.sql, e))?;
        }
        tx.commit().map_err(|e| sqlite_error("commit", e))?;
        Ok(())
    }

    fn execute_prepared_batch(&self, bounds: &[BoundSql]) -> JdbcResult<usize> {
        let mut conn = self.lock();
        let tx = conn
            .transaction()
            .map_err(|e| sqlite_error("begin transaction", e))?;
        let mut total = 0usize;
        {
            let mut stmt = tx
                .prepare_cached(bounds[0].sql.as_str())
                .map_err(|e| sqlite_error("prepare batch", e))?;
            for bound in bounds {
                let params: Vec<Value> = bound.params.iter().map(to_rusqlite_value).collect();
                total += stmt
                    .execute(rusqlite::params_from_iter(params))
                    .map_err(|e| sqlite_error(format!("batch {}", bound.sql), e))?;
            }
        }
        tx.commit().map_err(|e| sqlite_error("commit batch", e))?;
        Ok(total)
    }

    /// SQLite has no server-scoped advisory lock, so this backend never provides a native
    /// lock session. Callers use the portable [`crate::sandbox::TableLockStrategy`] on
    /// SQLite; MySQL/PostgreSQL executors live in their own driver crates and override this.
    fn open_native_lock_session(&self) -> JdbcResult<Box<dyn NativeLockSession>> {
        Err(JdbcError::Unsupported(
            "SQLite has no native advisory lock (GET_LOCK / pg_try_advisory_lock); \
             MySQL/PostgreSQL lock sessions are provided by their driver-layer executors"
                .to_string(),
        ))
    }

    /// Opens a real SQLite transaction; the callback's statements commit together or all
    /// roll back. Dropping an uncommitted `rusqlite::Transaction` rolls back, so an error
    /// from `work` needs no explicit rollback call.
    ///
    /// The transaction is `IMMEDIATE`, not SQLite's default `DEFERRED`: every caller uses
    /// this envelope for a **read-modify-write** (list appends read the stored hash and the
    /// item count first, versioned upserts read the version back), and a `DEFERRED`
    /// transaction only takes its write lock when the first write runs. Under two writers the
    /// upgrade then fails immediately with `SQLITE_BUSY` — the read lock cannot be promoted
    /// while the other connection holds the write lock — so the append would be lost even with
    /// a busy timeout. `IMMEDIATE` takes the write lock at `BEGIN`, where [`BUSY_TIMEOUT`]
    /// applies, and the writers queue instead of failing.
    fn in_transaction(
        &self,
        work: &mut dyn FnMut(&mut dyn JdbcSession) -> JdbcResult<()>,
    ) -> JdbcResult<()> {
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sqlite_error("begin transaction", e))?;
        let result = {
            let mut session = TxSession { tx: &tx };
            work(&mut session)
        };
        match result {
            Ok(()) => tx.commit().map_err(|e| sqlite_error("commit", e)),
            Err(e) => {
                // Dropping `tx` rolls back; keep the original error as the returned one.
                drop(tx);
                Err(e)
            }
        }
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
