//! JDBC/MySQL-backed sandbox snapshots, mirroring
//! `io.agentscope.extensions.mysql.snapshot`.
//!
//! The Java module stores workspace tar archives as BLOBs in a table with columns
//! `(snapshot_id VARCHAR PK, data LONGBLOB, created_at TIMESTAMP)`. On the Rust side this
//! is expressed as a [`RemoteSnapshotClient`] (the harness abstraction that
//! `RemoteSandboxSnapshot` delegates to), wrapped by `MysqlSnapshotSpec` — the required
//! `XxxSnapshotSpec` produced by this crate.

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::snapshot::{
    RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshotSpec,
};
use async_trait::async_trait;

use crate::error::MysqlResult;
use crate::identifier::validate_identifier;

/// Default snapshot table name, mirroring `DEFAULT_TABLE`.
pub const DEFAULT_SNAPSHOT_TABLE: &str = "agentscope_snapshots";

/// DDL for the snapshot BLOB table, mirroring `JdbcRemoteSnapshotClient#initSchema`.
///
/// `snapshot_id` is caller-supplied, so it pins `utf8mb4_bin` — the table default
/// (`utf8mb4` / case-insensitive collation) would make ids differing only in letter case
/// collide, and the second upload would silently overwrite the first. `ROW_FORMAT=DYNAMIC`
/// keeps the 512-character primary key under InnoDB's index limit. Both choices copy the
/// Java statement verbatim; `utf8mb4_bin` is also what this crate's `store` key columns use.
pub fn snapshot_table_ddl(table_name: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table_name} (\
         snapshot_id VARCHAR(512) COLLATE utf8mb4_bin NOT NULL PRIMARY KEY, \
         data LONGBLOB NOT NULL, \
         created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP\
         ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 ROW_FORMAT=DYNAMIC"
    )
}

/// Low-level blob persistence surface used by [`MysqlRemoteSnapshotClient`].
pub trait SnapshotBackend: Send + Sync + std::fmt::Debug {
    /// Executes a DDL statement (schema bootstrap). Mirrors the
    /// `Connection.createStatement().execute(ddl)` call in Java `initSchema`.
    fn execute_ddl(&self, sql: &str) -> Result<()>;

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
    initialize_schema: bool,
}

impl MysqlRemoteSnapshotClient {
    /// Creates a client over the given backend with the default table name.
    ///
    /// Equivalent to Java's `initializeSchema = false`: no DDL is issued.
    pub fn new(backend: Arc<dyn SnapshotBackend>) -> Self {
        Self::build(backend, DEFAULT_SNAPSHOT_TABLE, false)
            .expect("the default table name is a valid identifier")
    }

    /// Creates a client that bootstraps the default table on construction.
    pub fn new_with_schema(backend: Arc<dyn SnapshotBackend>) -> Self {
        Self::build(backend, DEFAULT_SNAPSHOT_TABLE, true)
            .expect("the default table name is a valid identifier")
    }

    /// Creates a client with an explicit table name.
    ///
    /// The name is spliced into SQL verbatim, so it must match `[A-Za-z_][A-Za-z0-9_]*`
    /// (max 64 characters). A blank name falls back to [`DEFAULT_SNAPSHOT_TABLE`]; anything
    /// else invalid is rejected instead of reaching a statement.
    ///
    /// Equivalent to Java's `initializeSchema = false`: no DDL is issued.
    pub fn with_table(
        backend: Arc<dyn SnapshotBackend>,
        table_name: impl Into<String>,
    ) -> MysqlResult<Self> {
        Self::build(backend, table_name, false)
    }

    /// Creates a client with an explicit table name that bootstraps the table on
    /// construction (`initializeSchema = true`, what `JdbcSnapshotSpec` always passes).
    pub fn with_table_and_schema(
        backend: Arc<dyn SnapshotBackend>,
        table_name: impl Into<String>,
    ) -> MysqlResult<Self> {
        Self::build(backend, table_name, true)
    }

    fn build(
        backend: Arc<dyn SnapshotBackend>,
        table_name: impl Into<String>,
        initialize_schema: bool,
    ) -> MysqlResult<Self> {
        let raw: String = table_name.into();
        let table_name = if raw.trim().is_empty() {
            DEFAULT_SNAPSHOT_TABLE.to_string()
        } else {
            validate_identifier(&raw, "Snapshot table name")?
        };
        let client = Self {
            backend,
            table_name,
            initialize_schema,
        };
        client.init_schema();
        Ok(client)
    }

    /// Issues the snapshot-table DDL when `initializeSchema` is on.
    ///
    /// Mirrors Java: a failing `CREATE TABLE` is **only logged** (`log.warn`) — the client
    /// is still usable, and the failure surfaces later as a failed upload/download rather
    /// than as a constructor error.
    fn init_schema(&self) {
        if !self.initialize_schema {
            return;
        }
        if let Err(err) = self
            .backend
            .execute_ddl(&snapshot_table_ddl(&self.table_name))
        {
            tracing::warn!(
                table = %self.table_name,
                error = %err,
                "failed to initialize snapshot table"
            );
        }
    }

    /// Table name this client writes to.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Whether this client bootstraps the table on construction.
    pub fn initialize_schema(&self) -> bool {
        self.initialize_schema
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
    ///
    /// Mirrors `JdbcSnapshotSpec`: the snapshot table DDL is always issued on construction
    /// (Java passes `initializeSchema = true`). A failing DDL is only logged, matching the
    /// Java `log.warn` in `initSchema`.
    pub fn new(backend: Arc<dyn SnapshotBackend>) -> Self {
        let client = Arc::new(MysqlRemoteSnapshotClient::new_with_schema(backend));
        Self {
            inner: RemoteSnapshotSpec::new(client),
        }
    }

    /// Builds a spec over the given blob backend with an explicit table name.
    pub fn with_table(
        backend: Arc<dyn SnapshotBackend>,
        table_name: impl Into<String>,
    ) -> MysqlResult<Self> {
        let client = Arc::new(MysqlRemoteSnapshotClient::with_table_and_schema(
            backend, table_name,
        )?);
        Ok(Self {
            inner: RemoteSnapshotSpec::new(client),
        })
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
        snapshot_table_ddl, MysqlRemoteSnapshotClient, MysqlSnapshotSpec, DEFAULT_SNAPSHOT_TABLE,
    };
    use agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec;
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
        let client = MysqlRemoteSnapshotClient::with_table(backend, "  ")
            .expect("blank falls back to the default table");
        assert_eq!(client.table_name(), DEFAULT_SNAPSHOT_TABLE);
        let backend = Arc::new(InMemorySnapshotBackend::default());
        let client = MysqlRemoteSnapshotClient::with_table(backend, "custom_snaps")
            .expect("valid identifier");
        assert_eq!(client.table_name(), "custom_snaps");
    }

    #[test]
    fn with_table_rejects_unsafe_identifiers() {
        for bad in [
            "agentscope_snapshots; DROP TABLE x",
            "`agentscope_snapshots`",
            "snaps--",
            "agent.snaps",
            "snaps table",
        ] {
            let backend = Arc::new(InMemorySnapshotBackend::default());
            assert!(
                MysqlRemoteSnapshotClient::with_table(backend, bad).is_err(),
                "expected rejection of {bad:?}"
            );
        }
        let backend = Arc::new(InMemorySnapshotBackend::default());
        assert!(MysqlSnapshotSpec::with_table(backend, "bad name").is_err());
        let backend = Arc::new(InMemorySnapshotBackend::default());
        assert!(MysqlRemoteSnapshotClient::with_table(backend, "a".repeat(65)).is_err());
    }

    // ---------------------------------------------------------------------------------
    // P1-7: snapshot table auto-creation (Java `JdbcRemoteSnapshotClient#initSchema`)
    // ---------------------------------------------------------------------------------

    /// The DDL must mirror Java verbatim: binary collation on the caller-supplied key
    /// column (case-insensitive would make `A` and `a` collide), LONGBLOB payload and
    /// `ROW_FORMAT=DYNAMIC` for the 512-char primary key.
    #[test]
    fn snapshot_ddl_matches_the_java_statement() {
        let ddl = snapshot_table_ddl("agentscope_snapshots");
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS agentscope_snapshots ("));
        assert!(ddl.contains("snapshot_id VARCHAR(512) COLLATE utf8mb4_bin NOT NULL PRIMARY KEY"));
        assert!(ddl.contains("data LONGBLOB NOT NULL"));
        assert!(ddl.contains("created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP"));
        assert!(ddl.contains("ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 ROW_FORMAT=DYNAMIC"));
    }

    /// `JdbcSnapshotSpec` always passes `initializeSchema = true`.
    #[test]
    fn spec_issues_the_table_ddl_on_construction() {
        let backend = Arc::new(InMemorySnapshotBackend::default());
        let _spec = MysqlSnapshotSpec::new(backend.clone());
        assert_eq!(
            backend.ddl_log(),
            vec![snapshot_table_ddl(DEFAULT_SNAPSHOT_TABLE)]
        );
    }

    #[test]
    fn spec_with_table_issues_ddl_for_the_custom_table() {
        let backend = Arc::new(InMemorySnapshotBackend::default());
        let _spec = MysqlSnapshotSpec::with_table(backend.clone(), "custom_snaps")
            .expect("valid identifier");
        assert_eq!(backend.ddl_log(), vec![snapshot_table_ddl("custom_snaps")]);
    }

    /// Java catches the `SQLException` and calls `log.warn` only — construction must not
    /// fail, and the store stays usable.
    #[tokio::test]
    async fn ddl_failure_is_logged_not_raised() {
        let backend = Arc::new(InMemorySnapshotBackend::default());
        backend.fail_ddl(true);
        let spec = MysqlSnapshotSpec::new(backend.clone());
        assert!(backend.ddl_log().is_empty());
        let snap = spec.build("sess-1").expect("build");
        snap.persist(b"archive").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());
        assert_eq!(snap.restore().await.unwrap(), b"archive");
    }

    /// `initializeSchema = false` (Java's explicit opt-out) must not touch the schema.
    #[test]
    fn client_without_initialize_schema_emits_no_ddl() {
        let backend = Arc::new(InMemorySnapshotBackend::default());
        let client = MysqlRemoteSnapshotClient::new(backend.clone());
        assert!(!client.initialize_schema());
        assert!(backend.ddl_log().is_empty());

        let backend = Arc::new(InMemorySnapshotBackend::default());
        let client = MysqlRemoteSnapshotClient::with_table(backend.clone(), "custom_snaps")
            .expect("valid identifier");
        assert!(!client.initialize_schema());
        assert!(backend.ddl_log().is_empty());

        let backend = Arc::new(InMemorySnapshotBackend::default());
        let client = MysqlRemoteSnapshotClient::new_with_schema(backend.clone());
        assert!(client.initialize_schema());
        assert_eq!(
            backend.ddl_log(),
            vec![snapshot_table_ddl(DEFAULT_SNAPSHOT_TABLE)]
        );
    }
}
