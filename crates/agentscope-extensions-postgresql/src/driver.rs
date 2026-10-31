//! Real `tokio-postgres`-backed [`PostgresClient`] adapter (feature `postgres-driver`).
//!
//! [`PostgresClient`] is synchronous by design (it serves the sync `BaseStore` trait),
//! while `tokio-postgres` is async. This adapter bridges the two by driving the async
//! client on a dedicated runtime with [`block_on`], which is safe to call **both** from a
//! plain thread and from inside an existing Tokio runtime (PG-01): the harness
//! `RemoteFilesystem` awaits the synchronous `BaseStore` methods from inside `async fn`s,
//! so a bare `Runtime::block_on` would panic on the first `read`/`write`.
//!
//! Calls made from a multi-threaded runtime still block the calling worker
//! (`block_in_place` hands it back to the scheduler first); the async traits additionally
//! hop onto the blocking pool via [`crate::blocking`].
//!
//! This module is intentionally minimal: it establishes a connection and executes
//! `(sql, params)` statements, mapping `$n` bind arguments in order.

use std::sync::Arc;

use tokio::runtime::{Runtime, RuntimeFlavor};
use tokio_postgres::{Client, Config, NoTls};

use crate::client::{
    Cell, PostgresClient, PostgresLockSession, QueryOutcome, Row, SqlStatement, SqlValue,
};
use crate::error::{PgResult, PostgresError};

/// Runs `future` to completion on `runtime`, **even when the caller already sits inside a
/// Tokio runtime** (PG-01).
///
/// `Runtime::block_on` panics ("cannot block the current thread from within a runtime") when
/// the calling thread is driving another runtime, and the harness `RemoteFilesystem` calls
/// the **synchronous** [`BaseStore`](agentscope_harness::filesystem::BaseStore) methods from
/// inside `async fn`s — exactly that situation. So:
///
/// * no ambient runtime (plain thread, `spawn_blocking` pool thread) → drive `runtime`;
/// * multi-threaded ambient runtime → [`tokio::task::block_in_place`] first, which hands the
///   worker back to its scheduler, then block on the ambient handle;
/// * current-thread ambient runtime → `block_in_place` is illegal there and blocking the only
///   worker would deadlock, so the future is driven on `runtime` from a scoped thread.
fn block_on<F, T>(runtime: &Runtime, future: F) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        Ok(_) => std::thread::scope(|scope| {
            match scope.spawn(|| runtime.block_on(future)).join() {
                Ok(value) => value,
                // Propagate a panic from the driving thread (e.g. a driver panic) instead of
                // swallowing it as a join error.
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }),
        Err(_) => runtime.block_on(future),
    }
}

/// A synchronous [`PostgresClient`] over a live `tokio-postgres` connection.
pub struct PgSimpleClient {
    runtime: Arc<Runtime>,
    client: Client,
    /// Retained so [`PostgresClient::open_lock_session`] /
    /// [`PostgresClient::execute_in_transaction`] can open *additional* dedicated
    /// connections to the same server (Java gets them from the `DataSource` pool).
    config: Config,
}

impl std::fmt::Debug for PgSimpleClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSimpleClient").finish_non_exhaustive()
    }
}

impl PgSimpleClient {
    /// Connects to PostgreSQL using a libpq-style connection string.
    ///
    /// Example: `host=localhost user=postgres password=secret dbname=agentscope`.
    pub fn connect(config: &str) -> PgResult<Arc<Self>> {
        let parsed: Config = config
            .parse()
            .map_err(|e| PostgresError::Query(format!("invalid connection string: {e}")))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| PostgresError::Query(format!("failed to build runtime: {e}")))?;
        let cfg = parsed.clone();
        let connecting = async move { cfg.connect(NoTls).await };
        let (client, connection) = block_on(&runtime, connecting)
            .map_err(|e| PostgresError::Query(format!("connection failed: {e}")))?;
        runtime.spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!("[postgres] connection error: {e}");
            }
        });
        Ok(Arc::new(Self {
            runtime: Arc::new(runtime),
            client,
            config: parsed,
        }))
    }

    /// Opens one extra connection driven by the same runtime.
    fn connect_extra(&self, purpose: &'static str) -> PgResult<Client> {
        let config = self.config.clone();
        let connecting = async move { config.connect(NoTls).await };
        let (client, connection) = block_on(&self.runtime, connecting)
            .map_err(|e| PostgresError::Query(format!("{purpose} connection failed: {e}")))?;
        self.runtime.spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!("[postgres] {purpose} connection error: {e}");
            }
        });
        Ok(client)
    }
}

/// A dedicated `tokio_postgres` connection used as an advisory-lock session.
///
/// Dropping the client ends the connection, which makes the server release every advisory
/// lock it still holds — the Rust equivalent of Java `Connection#close()`.
pub struct PgLockSession {
    runtime: Arc<Runtime>,
    client: Option<Client>,
}

impl std::fmt::Debug for PgLockSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgLockSession")
            .field("closed", &self.client.is_none())
            .finish()
    }
}

impl PgLockSession {
    fn client(&self) -> PgResult<&Client> {
        self.client
            .as_ref()
            .ok_or_else(|| PostgresError::Query("advisory-lock session is already closed".into()))
    }

    fn advisory_call(&self, sql: &str, lock_name: i64) -> PgResult<bool> {
        let client = self.client()?;
        block_on(&self.runtime, async {
            let row = client
                .query_one(sql, &[&lock_name])
                .await
                .map_err(|e| PostgresError::Query(e.to_string()))?;
            row.try_get::<_, bool>(0)
                .map_err(|e| PostgresError::Decode(e.to_string()))
        })
    }
}

impl PostgresLockSession for PgLockSession {
    fn try_lock(&self, lock_name: i64) -> PgResult<bool> {
        self.advisory_call("SELECT pg_try_advisory_lock($1)", lock_name)
    }

    fn unlock(&self, lock_name: i64) -> PgResult<bool> {
        self.advisory_call("SELECT pg_advisory_unlock($1)", lock_name)
    }

    fn close(&mut self) {
        // Dropping the client makes the connection task finish and the socket close, which is
        // what releases the session's advisory locks.
        self.client.take();
    }
}

/// Converts a [`SqlValue`] into an owned boxed `ToSql` value.
fn boxed(value: &SqlValue) -> Box<dyn tokio_postgres::types::ToSql + Sync + Send> {
    match value {
        SqlValue::Null => Box::new(Option::<String>::None),
        SqlValue::Text(s) => Box::new(s.clone()),
        SqlValue::Int(v) => Box::new(*v),
        SqlValue::BigInt(v) => Box::new(*v),
        SqlValue::Bytes(b) => Box::new(b.clone()),
        SqlValue::Timestamp(ms) => {
            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(*ms)
                .unwrap_or_else(chrono::Utc::now);
            Box::new(dt.naive_utc())
        }
    }
}

impl PostgresClient for PgSimpleClient {
    fn query(&self, sql: &str, params: &[SqlValue]) -> PgResult<QueryOutcome> {
        block_on(&self.runtime, execute_statement(&self.client, sql, params))
    }

    fn open_lock_session(&self) -> PgResult<Box<dyn PostgresLockSession>> {
        // One fresh connection per lease, exactly like Java's `dataSource.getConnection()`.
        let client = self.connect_extra("lock session")?;
        Ok(Box::new(PgLockSession {
            runtime: Arc::clone(&self.runtime),
            client: Some(client),
        }))
    }

    fn execute_in_transaction(&self, statements: &[SqlStatement]) -> PgResult<Vec<QueryOutcome>> {
        // Java `executeInWriteTransaction`: a dedicated connection, autocommit off,
        // commit on success, roll back on the first failure.
        let client = self.connect_extra("write transaction")?;
        block_on(&self.runtime, async {
            client
                .batch_execute("BEGIN")
                .await
                .map_err(|e| PostgresError::Query(format!("BEGIN failed: {e}")))?;
            let mut outcomes = Vec::with_capacity(statements.len());
            for stmt in statements {
                match execute_statement(&client, &stmt.sql, &stmt.params).await {
                    Ok(outcome) => outcomes.push(outcome),
                    Err(e) => {
                        // Preserve the original error; a rollback failure is only logged.
                        if let Err(rb) = client.batch_execute("ROLLBACK").await {
                            tracing::warn!("[postgres] rollback failed: {rb}");
                        }
                        return Err(e);
                    }
                }
            }
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|e| PostgresError::Query(format!("COMMIT failed: {e}")))?;
            Ok(outcomes)
        })
    }

    fn execute_batch(&self, sql: &str) -> PgResult<()> {
        block_on(&self.runtime, async {
            self.client
                .batch_execute(sql)
                .await
                .map_err(|e| PostgresError::Schema(e.to_string()))
        })
    }
}

/// Runs one `(sql, params)` statement on an explicit connection.
async fn execute_statement(
    client: &Client,
    sql: &str,
    params: &[SqlValue],
) -> PgResult<QueryOutcome> {
    let owned: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> =
        params.iter().map(boxed).collect();
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = owned
        .iter()
        .map(|b| b.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();

    let upper = sql.trim_start().to_uppercase();
    if upper.starts_with("SELECT") {
        let rows = client
            .query(sql, &refs)
            .await
            .map_err(|e| PostgresError::Query(e.to_string()))?;
        let mut out: Vec<Row> = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(decode_row(&row));
        }
        Ok(QueryOutcome::Rows(out))
    } else {
        let affected = client
            .execute(sql, &refs)
            .await
            .map_err(|e| PostgresError::Query(e.to_string()))?;
        Ok(QueryOutcome::Affected(affected as u64))
    }
}

/// Best-effort row decoding into [`Cell`]s (heterogeneous columns are tried in order).
fn decode_row(row: &tokio_postgres::Row) -> Row {
    let mut cells = Vec::with_capacity(row.len());
    for i in 0..row.len() {
        let cell = try_text(row, i)
            .or_else(|| try_i64(row, i).map(Cell::BigInt))
            .or_else(|| try_i32(row, i).map(Cell::Int))
            .or_else(|| try_bool(row, i).map(Cell::Bool))
            .or_else(|| try_bytes(row, i).map(Cell::Bytes))
            .unwrap_or(Cell::Null);
        cells.push(cell);
    }
    cells
}

fn try_text(row: &tokio_postgres::Row, i: usize) -> Option<Cell> {
    row.try_get::<_, Option<String>>(i)
        .ok()
        .flatten()
        .map(Cell::Text)
}
fn try_i64(row: &tokio_postgres::Row, i: usize) -> Option<i64> {
    row.try_get::<_, Option<i64>>(i).ok().flatten()
}
fn try_i32(row: &tokio_postgres::Row, i: usize) -> Option<i32> {
    row.try_get::<_, Option<i32>>(i).ok().flatten()
}
fn try_bool(row: &tokio_postgres::Row, i: usize) -> Option<bool> {
    row.try_get::<_, Option<bool>>(i).ok().flatten()
}
fn try_bytes(row: &tokio_postgres::Row, i: usize) -> Option<Vec<u8>> {
    row.try_get::<_, Option<Vec<u8>>>(i).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime shaped like the one [`PgSimpleClient`] owns.
    fn driver_runtime() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("driver runtime")
    }

    /// PG-01 regression: the driver used to call `Runtime::block_on` unconditionally, which
    /// panics with "cannot block the current thread from within a runtime" exactly here —
    /// on a Tokio worker, i.e. on the harness `RemoteFilesystem` path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_on_works_from_inside_a_multi_thread_runtime() {
        let runtime = driver_runtime();
        assert_eq!(block_on(&runtime, async { 1u32 }), 1);
        // The worker must still be usable afterwards (`block_in_place` restored it).
        assert_eq!(block_on(&runtime, async { 2u32 }), 2);
        let joined = tokio::spawn(async { 3u32 }).await.expect("worker survived");
        assert_eq!(joined, 3);
    }

    /// `block_in_place` is illegal on a current-thread runtime, so that branch drives the
    /// future on the driver runtime from a scoped thread instead of deadlocking the worker.
    #[tokio::test]
    async fn block_on_works_from_inside_a_current_thread_runtime() {
        let runtime = driver_runtime();
        assert_eq!(block_on(&runtime, async { 41u32 }), 41);
    }

    /// Plain threads (and the `spawn_blocking` pool, which has no ambient context) keep
    /// using the driver runtime directly.
    #[test]
    fn block_on_works_without_an_ambient_runtime() {
        let runtime = driver_runtime();
        assert_eq!(block_on(&runtime, async { 7u32 }), 7);
    }
}
