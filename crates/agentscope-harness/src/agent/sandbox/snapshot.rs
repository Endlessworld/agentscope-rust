//! Sandbox workspace snapshot persistence, mirroring
//! `io.agentscope.harness.agent.sandbox.snapshot.*`.
//!
//! Snapshots allow workspace state to be preserved between session stops and
//! restored on subsequent session starts. Implementations determine where and how
//! the snapshot is stored: local disk ([`LocalSandboxSnapshot`]), remote storage
//! ([`RemoteSandboxSnapshot`]), or not at all ([`NoopSandboxSnapshot`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::error::{Error, Result};

/// Represents a persisted snapshot of a sandbox workspace.
#[async_trait::async_trait]
pub trait SandboxSnapshot: Send + Sync + std::fmt::Debug {
    /// Persists the workspace archive to this snapshot.
    async fn persist(&self, workspace_archive: &[u8]) -> Result<()>;

    /// Restores the workspace archive from this snapshot.
    async fn restore(&self) -> Result<Vec<u8>>;

    /// Returns whether this snapshot can currently be restored.
    async fn is_restorable(&self) -> Result<bool>;

    /// Unique identifier for this snapshot.
    fn id(&self) -> &str;

    /// Snapshot type discriminator (`"noop"`, `"local"`, `"remote"`).
    fn snapshot_type(&self) -> &str;

    /// Whether this snapshot actually persists data.
    ///
    /// When `false`, `stop()` may skip the potentially expensive workspace archive
    /// step entirely. Defaults to `true`.
    fn is_persistence_enabled(&self) -> bool {
        true
    }
}

/// Validates a snapshot id is a safe single path segment.
fn validate_snapshot_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        return Err(Error::Tool(
            "Snapshot id must not be null or blank".to_string(),
        ));
    }
    if id.contains('/') || id.contains('\\') || id.contains("..") || id.contains('\0') {
        return Err(Error::Tool(format!(
            "Snapshot id contains unsafe characters: {id}"
        )));
    }
    Ok(())
}

/// Snapshot that persists workspace archives as tar files on the local filesystem.
///
/// Archives are written atomically: data is first written to a temporary file in the
/// same directory, then moved to the final path. This ensures the snapshot is either
/// fully written or not present — never partially written.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LocalSandboxSnapshot {
    base_path: String,
    id: String,
}

impl LocalSandboxSnapshot {
    pub fn new(base_path: impl Into<String>, id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        validate_snapshot_id(&id)?;
        Ok(Self {
            base_path: base_path.into(),
            id,
        })
    }

    pub fn base_path(&self) -> &str {
        &self.base_path
    }

    fn target_path(&self) -> PathBuf {
        Path::new(&self.base_path).join(format!("{}.tar", self.id))
    }
}

#[async_trait::async_trait]
impl SandboxSnapshot for LocalSandboxSnapshot {
    async fn persist(&self, workspace_archive: &[u8]) -> Result<()> {
        let target = self.target_path();
        let dir = target.parent().unwrap_or_else(|| Path::new("."));
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            return Err(Error::Tool(format!(
                "Failed to persist snapshot '{}': {e}",
                self.id
            )));
        }
        let tmp = dir.join(format!(".{}.{}.tmp", self.id, uuid::Uuid::new_v4()));
        let write_result = async {
            tokio::fs::write(&tmp, workspace_archive).await?;
            tokio::fs::rename(&tmp, &target).await
        }
        .await;
        if let Err(e) = write_result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::Tool(format!(
                "Failed to persist snapshot '{}': {e}",
                self.id
            )));
        }
        Ok(())
    }

    async fn restore(&self) -> Result<Vec<u8>> {
        let path = self.target_path();
        if !path.exists() {
            return Err(Error::Tool(format!("Snapshot not found: {}", self.id)));
        }
        tokio::fs::read(&path)
            .await
            .map_err(|e| Error::Tool(format!("Failed to read snapshot '{}': {e}", self.id)))
    }

    async fn is_restorable(&self) -> Result<bool> {
        Ok(self.target_path().exists())
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn snapshot_type(&self) -> &str {
        "local"
    }
}

/// User-implemented interface for uploading/downloading snapshot archives to/from
/// remote storage (e.g. S3, OSS, GCS, or a custom blob store).
#[async_trait::async_trait]
pub trait RemoteSnapshotClient: Send + Sync + std::fmt::Debug {
    async fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()>;
    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>>;
    async fn exists(&self, snapshot_id: &str) -> Result<bool>;
}

/// Snapshot backed by a [`RemoteSnapshotClient`], delegating all operations to it.
#[derive(Debug, Clone)]
pub struct RemoteSandboxSnapshot {
    client: Option<Arc<dyn RemoteSnapshotClient>>,
    id: String,
}

impl RemoteSandboxSnapshot {
    pub fn new(client: Arc<dyn RemoteSnapshotClient>, id: impl Into<String>) -> Self {
        Self {
            client: Some(client),
            id: id.into(),
        }
    }

    /// Creates a snapshot bound to an id only; the client must be re-injected at
    /// resume time (mirrors Java's JSON round-trip where the client is `@JsonIgnore`).
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            client: None,
            id: id.into(),
        }
    }

    pub fn id_value(&self) -> &str {
        &self.id
    }

    fn require_client(&self) -> Result<&Arc<dyn RemoteSnapshotClient>> {
        self.client.as_ref().ok_or_else(|| {
            Error::Tool(format!(
                "RemoteSnapshotClient is not bound to snapshot id: {}",
                self.id
            ))
        })
    }
}

#[async_trait::async_trait]
impl SandboxSnapshot for RemoteSandboxSnapshot {
    async fn persist(&self, workspace_archive: &[u8]) -> Result<()> {
        self.require_client()?
            .upload(&self.id, workspace_archive)
            .await
            .map_err(|e| Error::Tool(format!("Remote upload failed: {e}")))
    }

    async fn restore(&self) -> Result<Vec<u8>> {
        self.require_client()?
            .download(&self.id)
            .await
            .map_err(|e| Error::Tool(format!("Remote download failed: {e}")))
    }

    async fn is_restorable(&self) -> Result<bool> {
        self.require_client()?
            .exists(&self.id)
            .await
            .map_err(|e| Error::Tool(format!("Remote exists check failed: {e}")))
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn snapshot_type(&self) -> &str {
        "remote"
    }
}

/// No-op snapshot that does not persist anything.
///
/// Workspace state is NOT preserved between session stops; each fresh session start
/// applies the full manifest.
#[derive(Debug, Clone, Default)]
pub struct NoopSandboxSnapshot;

#[async_trait::async_trait]
impl SandboxSnapshot for NoopSandboxSnapshot {
    async fn persist(&self, _workspace_archive: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn restore(&self) -> Result<Vec<u8>> {
        Err(Error::Tool("Snapshot not found: noop".to_string()))
    }

    async fn is_restorable(&self) -> Result<bool> {
        Ok(false)
    }

    fn id(&self) -> &str {
        "noop"
    }

    fn snapshot_type(&self) -> &str {
        "noop"
    }

    fn is_persistence_enabled(&self) -> bool {
        false
    }
}

/// Factory that creates [`SandboxSnapshot`] instances for a given session id.
///
/// Implementations configure WHERE snapshots are stored: [`NoopSnapshotSpec`]
/// (disabled), [`LocalSnapshotSpec`] (local disk), [`RemoteSnapshotSpec`] (remote).
pub trait SandboxSnapshotSpec: Send + Sync + std::fmt::Debug {
    /// Creates a [`SandboxSnapshot`] for the given snapshot id (typically the session id).
    fn build(&self, snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>>;
}

/// Snapshot spec creating [`LocalSandboxSnapshot`] instances in a local directory.
///
/// Each session gets its own snapshot file at `{base_path}/{snapshot_id}.tar`.
#[derive(Debug, Clone)]
pub struct LocalSnapshotSpec {
    base_path: String,
}

impl LocalSnapshotSpec {
    pub fn new(base_path: impl Into<String>) -> Self {
        Self {
            base_path: base_path.into(),
        }
    }

    pub fn base_path(&self) -> &str {
        &self.base_path
    }
}

impl SandboxSnapshotSpec for LocalSnapshotSpec {
    fn build(&self, snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>> {
        Ok(Arc::new(LocalSandboxSnapshot::new(
            &self.base_path,
            snapshot_id,
        )?))
    }
}

/// Snapshot spec creating [`RemoteSandboxSnapshot`] instances backed by a
/// [`RemoteSnapshotClient`]. The client is shared across all sessions.
#[derive(Debug)]
pub struct RemoteSnapshotSpec {
    client: Arc<dyn RemoteSnapshotClient>,
}

impl RemoteSnapshotSpec {
    pub fn new(client: Arc<dyn RemoteSnapshotClient>) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &Arc<dyn RemoteSnapshotClient> {
        &self.client
    }
}

impl SandboxSnapshotSpec for RemoteSnapshotSpec {
    fn build(&self, snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>> {
        validate_snapshot_id(snapshot_id)?;
        Ok(Arc::new(RemoteSandboxSnapshot::new(
            self.client.clone(),
            snapshot_id,
        )))
    }
}

/// Snapshot spec creating [`NoopSandboxSnapshot`] instances (ignores the id).
#[derive(Debug, Clone, Default)]
pub struct NoopSnapshotSpec;

impl NoopSnapshotSpec {
    pub fn new() -> Self {
        Self
    }
}

impl SandboxSnapshotSpec for NoopSnapshotSpec {
    fn build(&self, _snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>> {
        Ok(Arc::new(NoopSandboxSnapshot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("harness-snapshot-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn local_snapshot_roundtrip() {
        let dir = temp_dir("local");
        let snap = LocalSandboxSnapshot::new(dir.to_string_lossy().to_string(), "sess-1").unwrap();
        assert_eq!(snap.snapshot_type(), "local");
        assert!(snap.is_persistence_enabled());
        assert!(!snap.is_restorable().await.unwrap());
        snap.persist(b"archive-bytes").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());
        assert_eq!(snap.restore().await.unwrap(), b"archive-bytes");
        assert!(dir.join("sess-1.tar").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_snapshot_rejects_unsafe_ids() {
        assert!(LocalSandboxSnapshot::new("/tmp", "../escape").is_err());
        assert!(LocalSandboxSnapshot::new("/tmp", "a/b").is_err());
        assert!(LocalSandboxSnapshot::new("/tmp", "  ").is_err());
    }

    #[tokio::test]
    async fn local_snapshot_restore_missing_errors() {
        let dir = temp_dir("missing");
        let snap = LocalSandboxSnapshot::new(dir.to_string_lossy().to_string(), "nope").unwrap();
        assert!(snap.restore().await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_spec_builds_session_snapshot() {
        let dir = temp_dir("spec");
        let spec = LocalSnapshotSpec::new(dir.to_string_lossy().to_string());
        let snap = spec.build("session-a").unwrap();
        assert_eq!(snap.id(), "session-a");
        assert_eq!(snap.snapshot_type(), "local");
        snap.persist(b"x").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn noop_snapshot_never_restores() {
        let snap = NoopSandboxSnapshot;
        assert!(!snap.is_persistence_enabled());
        snap.persist(b"ignored").await.unwrap();
        assert!(!snap.is_restorable().await.unwrap());
        assert!(snap.restore().await.is_err());
        assert_eq!(snap.snapshot_type(), "noop");
    }

    #[derive(Debug)]
    struct MockRemote {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl RemoteSnapshotClient for MockRemote {
        async fn upload(&self, id: &str, data: &[u8]) -> Result<()> {
            self.blobs
                .lock()
                .unwrap()
                .insert(id.to_string(), data.to_vec());
            Ok(())
        }
        async fn download(&self, id: &str) -> Result<Vec<u8>> {
            self.blobs
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| Error::Tool("not found".to_string()))
        }
        async fn exists(&self, id: &str) -> Result<bool> {
            Ok(self.blobs.lock().unwrap().contains_key(id))
        }
    }

    #[tokio::test]
    async fn remote_snapshot_delegates_to_client() {
        let client = Arc::new(MockRemote {
            blobs: Mutex::new(HashMap::new()),
        });
        let spec = RemoteSnapshotSpec::new(client);
        let snap = spec.build("s1").unwrap();
        assert_eq!(snap.snapshot_type(), "remote");
        assert!(!snap.is_restorable().await.unwrap());
        snap.persist(b"remote-bytes").await.unwrap();
        assert!(snap.is_restorable().await.unwrap());
        assert_eq!(snap.restore().await.unwrap(), b"remote-bytes");
    }

    #[tokio::test]
    async fn remote_snapshot_without_client_errors() {
        let snap = RemoteSandboxSnapshot::with_id("x");
        assert!(snap.persist(b"x").await.is_err());
    }
}
