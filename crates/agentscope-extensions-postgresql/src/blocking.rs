//! Bridge synchronous [`PostgresClient`] calls onto the async runtime.
//!
//! `BaseStore` is synchronous and calls the client directly. The async traits
//! (`AgentStateStore`, `RemoteSnapshotClient`) run the same blocking SQL inside
//! [`tokio::task::spawn_blocking`], so a single synchronous driver adapter serves both.

use std::sync::Arc;

use crate::client::{PostgresClient, QueryOutcome, SqlValue};
use crate::error::{PgResult, PostgresError};

/// Runs `client.query(sql, params)` on the blocking pool and awaits the result.
pub(crate) async fn query_async(
    client: Arc<dyn PostgresClient>,
    sql: String,
    params: Vec<SqlValue>,
) -> PgResult<QueryOutcome> {
    tokio::task::spawn_blocking(move || client.query(&sql, &params))
        .await
        .map_err(|e| PostgresError::Query(format!("blocking task join failed: {e}")))?
}

/// Runs `client.execute_batch(sql)` on the blocking pool and awaits the result.
pub(crate) async fn batch_async(client: Arc<dyn PostgresClient>, sql: String) -> PgResult<()> {
    tokio::task::spawn_blocking(move || client.execute_batch(&sql))
        .await
        .map_err(|e| PostgresError::Query(format!("blocking task join failed: {e}")))?
}
