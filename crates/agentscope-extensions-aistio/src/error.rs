//! Error type for the aistio extension, mirroring the unchecked exceptions the Java
//! control-plane clients raise on HTTP failures.

use thiserror::Error;

/// Errors surfaced by aistio transport / stores / bridge.
#[derive(Debug, Error)]
pub enum AistioError {
    /// Configuration was invalid (missing required field, inconsistent flags).
    #[error("invalid aistio configuration: {0}")]
    Config(String),

    /// HTTP transport failure (connect, timeout, decode).
    #[error("aistio http error: {0}")]
    Http(String),

    /// A control-plane endpoint returned an unexpected status code.
    #[error("aistio control-plane error: {status} {message}")]
    ControlPlane {
        /// HTTP status code returned by the control plane.
        status: u16,
        /// Human-readable detail (response body or operation label).
        message: String,
    },

    /// The requested snapshot does not exist.
    #[error("aistio snapshot not found: {0}")]
    SnapshotNotFound(String),

    /// The requested session / resource does not exist on this instance (maps to HTTP 404).
    #[error("aistio not found: {0}")]
    NotFound(String),

    /// The session is busy and the caller should wait until idle (maps to HTTP 409).
    #[error("aistio busy: {0}")]
    Busy(String),

    /// The request carried no / a wrong internal token (maps to HTTP 401).
    #[error("aistio unauthorized: {0}")]
    Unauthorized(String),

    /// The operation is declared as a capability but not implemented.
    #[error("unsupported aistio operation: {0}")]
    Unsupported(String),
}

/// Convenience result alias.
pub type AistioResult<T> = std::result::Result<T, AistioError>;
