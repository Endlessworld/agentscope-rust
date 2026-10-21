//! Artifact delivery SPI, mirroring `io.agentscope.harness.agent.artifact`.
//!
//! The framework stays transport-agnostic: the `deliver_artifact` tool reads the
//! file from the agent filesystem and hands the bytes to an application-provided
//! [`ArtifactDeliveryTarget`].

use async_trait::async_trait;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;

/// A single artifact delivery request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDeliveryRequest {
    /// Workspace-relative (or permitted absolute) source path.
    pub file_path: String,
    /// File bytes read from the agent filesystem.
    pub content: Vec<u8>,
    /// Validated plain target file name.
    pub file_name: String,
    /// Optional short description.
    pub description: Option<String>,
    /// Whether an existing destination artifact may be overwritten.
    pub force: bool,
}

/// Result of one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDeliveryResult {
    pub successful: bool,
    /// True when the target refused because a same-named artifact already exists
    /// and `force` was false.
    pub conflict: bool,
    pub error: Option<String>,
    pub message: Option<String>,
}

impl ArtifactDeliveryResult {
    pub fn success() -> Self {
        Self { successful: true, conflict: false, error: None, message: None }
    }

    pub fn success_with_message(message: impl Into<String>) -> Self {
        Self {
            successful: true,
            conflict: false,
            error: None,
            message: Some(message.into()),
        }
    }

    pub fn fail(error: impl Into<String>) -> Self {
        Self {
            successful: false,
            conflict: false,
            error: Some(error.into()),
            message: None,
        }
    }

    pub fn conflict(error: impl Into<String>) -> Self {
        Self {
            successful: false,
            conflict: true,
            error: Some(error.into()),
            message: None,
        }
    }
}

/// Application-provided delivery destination (host folder, artifact store, object
/// store, WebDAV, ...).
#[async_trait]
pub trait ArtifactDeliveryTarget: Send + Sync {
    async fn deliver(
        &self,
        runtime_context: &RuntimeContext,
        request: ArtifactDeliveryRequest,
    ) -> Result<ArtifactDeliveryResult>;
}
