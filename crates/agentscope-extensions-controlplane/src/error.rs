//! Error type for the controlplane extension, mirroring the unchecked exceptions the Java
//! control-plane clients raise on HTTP failures.

use thiserror::Error;

/// Errors surfaced by controlplane transport / stores / bridge.
#[derive(Debug, Error)]
pub enum ControlPlaneError {
    /// Configuration was invalid (missing required field, inconsistent flags).
    #[error("invalid controlplane configuration: {0}")]
    Config(String),

    /// HTTP transport failure (connect, timeout, decode).
    #[error("controlplane http error: {0}")]
    Http(String),

    /// A control-plane endpoint returned an unexpected status code.
    #[error("control plane error: {status} {message}")]
    ControlPlane {
        /// HTTP status code returned by the control plane.
        status: u16,
        /// Human-readable detail (response body or operation label).
        message: String,
    },

    /// The requested snapshot does not exist.
    #[error("controlplane snapshot not found: {0}")]
    SnapshotNotFound(String),

    /// The requested session / resource does not exist on this instance (maps to HTTP 404).
    #[error("controlplane not found: {0}")]
    NotFound(String),

    /// The session is busy and the caller should wait until idle (maps to HTTP 409).
    #[error("controlplane busy: {0}")]
    Busy(String),

    /// The request carried no / a wrong internal token (maps to HTTP 401).
    #[error("controlplane unauthorized: {0}")]
    Unauthorized(String),

    /// The operation is declared as a capability but not implemented.
    #[error("unsupported controlplane operation: {0}")]
    Unsupported(String),
}

/// Convenience result alias.
pub type ControlPlaneResult<T> = std::result::Result<T, ControlPlaneError>;
