//! Sandbox state types and storage.
use serde::{Deserialize, Serialize};
use agentscope_core::error::Result;

/// Serializable state of a sandbox, persisted by `SessionSandboxStateStore` so a sandbox can
/// be resumed across calls.
/// 
/// The `workspace_root_ready` flag drives the 4-branch start logic: it records whether
/// the workspace was fully initialized at the last stop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
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
    
    /// Docker container ID.
    pub container_id: String,
    
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
        let mut states = self.states.lock().map_err(|e| agentscope_core::error::Error::Other(e.to_string()))?;
        states.insert(key.to_string(), state.clone());
        Ok(())
    }
    
    async fn load(&self, key: &str) -> Result<Option<SandboxState>> {
        let states = self.states.lock().map_err(|e| agentscope_core::error::Error::Other(e.to_string()))?;
        Ok(states.get(key).cloned())
    }
    
    async fn delete(&self, key: &str) -> Result<()> {
        let mut states = self.states.lock().map_err(|e| agentscope_core::error::Error::Other(e.to_string()))?;
        states.remove(key);
        Ok(())
    }
    
    async fn list_keys(&self) -> Result<Vec<String>> {
        let states = self.states.lock().map_err(|e| agentscope_core::error::Error::Other(e.to_string()))?;
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
            container_id: "abc123".to_string(),
            workspace_root: "/workspace".to_string(),
            image: "ubuntu:20.04".to_string(),
            workspace_root_ready: true,
            workspace_projection_hash: Some("hash123".to_string()),
            metadata: std::collections::HashMap::new(),
        });
        
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("Docker"));
        assert!(json.contains("test-session"));
        
        let deserialized: SandboxState = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.session_id(), "test-session");
        assert!(deserialized.is_workspace_root_ready());
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