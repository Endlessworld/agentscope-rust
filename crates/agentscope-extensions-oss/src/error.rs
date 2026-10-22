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

    /// The generic S3 backend (`rust-s3`) failed.
    ///
    /// Kept as a typed variant (rather than a `String`) so callers can inspect
    /// HTTP status codes / retryability through [`s3::error::S3Error`].
    #[cfg(feature = "s3")]
    #[error("s3 backend error: {0}")]
    S3(#[from] s3::error::S3Error),

    /// S3 client configuration was invalid (bad region/endpoint/runtime).
    #[cfg(feature = "s3")]
    #[error("s3 configuration error: {0}")]
    S3Config(String),

    /// S3 credentials were neither supplied nor found in the environment.
    #[cfg(feature = "s3")]
    #[error(
        "S3 credentials are required: set access_key/secret_key or \
         AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY"
    )]
    MissingCredentials,

    /// An S3 request returned an unexpected HTTP status (only reachable when
    /// `rust-s3` does not turn the response into an error itself).
    #[cfg(feature = "s3")]
    #[error("s3 request failed with HTTP {status}: {body}")]
    S3Http { status: u16, body: String },
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
