//! JDBC-backed [`SandboxExecutionGuard`], mirroring `JdbcSandboxExecutionGuard`.
//!
//! Delegates all lock semantics to a [`SandboxLockStrategy`] provided by the dialect. The
//! component contains zero database-specific logic — the dialect handles whether to use
//! MySQL `GET_LOCK`, PostgreSQL `pg_try_advisory_lock`, or the portable table-based lock.
//!
//! Each lock is identified by a string key derived from the [`SandboxIsolationKey`]'s
//! scope and value.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey, SandboxLease};

use crate::backend::{JdbcResult, SharedExecutor};
use crate::dialect::{
    next_lock_poll_interval, AbstractJdbcDialect, BoundSql, LockLease, LockOutcome,
    SandboxLockStrategy, SqlParam, DEFAULT_LOCK_POLL_INTERVAL,
};

/// Default prefix applied to composed lock names.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:sandbox:lock:";
/// Default lock timeout, mirroring the Java 30-minute default.
pub const DEFAULT_LOCK_TIMEOUT_SECONDS: u64 = 1800;

/// Normalizes a lock-name prefix, appending a trailing `:` when absent.
///
/// Shared by [`JdbcSandboxExecutionGuard`] and [`super::NativeLockGuard`] so both guard
/// flavours compose identical lock names (a native lock must not be "the same lock" under a
/// different name than the table lock it replaces).
pub fn normalize_key_prefix(prefix: &str) -> Result<String, String> {
    if prefix.trim().is_empty() {
        return Err("keyPrefix must not be blank".to_string());
    }
    Ok(if prefix.ends_with(':') {
        prefix.to_string()
    } else {
        format!("{prefix}:")
    })
}

/// Composes `{prefix}{scope}:{value}` for an isolation key (Java `composeLockName`).
pub fn compose_lock_name(prefix: &str, key: &SandboxIsolationKey) -> String {
    format!("{}{}:{}", prefix, key.scope.to_ascii_lowercase(), key.value)
}

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
    pub fn new(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
    ) -> JdbcResult<Self> {
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
        // Same serialized bootstrap as the state/store/snapshot/skill tables: two processes
        // creating `agentscope_distributed_locks` at the same time hit PostgreSQL's
        // `pg_type_typname_nsp_index` race just like `CREATE TABLE ... sessions` did.
        if let Err(e) = crate::schema_bootstrap::run_ddl(
            &*self.executor,
            self.dialect.as_ref(),
            std::slice::from_ref(&ddl),
        ) {
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
        let released = self.executor.execute(&bound);
        tracing::debug!(
            dialect = ?self.dialect.kind(),
            table = %self.dialect.resolved_lock_table_name(),
            lock = %lock_name,
            released = released.is_ok(),
            "[jdbc-lock] table lock released"
        );
        released.map(|_| ())
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

    fn try_enter_once(
        &self,
        lock_name: &str,
        _attempt_timeout: Duration,
    ) -> Result<LockLease, LockOutcome> {
        match self.insert_lock(lock_name) {
            Ok(1) => {
                tracing::debug!(
                    dialect = ?self.dialect.kind(),
                    table = %self.dialect.resolved_lock_table_name(),
                    lock = %lock_name,
                    "[jdbc-lock] table lock acquired"
                );
                let name = lock_name.to_string();
                // Clone the executor+dialect handles into the release closure so the
                // lease releases the lock when dropped.
                let executor = self.executor.clone();
                let dialect = self.dialect.clone();
                Ok(LockLease::new(name.clone(), move || {
                    let bound = BoundSql::new(
                        format!(
                            "DELETE FROM {} WHERE lock_name = ?",
                            dialect.resolved_lock_table_name()
                        ),
                        vec![SqlParam::Text(name.clone())],
                    );
                    let released = executor.execute(&bound);
                    tracing::debug!(
                        dialect = ?dialect.kind(),
                        lock = %name,
                        released = released.is_ok(),
                        "[jdbc-lock] table lock lease released"
                    );
                }))
            }
            // Row already present (unique violation rendered as 0 affected rows).
            Ok(_) => Err(LockOutcome::TimedOut),
            Err(e) if crate::backend::is_unique_violation(&e) => Err(LockOutcome::TimedOut),
            // A broken executor / missing lock table is a backend failure, not a contended
            // lock: never fold it into `TimedOut`.
            Err(e) => Err(LockOutcome::Failed(e.to_string())),
        }
    }

    /// Blocking flavour for synchronous callers (the async path uses the trait's
    /// [`SandboxLockStrategy::try_enter_async`], which sleeps on the task instead).
    fn try_enter(&self, lock_name: &str, timeout_seconds: u64) -> Result<LockLease, LockOutcome> {
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        let mut interval = DEFAULT_LOCK_POLL_INTERVAL;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.try_enter_once(lock_name, remaining) {
                Ok(lease) => return Ok(lease),
                Err(LockOutcome::Acquired) | Err(LockOutcome::TimedOut) => {}
                Err(LockOutcome::Failed(message)) => {
                    return Err(LockOutcome::Failed(message));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LockOutcome::TimedOut);
            }
            // Java: `INITIAL_BACKOFF_MS` doubling up to `MAX_BACKOFF_MS`.
            std::thread::sleep(interval.min(remaining));
            interval = next_lock_poll_interval(interval);
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
    /// Leases acquired through the harness [`SandboxGuard`] trait, keyed by isolation key.
    ///
    /// The harness [`SandboxLease`] carries no release hook, so the concrete
    /// [`JdbcSandboxLease`] (which owns the dialect lock) must be retained here until
    /// [`SandboxGuard::release`] is called — otherwise dropping it would release the
    /// distributed lock immediately.
    trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<JdbcSandboxLease>>>>,
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
            trait_leases: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Creates a guard backed by the portable table-lock strategy.
    pub fn table_lock(
        executor: SharedExecutor,
        dialect: Arc<dyn AbstractJdbcDialect>,
    ) -> JdbcResult<Self> {
        let strategy = TableLockStrategy::new(executor, dialect)?;
        Ok(Self::new(Arc::new(strategy)))
    }

    /// Overrides the lock-name prefix (a trailing `:` is appended if absent).
    pub fn with_key_prefix(mut self, prefix: &str) -> Result<Self, String> {
        self.key_prefix = normalize_key_prefix(prefix)?;
        Ok(self)
    }

    /// Overrides the lock acquisition timeout in seconds.
    pub fn with_timeout_seconds(mut self, seconds: u64) -> Self {
        self.lock_timeout_seconds = seconds;
        self
    }

    /// Composes the lock name from the isolation key, mirroring Java.
    pub fn compose_lock_name(&self, key: &SandboxIsolationKey) -> String {
        compose_lock_name(&self.key_prefix, key)
    }
}

impl JdbcSandboxExecutionGuard {
    /// Tries to acquire the sandbox lock for the given isolation key.
    ///
    /// Returns a live [`JdbcSandboxLease`] on success — the lease owns the underlying
    /// [`LockLease`] and therefore keeps the lock held until it is dropped (or explicitly
    /// released). When the lock cannot be acquired before the timeout, a no-op lease is
    /// returned (mirroring the Java fallback).
    ///
    /// This is the **synchronous single-attempt** entry point; it cannot report an error.
    /// Use [`Self::try_enter_async`] on the async path: it polls without blocking and
    /// returns backend failures instead of degrading them.
    pub fn try_enter(&self, key: SandboxIsolationKey) -> JdbcSandboxLease {
        let lock_name = self.compose_lock_name(&key);
        match self
            .strategy
            .try_enter(&lock_name, self.lock_timeout_seconds)
        {
            Ok(lease) => JdbcSandboxLease::acquired(key, lease),
            Err(LockOutcome::TimedOut) | Err(LockOutcome::Acquired) => {
                tracing::warn!("[jdbc-guard] lock not acquired: {lock_name}");
                JdbcSandboxLease::noop()
            }
            Err(LockOutcome::Failed(message)) => {
                // No error channel on this signature: log loudly and degrade. Async callers
                // get the error from `try_enter_async`.
                tracing::error!("[jdbc-guard] lock backend failed for {lock_name}: {message}");
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

    /// Asynchronously acquires the sandbox lock for the given isolation key.
    ///
    /// The retry loop of the injected [`SandboxLockStrategy`] sleeps with
    /// `tokio::time::sleep`, so the acquisition never blocks a runtime worker.
    ///
    /// * `Ok(lease)` — acquired, **or** a no-op lease when the deadline expired (the Java
    ///   fallback: the call proceeds unprotected rather than deadlocking).
    /// * `Err(NativeLockError::Failed(_))` — the lock backend failed. Backend failures are
    ///   **never** downgraded to a no-op lease: silently running unprotected is exactly the
    ///   bug this guards against.
    pub async fn try_enter_async(
        &self,
        key: SandboxIsolationKey,
    ) -> Result<JdbcSandboxLease, crate::dialect::NativeLockError> {
        let lock_name = self.compose_lock_name(&key);
        match self
            .strategy
            .try_enter_async(&lock_name, self.lock_timeout_seconds)
            .await
        {
            Ok(lease) => Ok(JdbcSandboxLease::acquired(key, lease)),
            Err(LockOutcome::TimedOut) | Err(LockOutcome::Acquired) => {
                tracing::warn!("[jdbc-guard] lock not acquired before the timeout: {lock_name}");
                Ok(JdbcSandboxLease::noop())
            }
            Err(LockOutcome::Failed(message)) => Err(crate::dialect::NativeLockError::Failed(
                format!("{lock_name}: {message}"),
            )),
        }
    }
}

#[async_trait::async_trait]
impl SandboxGuard for JdbcSandboxExecutionGuard {
    /// Acquires the distributed lock asynchronously and retains the concrete
    /// [`JdbcSandboxLease`] so the lock stays held until [`SandboxGuard::release`] runs.
    ///
    /// **Fail-closed**: both a backend failure and a missed acquisition deadline return
    /// `Err`, so the caller aborts. They used to be downgraded to a no-op lease on this
    /// path, which silently ran the sandbox without the distributed lock. The inherent
    /// [`JdbcSandboxExecutionGuard::try_enter_async`] keeps its documented `Ok(noop)`
    /// fallback for callers that explicitly accept it.
    async fn try_enter(
        &self,
        key: SandboxIsolationKey,
    ) -> agentscope_core::error::Result<SandboxLease> {
        match JdbcSandboxExecutionGuard::try_enter_async(self, key.clone()).await {
            Ok(lease) if lease.is_acquired() => {
                if let Ok(mut leases) = self.trait_leases.lock() {
                    leases.entry(key.clone()).or_default().push(lease);
                }
                Ok(SandboxLease::new(key))
            }
            // `try_enter_async` reports a missed deadline as `Ok(noop)`: a no-op lease is
            // still a failed acquisition on this path.
            Ok(_) => {
                let lock_name = self.compose_lock_name(&key);
                tracing::error!(
                    lock = %lock_name,
                    "[jdbc-guard] sandbox lock not acquired before the timeout"
                );
                Err(agentscope_core::error::Error::Other(format!(
                    "JDBC sandbox guard lock not acquired before the timeout: {lock_name}"
                )))
            }
            Err(error) => {
                tracing::error!(error = %error, "[jdbc-guard] sandbox lock backend failed");
                Err(agentscope_core::error::Error::Other(format!(
                    "JDBC sandbox guard lock backend failed: {error}"
                )))
            }
        }
    }

    async fn release(&self, key: &SandboxIsolationKey) {
        let dropped =
            self.trait_leases
                .lock()
                .ok()
                .and_then(|mut leases| match leases.get_mut(key) {
                    Some(stack) => {
                        let lease = stack.pop();
                        if stack.is_empty() {
                            leases.remove(key);
                        }
                        lease
                    }
                    None => None,
                });
        match dropped {
            // Dropping the lease releases the dialect lock.
            Some(lease) => drop(lease),
            None => JdbcSandboxExecutionGuard::release(self, key),
        }
    }

    fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        JdbcSandboxExecutionGuard::is_held(self, key)
    }

    fn active_count(&self) -> usize {
        self.trait_leases
            .lock()
            .map(|leases| leases.values().map(|stack| stack.len()).sum())
            .unwrap_or(0)
    }

    /// JDBC locks (lock table or native advisory locks) serialise across processes.
    fn is_distributed(&self) -> bool {
        true
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
