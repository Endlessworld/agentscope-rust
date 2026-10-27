//! `JdbcRemoteSnapshotClient` / `JdbcSnapshotSpec` offline tests, mirroring the Java
//! `JdbcRemoteSnapshotClientH2Test` and the snapshot half of `JdbcDistributedStoreH2Test`.
//!
//! Backend: in-memory SQLite (`RusqliteExecutor::in_memory()`) with `SqliteDialect`.

use std::sync::Arc;

use agentscope_extensions_jdbc::dialect::vendor::SqliteDialect;
use agentscope_extensions_jdbc::dialect::AbstractJdbcDialect;
use agentscope_extensions_jdbc::{JdbcRemoteSnapshotClient, JdbcSnapshotSpec, RusqliteExecutor};
use agentscope_harness::agent::sandbox::snapshot::{RemoteSnapshotClient, SandboxSnapshotSpec};

fn new_client() -> JdbcRemoteSnapshotClient {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    JdbcRemoteSnapshotClient::with_schema(executor, dialect).unwrap()
}

// ---------------------------------------------------------------------------
//  Java: JdbcRemoteSnapshotClientH2Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_download_round_trip() {
    // Java: JdbcRemoteSnapshotClientH2Test.uploadDownloadRoundTrip
    let client = new_client();
    let data = b"workspace tar archive content";
    client.upload("snap-1", data).await.unwrap();

    let downloaded = client.download("snap-1").await.unwrap();
    assert_eq!(downloaded, data);
}

#[tokio::test]
async fn upload_overwrites_existing() {
    // Java: JdbcRemoteSnapshotClientH2Test.uploadOverwritesExisting
    let client = new_client();
    client.upload("snap-1", b"old").await.unwrap();
    client.upload("snap-1", b"new").await.unwrap();

    assert_eq!(client.download("snap-1").await.unwrap(), b"new");
}

#[tokio::test]
async fn exists_true_after_upload() {
    // Java: JdbcRemoteSnapshotClientH2Test.existsTrueAfterUpload
    let client = new_client();
    client.upload("snap-1", &[1, 2, 3]).await.unwrap();
    assert!(client.exists("snap-1").await.unwrap());
}

#[tokio::test]
async fn exists_false_for_missing() {
    // Java: JdbcRemoteSnapshotClientH2Test.existsFalseForMissing
    let client = new_client();
    assert!(!client.exists("nonexistent").await.unwrap());
}

#[tokio::test]
async fn download_missing_errors() {
    // Java: JdbcRemoteSnapshotClientH2Test.downloadMissingThrows (FileNotFoundException).
    // Rust surfaces a `agentscope_core::error::Error::Tool` — the crate has no
    // FileNotFoundException type.
    let client = new_client();
    assert!(client.download("missing").await.is_err());
}

#[tokio::test]
async fn download_large_binary_round_trips() {
    // Java: JdbcRemoteSnapshotClientH2Test.downloadLargeBinary
    let client = new_client();
    let large: Vec<u8> = (0..10_000usize).map(|i| (i % 256) as u8).collect();
    client.upload("large-snap", &large).await.unwrap();

    assert_eq!(client.download("large-snap").await.unwrap(), large);
}

// ---------------------------------------------------------------------------
//  JdbcSnapshotSpec — id validation (delegated to harness RemoteSnapshotSpec)
//  Java: JdbcDistributedStoreH2Test.snapshotSpecWorks / snapshotUploadDownload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_spec_builds_a_remote_snapshot() {
    // Java: JdbcDistributedStoreH2Test.snapshotSpecWorks
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let spec = JdbcSnapshotSpec::new(executor, dialect).unwrap();

    let snapshot = spec.build("test-snap").unwrap();
    assert_eq!(snapshot.id(), "test-snap");
    assert_eq!(snapshot.snapshot_type(), "remote");
}

#[tokio::test]
async fn snapshot_spec_client_round_trips_data() {
    // Java: JdbcDistributedStoreH2Test.snapshotUploadDownload (via spec.getClient())
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let spec = JdbcSnapshotSpec::new(executor, dialect).unwrap();

    let client = spec.client();
    let data = b"test archive";
    client.upload("facade-snap", data).await.unwrap();
    assert_eq!(client.download("facade-snap").await.unwrap(), data);
}

#[tokio::test]
async fn snapshot_spec_rejects_blank_and_unsafe_ids() {
    // Java: harness RemoteSnapshotSpec.validateSnapshotId (reused by JdbcSnapshotSpec)
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let spec = JdbcSnapshotSpec::new(executor, dialect).unwrap();

    assert!(spec.build("").is_err(), "blank id must be rejected");
    assert!(spec.build("  ").is_err(), "whitespace id must be rejected");
    assert!(
        spec.build("../escape").is_err(),
        "path traversal must be rejected"
    );
    assert!(
        spec.build("a/b").is_err(),
        "path separator must be rejected"
    );
    assert!(spec.build("a\\b").is_err(), "backslash must be rejected");
    assert!(spec.build("valid-id_1").is_ok());
}

#[tokio::test]
async fn snapshot_spec_snapshot_persists_through_the_client() {
    // Java: RemoteSandboxSnapshot.persist/restore delegate to the client (harness-defined).
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let spec = JdbcSnapshotSpec::new(executor, dialect).unwrap();
    let snapshot = spec.build("sess-a").unwrap();

    assert!(!snapshot.is_restorable().await.unwrap());
    snapshot.persist(b"archive").await.unwrap();
    assert!(snapshot.is_restorable().await.unwrap());
    assert_eq!(snapshot.restore().await.unwrap(), b"archive");
}
