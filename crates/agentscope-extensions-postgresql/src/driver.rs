//! Real `tokio-postgres`-backed [`PostgresClient`] adapter (feature `postgres-driver`).
//!
//! [`PostgresClient`] is synchronous by design (it serves the sync `BaseStore` trait),
//! while `tokio-postgres` is async. This adapter bridges the two by driving the async
//! client on a dedicated runtime handle with `block_on`. Callers who already run inside a
//! multi-threaded runtime should be aware that this blocks the calling thread; the stores
//! wrap it in `spawn_blocking` for the async traits.
//!
//! This module is intentionally minimal: it establishes a connection and executes
//! `(sql, params)` statements, mapping `$n` bind arguments in order.

use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio_postgres::{Client, Config, NoTls};

use crate::client::{
    Cell, PostgresClient, PostgresLockSession, QueryOutcome, Row, SqlStatement, SqlValue,
};
use crate::error::{PgResult, PostgresError};

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
        let (client, connection) = runtime
            .block_on(async { parsed.connect(NoTls).await })
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
        let (client, connection) = self
            .runtime
            .block_on(async { self.config.connect(NoTls).await })
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
        self.runtime.block_on(async {
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
        self.runtime
            .block_on(execute_statement(&self.client, sql, params))
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
        self.runtime.block_on(async {
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
        self.runtime.block_on(async {
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
