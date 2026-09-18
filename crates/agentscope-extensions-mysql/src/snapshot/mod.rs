//! JDBC/MySQL-backed sandbox snapshots, mirroring
//! `io.agentscope.extensions.mysql.snapshot`.
//!
//! The Java module stores workspace tar archives as BLOBs in a table with columns
//! `(snapshot_id VARCHAR PK, data LONGBLOB, created_at TIMESTAMP)`. On the Rust side this
//! is expressed as a [`RemoteSnapshotClient`] (the harness abstraction that
//! `RemoteSandboxSnapshot` delegates to), wrapped by `MysqlSnapshotSpec` — the required
//! `XxxSnapshotSpec` produced by this crate.

use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::snapshot::{
    RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshotSpec,
};

/// Default snapshot table name, mirroring `DEFAULT_TABLE`.
pub const DEFAULT_SNAPSHOT_TABLE: &str = "agentscope_snapshots";

/// Low-level blob persistence surface used by [`MysqlRemoteSnapshotClient`].
pub trait SnapshotBackend: Send + Sync + std::fmt::Debug {
    /// Upserts the archive bytes for `snapshot_id`.
    fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()>;

    /// Downloads the archive bytes for `snapshot_id`.
    fn download(&self, snapshot_id: &str) -> Result<Vec<u8>>;

    /// Whether a snapshot with `snapshot_id` exists.
    fn exists(&self, snapshot_id: &str) -> Result<bool>;
}

/// [`RemoteSnapshotClient`] backed by JDBC BLOB storage, mirroring
/// `JdbcRemoteSnapshotClient`.
#[derive(Debug)]
pub struct MysqlRemoteSnapshotClient {
    backend: Arc<dyn SnapshotBackend>,
    table_name: String,
}

impl MysqlRemoteSnapshotClient {
    /// Creates a client over the given backend with the default table name.
    pub fn new(backend: Arc<dyn SnapshotBackend>) -> Self {
        Self {
            backend,
            table_name: DEFAULT_SNAPSHOT_TABLE.to_string(),
        }
    }

    /// Creates a client with an explicit table name.
    pub fn with_table(backend: Arc<dyn SnapshotBackend>, table_name: impl Into<String>) -> Self {
        let table_name = table_name.into();
        Self {
            backend,
            table_name: if table_name.trim().is_empty() {
                DEFAULT_SNAPSHOT_TABLE.to_string()
            } else {
                table_name
            },
        }
    }

    /// Table name this client writes to.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}

#[async_trait]
impl RemoteSnapshotClient for MysqlRemoteSnapshotClient {
    async fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()> {
        self.backend
            .upload(snapshot_id, data)
            .map_err(|e| Error::Tool(format!("mysql snapshot upload failed: {e}")))
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>> {
        self.backend
            .download(snapshot_id)
            .map_err(|e| Error::Tool(format!("mysql snapshot download failed: {e}")))
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool> {
        self.backend
            .exists(snapshot_id)
            .map_err(|e| Error::Tool(format!("mysql snapshot exists check failed: {e}")))
    }
}

/// Convenience [`SandboxSnapshotSpec`] for JDBC-backed snapshot storage, mirroring
/// `JdbcSnapshotSpec`.
///
/// Wraps the harness [`RemoteSnapshotSpec`] so snapshot ids are validated by the harness
/// (per `EXTENSIONS_GAP_TRACKER.md` §2.1 — do not reinvent snapshot-id validation).
#[derive(Debug)]
pub struct MysqlSnapshotSpec {
    inner: RemoteSnapshotSpec,
}

impl MysqlSnapshotSpec {
    /// Builds a spec over the given blob backend with the default table.
    pub fn new(backend: Arc<dyn SnapshotBackend>) -> Self {
        let client = Arc::new(MysqlRemoteSnapshotClient::new(backend));
        Self {
            inner: RemoteSnapshotSpec::new(client),
        }
    }

    /// Builds a spec over the given blob backend with an explicit table name.
    pub fn with_table(backend: Arc<dyn SnapshotBackend>, table_name: impl Into<String>) -> Self {
        let client = Arc::new(MysqlRemoteSnapshotClient::with_table(backend, table_name));
        Self {
            inner: RemoteSnapshotSpec::new(client),
        }
    }

    /// Wraps an existing [`RemoteSnapshotSpec`].
    pub fn from_remote(inner: RemoteSnapshotSpec) -> Self {
        Self { inner }
    }
}

impl SandboxSnapshotSpec for MysqlSnapshotSpec {
    fn build(
        &self,
        snapshot_id: &str,
    ) -> Result<Arc<dyn agentscope_harness::agent::sandbox::snapshot::SandboxSnapshot>> {
        self.inner.build(snapshot_id)
    }
}

#[cfg(test)]
pub mod testing;

#[cfg(test)]
mod tests {
    use super::testing::InMemorySnapshotBackend;
    use super::{
        MysqlRemoteSnapshotClient, MysqlSnapshotSpec, DEFAULT_SNAPSHOT_TABLE,
    };
    use agentscope_harness::agent::sandbox::snapshot::{SandboxSnapshot, SandboxSnapshotSpec};
    use std::sync::Arc;

    fn spec() -> MysqlSnapshotSpec {
        MysqlSnapshotSpec::new(Arc::new(InMemorySnapshotBackend::default()))
    }

    #[tokio::test]
    async fn persist_then_restore_roundtrip() {
        let spec = spec();
        let snap = spec.build("sess-1").expect("build");
        assert!(!snap.is_restorable().await.unwrap());
        snap.persist(b"hello-archive").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());
        assert_eq!(snap.restore().await.unwrap(), b"hello-archive");
        assert_eq!(snap.snapshot_type(), "remote");
        assert_eq!(snap.id(), "sess-1");
    }

    #[tokio::test]
    async fn missing_snapshot_is_not_restorable() {
        let spec = spec();
        let snap = spec.build("absent").expect("build");
        assert!(!snap.is_restorable().await.unwrap());
    }

    #[test]
    fn build_rejects_unsafe_snapshot_id() {
        let spec = spec();
        assert!(spec.build("../escape").is_err());
        assert!(spec.build("").is_err());
    }

    #[test]
    fn default_table_name_applied() {
        let backend = Arc::new(InMemorySnapshotBackend::default());
        let client = MysqlRemoteSnapshotClient::new(backend);
        assert_eq!(client.table_name(), DEFAULT_SNAPSHOT_TABLE);
        let backend = Arc::new(InMemorySnapshotBackend::default());
        let client = MysqlRemoteSnapshotClient::with_table(backend, "  ");
        assert_eq!(client.table_name(), DEFAULT_SNAPSHOT_TABLE);
        let backend = Arc::new(InMemorySnapshotBackend::default());
        let client = MysqlRemoteSnapshotClient::with_table(backend, "custom_snaps");
        assert_eq!(client.table_name(), "custom_snaps");
        let _ = Arc::new(InMemorySnapshotBackend::default());
    }
}
