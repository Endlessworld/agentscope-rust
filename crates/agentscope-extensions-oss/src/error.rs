//! Error type for the Alibaba Cloud OSS extension.
//!
//! Mirrors the Java module which throws `RuntimeException` on I/O failures. Rust
//! callers get a typed [`OssError`] that can be converted into
//! [`agentscope_core::error::Error`] when crossing into core/harness traits.

use agentscope_core::error::Error as CoreError;

/// Errors produced by the OSS-backed stores.
#[derive(Debug, thiserror::Error)]
pub enum OssError {
    /// The OSS client (bucket) was not configured.
    #[error("OSS client must not be null")]
    MissingClient,

    /// The bucket name was blank.
    #[error("bucketName must not be blank")]
    BlankBucket,

    /// A required identifier was blank.
    #[error("{0} must not be blank")]
    BlankIdentifier(&'static str),

    /// A snapshot id contained unsafe characters.
    #[error("snapshot id contains unsafe characters: {0}")]
    UnsafeSnapshotId(String),

    /// A snapshot was not found in OSS.
    #[error("snapshot not found in OSS: {0}")]
    SnapshotNotFound(String),

    /// JSON (de)serialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Underlying backend/transport failure.
    #[error("oss backend error: {0}")]
    Backend(String),
}

impl From<OssError> for CoreError {
    fn from(value: OssError) -> Self {
        match value {
            OssError::MissingClient | OssError::BlankBucket | OssError::BlankIdentifier(_) => {
                CoreError::Tool(value.to_string())
            }
            other => CoreError::Tool(other.to_string()),
        }
    }
}

/// Convenience result alias for OSS operations.
pub type OssResult<T> = std::result::Result<T, OssError>;
