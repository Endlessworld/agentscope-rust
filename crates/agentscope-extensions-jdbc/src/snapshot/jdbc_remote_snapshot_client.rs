//! JDBC-backed [`RemoteSnapshotClient`], mirroring `JdbcRemoteSnapshotClient`.
//!
//! Stores snapshot archives in a database BLOB column, with zero inline SQL — all SQL is
//! sourced from [`SnapshotDialect`].

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient;
use async_trait::async_trait;

use crate::backend::{JdbcResult, SharedExecutor};
use crate::dialect::{validate_table_schema, AbstractJdbcDialect, BoundSql, SnapshotDialect};

/// [`RemoteSnapshotClient`] backed by a JDBC BLOB column.
#[derive(Clone)]
pub struct JdbcRemoteSnapshotClient {
    executor: SharedExecutor,
    dialect: Arc<dyn AbstractJdbcDialect>,
}

impl std::fmt::Debug for JdbcRemoteSnapshotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JdbcRemoteSnapshotClient")
            .field("kind", &self.dialect.kind())
            .finish_non_exhaustive()
    }
}

impl JdbcRemoteSnapshotClient {
    /// Creates a client, auto-creating the schema when `initialize_schema` is true.
    pub fn new(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
        initialize_schema: bool,
    ) -> JdbcResult<Self> {
        let client = Self { executor, dialect };
        if initialize_schema {
            client.init_schema()?;
        }
        Ok(client)
    }

    /// Creates a client with schema initialization enabled.
    pub fn with_schema(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
    ) -> JdbcResult<Self> {
        Self::new(executor, dialect, true)
    }

    /// Runs the dialect's idempotent DDL for the snapshots table, then validates the
    /// resulting schema (Java `TableSchemaValidator`).
    pub fn init_schema(&self) -> JdbcResult<()> {
        let ddls = self.dialect.snapshot_create_table_ddls();
        for ddl in &ddls {
            self.executor.execute(&BoundSql::no_params(ddl.clone()))?;
        }
        validate_table_schema(
            &*self.executor,
            self.dialect.as_ref(),
            &self.dialect.resolved_snapshot_table_name(),
            &ddls,
        )?;
        Ok(())
    }
}

#[async_trait]
impl RemoteSnapshotClient for JdbcRemoteSnapshotClient {
    async fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()> {
        let bound = self.dialect.snapshot_upsert(snapshot_id, data);
        self.executor
            .execute(&bound)
            .map(|_| ())
            .map_err(|e| Error::Tool(format!("snapshot upload failed for '{snapshot_id}': {e}")))
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>> {
        let bound = self.dialect.snapshot_select(snapshot_id);
        let rows = self.executor.query(&bound).map_err(|e| {
            Error::Tool(format!("snapshot download failed for '{snapshot_id}': {e}"))
        })?;
        let data = rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_blob().map(|b| b.to_vec()))
            .ok_or_else(|| Error::Tool(format!("Snapshot not found: {snapshot_id}")))?;
        Ok(data)
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool> {
        let bound = self.dialect.snapshot_exists(snapshot_id);
        self.executor
            .query(&bound)
            .map(|rows| !rows.is_empty())
            .map_err(|e| {
                Error::Tool(format!(
                    "snapshot exists check failed for '{snapshot_id}': {e}"
                ))
            })
    }
}
