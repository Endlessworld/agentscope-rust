//! Error type for the Tencent Cloud COS extension.

use thiserror::Error;

/// Errors produced by the COS-backed storage components.
#[derive(Debug, Error)]
pub enum CosError {
    /// A required configuration value was missing or invalid.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The backing object store returned an error.
    #[error("cos object store error: {0}")]
    Backend(String),

    /// A stored payload could not be (de)serialized.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// A required snapshot object was not found.
    #[error("snapshot not found: {0}")]
    NotFound(String),
}

/// Convenience result alias for this crate.
pub type Result<T> = std::result::Result<T, CosError>;
