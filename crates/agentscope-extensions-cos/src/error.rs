//! Error type for the Tencent Cloud COS extension.

use thiserror::Error;

/// Errors produced by the COS-backed storage components.
#[derive(Debug, Error)]
pub enum CosError {
    /// A required configuration value was missing or invalid.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The backing object store failed before producing a response (network, TLS,
    /// body read, ...).
    #[error("cos object store error: {0}")]
    Backend(String),

    /// The COS service answered with a non-2xx status.
    ///
    /// `code`/`message` come from the COS `<Error>` XML body when present. Keeping
    /// the status on the error is what lets callers distinguish an authentication
    /// failure (`403`) from a missing object (`404`).
    #[error("cos service error: status={status} code={code} message={message}")]
    Service {
        /// HTTP status code returned by COS.
        status: u16,
        /// COS error code (`SignatureDoesNotMatch`, `NoSuchKey`, ...), empty when
        /// the body carried no `<Error><Code>` element.
        code: String,
        /// Human-readable message, falling back to the raw body.
        message: String,
    },

    /// A stored payload could not be (de)serialized.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// A required snapshot object was not found.
    #[error("snapshot not found: {0}")]
    NotFound(String),

    /// The client was closed and can no longer issue requests.
    #[error("cos client is closed")]
    Closed,
}

impl CosError {
    /// The HTTP status code, when the error came from the service.
    pub fn status(&self) -> Option<u16> {
        match self {
            CosError::Service { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Whether this is a `404 Not Found` from the service.
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }

    /// Whether this is an authentication/authorization rejection (`401`/`403`).
    ///
    /// A COS signature mismatch surfaces as `403` with
    /// `<Code>SignatureDoesNotMatch</Code>`; this predicate separates it from the
    /// `404` case, which `CosObjectStore::get` maps to `Ok(None)` instead.
    pub fn is_signature_error(&self) -> bool {
        matches!(self.status(), Some(401) | Some(403))
    }

    /// Whether retrying the request could succeed.
    ///
    /// Transport failures and timeouts ([`CosError::Backend`] as returned by the
    /// HTTP layer) plus the transient statuses `408`, `429` and `5xx` are
    /// retryable; any other `4xx` (signature errors, missing objects, conflicts) is
    /// a caller error. Mirrors the Java SDK `ClientConfig.maxErrorRetry` notion of
    /// a retryable service exception.
    pub fn is_retryable(&self) -> bool {
        match self {
            CosError::Backend(_) => true,
            CosError::Service { status, .. } => matches!(status, 408 | 429 | 500..=599),
            _ => false,
        }
    }
}

/// Convenience result alias for this crate.
pub type Result<T> = std::result::Result<T, CosError>;
