//! Error types shared by the sandbox extension backends.
//!
//! Mirrors the Java `SandboxException` hierarchy (configuration / runtime errors)
//! and provides a `SandboxErrorCode` enum equivalent to
//! `io.agentscope.harness.agent.sandbox.SandboxErrorCode`.

use agentscope_core::error::Error;

/// Error codes mirroring `SandboxErrorCode` in the Java harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxErrorCode {
    /// Failure while creating or starting the remote workspace.
    WorkspaceStartError,
    /// Failure while stopping / persisting the workspace.
    WorkspaceStopError,
    /// A command execution failed on the remote side.
    CommandExecutionError,
    /// Failure while reading a workspace archive from the remote sandbox.
    WorkspaceArchiveReadError,
    /// Failure while writing a workspace archive to the remote sandbox.
    WorkspaceArchiveWriteError,
    /// The remote sandbox could not be reached or answered with an unexpected payload.
    RemoteTransportError,
    /// The supplied client options are invalid.
    ConfigurationError,
}

impl SandboxErrorCode {
    /// Stable string discriminator used in error messages and (de)serialization.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxErrorCode::WorkspaceStartError => "WORKSPACE_START_ERROR",
            SandboxErrorCode::WorkspaceStopError => "WORKSPACE_STOP_ERROR",
            SandboxErrorCode::CommandExecutionError => "COMMAND_EXECUTION_ERROR",
            SandboxErrorCode::WorkspaceArchiveReadError => "WORKSPACE_ARCHIVE_READ_ERROR",
            SandboxErrorCode::WorkspaceArchiveWriteError => "WORKSPACE_ARCHIVE_WRITE_ERROR",
            SandboxErrorCode::RemoteTransportError => "REMOTE_TRANSPORT_ERROR",
            SandboxErrorCode::ConfigurationError => "CONFIGURATION_ERROR",
        }
    }
}

/// Errors produced by the sandbox extension backends.
#[derive(Debug, thiserror::Error)]
pub enum SandboxExtensionError {
    /// Invalid client options (checked eagerly before any network call).
    #[error("sandbox configuration error: {0}")]
    Configuration(String),

    /// A remote runtime failure carrying an explicit error code.
    #[error("sandbox runtime error [{code}]: {msg}", code = .0.as_str(), msg = .1)]
    Runtime(SandboxErrorCode, String),

    /// Transport-level failure (connect/timeout/body decode).
    #[error("sandbox transport error: {0}")]
    Transport(String),

    /// The HTTP response carried a non-success status.
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    /// Serialization / deserialization failure.
    #[error("sandbox serialization error: {0}")]
    Serialization(String),

    /// The operation is not supported by this backend.
    #[error("unsupported sandbox operation: {0}")]
    Unsupported(String),
}

impl SandboxExtensionError {
    /// Creates an HTTP error, used by the retry helper to classify transient failures.
    pub fn http(status: u16, body: impl Into<String>) -> Self {
        SandboxExtensionError::Http {
            status,
            body: body.into(),
        }
    }

    /// Returns the HTTP status code if this is an [`SandboxExtensionError::Http`].
    pub fn status(&self) -> Option<u16> {
        match self {
            SandboxExtensionError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl From<SandboxExtensionError> for Error {
    fn from(err: SandboxExtensionError) -> Self {
        Error::Other(err.to_string())
    }
}

/// Convenience alias for sandbox extension results.
pub type SandboxResult<T> = std::result::Result<T, SandboxExtensionError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_strings() {
        assert_eq!(
            SandboxErrorCode::WorkspaceStartError.as_str(),
            "WORKSPACE_START_ERROR"
        );
        assert_eq!(
            SandboxErrorCode::ConfigurationError.as_str(),
            "CONFIGURATION_ERROR"
        );
    }

    #[test]
    fn runtime_error_display_contains_code() {
        let err = SandboxExtensionError::Runtime(
            SandboxErrorCode::WorkspaceStartError,
            "boom".to_string(),
        );
        let msg = err.to_string();
        assert!(msg.contains("WORKSPACE_START_ERROR"));
        assert!(msg.contains("boom"));
    }

    #[test]
    fn converts_to_core_error() {
        let err: Error = SandboxExtensionError::Configuration("no key".into()).into();
        assert!(err.to_string().contains("no key"));
    }
}
