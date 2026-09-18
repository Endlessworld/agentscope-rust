//! PostgreSQL-backed sandbox snapshots.
//!
//! Mirrors the Java `PostgresRemoteSnapshotClient` (a [`RemoteSnapshotClient`] storing
//! workspace tar archives in a `BYTEA` column) and `PostgresSnapshotSpec` (a
//! [`RemoteSnapshotSpec`] built over that client).

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::snapshot::{
    RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshot, SandboxSnapshotSpec,
};
use async_trait::async_trait;

use crate::blocking::{batch_async, query_async};
use crate::client::{Cell, PostgresClient, QueryOutcome, SqlValue};
use crate::error::PostgresError;
use crate::sql::{qualified_table, snapshots_ddl, validate_identifier};

/// Default snapshot table name.
pub const DEFAULT_TABLE: &str = "agentscope_snapshots";

/// [`RemoteSnapshotClient`] backed by a PostgreSQL `BYTEA` column.
#[derive(Debug, Clone)]
pub struct PostgresRemoteSnapshotClient {
    client: Arc<dyn PostgresClient>,
    table: String,
    full_table: String,
}

impl PostgresRemoteSnapshotClient {
    /// Creates a client over a SQL client, optionally creating the table.
    pub fn new(client: Arc<dyn PostgresClient>, initialize_schema: bool) -> Result<Self> {
        Self::with_table(client, DEFAULT_TABLE, initialize_schema)
    }

    /// Creates a client over a named table, optionally creating it.
    pub fn with_table(
        client: Arc<dyn PostgresClient>,
        table: &str,
        initialize_schema: bool,
    ) -> Result<Self> {
        let table = validate_identifier(table, "tableName").map_err(Error::from)?;
        let full_table = qualified_table(None, &table);
        if initialize_schema {
            client
                .execute_batch(&snapshots_ddl(&full_table))
                .map_err(Error::from)?;
        }
        Ok(Self {
            client,
            table,
            full_table,
        })
    }

    pub fn table_name(&self) -> &str {
        &self.table
    }
}

#[async_trait]
impl RemoteSnapshotClient for PostgresRemoteSnapshotClient {
    async fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()> {
        if snapshot_id.is_empty() {
            return Err(Error::Tool("snapshotId must not be null or blank".into()));
        }
        let sql = format!(
            "INSERT INTO {} (snapshot_id, data) VALUES ($1, $2) \
             ON CONFLICT (snapshot_id) DO UPDATE SET \
                 data = EXCLUDED.data, \
                 created_at = CURRENT_TIMESTAMP",
            self.full_table
        );
        let params = vec![SqlValue::text(snapshot_id), SqlValue::Bytes(data.to_vec())];
        query_async(Arc::clone(&self.client), sql, params)
            .await
            .map_err(Error::from)?;
        Ok(())
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>> {
        let sql = format!("SELECT data FROM {} WHERE snapshot_id = $1", self.full_table);
        let params = vec![SqlValue::text(snapshot_id)];
        let outcome = query_async(Arc::clone(&self.client), sql, params)
            .await
            .map_err(Error::from)?;
        match outcome {
            QueryOutcome::Rows(rows) => rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_bytes)
                .map(|b| b.to_vec())
                .ok_or_else(|| Error::Tool(format!("Snapshot not found: {snapshot_id}"))),
            QueryOutcome::Affected(_) => {
                Err(Error::Tool(format!("Snapshot not found: {snapshot_id}")))
            }
        }
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool> {
        let sql = format!(
            "SELECT 1 FROM {} WHERE snapshot_id = $1 LIMIT 1",
            self.full_table
        );
        let params = vec![SqlValue::text(snapshot_id)];
        let outcome = query_async(Arc::clone(&self.client), sql, params)
            .await
            .map_err(Error::from)?;
        Ok(matches!(outcome, QueryOutcome::Rows(rows) if !rows.is_empty()))
    }
}

/// Convenience [`SandboxSnapshotSpec`] for PostgreSQL-backed snapshot storage.
///
/// Wraps a [`RemoteSnapshotSpec`] over a [`PostgresRemoteSnapshotClient`], exactly like
/// the Java `PostgresSnapshotSpec extends RemoteSnapshotSpec`.
pub fn postgres_snapshot_spec(
    client: Arc<dyn PostgresClient>,
    initialize_schema: bool,
) -> Result<RemoteSnapshotSpec> {
    let remote = PostgresRemoteSnapshotClient::new(client, initialize_schema)?;
    Ok(RemoteSnapshotSpec::new(Arc::new(remote)))
}

/// Builds a PostgreSQL snapshot spec over a named table.
pub fn postgres_snapshot_spec_with_table(
    client: Arc<dyn PostgresClient>,
    table: &str,
    initialize_schema: bool,
) -> Result<RemoteSnapshotSpec> {
    let remote = PostgresRemoteSnapshotClient::with_table(client, table, initialize_schema)?;
    Ok(RemoteSnapshotSpec::new(Arc::new(remote)))
}

/// A named newtype around [`RemoteSnapshotSpec`] so callers can refer to the
/// PostgreSQL spec uniformly across the storage modules.
#[derive(Debug)]
pub struct PostgresSnapshotSpec(RemoteSnapshotSpec);

impl PostgresSnapshotSpec {
    /// Creates a spec over a SQL client (schema auto-created).
    pub fn new(client: Arc<dyn PostgresClient>) -> Result<Self> {
        Ok(Self(postgres_snapshot_spec(client, true)?))
    }

    /// Creates a spec over a named table (schema auto-created).
    pub fn with_table(client: Arc<dyn PostgresClient>, table: &str) -> Result<Self> {
        Ok(Self(postgres_snapshot_spec_with_table(client, table, true)?))
    }

    /// Access the inner [`RemoteSnapshotSpec`].
    pub fn inner(&self) -> &RemoteSnapshotSpec {
        &self.0
    }
}

impl SandboxSnapshotSpec for PostgresSnapshotSpec {
    fn build(&self, snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>> {
        self.0.build(snapshot_id)
    }
}

/// Initializes the snapshot table asynchronously (for callers not using
/// [`PostgresRemoteSnapshotClient::new`] with `initialize_schema = true`).
pub async fn initialize_snapshot_table(
    client: Arc<dyn PostgresClient>,
    table: &str,
) -> std::result::Result<(), PostgresError> {
    let table = validate_identifier(table, "tableName")?;
    let full_table = qualified_table(None, &table);
    batch_async(client, snapshots_ddl(&full_table)).await
}
