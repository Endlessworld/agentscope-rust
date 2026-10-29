//! Sandbox state types and storage.
use agentscope_core::error::Result;
use serde::{Deserialize, Serialize};

use super::{VolumeMount, WorkspaceSpec};

/// Serializable state of a sandbox, persisted by `SessionSandboxStateStore` so a sandbox can
/// be resumed across calls.
///
/// The `workspace_root_ready` flag drives the 4-branch start logic: it records whether
/// the workspace was fully initialized at the last stop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
// The Docker variant carries the container/workspace options needed to recreate a container,
// mirroring `DockerSandboxState`. Boxing it would diverge from the Java state layout, so the
// size gap is accepted instead.
#[allow(clippy::large_enum_variant)]
pub enum SandboxState {
    /// Docker container state.
    Docker(DockerSandboxState),

    /// Local temporary directory state.
    Local(LocalSandboxState),

    /// Remote sandbox state.
    Remote(RemoteSandboxState),
}

/// Docker container sandbox state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerSandboxState {
    /// Session ID for this sandbox.
    pub session_id: String,

    /// Docker container ID (absent until the container has been created).
    pub container_id: Option<String>,

    /// Workspace root path inside the container.
    pub workspace_root: String,

    /// Container image.
    pub image: String,

    /// Whether the workspace root is ready.
    pub workspace_root_ready: bool,

    /// Workspace projection hash.
    pub workspace_projection_hash: Option<String>,

    /// Additional metadata.
    pub metadata: std::collections::HashMap<String, String>,

    /// Snapshot the workspace was persisted as, re-bound to a client on resume.
    #[serde(default)]
    pub snapshot: Option<SandboxSnapshotRef>,

    /// Default for [`DockerSandboxState::container_owned`]: the SDK owns containers it created.
    /// Human-readable container name (`agentscope-sandbox-<sessionId>`).
    #[serde(default)]
    pub container_name: Option<String>,

    /// Whether the SDK owns the container lifecycle (create/stop/remove).
    #[serde(default = "default_container_owned")]
    pub container_owned: bool,

    /// Optional memory limit in bytes, kept for container recreation on resume.
    #[serde(default)]
    pub memory_size_bytes: Option<u64>,

    /// Optional CPU count limit, kept for container recreation on resume.
    #[serde(default)]
    pub cpu_count: Option<u64>,

    /// Exposed port numbers, kept for container recreation on resume.
    #[serde(default)]
    pub exposed_ports: Vec<u16>,

    /// Docker network mode or network name.
    #[serde(default)]
    pub network: Option<String>,

    /// Additional raw arguments appended to `docker run` before the image name.
    #[serde(default)]
    pub additional_run_args: Vec<String>,

    /// Bind mounts passed to `docker run -v` (host path : container path).
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,

    /// Workspace specification used to (re)build the container workspace.
    #[serde(default)]
    pub workspace_spec: Option<WorkspaceSpec>,
}

fn default_container_owned() -> bool {
    true
}

/// Local temporary directory sandbox state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSandboxState {
    /// Session ID for this sandbox.
    pub session_id: String,

    /// Temporary directory path.
    pub temp_dir: String,

    /// Whether the workspace root is ready.
    pub workspace_root_ready: bool,

    /// Workspace projection hash.
    pub workspace_projection_hash: Option<String>,

    /// Additional metadata.
    pub metadata: std::collections::HashMap<String, String>,

    /// Snapshot the workspace was persisted as, re-bound to a client on resume.
    #[serde(default)]
    pub snapshot: Option<SandboxSnapshotRef>,
}

/// Remote sandbox state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSandboxState {
    /// Session ID for this sandbox.
    pub session_id: String,

    /// Remote endpoint URL.
    pub endpoint: String,

    /// Authentication token.
    pub token: Option<String>,

    /// Whether the workspace root is ready.
    pub workspace_root_ready: bool,

    /// Workspace projection hash.
    pub workspace_projection_hash: Option<String>,

    /// Additional metadata.
    pub metadata: std::collections::HashMap<String, String>,

    /// Snapshot the workspace was persisted as, re-bound to a client on resume.
    #[serde(default)]
    pub snapshot: Option<SandboxSnapshotRef>,
}

/// Serializable identity of a workspace snapshot, persisted inside [`SandboxState`].
///
/// Mirrors the JSON shape of the Java `SandboxSnapshot` hierarchy (`type` discriminator plus an
/// `id`). Only the identity is persisted: the `RemoteSnapshotClient` cannot be serialized and is
/// re-bound from the builder at resume time, exactly like Java `RemoteSandboxSnapshot` whose
/// resolved client is `@JsonIgnore`. The local snapshot's `basePath` is likewise not persisted —
/// the resume spec (`LocalSnapshotSpec`) owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SandboxSnapshotRef {
    /// Snapshotting disabled.
    Noop,

    /// Snapshot stored on the local filesystem.
    Local {
        /// Snapshot identifier (also the archive file name).
        id: String,
    },

    /// Snapshot stored by a remote [`RemoteSnapshotClient`](super::RemoteSnapshotClient).
    Remote {
        /// Snapshot identifier understood by the remote client.
        id: String,
    },
}

impl SandboxSnapshotRef {
    /// Derives the persisted identity from a runtime snapshot.
    pub fn from_snapshot(snapshot: &dyn super::SandboxSnapshot) -> Self {
        match snapshot.snapshot_type() {
            "noop" => Self::Noop,
            "local" => Self::Local {
                id: snapshot.id().to_string(),
            },
            _ => Self::Remote {
                id: snapshot.id().to_string(),
            },
        }
    }
}

impl SandboxState {
    /// Returns the session ID.
    pub fn session_id(&self) -> &str {
        match self {
            SandboxState::Docker(state) => &state.session_id,
            SandboxState::Local(state) => &state.session_id,
            SandboxState::Remote(state) => &state.session_id,
        }
    }

    /// Returns whether the workspace root is ready.
    pub fn is_workspace_root_ready(&self) -> bool {
        match self {
            SandboxState::Docker(state) => state.workspace_root_ready,
            SandboxState::Local(state) => state.workspace_root_ready,
            SandboxState::Remote(state) => state.workspace_root_ready,
        }
    }

    /// Sets the workspace root ready flag.
    pub fn set_workspace_root_ready(&mut self, ready: bool) {
        match self {
            SandboxState::Docker(state) => state.workspace_root_ready = ready,
            SandboxState::Local(state) => state.workspace_root_ready = ready,
            SandboxState::Remote(state) => state.workspace_root_ready = ready,
        }
    }

    /// Returns the workspace projection hash.
    pub fn workspace_projection_hash(&self) -> Option<&str> {
        match self {
            SandboxState::Docker(state) => state.workspace_projection_hash.as_deref(),
            SandboxState::Local(state) => state.workspace_projection_hash.as_deref(),
            SandboxState::Remote(state) => state.workspace_projection_hash.as_deref(),
        }
    }

    /// Returns the persisted snapshot identity, when the workspace was snapshotted.
    pub fn snapshot(&self) -> Option<&SandboxSnapshotRef> {
        match self {
            SandboxState::Docker(state) => state.snapshot.as_ref(),
            SandboxState::Local(state) => state.snapshot.as_ref(),
            SandboxState::Remote(state) => state.snapshot.as_ref(),
        }
    }

    /// Records the snapshot identity the workspace was persisted as.
    pub fn set_snapshot(&mut self, snapshot: Option<SandboxSnapshotRef>) {
        match self {
            SandboxState::Docker(state) => state.snapshot = snapshot,
            SandboxState::Local(state) => state.snapshot = snapshot,
            SandboxState::Remote(state) => state.snapshot = snapshot,
        }
    }
}

/// Trait for storing and retrieving sandbox state.
#[async_trait::async_trait]
pub trait SandboxStateStore: Send + Sync {
    /// Saves sandbox state for a given isolation key.
    async fn save(&self, key: &str, state: &SandboxState) -> Result<()>;

    /// Loads sandbox state for a given isolation key.
    async fn load(&self, key: &str) -> Result<Option<SandboxState>>;

    /// Deletes sandbox state for a given isolation key.
    async fn delete(&self, key: &str) -> Result<()>;

    /// Lists all stored isolation keys.
    async fn list_keys(&self) -> Result<Vec<String>>;
}

/// In-memory implementation of `SandboxStateStore`.
pub struct InMemorySandboxStateStore {
    states: std::sync::Mutex<std::collections::HashMap<String, SandboxState>>,
}

impl InMemorySandboxStateStore {
    pub fn new() -> Self {
        Self {
            states: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemorySandboxStateStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SandboxStateStore for InMemorySandboxStateStore {
    async fn save(&self, key: &str, state: &SandboxState) -> Result<()> {
        let mut states = self
            .states
            .lock()
            .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))?;
        states.insert(key.to_string(), state.clone());
        Ok(())
    }

    async fn load(&self, key: &str) -> Result<Option<SandboxState>> {
        let states = self
            .states
            .lock()
            .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))?;
        Ok(states.get(key).cloned())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut states = self
            .states
            .lock()
            .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))?;
        states.remove(key);
        Ok(())
    }

    async fn list_keys(&self) -> Result<Vec<String>> {
        let states = self
            .states
            .lock()
            .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))?;
        Ok(states.keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_state_serialization() {
        let state = SandboxState::Docker(DockerSandboxState {
            session_id: "test-session".to_string(),
            container_id: Some("abc123".to_string()),
            container_name: Some("agentscope-sandbox-test-session".to_string()),
            workspace_root: "/workspace".to_string(),
            image: "ubuntu:20.04".to_string(),
            workspace_root_ready: true,
            container_owned: true,
            memory_size_bytes: None,
            cpu_count: None,
            exposed_ports: Vec::new(),
            network: None,
            additional_run_args: Vec::new(),
            volumes: Vec::new(),
            workspace_spec: None,
            workspace_projection_hash: Some("hash123".to_string()),
            metadata: std::collections::HashMap::new(),
            snapshot: None,
        });

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("Docker"));
        assert!(json.contains("test-session"));

        let deserialized: SandboxState = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.session_id(), "test-session");
        assert!(deserialized.is_workspace_root_ready());
    }

    #[test]
    fn test_docker_state_defaults_for_legacy_json() {
        // JSON written before `container_owned`/`container_name` existed must still load,
        // defaulting to SDK ownership.
        let json = r#"{"type":"Docker","session_id":"s1","container_id":null,
            "workspace_root":"/workspace","image":"ubuntu:22.04",
            "workspace_root_ready":false,"workspace_projection_hash":null,"metadata":{}}"#;
        let state: SandboxState = serde_json::from_str(json).unwrap();
        match state {
            SandboxState::Docker(docker) => {
                assert!(docker.container_owned);
                assert!(docker.container_name.is_none());
                assert!(docker.workspace_spec.is_none());
                assert!(docker.exposed_ports.is_empty());
            }
            other => panic!("expected Docker state, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_in_memory_state_store() {
        let store = InMemorySandboxStateStore::new();

        let state = SandboxState::Local(LocalSandboxState {
            session_id: "test-session".to_string(),
            temp_dir: "/tmp/test".to_string(),
            workspace_root_ready: false,
            workspace_projection_hash: None,
            metadata: std::collections::HashMap::new(),
            snapshot: None,
        });

        // Save state
        store.save("key1", &state).await.unwrap();

        // Load state
        let loaded = store.load("key1").await.unwrap().unwrap();
        assert_eq!(loaded.session_id(), "test-session");

        // List keys
        let keys = store.list_keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&"key1".to_string()));

        // Delete state
        store.delete("key1").await.unwrap();
        let loaded = store.load("key1").await.unwrap();
        assert!(loaded.is_none());
    }
}
