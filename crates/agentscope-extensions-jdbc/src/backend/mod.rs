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

use super::dialect::{BoundSql, NativeLockSession};

pub mod driver;
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
    /// The backend is not configured (missing connection string / environment variable).
    ///
    /// Distinct from [`JdbcError::Database`] so callers — and the env-gated integration
    /// tests — can tell "no server configured" apart from "server said no", instead of
    /// silently dialling `localhost`.
    #[error("invalid configuration: {0}")]
    Configuration(String),
}

/// Minimal statement-execution surface used inside a transaction.
///
/// A [`JdbcExecutor`] is `Send + Sync` so it can be shared across threads; a transaction
/// is bound to one connection and therefore must not escape the closure it was opened
/// in. This trait is that connection-scoped view: the default
/// [`JdbcExecutor::in_transaction`] hands out a non-escaping view of the executor itself,
/// while real backends (e.g. [`RusqliteExecutor`]) hand out a view over an open
/// transaction.
pub trait JdbcSession {
    /// Runs a mutating statement on the session's connection.
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize>;

    /// Runs a SELECT on the session's connection.
    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>>;
}

/// Session view over an executor that has no transaction support.
struct ExecutorSession<'a, T: ?Sized>(&'a T);

impl<T: JdbcExecutor + ?Sized> JdbcSession for ExecutorSession<'_, T> {
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize> {
        self.0.execute(bound)
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        self.0.query(bound)
    }
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

    /// Opens a **dedicated** database session able to hold a server-scoped advisory lock.
    ///
    /// Native locks (`MySQL GET_LOCK`, `PostgreSQL pg_try_advisory_lock`) are session-scoped:
    /// the Java implementation opens one `Connection` per lease and releases the lock by
    /// closing it. [`JdbcExecutor`] itself is a *shared* handle (see [`RusqliteExecutor`]:
    /// one connection behind a mutex) and cannot express that ownership, so a backend that
    /// can open exclusive connections overrides this method to hand out a
    /// [`NativeLockSession`]; [`crate::dialect::ExecutorNativeLockProvider`] turns such an
    /// executor into a [`crate::dialect::NativeLockSessionProvider`] for
    /// [`crate::sandbox::NativeLockGuard`].
    ///
    /// The default (and [`RusqliteExecutor`]'s) answer is [`JdbcError::Unsupported`] — the
    /// caller should fall back to the portable [`crate::sandbox::TableLockStrategy`].
    fn open_native_lock_session(&self) -> JdbcResult<Box<dyn NativeLockSession>> {
        Err(JdbcError::Unsupported(format!(
            "{} does not provide exclusive native advisory-lock sessions",
            std::any::type_name::<Self>()
        )))
    }

    /// Runs `work` as one transaction, rolling back on error.
    ///
    /// Needed by components whose statements depend on a value produced by an earlier
    /// statement (e.g. the skill repository reads the auto-increment id before inserting
    /// resources), which [`Self::execute_batch`] cannot express. The default
    /// implementation is non-transactional (it simply runs `work` against `self`); real
    /// backends override it.
    ///
    /// **Precondition for multi-statement writers** — `JdbcAgentSkillRepository` writes
    /// `agentscope_skills` and then, using the id it read back, `agentscope_skill_resources`;
    /// `JdbcAgentStateStore` reads a list's hash and item count and then writes the append (or
    /// the rewrite) plus the hash row; `save_if_version(UNVERSIONED)` on a dialect without a
    /// returning UPSERT writes and then reads the version. An executor that does **not**
    /// override this method has no atomicity at all in those paths: a failed resource insert
    /// leaves an orphan skill row, a concurrent writer makes the append collide on the item
    /// index, and the version read can report another writer's version. In this crate
    /// `RusqliteExecutor` and every `backend::driver::DriverExecutor` override it (`BEGIN` …
    /// `COMMIT` / `ROLLBACK` on one connection); any new executor **must** do the same, or the
    /// skill repository and the state store must not be wired to it.
    fn in_transaction(
        &self,
        work: &mut dyn FnMut(&mut dyn JdbcSession) -> JdbcResult<()>,
    ) -> JdbcResult<()> {
        let mut session = ExecutorSession(self);
        work(&mut session)
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
