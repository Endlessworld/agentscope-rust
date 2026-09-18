//! Execution backend abstraction for JDBC components.
//!
//! Java's components take a `javax.sql.DataSource` and run `PreparedStatement`s directly.
//! Rust has no universal JDBC `DataSource`, so components depend on the [`JdbcExecutor`]
//! trait — a minimal "execute a [`BoundSql`] against a database" surface. This keeps the
//! component layer fully database-agnostic (mirroring the Java "zero inline SQL" rule)
//! while allowing arbitrary backends.
//!
//! The default concrete backend is [`RusqliteExecutor`] (SQLite, incl. in-memory), which
//! lets the H2/SQLite parity tests run without external services.

use std::sync::Arc;

use super::dialect::BoundSql;

mod sqlite;
pub use sqlite::{in_memory_executor, RusqliteExecutor};

/// A row returned by a SELECT, as already-decoded typed column values.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnValue {
    /// A text column.
    Text(String),
    /// An integer column.
    Int(i64),
    /// A binary column.
    Blob(Vec<u8>),
    /// SQL NULL.
    Null,
}

impl ColumnValue {
    /// Returns the text value, or `None` if the column is not text / is NULL.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ColumnValue::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the integer value, or `None` if the column is not an integer.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            ColumnValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns the blob value, or `None` if the column is not a blob.
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            ColumnValue::Blob(b) => Some(b),
            _ => None,
        }
    }

    /// Whether this is SQL NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, ColumnValue::Null)
    }
}

/// Result type for backend operations.
pub type JdbcResult<T> = Result<T, JdbcError>;

/// Error type for the execution backend.
#[derive(Debug, thiserror::Error)]
pub enum JdbcError {
    /// An error originating from the underlying database driver.
    #[error("database error: {0}")]
    Database(String),
    /// A feature is not supported by this backend.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

/// Minimal "execute a [`BoundSql`] against a database" surface.
///
/// All JDBC components (store, state store, snapshot client, lock strategy) operate on
/// this trait, so the same component code runs against SQLite, MySQL, PostgreSQL or H2
/// given an appropriate backend.
pub trait JdbcExecutor: Send + Sync + std::fmt::Debug {
    /// Runs a statement that mutates data (INSERT/UPDATE/DELETE).
    ///
    /// Returns the number of affected rows. A failed constraint (e.g. duplicate PK on a
    /// create-only insert) is reported as [`JdbcError::Database`] so the caller can detect
    /// it via [`is_unique_violation`].
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize>;

    /// Runs a SELECT and returns all rows, each a vector of column values in projection
    /// order.
    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>>;

    /// Runs several mutating statements atomically in a single transaction.
    ///
    /// The default implementation runs them sequentially (non-transactional); real
    /// backends should override this to provide atomicity.
    fn execute_batch(&self, bounds: &[BoundSql]) -> JdbcResult<()> {
        for bound in bounds {
            self.execute(bound)?;
        }
        Ok(())
    }

    /// Runs a batch of statements that share a SQL template, returning the total affected
    /// rows. Used for list-state appends. The default implementation runs them
    /// sequentially.
    fn execute_prepared_batch(&self, bounds: &[BoundSql]) -> JdbcResult<usize> {
        let mut total = 0;
        for bound in bounds {
            total += self.execute(bound)?;
        }
        Ok(total)
    }
}

/// A shared, thread-safe executor handle.
pub type SharedExecutor = Arc<dyn JdbcExecutor>;

/// Returns whether the database error looks like a unique/primary-key violation.
///
/// Mirrors Java `JdbcAgentStateStore.isDuplicateKey`: a `SQLIntegrityConstraintViolationException`
/// or SQLState starting with `23`. Backends render these strings differently, so match on
/// common substrings.
pub fn is_unique_violation(error: &JdbcError) -> bool {
    let JdbcError::Database(msg) = error else {
        return false;
    };
    let lower = msg.to_ascii_lowercase();
    lower.contains("unique")
        || lower.contains("primary key")
        || lower.contains("constraint failed")
        || lower.contains("duplicate")
        || lower.contains("23505")
        || msg.starts_with("23")
}
