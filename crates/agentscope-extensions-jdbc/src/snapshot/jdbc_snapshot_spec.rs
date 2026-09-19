//! Convenience [`SandboxSnapshotSpec`] for JDBC-backed snapshot storage, mirroring
//! `JdbcSnapshotSpec`.
//!
//! Delegates to harness [`RemoteSnapshotSpec`] with a [`JdbcRemoteSnapshotClient`], so
//! snapshot id validation and the `RemoteSandboxSnapshot` construction are entirely
//! harness-defined.

use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_harness::agent::sandbox::snapshot::{
    RemoteSnapshotSpec, SandboxSnapshot, SandboxSnapshotSpec,
};

use crate::backend::{JdbcResult, SharedExecutor};
use crate::dialect::AbstractJdbcDialect;

use super::JdbcRemoteSnapshotClient;

/// Factory that creates JDBC-backed sandbox snapshots.
#[derive(Debug)]
pub struct JdbcSnapshotSpec {
    inner: RemoteSnapshotSpec,
}

impl Clone for JdbcSnapshotSpec {
    fn clone(&self) -> Self {
        Self {
            inner: RemoteSnapshotSpec::new(self.inner.client().clone()),
        }
    }
}

impl JdbcSnapshotSpec {
    /// Creates a spec with auto table creation.
    pub fn new(executor: SharedExecutor, dialect: Arc<dyn AbstractJdbcDialect>) -> JdbcResult<Self> {
        Self::with_schema_control(executor, dialect, true)
    }

    /// Creates a spec with explicit table-creation control.
    pub fn with_schema_control(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
        initialize_schema: bool,
    ) -> JdbcResult<Self> {
        let client = JdbcRemoteSnapshotClient::new(executor, dialect, initialize_schema)?;
        Ok(Self {
            inner: RemoteSnapshotSpec::new(Arc::new(client)),
        })
    }

    /// The underlying [`RemoteSnapshotClient`], mirroring the Java `getClient()`.
    pub fn client(&self) -> &Arc<dyn agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient> {
        self.inner.client()
    }
}

impl SandboxSnapshotSpec for JdbcSnapshotSpec {
    fn build(&self, snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>> {
        // Snapshot id validation is delegated to the harness RemoteSnapshotSpec.
        self.inner.build(snapshot_id)
    }
}
