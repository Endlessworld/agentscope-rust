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
use tokio_postgres::{Client, NoTls};

use crate::client::{Cell, PostgresClient, QueryOutcome, Row, SqlValue};
use crate::error::{PgResult, PostgresError};

/// A synchronous [`PostgresClient`] over a live `tokio-postgres` connection.
pub struct PgSimpleClient {
    runtime: Runtime,
    client: Client,
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| PostgresError::Query(format!("failed to build runtime: {e}")))?;
        let (client, connection) = runtime
            .block_on(async { tokio_postgres::connect(config, NoTls).await })
            .map_err(|e| PostgresError::Query(format!("connection failed: {e}")))?;
        runtime.spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!("[postgres] connection error: {e}");
            }
        });
        Ok(Arc::new(Self { runtime, client }))
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
        let owned: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> =
            params.iter().map(boxed).collect();
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            owned.iter().map(|b| b.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync)).collect();

        self.runtime.block_on(async {
            let upper = sql.trim_start().to_uppercase();
            if upper.starts_with("SELECT") {
                let rows = self
                    .client
                    .query(sql, &refs)
                    .await
                    .map_err(|e| PostgresError::Query(e.to_string()))?;
                let mut out: Vec<Row> = Vec::with_capacity(rows.len());
                for row in rows {
                    out.push(decode_row(&row));
                }
                Ok(QueryOutcome::Rows(out))
            } else {
                let affected = self
                    .client
                    .execute(sql, &refs)
                    .await
                    .map_err(|e| PostgresError::Query(e.to_string()))?;
                Ok(QueryOutcome::Affected(affected as u64))
            }
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
    row.try_get::<_, Option<String>>(i).ok().flatten().map(Cell::Text)
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
