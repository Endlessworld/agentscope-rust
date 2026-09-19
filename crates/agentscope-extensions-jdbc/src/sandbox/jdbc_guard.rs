//! JDBC-backed [`SandboxExecutionGuard`], mirroring `JdbcSandboxExecutionGuard`.
//!
//! Delegates all lock semantics to a [`SandboxLockStrategy`] provided by the dialect. The
//! component contains zero database-specific logic — the dialect handles whether to use
//! MySQL `GET_LOCK`, PostgreSQL `pg_try_advisory_lock`, or the portable table-based lock.
//!
//! Each lock is identified by a string key derived from the [`SandboxIsolationKey`]'s
//! scope and value.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_harness::agent::sandbox::{SandboxIsolationKey, SandboxLease};

use crate::backend::{JdbcResult, SharedExecutor};
use crate::dialect::{AbstractJdbcDialect, BoundSql, LockLease, LockOutcome, SandboxLockStrategy, SqlParam};

/// Default prefix applied to composed lock names.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:sandbox:lock:";
/// Default lock timeout, mirroring the Java 30-minute default.
pub const DEFAULT_LOCK_TIMEOUT_SECONDS: u64 = 1800;

/// A portable, table-backed [`SandboxLockStrategy`].
///
/// Acquires a named lock by inserting a row into the dialect's lock table. Acquisition
/// polls until the timeout expires; release deletes the row. This works on any JDBC
/// database without native advisory locks (the H2/SQLite/portable default). MySQL and
/// PostgreSQL dialects can expose native strategies instead.
pub struct TableLockStrategy {
    executor: SharedExecutor,
    dialect: Arc<dyn AbstractJdbcDialect>,
    ensured: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for TableLockStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableLockStrategy")
            .field("kind", &self.dialect.kind())
            .finish_non_exhaustive()
    }
}

impl TableLockStrategy {
    /// Creates a portable table lock strategy, ensuring the lock table exists.
    pub fn new(executor: SharedExecutor, dialect: Arc<dyn AbstractJdbcDialect>) -> JdbcResult<Self> {
        let strategy = Self {
            executor,
            dialect,
            ensured: std::sync::atomic::AtomicBool::new(false),
        };
        strategy.ensure_lock_table()?;
        Ok(strategy)
    }

    fn ensure_lock_table(&self) -> JdbcResult<()> {
        use std::sync::atomic::Ordering;
        if self.ensured.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let ddl = self.dialect.lock_create_table_ddl();
        if let Err(e) = self.executor.execute(&BoundSql::no_params(ddl)) {
            self.ensured.store(false, Ordering::SeqCst);
            return Err(e);
        }
        Ok(())
    }

    fn insert_lock(&self, lock_name: &str) -> JdbcResult<usize> {
        // Mirrors the Java table lock: the lock row carries only the lock name.
        let bound = BoundSql::new(
            format!(
                "INSERT INTO {} (lock_name) VALUES (?)",
                self.dialect.resolved_lock_table_name()
            ),
            vec![SqlParam::Text(lock_name.to_string())],
        );
        self.executor.execute(&bound)
    }

    fn delete_lock(&self, lock_name: &str) -> JdbcResult<()> {
        let bound = BoundSql::new(
            format!(
                "DELETE FROM {} WHERE lock_name = ?",
                self.dialect.resolved_lock_table_name()
            ),
            vec![SqlParam::Text(lock_name.to_string())],
        );
        self.executor.execute(&bound).map(|_| ())
    }
}

impl SandboxLockStrategy for TableLockStrategy {
    fn release(&self, lock_name: &str) {
        let _ = self.delete_lock(lock_name);
    }

    fn is_held(&self, lock_name: &str) -> bool {
        let bound = BoundSql::new(
            format!(
                "SELECT 1 FROM {} WHERE lock_name = ? LIMIT 1",
                self.dialect.resolved_lock_table_name()
            ),
            vec![SqlParam::Text(lock_name.to_string())],
        );
        self.executor
            .query(&bound)
            .map(|rows| !rows.is_empty())
            .unwrap_or(false)
    }

    fn try_enter(&self, lock_name: &str, timeout_seconds: u64) -> Result<LockLease, LockOutcome> {
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        loop {
            match self.insert_lock(lock_name) {
                Ok(1) => {
                    let name = lock_name.to_string();
                    // Clone the executor+dialect handles into the release closure so the
                    // lease releases the lock when dropped.
                    let executor = self.executor.clone();
                    let dialect = self.dialect.clone();
                    return Ok(LockLease::new(name.clone(), move || {
                        let bound = BoundSql::new(
                            format!(
                                "DELETE FROM {} WHERE lock_name = ?",
                                dialect.resolved_lock_table_name()
                            ),
                            vec![SqlParam::Text(name.clone())],
                        );
                        let _ = executor.execute(&bound);
                    }));
                }
                Ok(_) => {
                    // Row already present (unique violation rendered as 0 affected).
                }
                Err(e) if crate::backend::is_unique_violation(&e) => {
                    // Lock already held — fall through to polling.
                }
                Err(_e) => {
                    return Err(LockOutcome::TimedOut);
                }
            }
            if Instant::now() >= deadline {
                return Err(LockOutcome::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// JDBC-backed execution guard delegating to a [`SandboxLockStrategy`].
///
/// Mirrors the Java `JdbcSandboxExecutionGuard`. In Rust the harness
/// [`SandboxExecutionGuard`] is a concrete struct (not a trait), so this type is a
/// standalone JDBC lock guard that composes the same isolation-key naming scheme and
/// honours the [`SandboxLockStrategy`] lock semantics. Reuse it via
/// `Arc<JdbcSandboxExecutionGuard>` where shared ownership across tasks is needed.
#[derive(Clone)]
pub struct JdbcSandboxExecutionGuard {
    strategy: Arc<dyn SandboxLockStrategy>,
    key_prefix: String,
    lock_timeout_seconds: u64,
}

impl std::fmt::Debug for JdbcSandboxExecutionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JdbcSandboxExecutionGuard")
            .field("key_prefix", &self.key_prefix)
            .field("lock_timeout_seconds", &self.lock_timeout_seconds)
            .finish_non_exhaustive()
    }
}

impl JdbcSandboxExecutionGuard {
    /// Creates a guard from a lock strategy with default prefix and timeout.
    pub fn new(strategy: Arc<dyn SandboxLockStrategy>) -> Self {
        Self {
            strategy,
            key_prefix: DEFAULT_KEY_PREFIX.to_string(),
            lock_timeout_seconds: DEFAULT_LOCK_TIMEOUT_SECONDS,
        }
    }

    /// Creates a guard backed by the portable table-lock strategy.
    pub fn table_lock(executor: SharedExecutor, dialect: Arc<dyn AbstractJdbcDialect>) -> JdbcResult<Self> {
        let strategy = TableLockStrategy::new(executor, dialect)?;
        Ok(Self::new(Arc::new(strategy)))
    }

    /// Overrides the lock-name prefix (a trailing `:` is appended if absent).
    pub fn with_key_prefix(mut self, prefix: &str) -> Result<Self, String> {
        if prefix.trim().is_empty() {
            return Err("keyPrefix must not be blank".to_string());
        }
        self.key_prefix = if prefix.ends_with(':') {
            prefix.to_string()
        } else {
            format!("{prefix}:")
        };
        Ok(self)
    }

    /// Overrides the lock acquisition timeout in seconds.
    pub fn with_timeout_seconds(mut self, seconds: u64) -> Self {
        self.lock_timeout_seconds = seconds;
        self
    }

    /// Composes the lock name from the isolation key, mirroring Java.
    pub fn compose_lock_name(&self, key: &SandboxIsolationKey) -> String {
        format!(
            "{}{}:{}",
            self.key_prefix,
            key.scope.to_ascii_lowercase(),
            key.value
        )
    }
}

impl JdbcSandboxExecutionGuard {
    /// Tries to acquire the sandbox lock for the given isolation key.
    ///
    /// Returns a live [`JdbcSandboxLease`] on success — the lease owns the underlying
    /// [`LockLease`] and therefore keeps the lock held until it is dropped (or explicitly
    /// released). When the lock cannot be acquired before the timeout, a no-op lease is
    /// returned (mirroring the Java fallback).
    pub fn try_enter(&self, key: SandboxIsolationKey) -> JdbcSandboxLease {
        let lock_name = self.compose_lock_name(&key);
        match self.strategy.try_enter(&lock_name, self.lock_timeout_seconds) {
            Ok(lease) => JdbcSandboxLease::acquired(key, lease),
            Err(LockOutcome::TimedOut) | Err(LockOutcome::Acquired) => {
                tracing::warn!("[jdbc-guard] lock not acquired: {lock_name}");
                JdbcSandboxLease::noop()
            }
        }
    }

    /// Releases the lock previously acquired for the given isolation key (if held).
    pub fn release(&self, key: &SandboxIsolationKey) {
        let lock_name = self.compose_lock_name(key);
        self.strategy.release(&lock_name);
    }

    /// Reports whether the key's lock is currently held.
    pub fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        let lock_name = self.compose_lock_name(key);
        self.strategy.is_held(&lock_name)
    }
}

/// A live JDBC sandbox lease.
///
/// The harness [`SandboxLease`] has no release callback (its `Drop` is a documented
/// no-op), so this crate-level lease owns the dialect [`LockLease`] to keep the lock held
/// for the lifetime of the lease — dropping it releases the lock.
#[derive(Debug)]
pub struct JdbcSandboxLease {
    inner: SandboxLease,
    lock: Option<LockLease>,
}

impl JdbcSandboxLease {
    /// Wraps an acquired dialect lock. Public for adapter layers that drive a custom
    /// [`SandboxLockStrategy`].
    pub fn acquired(key: SandboxIsolationKey, lock: LockLease) -> Self {
        Self {
            inner: SandboxLease::new(key),
            lock: Some(lock),
        }
    }

    /// A lease that holds no lock (acquisition failed or no guard configured).
    pub fn noop() -> Self {
        Self {
            inner: SandboxLease::noop(),
            lock: None,
        }
    }

    /// Whether the lock was actually acquired.
    pub fn is_acquired(&self) -> bool {
        self.lock.is_some()
    }

    /// The isolation key this lease was requested for.
    pub fn key(&self) -> &SandboxIsolationKey {
        self.inner.key()
    }

    /// The underlying harness lease (key-only view; it carries no release hook).
    pub fn sandbox_lease(&self) -> &SandboxLease {
        &self.inner
    }

    /// Explicitly releases the lock. Idempotent.
    pub fn release(&mut self) {
        if let Some(mut lock) = self.lock.take() {
            lock.release();
        }
    }
}

impl Drop for JdbcSandboxLease {
    fn drop(&mut self) {
        // `LockLease::drop` releases the lock; taking here makes the intent explicit.
        if let Some(mut lock) = self.lock.take() {
            lock.release();
        }
    }
}
