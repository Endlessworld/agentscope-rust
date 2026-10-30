//! Error type for the PostgreSQL extension.
//!
//! Mirrors the `RuntimeException`/`IllegalStateException` surfaces of the Java module.

use agentscope_core::error::Error as CoreError;

/// Errors raised by the PostgreSQL-backed stores.
#[derive(Debug, thiserror::Error)]
pub enum PostgresError {
    /// A SQL statement failed to execute.
    #[error("PostgreSQL query failed: {0}")]
    Query(String),
    /// Schema initialization failed.
    #[error("PostgreSQL schema initialization failed: {0}")]
    Schema(String),
    /// A row could not be decoded into the expected shape.
    #[error("PostgreSQL row decode failed: {0}")]
    Decode(String),
    /// A value could not be encoded for storage.
    #[error("PostgreSQL value encode failed: {0}")]
    Encode(String),
    /// A supplied identifier (schema/table/key) is invalid.
    #[error("Invalid identifier: {0}")]
    InvalidIdentifier(String),
    /// A lock could not be acquired within the configured timeout.
    #[error("Timed out waiting for PostgreSQL advisory lock: {0}")]
    LockTimeout(String),
    /// The optional `postgres-driver` feature was not enabled.
    #[error("The `postgres-driver` feature is required for a live PostgreSQL connection: {0}")]
    DriverUnavailable(String),
}

impl From<PostgresError> for CoreError {
    fn from(e: PostgresError) -> Self {
        CoreError::Other(e.to_string())
    }
}

/// Convenience alias for results produced by this crate.
pub type PgResult<T> = std::result::Result<T, PostgresError>;
