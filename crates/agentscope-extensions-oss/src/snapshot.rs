//! Alibaba Cloud OSS backed sandbox snapshot storage, mirroring
//! `io.agentscope.extensions.oss.OssRemoteSnapshotClient` and `OssSnapshotSpec`.
//!
//! Objects are stored as `{keyPrefix}{snapshotId}.tar`.

use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::snapshot::{
    RemoteSandboxSnapshot, RemoteSnapshotClient, RemoteSnapshotSpec, SandboxSnapshot,
    SandboxSnapshotSpec,
};
use async_trait::async_trait;

use crate::client::{normalize_prefix, validate_snapshot_id, OssClient};
use crate::error::OssError;

/// OSS-backed [`RemoteSnapshotClient`] uploading/downloading `.tar` snapshots.
#[derive(Debug, Clone)]
pub struct OssRemoteSnapshotClient {
    client: Arc<dyn OssClient>,
    bucket_name: String,
    key_prefix: String,
}

impl OssRemoteSnapshotClient {
    /// Creates a client. Returns an error when the bucket name is blank.
    pub fn new(
        client: Arc<dyn OssClient>,
        bucket_name: impl Into<String>,
        key_prefix: Option<&str>,
    ) -> std::result::Result<Self, OssError> {
        let bucket_name = bucket_name.into();
        if bucket_name.trim().is_empty() {
            return Err(OssError::BlankBucket);
        }
        Ok(Self {
            client,
            bucket_name,
            key_prefix: normalize_prefix(key_prefix, ""),
        })
    }

    /// Returns the configured bucket name.
    pub fn bucket_name(&self) -> &str {
        &self.bucket_name
    }

    /// Returns the normalized key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    fn object_key(&self, snapshot_id: &str) -> Result<String> {
        validate_snapshot_id(snapshot_id).map_err(Error::from)?;
        Ok(format!("{}{}.tar", self.key_prefix, snapshot_id))
    }
}

#[async_trait]
impl RemoteSnapshotClient for OssRemoteSnapshotClient {
    async fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()> {
        let key = self.object_key(snapshot_id)?;
        self.client
            .put(&key, data)
            .map_err(|e| Error::Tool(format!("OSS snapshot upload failed: {e}")))
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>> {
        let key = self.object_key(snapshot_id)?;
        match self.client.get(&key) {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(Error::Tool(format!("Snapshot not found in OSS: {key}"))),
            Err(e) => Err(Error::Tool(format!("OSS snapshot download failed: {e}"))),
        }
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool> {
        let key = self.object_key(snapshot_id)?;
        self.client
            .exists(&key)
            .map_err(|e| Error::Tool(format!("OSS snapshot exists check failed: {e}")))
    }
}

/// Convenience [`SandboxSnapshotSpec`] for Alibaba Cloud OSS snapshot storage.
///
/// Wraps a [`RemoteSnapshotSpec`] bound to an [`OssRemoteSnapshotClient`].
#[derive(Debug)]
pub struct OssSnapshotSpec {
    inner: RemoteSnapshotSpec,
    /// Kept for introspection; the inner spec holds the shared client.
    client: Arc<OssRemoteSnapshotClient>,
}

impl OssSnapshotSpec {
    /// Creates a spec from an existing OSS client.
    pub fn new(
        client: Arc<dyn OssClient>,
        bucket_name: impl Into<String>,
        key_prefix: Option<&str>,
    ) -> std::result::Result<Self, OssError> {
        let client = Arc::new(OssRemoteSnapshotClient::new(client, bucket_name, key_prefix)?);
        Ok(Self {
            inner: RemoteSnapshotSpec::new(client.clone()),
            client,
        })
    }

    /// Returns the underlying OSS snapshot client.
    pub fn client(&self) -> &Arc<OssRemoteSnapshotClient> {
        &self.client
    }
}

impl SandboxSnapshotSpec for OssSnapshotSpec {
    fn build(&self, snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>> {
        // Delegate to the harness RemoteSnapshotSpec so id validation and the
        // RemoteSandboxSnapshot wiring stay identical to the built-in backend.
        self.inner.build(snapshot_id)
    }
}

// Silence unused import warning when the concrete snapshot type is not named here.
#[allow(dead_code)]
fn _assert_remote_snapshot_used() -> Option<RemoteSandboxSnapshot> {
    None
}
