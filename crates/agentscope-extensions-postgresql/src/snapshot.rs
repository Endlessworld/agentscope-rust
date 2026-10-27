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
        let sql = format!(
            "SELECT data FROM {} WHERE snapshot_id = $1",
            self.full_table
        );
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
        Ok(Self(postgres_snapshot_spec_with_table(
            client, table, true,
        )?))
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

#[cfg(test)]
mod tests {
    //! Offline unit tests mirroring `PostgresRemoteSnapshotClientTest` (JUnit5 + Mockito +
    //! H2 on the Java side). `MemoryClient` stores `BYTEA` blobs in a `HashMap`, so the
    //! upload/download/exists round-trips run without a live PostgreSQL server.

    use super::*;
    use crate::memory::MemoryClient;

    fn client() -> Arc<MemoryClient> {
        Arc::new(MemoryClient::new())
    }

    fn remote(c: &Arc<MemoryClient>, init: bool) -> PostgresRemoteSnapshotClient {
        PostgresRemoteSnapshotClient::new(c.clone(), init).unwrap()
    }

    // Java: PostgresRemoteSnapshotClientTest#constructorWithDefaultTableInitializesSchema
    #[test]
    fn constructor_initializes_default_table() {
        let c = client();
        let r = remote(&c, true);
        assert_eq!(r.table_name(), DEFAULT_TABLE);
        assert!(c.has_table(DEFAULT_TABLE));
    }

    // Java: PostgresRemoteSnapshotClientTest#constructorWithCustomTableInitializesSchema
    #[test]
    fn constructor_initializes_custom_table() {
        let c = client();
        let r =
            PostgresRemoteSnapshotClient::with_table(c.clone(), "custom_snapshots", true).unwrap();
        assert_eq!(r.table_name(), "custom_snapshots");
        assert!(c.has_table("custom_snapshots"));
    }

    // Java: PostgresRemoteSnapshotClientTest#constructorDoesNotInitializeSchemaWhenFalse
    #[test]
    fn constructor_skips_schema_init_when_false() {
        let c = client();
        let _ = remote(&c, false);
        assert!(!c.has_table(DEFAULT_TABLE));
        assert!(c.ddl_statements().is_empty());
    }

    // Java: PostgresRemoteSnapshotClientTest#constructorRejectsNullDataSource (Rust: a blank
    // / invalid table name is the analogue of the rejected null data source).
    #[test]
    fn constructor_rejects_invalid_table_name() {
        let c = client();
        assert!(PostgresRemoteSnapshotClient::with_table(c.clone(), "bad name", false).is_err());
        assert!(PostgresRemoteSnapshotClient::with_table(c.clone(), "1bad", false).is_err());
        assert!(PostgresRemoteSnapshotClient::with_table(c, "", false).is_err());
    }

    // Java: PostgresRemoteSnapshotClientTest#uploadReadsStreamAndExecutesUpsert,
    //       #downloadReturnsByteArrayInputStream
    #[tokio::test]
    async fn upload_then_download_roundtrip() {
        let c = client();
        let r = remote(&c, true);
        let data = b"snapshot-data".to_vec();
        r.upload("snap-1", &data).await.unwrap();
        assert_eq!(r.download("snap-1").await.unwrap(), data);
        assert_eq!(c.snapshot_row_count(), 1);
    }

    // Java: PostgresRemoteSnapshotClientTest#uploadReadsStreamAndExecutesUpsert (ON CONFLICT
    // DO UPDATE: re-uploading the same id overwrites in place rather than duplicating).
    #[tokio::test]
    async fn upload_overwrites_existing_snapshot() {
        let c = client();
        let r = remote(&c, true);
        r.upload("snap-1", b"first").await.unwrap();
        r.upload("snap-1", b"second-longer").await.unwrap();
        assert_eq!(c.snapshot_row_count(), 1);
        assert_eq!(r.download("snap-1").await.unwrap(), b"second-longer");
    }

    // Java: PostgresRemoteSnapshotClientTest#uploadRejectsNullSnapshotId
    #[tokio::test]
    async fn upload_rejects_blank_snapshot_id() {
        let c = client();
        let r = remote(&c, false);
        assert!(r.upload("", b"x").await.is_err());
        assert_eq!(c.snapshot_row_count(), 0);
    }

    // Java: PostgresRemoteSnapshotClientTest#downloadThrowsWhenSnapshotMissing
    #[tokio::test]
    async fn download_missing_snapshot_is_error() {
        let c = client();
        let r = remote(&c, true);
        let err = r.download("absent").await.unwrap_err();
        assert!(err.to_string().contains("Snapshot not found"));
    }

    // Java: PostgresRemoteSnapshotClientTest#existsReturnsTrue, #existsReturnsFalse
    #[tokio::test]
    async fn exists_reflects_stored_snapshots() {
        let c = client();
        let r = remote(&c, true);
        assert!(!r.exists("snap-1").await.unwrap());
        r.upload("snap-1", b"data").await.unwrap();
        assert!(r.exists("snap-1").await.unwrap());
    }

    // Java: PostgresRemoteSnapshotClientTest#initSchemaFailureIsLogged (Java tolerates init
    // failure; the Rust constructor propagates it — see the gap tracker §4.2).
    #[test]
    fn initialize_snapshot_table_is_idempotent() {
        let c = client();
        let handle = tokio::runtime::Runtime::new().unwrap();
        handle.block_on(async {
            initialize_snapshot_table(c.clone(), DEFAULT_TABLE)
                .await
                .unwrap();
            initialize_snapshot_table(c.clone(), DEFAULT_TABLE)
                .await
                .unwrap();
        });
        assert!(c.has_table(DEFAULT_TABLE));
    }

    #[test]
    fn initialize_snapshot_table_rejects_invalid_name() {
        let c = client();
        let handle = tokio::runtime::Runtime::new().unwrap();
        let err = handle
            .block_on(initialize_snapshot_table(c, "bad-name"))
            .unwrap_err();
        assert!(matches!(err, PostgresError::InvalidIdentifier(_)));
    }

    // Java: PostgresRemoteSnapshotClientTest#snapshotSpecWithCustomTableName
    #[test]
    fn snapshot_spec_helpers_build_expected_shape() {
        let c = client();
        let spec = postgres_snapshot_spec(c.clone(), true).unwrap();
        assert!(spec.build("snap-1").is_ok());
        assert!(c.has_table(DEFAULT_TABLE));

        let c2 = client();
        let _ = postgres_snapshot_spec_with_table(c2.clone(), "custom_snapshots", true).unwrap();
        assert!(c2.has_table("custom_snapshots"));

        let c3 = client();
        let wrapped = PostgresSnapshotSpec::new(c3.clone()).unwrap();
        let _ = wrapped.inner();
        assert!(c3.has_table(DEFAULT_TABLE));
    }

    // Java: the same test class exercises the spec's `build` path indirectly.
    #[tokio::test]
    async fn snapshot_spec_builds_persisting_snapshot() {
        let c = client();
        let spec = PostgresSnapshotSpec::with_table(c.clone(), "custom_snapshots").unwrap();
        let snap = spec.build("session-1").unwrap();
        assert_eq!(snap.id(), "session-1");
        assert!(!snap.is_restorable().await.unwrap());
        snap.persist(b"archive-bytes").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());
        assert_eq!(snap.restore().await.unwrap(), b"archive-bytes");
    }

    // Java: RemoteSnapshotSpec validates snapshot ids before binding them.
    #[test]
    fn snapshot_spec_rejects_unsafe_ids() {
        let c = client();
        let spec = PostgresSnapshotSpec::new(c).unwrap();
        assert!(spec.build("../escape").is_err());
        assert!(spec.build("  ").is_err());
        assert!(spec.build("ok-id").is_ok());
    }
}
