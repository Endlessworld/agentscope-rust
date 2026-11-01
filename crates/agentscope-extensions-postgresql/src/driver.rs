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
//! Statement routing is decided by the **server**: the statement is prepared and
//! `Statement::columns()` decides between `query` (result set) and `execute` (row count),
//! so `WITH ... SELECT`, `EXPLAIN` or a leading comment cannot be mistaken for a write
//! (PG-09).
//!
//! Feature `postgres-pool` adds a bounded pool of connections for statement execution
//! (PG-05, [`ConnectionPool`]). Advisory-lock leases and write transactions keep opening
//! their own dedicated connection: PG locks are session-scoped.
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

/// The future returned by a [`ConnectionPool`] factory.
#[cfg(feature = "postgres-pool")]
type ConnectFuture<T> = std::pin::Pin<
    Box<dyn std::future::Future<Output = PgResult<T>> + Send + 'static>,
>;

/// A bounded, self-contained pool of driver connections (feature `postgres-pool`, PG-05).
///
/// Without it every statement of [`PgSimpleClient`] is serialised on one connection while
/// lock leases and write transactions open *extra* connections with no cap at all. The pool
/// hands at most `max_connections` connections to concurrent callers and parks them for
/// reuse on drop; a connection that failed at the connection level is [`discard`]ed instead.
///
/// Deliberately no `deadpool-postgres`/`bb8`: a semaphore plus an idle vector covers what
/// this crate needs, and the default build keeps pulling in no extra crates.
///
/// [`discard`]: PooledConnection::discard
#[cfg(feature = "postgres-pool")]
pub struct ConnectionPool<T> {
    max_connections: usize,
    idle: std::sync::Mutex<Vec<T>>,
    permits: Arc<tokio::sync::Semaphore>,
    factory: Box<dyn Fn() -> ConnectFuture<T> + Send + Sync>,
}

#[cfg(feature = "postgres-pool")]
impl<T> std::fmt::Debug for ConnectionPool<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionPool")
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "postgres-pool")]
impl<T: Send + 'static> ConnectionPool<T> {
    /// Creates a pool that opens new connections through `factory` up to `max_connections`.
    pub fn new(
        max_connections: usize,
        factory: impl Fn() -> ConnectFuture<T> + Send + Sync + 'static,
    ) -> Self {
        assert!(max_connections > 0, "connection pool needs a capacity");
        Self {
            max_connections,
            idle: std::sync::Mutex::new(Vec::new()),
            permits: Arc::new(tokio::sync::Semaphore::new(max_connections)),
            factory: Box::new(factory),
        }
    }

    /// Upper bound on simultaneously open connections.
    pub fn max_connections(&self) -> usize {
        self.max_connections
    }

    /// Connections currently parked for reuse.
    pub fn idle_connections(&self) -> usize {
        self.idle.lock().map(|idle| idle.len()).unwrap_or(0)
    }

    /// Borrows a connection, reusing an idle one or opening a new one. Waits while all
    /// `max_connections` are in use.
    pub async fn acquire(self: &Arc<Self>) -> PgResult<PooledConnection<T>> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|e| PostgresError::Query(format!("connection pool is closed: {e}")))?;
        let recycled = self.idle.lock().ok().and_then(|mut idle| idle.pop());
        let item = match recycled {
            Some(item) => item,
            None => (self.factory)().await?,
        };
        Ok(PooledConnection {
            pool: Arc::clone(self),
            item: Some(item),
            _permit: permit,
        })
    }
}

/// A connection borrowed from a [`ConnectionPool`]; returning it to the idle list happens on
/// drop, and dropping the permit frees the capacity slot.
#[cfg(feature = "postgres-pool")]
pub struct PooledConnection<T> {
    pool: Arc<ConnectionPool<T>>,
    item: Option<T>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(feature = "postgres-pool")]
impl<T> PooledConnection<T> {
    /// Drops the connection instead of returning it to the pool (a closed backend must not
    /// be handed to the next caller).
    pub fn discard(&mut self) {
        self.item.take();
    }
}

#[cfg(feature = "postgres-pool")]
impl<T> std::ops::Deref for PooledConnection<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.item
            .as_ref()
            .expect("a pooled connection is present until it is dropped")
    }
}

#[cfg(feature = "postgres-pool")]
impl<T> Drop for PooledConnection<T> {
    fn drop(&mut self) {
        if let Some(item) = self.item.take() {
            if let Ok(mut idle) = self.pool.idle.lock() {
                idle.push(item);
            }
        }
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
    /// Bounded connection reuse for [`PostgresClient::query`] (feature `postgres-pool`,
    /// PG-05). Lock sessions and write transactions never borrow from it.
    #[cfg(feature = "postgres-pool")]
    pool: Option<Arc<ConnectionPool<Client>>>,
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
        Ok(Arc::new(Self::open(config)?))
    }

    /// Connects with a bounded pool of at most `max_connections` connections serving
    /// [`PostgresClient::query`] (feature `postgres-pool`, PG-05).
    ///
    /// Advisory-lock sessions and write transactions still open their own connection: a lock
    /// belongs to the session that took it, so it must never ride on a shared one.
    #[cfg(feature = "postgres-pool")]
    pub fn connect_with_pool(config: &str, max_connections: usize) -> PgResult<Arc<Self>> {
        if max_connections == 0 {
            // A zero-sized pool would wait forever instead of failing.
            return Err(PostgresError::Query(
                "max_connections must be at least 1".into(),
            ));
        }
        let mut owner = Self::open(config)?;
        let cfg = owner.config.clone();
        let runtime = Arc::clone(&owner.runtime);
        let pool = ConnectionPool::new(max_connections, move || -> ConnectFuture<Client> {
            let cfg = cfg.clone();
            let runtime = Arc::clone(&runtime);
            Box::pin(async move {
                let (client, connection) = cfg
                    .connect(NoTls)
                    .await
                    .map_err(|e| PostgresError::Query(format!("pool connection failed: {e}")))?;
                runtime.spawn(async move {
                    if let Err(e) = connection.await {
                        tracing::warn!("[postgres] pooled connection error: {e}");
                    }
                });
                Ok(client)
            })
        });
        owner.pool = Some(Arc::new(pool));
        Ok(Arc::new(owner))
    }

    fn open(config: &str) -> PgResult<Self> {
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
        Ok(Self {
            runtime: Arc::new(runtime),
            client,
            config: parsed,
            #[cfg(feature = "postgres-pool")]
            pool: None,
        })
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
                .map_err(driver_error)?;
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

/// Maps a driver error to [`PostgresError`], preserving the server-reported `SQLSTATE`.
///
/// Java reads `SQLException#getSQLState()` (its duplicate-key check accepts class `23`), so the
/// code must not be folded into a message: `lc_messages` may be non-English, in which case the
/// message-based fallback in `store::is_duplicate_key` cannot classify the failure.
fn driver_error(e: tokio_postgres::Error) -> PostgresError {
    match e.code() {
        Some(code) => PostgresError::Sql {
            code: Some(code.code().to_string()),
            message: e.to_string(),
        },
        None => PostgresError::Query(e.to_string()),
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
        // PG-05: with the pool enabled, statements borrow a pooled connection instead of
        // queueing on the single one; a backend that died is not returned to the pool.
        #[cfg(feature = "postgres-pool")]
        if let Some(pool) = &self.pool {
            let pool = Arc::clone(pool);
            return block_on(&self.runtime, async move {
                let mut connection = pool.acquire().await?;
                let outcome = execute_statement(&connection, sql, params).await;
                if connection.is_closed() {
                    connection.discard();
                }
                outcome
            });
        }
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

    // PG-09: ask the server what the statement returns instead of guessing from a `SELECT`
    // prefix. `WITH x AS (SELECT ...) SELECT ...`, `EXPLAIN`, `TABLE` and statements with a
    // leading comment were routed to `execute`, so the caller got `Affected` and read an
    // empty result set. `Statement::columns()` is empty exactly when there is no result set.
    let statement = client.prepare(sql).await.map_err(driver_error)?;
    if statement.columns().is_empty() {
        let affected = client.execute(&statement, &refs).await.map_err(driver_error)?;
        return Ok(QueryOutcome::Affected(affected as u64));
    }
    let rows = client.query(&statement, &refs).await.map_err(driver_error)?;
    let mut out: Vec<Row> = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(decode_row(&row));
    }
    Ok(QueryOutcome::Rows(out))
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A runtime shaped like the one [`PgSimpleClient`] owns.
    fn driver_runtime() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("driver runtime")
    }

    /// Drops the driver runtime **off** the async context.
    ///
    /// `Runtime::drop` panics with "Cannot drop a runtime in a context where blocking is not
    /// allowed" when it happens on a runtime thread, so the test hands it to a plain thread.
    fn drop_off_runtime(runtime: Runtime) {
        std::thread::spawn(move || drop(runtime))
            .join()
            .expect("drop the driver runtime on a plain thread");
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
        drop_off_runtime(runtime);
    }

    /// `block_in_place` is illegal on a current-thread runtime, so that branch drives the
    /// future on the driver runtime from a scoped thread instead of deadlocking the worker.
    #[tokio::test]
    async fn block_on_works_from_inside_a_current_thread_runtime() {
        let runtime = driver_runtime();
        assert_eq!(block_on(&runtime, async { 41u32 }), 41);
        drop_off_runtime(runtime);
    }

    /// Plain threads (and the `spawn_blocking` pool, which has no ambient context) keep
    /// using the driver runtime directly.
    #[test]
    fn block_on_works_without_an_ambient_runtime() {
        let runtime = driver_runtime();
        assert_eq!(block_on(&runtime, async { 7u32 }), 7);
    }

    /// PG-05: the pool never opens more connections than its capacity and parks them for
    /// reuse instead of dialling again (`deadpool-postgres` is not needed for that).
    #[cfg(feature = "postgres-pool")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pool_caps_concurrent_connections_and_reuses_them() {
        let created = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(2, Arc::clone(&created));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let pool = Arc::clone(&pool);
            tasks.push(tokio::spawn(async move {
                let connection = pool.acquire().await.expect("pooled connection");
                // Hold the connection across a suspension point so 8 callers really contend.
                tokio::task::yield_now().await;
                *connection
            }));
        }
        for task in tasks {
            task.await.expect("pooled task");
        }
        assert_eq!(
            created.load(Ordering::SeqCst),
            2,
            "the pool must never open more connections than its capacity"
        );
        assert_eq!(pool.idle_connections(), 2);
        assert_eq!(pool.max_connections(), 2);
    }

    /// A connection that is `discard`ed must not be handed to the next caller.
    #[cfg(feature = "postgres-pool")]
    #[tokio::test]
    async fn discarded_connections_are_replaced_instead_of_reused() {
        let created = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(1, Arc::clone(&created));
        {
            let mut connection = pool.acquire().await.expect("first connection");
            assert_eq!(*connection, 0);
            connection.discard();
        }
        assert_eq!(pool.idle_connections(), 0);
        let connection = pool.acquire().await.expect("second connection");
        assert_eq!(*connection, 1);
    }

    /// A failed connect must release its capacity slot; leaking it would deadlock the pool
    /// (the second `acquire` would wait forever).
    #[cfg(feature = "postgres-pool")]
    #[tokio::test]
    async fn a_failed_connect_releases_the_capacity_slot() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let pool: Arc<ConnectionPool<usize>> = Arc::new(ConnectionPool::new(1, move || {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(PostgresError::Query("connection refused".into()))
            })
        }));
        assert!(pool.acquire().await.is_err());
        assert!(pool.acquire().await.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[cfg(feature = "postgres-pool")]
    fn counting_pool(max_connections: usize, created: Arc<AtomicUsize>) -> Arc<ConnectionPool<usize>> {
        Arc::new(ConnectionPool::new(max_connections, move || {
            let created = Arc::clone(&created);
            Box::pin(async move { Ok(created.fetch_add(1, Ordering::SeqCst)) })
        }))
    }
}
