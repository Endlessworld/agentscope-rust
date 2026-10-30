//! Error types for the MySQL extension crate.
//!
//! Mirrors the failure modes surfaced by the Java `MysqlAgentStateStore`,
//! `JdbcStore`, `JdbcSnapshotSpec` and `JdbcSandboxExecutionGuard`.

use thiserror::Error;

/// Errors produced by the MySQL/JDBC-backed storage components.
#[derive(Debug, Error)]
pub enum MysqlError {
    /// A generic SQL/JDBC-level failure.
    #[error("mysql error: {0}")]
    Sql(String),

    /// A key/namespace/identifier failed validation.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Session id was blank.
    #[error("sessionId must not be blank")]
    BlankSessionId,

    /// A stored value could not be (de)serialised from/to JSON.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A schema DDL or existence verification failed.
    #[error("schema error: {0}")]
    Schema(String),

    /// A distributed lock could not be acquired within its timeout.
    #[error("timed out waiting for lock: {0}")]
    LockTimeout(String),

    /// Internal consistency failure (e.g. lock bookkeeping).
    #[error("internal error: {0}")]
    Internal(String),

    /// No usable database driver: the `mysql_async` feature is off, the URL is invalid, or
    /// the server could not be reached.
    ///
    /// The driver-backed backend ([`crate::MysqlAsyncBackend`]) reports this **at
    /// construction**, so a missing driver or an unreachable server can never be mistaken
    /// for a working store that silently drops writes.
    #[error("mysql driver unavailable: {0}")]
    DriverUnavailable(String),
}

impl MysqlError {
    /// Returns `true` when the underlying database reported a primary-key / unique
    /// constraint violation, which the CAS create-if-absent path treats as a failed CAS.
    ///
    /// Mirrors `JdbcStore.isDuplicateKey`: MySQL/Postgres/H2 report SQLSTATE `23xxx`,
    /// SQLite reports error code 19.
    pub fn is_duplicate_key(&self) -> bool {
        match self {
            MysqlError::Sql(msg) => {
                let lower = msg.to_ascii_lowercase();
                lower.contains("duplicate entry")
                    || lower.contains("duplicate key")
                    || lower.contains("unique constraint")
                    || lower.contains("integrity constraint")
                    || lower.contains("sqlite_constraint")
                    || lower.contains("constraint failed")
            }
            _ => false,
        }
    }
}

impl From<agentscope_core::error::Error> for MysqlError {
    fn from(e: agentscope_core::error::Error) -> Self {
        MysqlError::Internal(e.to_string())
    }
}

/// Convenience alias.
pub type MysqlResult<T> = Result<T, MysqlError>;
