//! Channel adapter error type.

use thiserror::Error;

/// Result alias used across the channel crate.
pub type Result<T> = std::result::Result<T, ChannelError>;

/// Errors raised by channel adapters.
#[derive(Debug, Error)]
#[error("channel error")]
pub enum ChannelError {
    /// Transport-level failure (DNS, TLS, timeout, connection reset, ...).
    #[error("http transport error: {0}")]
    Http(String),
    /// JSON encode/decode failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Invalid provider configuration (a required property is missing or malformed).
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The payload could not be parsed into the expected shape.
    #[error("payload parse error: {0}")]
    Parse(String),
    /// The input/peer kind is not supported by this adapter (mirrors the Java
    /// `IllegalArgumentException` branch in the outbound clients).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Cryptographic helper failure (signature mismatch, bad key material, ...).
    #[error("crypto error: {0}")]
    Crypto(String),
    /// The remote API answered with a non-success business code.
    #[error("remote error: status={status} body={body}")]
    Remote { status: u16, body: String },
}
