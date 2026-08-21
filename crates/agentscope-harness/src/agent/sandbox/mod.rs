//! Sandbox abstraction layer, mirroring `io.agentscope.harness.agent.sandbox`.
//! The docker backend (via `bollard`) and snapshot persistence are pending; this module
//! provides the spec/result/applier surface so `HarnessAgent` can project a workspace layout.
//! 
//! # Architecture
//! 
//! - `Sandbox` trait: Active sandbox lifecycle (start/stop/shutdown/exec/persist/hydrate)
//! - `SandboxClient` trait: Factory for creating and resuming sandboxes
//! - `SandboxManager`: Manages sandbox lifecycle for the current call
//! - `SandboxContext`: Immutable configuration for sandbox behavior
//! - `SandboxState`: Serializable state of a sandbox
//! - `SandboxAcquireResult`: Result of acquiring a sandbox from SandboxManager
//! - `SandboxLease`: Guard lease for execution isolation
//! - `SandboxExecutionGuard`: Prevents concurrent execution for isolation keys
//! - `SandboxIsolationKey`: Key for sandbox isolation

mod context;
mod guard;
mod key;
mod lease;
mod manager;
mod state;
mod workspace_spec;

pub use context::SandboxContext;
pub use guard::SandboxExecutionGuard;
pub use key::SandboxIsolationKey;
pub use lease::SandboxLease;
pub use manager::{SandboxAcquireResult, SandboxManager};
pub use state::{InMemorySandboxStateStore, LocalSandboxState, SandboxState, SandboxStateStore};
pub use workspace_spec::{
    CopySpec, DefaultWorkspaceSpecApplier, ExecResult, WorkspaceSpec, WorkspaceSpecApplier,
};

// Core traits
use agentscope_core::error::Result;
use agentscope_core::agent::RuntimeContext;

/// An active sandbox with a fully isolated workspace.
/// 
/// # Lifecycle
/// 
/// 1. Acquire via `SandboxClient::create` (new) or `SandboxClient::resume` (existing)
/// 2. Call `start()` — initializes or restores the workspace
/// 3. Use `exec` for command execution, `persist_workspace`/`hydrate_workspace` for archive operations
/// 4. Call `stop()` — persists the snapshot (does NOT destroy resources)
/// 5. Call `shutdown()` — destroys backend resources (tmpdir, container)
/// 6. Or use `close()` which calls stop + shutdown in sequence
/// 
/// The distinction between `stop()` and `shutdown()` is critical:
/// - `stop()`: persist snapshot only — safe for both self-managed and user-managed sandboxes
/// - `shutdown()`: destroy backend resources — only called on self-managed sandboxes
#[async_trait::async_trait]
pub trait Sandbox: Send + Sync + std::fmt::Debug {
    /// Initializes or restores the workspace.
    async fn start(&mut self) -> Result<()>;
    
    /// Persists the snapshot (does NOT destroy resources).
    async fn stop(&mut self) -> Result<()>;
    
    /// Destroys backend resources (tmpdir, container).
    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
    
    /// Close = stop + shutdown in sequence.
    async fn close(&mut self) -> Result<()> {
        self.stop().await?;
        self.shutdown().await?;
        Ok(())
    }
    
    /// Returns true if the sandbox is currently running.
    fn is_running(&self) -> bool;
    
    /// Returns the current serializable state of this sandbox.
    fn state(&self) -> &SandboxState;
    
    /// Runs a shell command in the sandbox workspace.
    async fn exec(
        &self,
        runtime_context: Option<&RuntimeContext>,
        command: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<ExecResult>;
    
    /// Archives the workspace and returns the archive data.
    async fn persist_workspace(&self) -> Result<Vec<u8>>;
    
    /// Restores the workspace from archive data.
    async fn hydrate_workspace(&self, archive: &[u8]) -> Result<()>;
}

/// Factory for creating and resuming `Sandbox` instances.
#[async_trait::async_trait]
pub trait SandboxClient: Send + Sync {
    /// Creates a new sandbox with the given workspace spec and snapshot spec.
    /// 
    /// Returned in a pre-start state; call `Sandbox::start()` before use.
    async fn create(
        &self,
        workspace_spec: &WorkspaceSpec,
        _snapshot_spec: Option<&SandboxSnapshotSpec>,
        _options: Option<&SandboxClientOptions>,
    ) -> Result<Box<dyn Sandbox>>;
    
    /// Resumes a sandbox from previously serialized `SandboxState`.
    async fn resume(&self, state: &SandboxState) -> Result<Box<dyn Sandbox>>;
    
    /// Deletes a sandbox and its resources.
    async fn delete(&self, sandbox: &dyn Sandbox) -> Result<()>;
    
    /// Serializes sandbox state to JSON.
    fn serialize_state(&self, state: &SandboxState) -> Result<String>;
    
    /// Deserializes sandbox state from JSON.
    fn deserialize_state(&self, json: &str) -> Result<SandboxState>;
    
    /// Deserializes sandbox state from JSON with snapshot spec.
    fn deserialize_state_with_spec(
        &self,
        json: &str,
        _snapshot_spec: Option<&SandboxSnapshotSpec>,
    ) -> Result<SandboxState> {
        self.deserialize_state(json)
    }
}

/// Options for creating a sandbox client.
#[derive(Debug, Clone, Default)]
pub struct SandboxClientOptions {
    /// Docker image to use (for Docker sandboxes).
    pub docker_image: Option<String>,
    
    /// Resource limits.
    pub resource_limits: ResourceLimits,
    
    /// Additional environment variables.
    pub environment: std::collections::HashMap<String, String>,
    
    /// Network mode.
    pub network_mode: Option<String>,
    
    /// Volumes to mount.
    pub volumes: Vec<VolumeMount>,
}

/// Resource limits for a sandbox.
#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    /// Memory limit in bytes.
    pub memory_limit: Option<u64>,
    
    /// CPU limit in cores.
    pub cpu_limit: Option<f64>,
    
    /// Disk limit in bytes.
    pub disk_limit: Option<u64>,
}

/// Volume mount configuration.
#[derive(Debug, Clone)]
pub struct VolumeMount {
    /// Host path.
    pub host_path: String,
    
    /// Container path.
    pub container_path: String,
    
    /// Read-only mount.
    pub read_only: bool,
}

/// Snapshot specification for sandbox persistence.
#[derive(Debug, Clone, Default)]
pub struct SandboxSnapshotSpec {
    /// Whether to enable snapshots.
    pub enabled: bool,
    
    /// Snapshot directory path.
    pub snapshot_dir: Option<String>,
    
    /// Maximum number of snapshots to keep.
    pub max_snapshots: Option<usize>,
}

/// Errors specific to sandbox operations.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("Sandbox not running")]
    NotRunning,
    
    #[error("Sandbox already running")]
    AlreadyRunning,
    
    #[error("Command execution failed: {0}")]
    ExecutionFailed(String),
    
    #[error("Workspace operation failed: {0}")]
    WorkspaceOperationFailed(String),
    
    #[error("State serialization failed: {0}")]
    SerializationFailed(String),
    
    #[error("Docker error: {0}")]
    DockerError(String),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Other error: {0}")]
    Other(String),
}

// Convert SandboxError to agentscope_core::error::Error
impl From<SandboxError> for agentscope_core::error::Error {
    fn from(err: SandboxError) -> Self {
        agentscope_core::error::Error::Other(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_sandbox_error_display() {
        let err = SandboxError::NotRunning;
        assert_eq!(err.to_string(), "Sandbox not running");
        
        let err = SandboxError::ExecutionFailed("command not found".to_string());
        assert_eq!(err.to_string(), "Command execution failed: command not found");
    }
    
    #[test]
    fn test_resource_limits_default() {
        let limits = ResourceLimits::default();
        assert!(limits.memory_limit.is_none());
        assert!(limits.cpu_limit.is_none());
        assert!(limits.disk_limit.is_none());
    }
}