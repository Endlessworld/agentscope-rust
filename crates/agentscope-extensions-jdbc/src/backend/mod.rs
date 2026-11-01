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

/// A failure reported by the database, together with the driver's structured codes when it
/// exposes them.
///
/// Java's components classify failures on the thrown exception type and its `getSQLState()`
/// (`JdbcAgentStateStore.isDuplicateKey`). Rust drivers hand back an error value, so the
/// classification lives here instead of on the message text: SQLite's message reads
/// "constraint failed" for **every** constraint class, which made a NOT NULL / CHECK /
/// FOREIGN KEY violation indistinguishable from a duplicate key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    message: String,
    /// The five-character ANSI `SQLSTATE` (e.g. `23505`), when the driver reports one.
    sql_state: Option<String>,
    /// The driver's own error code: MySQL `ER_DUP_ENTRY = 1062`, SQLite's **extended**
    /// result code (`2067` unique, `1555` primary key), and so on.
    vendor_code: Option<i64>,
}

/// `SQLSTATE` of a unique/primary-key violation (`unique_violation`). Other `23xxx`
/// integrity violations — `23502` NOT NULL, `23503` foreign key, `23514` CHECK — are not
/// duplicates.
const UNIQUE_VIOLATION_SQL_STATE: &str = "23505";

impl DbError {
    /// Wraps a message from a driver that reported no structured code.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            sql_state: None,
            vendor_code: None,
        }
    }

    /// Attaches the driver's `SQLSTATE`.
    pub fn with_sql_state(mut self, sql_state: impl Into<String>) -> Self {
        self.sql_state = Some(sql_state.into());
        self
    }

    /// Attaches the driver's own error code.
    pub fn with_vendor_code(mut self, code: i64) -> Self {
        self.vendor_code = Some(code);
        self
    }

    /// The human-readable message (no driver code prefix).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The `SQLSTATE`, when the driver reported one.
    pub fn sql_state(&self) -> Option<&str> {
        self.sql_state.as_deref()
    }

    /// The driver's own error code, when it reported one.
    pub fn vendor_code(&self) -> Option<i64> {
        self.vendor_code
    }

    /// Whether this is a unique/primary-key violation, mirroring the Java `isDuplicateKey`.
    ///
    /// Structured codes decide when present; only an error that carries none falls back to
    /// the message, and then only on unambiguous wording (`unique` / `duplicate` /
    /// `primary key`) — the SQLite messages for the other constraint classes say
    /// "constraint failed" and must not match.
    pub fn is_unique_violation(&self) -> bool {
        // Driver codes are the strongest signal: MySQL reports `SQLSTATE 23000` (generic
        // integrity violation) for a duplicate key, so its state alone cannot decide.
        if let Some(code) = self.vendor_code {
            if matches!(
                code,
                MYSQL_DUPLICATE_ENTRY | SQLITE_CONSTRAINT_UNIQUE | SQLITE_CONSTRAINT_PRIMARYKEY
            ) {
                return true;
            }
        }
        match self.sql_state.as_deref() {
            Some(UNIQUE_VIOLATION_SQL_STATE) => true,
            // Any other integrity violation: the driver named the class, so the text must not
            // be re-interpreted (this is the NOT NULL / CHECK / FK false positive).
            Some(state) if state.starts_with("23") => false,
            _ => {
                let lower = self.message.to_ascii_lowercase();
                lower.contains("unique")
                    || lower.contains("duplicate")
                    || lower.contains("primary key")
                    || lower.contains("23505")
            }
        }
    }
}

/// MySQL `ER_DUP_ENTRY`.
const MYSQL_DUPLICATE_ENTRY: i64 = 1062;
/// SQLite `SQLITE_CONSTRAINT_UNIQUE` (extended; base code is 19).
const SQLITE_CONSTRAINT_UNIQUE: i64 = 2067;
/// SQLite `SQLITE_CONSTRAINT_PRIMARYKEY` (extended; base code is 19).
const SQLITE_CONSTRAINT_PRIMARYKEY: i64 = 1555;

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Error type for the execution backend.
#[derive(Debug, thiserror::Error)]
pub enum JdbcError {
    /// An error originating from the underlying database driver.
    #[error("database error: {0}")]
    Database(DbError),
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

impl JdbcError {
    /// Wraps a database failure for which the driver reported no structured code.
    ///
    /// Use [`DbError::new`] + [`DbError::with_sql_state`] / [`DbError::with_vendor_code`]
    /// directly when the driver does report one (see `backend::driver::*::map_error` and
    /// `backend::sqlite`).
    pub fn database(message: impl Into<String>) -> Self {
        JdbcError::Database(DbError::new(message))
    }
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

/// Returns whether the database error is a unique/primary-key violation.
///
/// Mirrors Java `JdbcAgentStateStore.isDuplicateKey`, but on the driver's structured code
/// ([`DbError::is_unique_violation`]) rather than a SQLState prefix: a `23xxx` prefix also
/// covers NOT NULL (`23502`), foreign-key (`23503`) and CHECK (`23514`) violations, which
/// must not be mistaken for "the row already exists" (a lock-table insert would then poll as
/// if the lock were held, and `save_if_version(0)` would take the self-heal branch).
pub fn is_unique_violation(error: &JdbcError) -> bool {
    match error {
        JdbcError::Database(db) => db.is_unique_violation(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(db: DbError) -> JdbcError {
        JdbcError::Database(db)
    }

    #[test]
    fn unique_sqlstate_is_a_duplicate() {
        // PostgreSQL / H2 unique violation.
        let error = database(DbError::new("duplicate").with_sql_state("23505"));
        assert!(is_unique_violation(&error));
    }

    #[test]
    fn other_integrity_sqlstates_are_not_duplicates() {
        // PostgreSQL words a NOT NULL violation as "23502", a FK violation as "23503" and a
        // CHECK violation as "23514"; all of them are `23xxx` class yet none is a duplicate.
        for state in ["23502", "23503", "23514", "23000"] {
            let error = database(DbError::new("constraint violated").with_sql_state(state));
            assert!(!is_unique_violation(&error), "{state} is not a duplicate");
        }
        // The message text must not override a structured "not a duplicate" verdict, even
        // when it happens to contain the word SQLite uses for every constraint class.
        let error = database(
            DbError::new("duplicate key value violates ... constraint failed")
                .with_sql_state("23502"),
        );
        assert!(!is_unique_violation(&error));
    }

    #[test]
    fn vendor_codes_decide_ahead_of_a_generic_sqlstate() {
        // MySQL reports a duplicate key as SQLSTATE 23000 (generic integrity violation) plus
        // its own ER_DUP_ENTRY = 1062.
        let error = database(
            DbError::new("Duplicate entry 'k' for key 'PRIMARY'").with_sql_state("23000"),
        );
        assert!(!is_unique_violation(&error), "23000 alone is ambiguous");
        let error = database(
            DbError::new("Duplicate entry 'k' for key 'PRIMARY'")
                .with_sql_state("23000")
                .with_vendor_code(1062),
        );
        assert!(is_unique_violation(&error));
        // SQLite extended result codes.
        for code in [2067, 1555] {
            let error = database(DbError::new("constraint failed").with_vendor_code(code));
            assert!(is_unique_violation(&error), "sqlite {code} is a duplicate");
        }
        // SQLite NOT NULL (1299) / CHECK (275) / FOREIGN KEY (787) are not.
        for code in [1299, 275, 787] {
            let error = database(DbError::new("constraint failed").with_vendor_code(code));
            assert!(!is_unique_violation(&error), "sqlite {code} is not a duplicate");
        }
    }

    #[test]
    fn text_only_errors_use_unambiguous_wording() {
        // Drivers/backends that only surface text (or hosts constructing their own errors).
        assert!(is_unique_violation(&JdbcError::database(
            "UNIQUE constraint failed: agentscope_store.item_key"
        )));
        assert!(is_unique_violation(&JdbcError::database(
            "duplicate key value violates unique constraint"
        )));
        assert!(is_unique_violation(&JdbcError::database(
            "PRIMARY KEY must be unique"
        )));
        // "constraint failed" alone used to match every constraint class.
        assert!(!is_unique_violation(&JdbcError::database(
            "NOT NULL constraint failed: agentscope_store.value_json"
        )));
        assert!(!is_unique_violation(&JdbcError::database(
            "CHECK constraint failed: version > 0"
        )));
        assert!(!is_unique_violation(&JdbcError::database(
            "FOREIGN KEY constraint failed"
        )));
        // Non-database failures are never duplicates.
        assert!(!is_unique_violation(&JdbcError::Unsupported("nope".into())));
    }
}
