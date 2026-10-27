//! MongoDB-backed snapshot persistence, mirroring
//! `io.agentscope.extensions.mongodb.snapshot.*`.
//!
//! [`MongodbRemoteSnapshotClient`] implements the harness
//! [`RemoteSnapshotClient`](agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient)
//! over GridFS-style chunked blob storage: snapshots are stored as
//! `<bucket>.files` + `<bucket>.chunks` documents, so payloads are transparently
//! chunked into 255 KB segments (the driver default) instead of being constrained
//! by the 16 MB BSON document limit. [`MongodbSnapshotSpec`] is the corresponding
//! [`SandboxSnapshotSpec`] factory.
//!
//! For backward compatibility, [`MongodbRemoteSnapshotClient::download`] and
//! [`MongodbRemoteSnapshotClient::exists`] fall back to the legacy single-document
//! representation (`{_id, data}`) when the snapshot is not present in GridFS, and
//! `delete` cleans up both representations.
//!
//! Snapshots have no independent TTL — aligned with Postgres/JDBC/Redis, which
//! retain snapshots indefinitely. A snapshot is reclaimed only by an explicit
//! delete when the owning session is removed (cascade cleanup driven by
//! [`crate::MongodbDistributedStore`]).

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::{
    RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshot, SandboxSnapshotSpec,
};
use async_trait::async_trait;

use crate::client::{decode_binary, encode_binary, FindFilter, MongoClient, MongoError};
use crate::constants::{DEFAULT_DATABASE, SNAPSHOTS_COLLECTION};

/// Log target for this module's diagnostics.
const LOG_TARGET: &str = "mongo-snapshot";

/// [`RemoteSnapshotClient`] backed by MongoDB blob storage (GridFS / legacy).
#[derive(Clone)]
pub struct MongodbRemoteSnapshotClient {
    client: Arc<dyn MongoClient>,
    /// The database name; retained for API parity with the Java constructor (the
    /// in-process backend resolves collections globally, not per database).
    database: String,
    /// The GridFS bucket / legacy collection name.
    bucket: String,
}

impl std::fmt::Debug for MongodbRemoteSnapshotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongodbRemoteSnapshotClient")
            .field("database", &self.database)
            .field("bucket", &self.bucket)
            .finish()
    }
}

impl MongodbRemoteSnapshotClient {
    /// Creates a client over the given MongoDB client, database and (optional)
    /// collection/bucket name.
    ///
    /// `database_name` defaults to [`DEFAULT_DATABASE`] and `collection_name` to
    /// [`SNAPSHOTS_COLLECTION`].
    pub fn new(
        client: Arc<dyn MongoClient>,
        database_name: Option<&str>,
        collection_name: Option<&str>,
    ) -> Self {
        Self {
            client,
            database: database_name.unwrap_or(DEFAULT_DATABASE).to_string(),
            bucket: collection_name.unwrap_or(SNAPSHOTS_COLLECTION).to_string(),
        }
    }

    /// The database name configured for this client.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// The bucket / collection name backing this client.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The database-scoped backing collection/bucket key used for client calls.
    ///
    /// The Java client binds its GridFS bucket and legacy collection to a specific
    /// `MongoDatabase`, so snapshots written under one database are invisible under
    /// another. The in-process backend keeps a flat collection map, so the database
    /// is encoded into the collection key to preserve that isolation.
    fn scoped_bucket(&self) -> String {
        crate::client::scoped_collection(&self.database, &self.bucket)
    }

    /// The backing MongoDB client.
    pub fn client(&self) -> &Arc<dyn MongoClient> {
        &self.client
    }

    fn legacy_filter(snapshot_id: &str) -> FindFilter {
        FindFilter::eq("_id", serde_json::json!(snapshot_id))
    }

    /// Whether the snapshot exists in the legacy single-document collection.
    async fn exists_legacy(&self, snapshot_id: &str) -> Result<bool> {
        let doc = self
            .client
            .find_one(&self.scoped_bucket(), &Self::legacy_filter(snapshot_id))
            .await
            .map_err(map_err)?;
        Ok(doc.is_some())
    }

    /// Reads a legacy single-document snapshot payload (`{_id, data}`).
    async fn download_legacy(&self, snapshot_id: &str) -> Result<Option<Vec<u8>>> {
        let doc = self
            .client
            .find_one(&self.scoped_bucket(), &Self::legacy_filter(snapshot_id))
            .await
            .map_err(map_err)?;
        let Some(doc) = doc else {
            return Ok(None);
        };
        let Some(raw) = doc
            .get(crate::client::LEGACY_FIELD_DATA)
            .and_then(|v| v.as_str())
        else {
            tracing::warn!(
                target = LOG_TARGET,
                snapshot_id,
                "legacy snapshot document has no data field"
            );
            return Ok(None);
        };
        match decode_binary(raw) {
            Some(data) => Ok(Some(data)),
            None => {
                tracing::warn!(
                    target = LOG_TARGET,
                    snapshot_id,
                    "legacy snapshot data is not valid base64"
                );
                Ok(None)
            }
        }
    }

    /// Deletes the legacy single-document snapshot; returns whether one was removed.
    async fn delete_legacy(&self, snapshot_id: &str) -> Result<bool> {
        self.client
            .delete_one(&self.scoped_bucket(), &Self::legacy_filter(snapshot_id))
            .await
            .map_err(map_err)
    }

    /// Deletes a snapshot; returns whether anything was removed.
    ///
    /// Mirrors Java `MongoRemoteSnapshotClient.delete`: deletes the GridFS file when
    /// present (and then also cleans up any legacy document left over from a
    /// migration), otherwise falls back to deleting the legacy document.
    ///
    /// Exposed publicly (in addition to the trait's operations) so cascade cleanup
    /// can distinguish "deleted" from "not present".
    pub async fn delete(&self, snapshot_id: &str) -> Result<bool> {
        let deleted_gridfs = self
            .client
            .gridfs_delete(&self.scoped_bucket(), snapshot_id)
            .await
            .map_err(map_err)?;
        if deleted_gridfs {
            tracing::info!(
                target = LOG_TARGET,
                snapshot_id,
                "deleted snapshot from GridFS"
            );
            // Also clean up a legacy document if it exists (migration scenario).
            let _ = self.delete_legacy(snapshot_id).await;
            return Ok(true);
        }
        self.delete_legacy(snapshot_id).await
    }

    /// Test/seeding hook: writes a legacy single-document snapshot, mirroring the
    /// pre-GridFS storage format `{_id, data: Binary}`.
    ///
    /// Used by tests to simulate databases created before the GridFS migration; not
    /// part of the production write path ([`RemoteSnapshotClient::upload`] always
    /// writes GridFS).
    pub async fn insert_legacy(&self, snapshot_id: &str, data: &[u8]) -> Result<()> {
        self.client
            .delete_one(&self.scoped_bucket(), &Self::legacy_filter(snapshot_id))
            .await
            .ok();
        let mut doc = serde_json::Map::new();
        doc.insert("_id".to_string(), serde_json::json!(snapshot_id));
        doc.insert(
            crate::client::LEGACY_FIELD_DATA.to_string(),
            serde_json::json!(encode_binary(data)),
        );
        self.client
            .insert_one(&self.scoped_bucket(), doc)
            .await
            .map_err(map_err)
    }
}

fn map_err(e: MongoError) -> Error {
    Error::Tool(format!("MongoDB snapshot operation failed: {e}"))
}

#[async_trait]
impl RemoteSnapshotClient for MongodbRemoteSnapshotClient {
    async fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()> {
        if snapshot_id.is_empty() {
            return Err(Error::Tool("snapshotId must not be empty".to_string()));
        }
        self.client
            .gridfs_upload(&self.scoped_bucket(), snapshot_id, data.to_vec())
            .await
            .map_err(map_err)
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>> {
        match self
            .client
            .gridfs_download(&self.scoped_bucket(), snapshot_id)
            .await
        {
            Ok(Some(data)) => Ok(data),
            Ok(None) => match self.download_legacy(snapshot_id).await? {
                Some(data) => {
                    tracing::debug!(
                        target = LOG_TARGET,
                        snapshot_id,
                        "downloaded snapshot from legacy collection"
                    );
                    Ok(data)
                }
                None => Err(Error::Tool(format!(
                    "Snapshot not found in MongoDB: {snapshot_id}"
                ))),
            },
            Err(e) => Err(map_err(e)),
        }
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool> {
        if self
            .client
            .gridfs_exists(&self.scoped_bucket(), snapshot_id)
            .await
            .map_err(map_err)?
        {
            return Ok(true);
        }
        self.exists_legacy(snapshot_id).await
    }
}

/// [`SandboxSnapshotSpec`] for MongoDB-backed snapshot storage, mirroring Java
/// `MongoSnapshotSpec` (which extends `RemoteSnapshotSpec`).
///
/// The inner [`RemoteSnapshotSpec`] is not `Clone`, so this spec holds the shared
/// [`MongodbRemoteSnapshotClient`] directly and builds snapshots through a per-call
/// [`RemoteSnapshotSpec`].
#[derive(Debug, Clone)]
pub struct MongodbSnapshotSpec {
    client: Arc<MongodbRemoteSnapshotClient>,
}

impl MongodbSnapshotSpec {
    /// Creates a spec over the given MongoDB client and database/collection names.
    pub fn new(
        client: Arc<dyn MongoClient>,
        database_name: Option<&str>,
        collection_name: Option<&str>,
    ) -> Self {
        Self {
            client: Arc::new(MongodbRemoteSnapshotClient::new(
                client,
                database_name,
                collection_name,
            )),
        }
    }

    /// The backing remote snapshot client.
    pub fn client(&self) -> &Arc<MongodbRemoteSnapshotClient> {
        &self.client
    }
}

impl SandboxSnapshotSpec for MongodbSnapshotSpec {
    fn build(&self, snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>> {
        let client: Arc<dyn RemoteSnapshotClient> = self.client.clone();
        RemoteSnapshotSpec::new(client).build(snapshot_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::InMemoryMongoClient;
    use serde_json::{json, Value};

    fn client() -> Arc<dyn MongoClient> {
        Arc::new(InMemoryMongoClient::new())
    }

    #[tokio::test]
    async fn upload_download_exists_delete_roundtrip() {
        let c = MongodbRemoteSnapshotClient::new(client(), Some("db"), None);
        assert!(!c.exists("snap-1").await.unwrap());
        c.upload("snap-1", b"archive").await.unwrap();
        assert!(c.exists("snap-1").await.unwrap());
        assert_eq!(c.download("snap-1").await.unwrap(), b"archive");
        assert!(c.delete("snap-1").await.unwrap());
        assert!(!c.exists("snap-1").await.unwrap());
        assert!(!c.delete("snap-1").await.unwrap());
    }

    #[tokio::test]
    async fn upload_replaces_existing() {
        let c = MongodbRemoteSnapshotClient::new(client(), None, None);
        c.upload("s", b"one").await.unwrap();
        c.upload("s", b"two").await.unwrap();
        assert_eq!(c.download("s").await.unwrap(), b"two");
    }

    #[tokio::test]
    async fn download_missing_errors() {
        let c = MongodbRemoteSnapshotClient::new(client(), None, None);
        assert!(c.download("nope").await.is_err());
    }

    #[tokio::test]
    async fn spec_builds_snapshot_and_round_trips() {
        let spec = MongodbSnapshotSpec::new(client(), Some("db"), None);
        let snap = spec.build("session-a").unwrap();
        assert_eq!(snap.id(), "session-a");
        assert_eq!(snap.snapshot_type(), "remote");
        snap.persist(b"bytes").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());
        assert_eq!(snap.restore().await.unwrap(), b"bytes");
    }

    #[tokio::test]
    async fn spec_rejects_unsafe_snapshot_id() {
        let spec = MongodbSnapshotSpec::new(client(), None, None);
        assert!(spec.build("../escape").is_err());
    }

    #[tokio::test]
    async fn database_name_is_retained() {
        let c = MongodbRemoteSnapshotClient::new(client(), None, None);
        assert_eq!(c.database(), "agentscope");
        let c = MongodbRemoteSnapshotClient::new(client(), Some("mydb"), Some("snaps"));
        assert_eq!(c.database(), "mydb");
        assert_eq!(c.bucket(), "snaps");
    }

    #[tokio::test]
    async fn gridfs_chunks_large_payload_in_255kb_segments() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let c = MongodbRemoteSnapshotClient::new(raw.clone(), Some("db"), None);
        // Four chunks: 3 full 255 KB chunks + 1 partial.
        let chunk = crate::client::GRIDFS_DEFAULT_CHUNK_SIZE;
        let payload: Vec<u8> = (0..(chunk * 3 + 7)).map(|i| (i % 251) as u8).collect();
        c.upload("big", &payload).await.unwrap();

        assert_eq!(raw.document_count(&files_collection(&c)), 1);
        assert_eq!(raw.document_count(&chunks_collection(&c)), 4);
        assert_eq!(c.download("big").await.unwrap(), payload);
    }

    #[tokio::test]
    async fn reupload_drops_stale_chunks() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let c = MongodbRemoteSnapshotClient::new(raw.clone(), None, None);
        let chunk = crate::client::GRIDFS_DEFAULT_CHUNK_SIZE;
        let files = files_collection(&c);
        let chunks = chunks_collection(&c);
        c.upload("s", &vec![1u8; chunk * 3]).await.unwrap();
        assert_eq!(raw.document_count(&chunks), 3);
        c.upload("s", b"tiny").await.unwrap();
        assert_eq!(raw.document_count(&chunks), 1);
        assert_eq!(raw.document_count(&files), 1);
        assert_eq!(c.download("s").await.unwrap(), b"tiny");
    }

    #[tokio::test]
    async fn download_and_exists_fall_back_to_legacy_document() {
        let c = MongodbRemoteSnapshotClient::new(client(), Some("db"), None);
        c.insert_legacy("legacy-1", b"legacy-workspace-tar")
            .await
            .unwrap();

        assert!(c.exists("legacy-1").await.unwrap());
        assert_eq!(
            c.download("legacy-1").await.unwrap(),
            b"legacy-workspace-tar"
        );
        // Deleting falls back to the legacy document.
        assert!(c.delete("legacy-1").await.unwrap());
        assert!(!c.exists("legacy-1").await.unwrap());
        assert!(c.download("legacy-1").await.is_err());
    }

    #[tokio::test]
    async fn gridfs_takes_precedence_over_legacy_document() {
        let c = MongodbRemoteSnapshotClient::new(client(), None, None);
        c.insert_legacy("dup", b"old").await.unwrap();
        c.upload("dup", b"new").await.unwrap();
        assert_eq!(c.download("dup").await.unwrap(), b"new");
        // Delete removes both the GridFS file and the leftover legacy document.
        assert!(c.delete("dup").await.unwrap());
        assert!(!c.exists("dup").await.unwrap());
    }

    #[tokio::test]
    async fn delete_gridfs_also_cleans_up_legacy_document() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let c = MongodbRemoteSnapshotClient::new(raw.clone(), None, None);
        c.insert_legacy("migrated", b"old").await.unwrap();
        c.upload("migrated", b"new").await.unwrap();
        assert!(c.delete("migrated").await.unwrap());
        // Legacy collection is empty again (the migrated document was cleaned up).
        assert_eq!(
            raw.document_count(&crate::client::scoped_collection(
                c.database(),
                crate::constants::SNAPSHOTS_COLLECTION
            )),
            0
        );
    }

    /// The backing collection key for a client, database-scoped the way the client
    /// itself resolves it.
    fn files_collection(c: &MongodbRemoteSnapshotClient) -> String {
        crate::client::gridfs_files_collection(&c.scoped_bucket())
    }

    fn chunks_collection(c: &MongodbRemoteSnapshotClient) -> String {
        crate::client::gridfs_chunks_collection(&c.scoped_bucket())
    }

    async fn gridfs_file_doc(
        raw: &InMemoryMongoClient,
        c: &MongodbRemoteSnapshotClient,
        snapshot_id: &str,
    ) -> Value {
        use crate::client::{FindFilter, MongoClient as _};
        let filter = FindFilter::eq(crate::client::GRIDFS_FIELD_FILENAME, json!(snapshot_id));
        let doc = raw
            .find_one(&files_collection(c), &filter)
            .await
            .unwrap()
            .expect("gridfs file document");
        Value::Object(doc)
    }

    #[tokio::test]
    async fn gridfs_file_document_records_length_chunksize_and_upload_date() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let c = MongodbRemoteSnapshotClient::new(raw.clone(), Some("db"), None);
        let chunk = crate::client::GRIDFS_DEFAULT_CHUNK_SIZE;
        let payload = vec![7u8; chunk + 3];
        c.upload("meta", &payload).await.unwrap();

        let doc = gridfs_file_doc(&raw, &c, "meta").await;
        assert_eq!(
            doc.get("length").and_then(|v| v.as_u64()),
            Some(payload.len() as u64)
        );
        assert_eq!(
            doc.get("chunkSize").and_then(|v| v.as_u64()),
            Some(chunk as u64)
        );
        assert!(doc
            .get(crate::client::GRIDFS_FIELD_UPLOAD_DATE)
            .and_then(|v| v.as_i64())
            .is_some());
        // Metadata mirrors Java `GridFSUploadOptions.metadata({createdAt})`.
        let created_at = doc
            .get(crate::client::GRIDFS_FIELD_METADATA)
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(crate::client::GRIDFS_META_CREATED_AT))
            .and_then(|v| v.as_i64());
        assert!(created_at.is_some());
    }

    #[tokio::test]
    async fn gridfs_chunk_boundary_sizes_round_trip() {
        let chunk = crate::client::GRIDFS_DEFAULT_CHUNK_SIZE;
        for (label, len, expected_chunks) in [
            ("one-below", chunk - 1, 1usize),
            ("exact", chunk, 1usize),
            ("one-above", chunk + 1, 2usize),
        ] {
            let raw = Arc::new(InMemoryMongoClient::new());
            let c = MongodbRemoteSnapshotClient::new(raw.clone(), Some("db"), None);
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            c.upload("boundary", &payload).await.unwrap();
            assert_eq!(
                raw.document_count(&chunks_collection(&c)),
                expected_chunks,
                "{label} chunking"
            );
            assert_eq!(
                c.download("boundary").await.unwrap(),
                payload,
                "{label} round trip"
            );
        }
    }

    #[tokio::test]
    async fn gridfs_empty_payload_round_trips() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let c = MongodbRemoteSnapshotClient::new(raw.clone(), None, None);
        c.upload("empty", b"").await.unwrap();
        assert_eq!(raw.document_count(&chunks_collection(&c)), 0);
        assert!(c.exists("empty").await.unwrap());
        assert_eq!(c.download("empty").await.unwrap(), Vec::<u8>::new());
    }

    #[tokio::test]
    async fn missing_chunk_is_reported_as_corruption() {
        use crate::client::{FindFilter, MongoClient as _};
        let raw = Arc::new(InMemoryMongoClient::new());
        let c = MongodbRemoteSnapshotClient::new(raw.clone(), Some("db"), None);
        let chunk = crate::client::GRIDFS_DEFAULT_CHUNK_SIZE;
        c.upload("gap", &vec![1u8; chunk * 3]).await.unwrap();
        // Drop the middle chunk (n = 1).
        let file_id = gridfs_file_doc(&raw, &c, "gap")
            .await
            .get("_id")
            .cloned()
            .unwrap();
        let removed = raw
            .delete_one(
                &chunks_collection(&c),
                &FindFilter::eq("_id", json!(format!("{}:1", file_id.as_str().unwrap()))),
            )
            .await
            .unwrap();
        assert!(removed);
        let err = c.download("gap").await.unwrap_err().to_string();
        assert!(
            err.contains("corrupt GridFS file"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn out_of_order_duplicate_chunk_is_reported_as_corruption() {
        use crate::client::MongoClient as _;
        let raw = Arc::new(InMemoryMongoClient::new());
        let c = MongodbRemoteSnapshotClient::new(raw.clone(), Some("db"), None);
        c.upload("dup", b"short").await.unwrap();
        // A stray second chunk with n = 0 makes the sequence non-contiguous and the
        // reassembled length disagree with the stored `length`.
        let mut stray = serde_json::Map::new();
        stray.insert("_id".to_string(), json!("stray"));
        stray.insert(
            crate::client::GRIDFS_FIELD_FILES_ID.to_string(),
            gridfs_file_doc(&raw, &c, "dup")
                .await
                .get("_id")
                .cloned()
                .unwrap(),
        );
        stray.insert(crate::client::GRIDFS_FIELD_CHUNK_N.to_string(), json!(0));
        stray.insert(
            crate::client::LEGACY_FIELD_DATA.to_string(),
            json!(crate::client::encode_binary(b"x")),
        );
        raw.insert_one(&chunks_collection(&c), stray).await.unwrap();
        assert!(c.download("dup").await.is_err());
    }

    #[tokio::test]
    async fn database_name_isolates_snapshots() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let a = MongodbRemoteSnapshotClient::new(raw.clone(), Some("db_a"), None);
        let b = MongodbRemoteSnapshotClient::new(raw.clone(), Some("db_b"), None);
        a.upload("shared-id", b"from-a").await.unwrap();
        assert!(a.exists("shared-id").await.unwrap());
        // The Java client binds its bucket to one database; another database must not
        // observe the snapshot.
        assert!(!b.exists("shared-id").await.unwrap());
        assert!(b.download("shared-id").await.is_err());
        b.upload("shared-id", b"from-b").await.unwrap();
        assert_eq!(a.download("shared-id").await.unwrap(), b"from-a");
        assert_eq!(b.download("shared-id").await.unwrap(), b"from-b");
    }

    #[tokio::test]
    async fn spec_exposes_configured_database() {
        let spec = MongodbSnapshotSpec::new(client(), Some("tenant_db"), Some("snaps"));
        assert_eq!(spec.client().database(), "tenant_db");
        assert_eq!(spec.client().bucket(), "snaps");
    }
}
