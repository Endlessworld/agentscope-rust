//! Tencent Cloud COS backed snapshot components, mirroring
//! `io.agentscope.extensions.cos.CosRemoteSnapshotClient` and `CosSnapshotSpec`.
//!
//! Reuses the harness [`RemoteSnapshotClient`] / [`RemoteSnapshotSpec`] /
//! [`SandboxSnapshotSpec`] abstractions — it does **not** re-invent the snapshot
//! contract. Snapshot archives are stored as `{prefix}{snapshotId}.tar`.

use std::sync::Arc;

use agentscope_core::error::{Error, Result as HarnessResult};
use agentscope_harness::agent::sandbox::snapshot::{
    RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshot, SandboxSnapshotSpec,
};
use async_trait::async_trait;

use crate::layout::{normalize_optional_prefix, validate_snapshot_id, SNAPSHOT_SUFFIX};
use crate::object_store::SharedObjectStore;
use crate::{CosError, Result};

/// COS-backed [`RemoteSnapshotClient`].
#[derive(Debug, Clone)]
pub struct CosRemoteSnapshotClient {
    object_store: SharedObjectStore,
    key_prefix: String,
}

impl CosRemoteSnapshotClient {
    /// Creates a client with an explicit (possibly blank) key prefix.
    pub fn new(object_store: SharedObjectStore, key_prefix: Option<&str>) -> Self {
        Self {
            object_store,
            key_prefix: normalize_optional_prefix(key_prefix),
        }
    }

    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    fn object_key(&self, snapshot_id: &str) -> HarnessResult<String> {
        validate_snapshot_id(snapshot_id).map_err(|e| Error::Tool(e.to_string()))?;
        Ok(format!("{}{snapshot_id}{SNAPSHOT_SUFFIX}", self.key_prefix))
    }
}

#[async_trait]
impl RemoteSnapshotClient for CosRemoteSnapshotClient {
    async fn upload(&self, snapshot_id: &str, data: &[u8]) -> HarnessResult<()> {
        let key = self.object_key(snapshot_id)?;
        self.object_store
            .put(&key, data)
            .await
            .map_err(|e| Error::Tool(e.to_string()))
    }

    async fn download(&self, snapshot_id: &str) -> HarnessResult<Vec<u8>> {
        let key = self.object_key(snapshot_id)?;
        match self.object_store.get(&key).await {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(Error::Tool(format!("Snapshot not found in COS: {key}"))),
            Err(e) => Err(Error::Tool(e.to_string())),
        }
    }

    async fn exists(&self, snapshot_id: &str) -> HarnessResult<bool> {
        let key = self.object_key(snapshot_id)?;
        self.object_store
            .exists(&key)
            .await
            .map_err(|e| Error::Tool(e.to_string()))
    }
}

/// Convenience [`SandboxSnapshotSpec`] for COS snapshot storage.
///
/// Wraps the harness [`RemoteSnapshotSpec`] with a [`CosRemoteSnapshotClient`],
/// exactly as the Java `CosSnapshotSpec` wraps `RemoteSnapshotSpec`.
#[derive(Debug)]
pub struct CosSnapshotSpec {
    inner: RemoteSnapshotSpec,
    key_prefix: String,
}

impl CosSnapshotSpec {
    pub fn new(object_store: SharedObjectStore, key_prefix: Option<&str>) -> Self {
        let key_prefix = normalize_optional_prefix(key_prefix);
        let client = Arc::new(CosRemoteSnapshotClient::new(object_store, key_prefix.as_str().into()).clone());
        Self {
            inner: RemoteSnapshotSpec::new(client),
            key_prefix,
        }
    }

    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }
}

impl SandboxSnapshotSpec for CosSnapshotSpec {
    fn build(&self, snapshot_id: &str) -> HarnessResult<Arc<dyn SandboxSnapshot>> {
        self.inner.build(snapshot_id)
    }
}

/// The typed [`CosError`] returned when a snapshot id is invalid.
impl From<CosError> for Error {
    fn from(value: CosError) -> Self {
        Error::Tool(value.to_string())
    }
}

/// Helper: build a client directly, surfacing configuration errors as
/// [`CosError`] rather than the harness error type.
pub fn cos_remote_snapshot_client(
    object_store: SharedObjectStore,
    key_prefix: Option<&str>,
) -> Result<CosRemoteSnapshotClient> {
    Ok(CosRemoteSnapshotClient::new(object_store, key_prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::InMemoryCosObjectStore;
    use agentscope_harness::agent::sandbox::snapshot::{RemoteSandboxSnapshot, SandboxSnapshot};

    #[tokio::test]
    async fn upload_download_exists_roundtrip() {
        let client = CosRemoteSnapshotClient::new(InMemoryCosObjectStore::new().shared(), Some("snap/"));
        assert!(!client.exists("s1").await.unwrap());
        client.upload("s1", b"archive").await.unwrap();
        assert!(client.exists("s1").await.unwrap());
        assert_eq!(client.download("s1").await.unwrap(), b"archive");
        assert!(client.download("nope").await.is_err());
    }

    #[tokio::test]
    async fn unsafe_snapshot_id_rejected() {
        let client = CosRemoteSnapshotClient::new(InMemoryCosObjectStore::new().shared(), None);
        assert!(client.upload("../escape", b"x").await.is_err());
        assert!(client.upload("", b"x").await.is_err());
    }

    #[tokio::test]
    async fn spec_builds_remote_snapshot() {
        let spec = CosSnapshotSpec::new(InMemoryCosObjectStore::new().shared(), Some("snap/"));
        let snap = spec.build("sess-1").unwrap();
        assert_eq!(snap.id(), "sess-1");
        assert_eq!(snap.snapshot_type(), "remote");
        assert!(!snap.is_restorable().await.unwrap());
        snap.persist(b"bytes").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());
        assert_eq!(snap.restore().await.unwrap(), b"bytes");
    }

    #[test]
    fn spec_rejects_unsafe_ids() {
        let spec = CosSnapshotSpec::new(InMemoryCosObjectStore::new().shared(), None);
        assert!(spec.build("a/b").is_err());
    }

    #[test]
    fn remote_snapshot_spec_type_is_honoured() {
        let store = InMemoryCosObjectStore::new().shared();
        let client = Arc::new(CosRemoteSnapshotClient::new(store.clone(), None));
        let snap = RemoteSandboxSnapshot::new(client, "direct");
        assert_eq!(snap.snapshot_type(), "remote");
    }
}
