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

    /// A compare-and-set expectation did not match the stored version.
    ///
    /// Mirrors Java `MysqlAgentStateStore#updateIfVersion`, which reports the sentinel
    /// `UNVERSIONED` and never throws: a stale expectation is a normal optimistic-locking
    /// outcome (re-read and retry), not a caller mistake. The infallible `AgentStateStore`
    /// path already folds it back into `UNVERSIONED`; this variant is what the fallible
    /// inherent API returns, so callers can branch on it without parsing the message.
    #[error("version conflict for key '{key}': expected {expected}, found {found}")]
    Conflict {
        /// The key whose expectation failed.
        key: String,
        /// The version the caller expected.
        expected: i64,
        /// The version found in the store (`0` when the row is missing).
        found: i64,
    },

    /// A server-reported error, classified by MySQL error code and SQLSTATE instead of by
    /// the (localisable, driver-versioned) message text.
    ///
    /// Mirrors Java `MysqlAgentStateStore#insertIfAbsent` (`errorCode == 1062 ||
    /// "23000".equals(sqlState)`) and `JdbcStore#isDuplicateKey` (SQLSTATE prefix `23`):
    /// both decide on the code, never on the wording. `sql_state` is `None` when the
    /// server sent no SQLSTATE.
    #[error("mysql server error {code}: {message}")]
    Server {
        /// MySQL server error code (`1062` = duplicate entry, `1054` = unknown column, …).
        code: u16,
        /// SQLSTATE as reported by the server (`"23000"` for integrity violations).
        sql_state: Option<String>,
        /// The server message, kept for diagnostics only.
        message: String,
    },

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

/// MySQL error code for a duplicate key / unique constraint violation
/// (`ER_DUP_ENTRY`), mirroring Java's `errorCode == 1062`.
pub const DUPLICATE_KEY_CODE: u16 = 1062;

/// SQLSTATE class `23` = integrity constraint violation (unique / primary key included).
pub const INTEGRITY_CONSTRAINT_CLASS: &str = "23";

/// Message heuristic kept as the last-resort fallback for `MysqlError::Sql` values that
/// carry no structured code (e.g. an error stringified by a non-`mysql_async` seam).
fn message_reports_duplicate_key(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("duplicate entry")
        || lower.contains("duplicate key")
        || lower.contains("unique constraint")
        || lower.contains("integrity constraint")
        || lower.contains("sqlite_constraint")
        || lower.contains("constraint failed")
}

impl MysqlError {
    /// Returns `true` when the underlying database reported a primary-key / unique
    /// constraint violation, which the CAS create-if-absent path treats as a failed CAS.
    ///
    /// Mirrors `JdbcStore.isDuplicateKey`: MySQL/Postgres/H2 report SQLSTATE `23xxx`,
    /// SQLite reports error code 19. Classification prefers the structured
    /// [`MysqlError::Server`] code / SQLSTATE and only falls back to the message text,
    /// because a localised or re-worded driver message must not turn a duplicate key into a
    /// hard failure.
    pub fn is_duplicate_key(&self) -> bool {
        match self {
            MysqlError::Server {
                code,
                sql_state,
                message,
            } => {
                *code == DUPLICATE_KEY_CODE
                    || sql_state
                        .as_deref()
                        .is_some_and(|s| s.starts_with(INTEGRITY_CONSTRAINT_CLASS))
                    || message_reports_duplicate_key(message)
            }
            MysqlError::Sql(msg) => message_reports_duplicate_key(msg),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn server(code: u16, state: Option<&str>, message: &str) -> MysqlError {
        MysqlError::Server {
            code,
            sql_state: state.map(str::to_string),
            message: message.to_string(),
        }
    }

    /// Java `insertIfAbsent`: `errorCode == 1062 || "23000".equals(sqlState)`.
    #[test]
    fn server_code_classifies_duplicate_keys() {
        assert!(server(DUPLICATE_KEY_CODE, None, "whatever the driver says").is_duplicate_key());
        // SQLSTATE class 23 alone is enough, even with an unfamiliar code/message.
        assert!(server(9999, Some("23000"), "constraint violated").is_duplicate_key());
    }

    #[test]
    fn other_server_errors_are_not_duplicate_keys() {
        assert!(!server(1054, Some("42S22"), "Unknown column 'x' in 'field list'")
            .is_duplicate_key());
        assert!(!server(1064, Some("42000"), "syntax error").is_duplicate_key());
    }

    /// The legacy stringified path keeps working as a fallback.
    #[test]
    fn message_text_remains_a_fallback() {
        assert!(MysqlError::Sql("Duplicate entry 'k' for key 'PRIMARY'".into()).is_duplicate_key());
        assert!(!MysqlError::Sql("connection reset by peer".into()).is_duplicate_key());
        assert!(!MysqlError::LockTimeout("l".into()).is_duplicate_key());
    }

    #[test]
    fn server_error_display_carries_code_and_message() {
        let err = server(1062, Some("23000"), "Duplicate entry 'a' for key 'PRIMARY'");
        let text = err.to_string();
        assert!(text.contains("1062"), "unexpected display: {text}");
        assert!(text.contains("Duplicate entry"), "unexpected display: {text}");
    }
}
