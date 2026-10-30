//! Provider-backed native-advisory-lock guard.
//!
//! [`JdbcSandboxExecutionGuard`](super::JdbcSandboxExecutionGuard) takes a *ready-made*
//! [`SandboxLockStrategy`]. That works for the portable table lock, but a native strategy
//! needs a session that does not exist yet: `GET_LOCK` / `pg_try_advisory_lock` are
//! session-scoped and the Java implementation opens **one connection per lease**. This module
//! closes that gap:
//!
//! ```text
//! NativeLockGuard ──(per try_enter)──> NativeLockSessionProvider::open()
//!        │                                        │
//!        │                              Box<dyn NativeLockSession>
//!        │                                        │
//!        │                          SessionScopedLockSession (exclusive view)
//!        │                                        │
//!        └──> MysqlNativeLockStrategy / PostgresNativeLockStrategy ──> LockLease
//! ```
//!
//! The returned [`NativeLockGuardLease`] owns both the [`LockLease`] and the strategy, hence
//! the exclusive session: dropping (or explicitly releasing) it releases the server-side
//! lock and closes the connection, exactly like the Java `lease.close()`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_harness::agent::sandbox::SandboxIsolationKey;

use super::jdbc_guard::{
    compose_lock_name, normalize_key_prefix, JdbcSandboxExecutionGuard, JdbcSandboxLease,
    DEFAULT_KEY_PREFIX, DEFAULT_LOCK_TIMEOUT_SECONDS,
};
use crate::backend::SharedExecutor;
use crate::dialect::{
    ExecutorNativeLockProvider, LockLease, LockOutcome, MysqlNativeLockStrategy, NativeLockError,
    NativeLockSession, NativeLockSessionProvider, PostgresNativeLockStrategy, SandboxLockStrategy,
    SessionScopedLockSession, DEFAULT_LOCK_POLL_INTERVAL,
};

/// Which native lock identity [`NativeLockGuard`] uses (Java `MysqlDialect` vs
/// `PostgresDialect` `tryEnter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeLockFlavor {
    /// MySQL `GET_LOCK` / `RELEASE_LOCK`; the lock key is the normalized lock name.
    Mysql,
    /// PostgreSQL `pg_try_advisory_lock` / `pg_advisory_unlock`; the lock key is the
    /// decimal 64-bit advisory key.
    Postgres,
}

impl NativeLockFlavor {
    /// Human-readable name used in logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            NativeLockFlavor::Mysql => "mysql",
            NativeLockFlavor::Postgres => "postgres",
        }
    }

    /// Builds the matching native strategy over an already-opened exclusive session.
    fn strategy(&self, session: Arc<dyn NativeLockSession>) -> Arc<dyn SandboxLockStrategy> {
        match self {
            NativeLockFlavor::Mysql => Arc::new(MysqlNativeLockStrategy::new(session)),
            NativeLockFlavor::Postgres => Arc::new(PostgresNativeLockStrategy::new(session)),
        }
    }
}

/// A [`SandboxExecutionGuard`](agentscope_harness::agent::sandbox::SandboxExecutionGuard)
/// -shaped guard that opens a fresh exclusive session per acquisition through a
/// [`NativeLockSessionProvider`].
///
/// Naming, prefix handling and timeouts mirror [`super::JdbcSandboxExecutionGuard`]; the only
/// difference is that the lock strategy is built **per acquisition** over a provider-supplied
/// session instead of being injected once.
///
/// ```no_run
/// use std::sync::Arc;
/// use agentscope_extensions_jdbc::sandbox::{NativeLockFlavor, NativeLockGuard};
/// // A driver crate supplies the executor that can open exclusive connections.
/// # fn build(executor: agentscope_extensions_jdbc::SharedExecutor) {
/// let guard = NativeLockGuard::from_executor(executor, NativeLockFlavor::Postgres)
///     .with_timeout_seconds(30);
/// let _guard = guard;
/// # }
/// ```
pub struct NativeLockGuard<P> {
    provider: P,
    flavor: NativeLockFlavor,
    key_prefix: String,
    lock_timeout_seconds: u64,
}

/// Alias kept for callers that prefer the descriptive name.
pub type ProviderBackedGuard<P> = NativeLockGuard<P>;

impl<P> std::fmt::Debug for NativeLockGuard<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLockGuard")
            .field("flavor", &self.flavor)
            .field("key_prefix", &self.key_prefix)
            .field("lock_timeout_seconds", &self.lock_timeout_seconds)
            .finish_non_exhaustive()
    }
}

impl<P: NativeLockSessionProvider> NativeLockGuard<P> {
    /// Creates a guard over `provider` with the default prefix and 30-minute timeout.
    pub fn new(provider: P, flavor: NativeLockFlavor) -> Self {
        Self {
            provider,
            flavor,
            key_prefix: DEFAULT_KEY_PREFIX.to_string(),
            lock_timeout_seconds: DEFAULT_LOCK_TIMEOUT_SECONDS,
        }
    }

    /// Creates a MySQL (`GET_LOCK`) guard.
    pub fn mysql(provider: P) -> Self {
        Self::new(provider, NativeLockFlavor::Mysql)
    }

    /// Creates a PostgreSQL (`pg_try_advisory_lock`) guard.
    pub fn postgres(provider: P) -> Self {
        Self::new(provider, NativeLockFlavor::Postgres)
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

    /// Composes the lock name from the isolation key, mirroring
    /// [`super::JdbcSandboxExecutionGuard::compose_lock_name`].
    pub fn compose_lock_name(&self, key: &SandboxIsolationKey) -> String {
        compose_lock_name(&self.key_prefix, key)
    }

    /// The native flavour this guard acquires.
    pub fn flavor(&self) -> NativeLockFlavor {
        self.flavor
    }

    /// The configured acquisition timeout in seconds.
    pub fn lock_timeout_seconds(&self) -> u64 {
        self.lock_timeout_seconds
    }

    /// The provider this guard opens sessions from.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Tries to acquire the sandbox lock on a **fresh exclusive session**.
    ///
    /// The acquisition polls until [`Self::lock_timeout_seconds`] expires; the retry loop
    /// sleeps on the task (`tokio::time::sleep` inside
    /// [`SandboxLockStrategy::try_enter_async`]), so it never blocks a runtime worker.
    ///
    /// On timeout a no-op lease is returned (the caller then runs unprotected rather than
    /// deadlocking). A **backend failure** — the session cannot be opened, or the acquire
    /// probe errors — degrades to a no-op lease here for compatibility with the historical
    /// signature; use [`Self::try_enter_checked`] to receive the error instead.
    pub async fn try_enter(&self, key: SandboxIsolationKey) -> NativeLockGuardLease {
        match self.try_enter_checked(key).await {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(
                    "[jdbc-native-lock] {} lock unavailable: {error}",
                    self.flavor.as_str()
                );
                NativeLockGuardLease::noop()
            }
        }
    }

    /// Tries to acquire the sandbox lock, reporting backend failures as errors.
    ///
    /// * `Ok(lease)` — acquired, **or** a no-op lease when the deadline expired (the lock is
    ///   simply busy; the caller proceeds unprotected, mirroring the Java fallback).
    /// * `Err(NativeLockError::Failed(_))` — the session could not be opened or the acquire
    ///   probe failed. A failing lock backend is **never** downgraded to a no-op lease: that
    ///   would let several processes run the same isolation key unprotected.
    pub async fn try_enter_checked(
        &self,
        key: SandboxIsolationKey,
    ) -> Result<NativeLockGuardLease, NativeLockError> {
        let lock_name = self.compose_lock_name(&key);
        let session = self.provider.open().await?;
        // The exclusive view releases anything it still holds when it is dropped, which also
        // happens when the acquisition below times out.
        let scoped: Arc<dyn NativeLockSession> = Arc::from(Box::new(SessionScopedLockSession::new(
            session,
        ))
            as Box<dyn NativeLockSession>);
        let strategy = self.flavor.strategy(scoped);
        match strategy
            .try_enter_async(&lock_name, self.lock_timeout_seconds)
            .await
        {
            Ok(lease) => Ok(NativeLockGuardLease::acquired(key, lease, strategy)),
            Err(LockOutcome::TimedOut) | Err(LockOutcome::Acquired) => {
                tracing::warn!(
                    "[jdbc-native-lock] {} lock not acquired before the timeout: {lock_name}",
                    self.flavor.as_str()
                );
                Ok(NativeLockGuardLease::noop())
            }
            Err(LockOutcome::Failed(message)) => {
                Err(NativeLockError::Failed(format!("{lock_name}: {message}")))
            }
        }
    }
}

impl NativeLockGuard<ExecutorNativeLockProvider> {
    /// Creates a guard whose sessions come from a [`crate::backend::JdbcExecutor`]'s
    /// [`crate::backend::JdbcExecutor::open_native_lock_session`].
    pub fn from_executor(executor: SharedExecutor, flavor: NativeLockFlavor) -> Self {
        Self::new(ExecutorNativeLockProvider::new(executor), flavor)
    }
}

/// A live native-lock lease.
///
/// Owns the dialect [`LockLease`] (which releases the server-side lock when dropped) **and**
/// the strategy holding the exclusive session, so both live exactly as long as the lease.
/// Release order on drop: lock first, then the session (which closes the exclusive
/// connection).
pub struct NativeLockGuardLease {
    inner: JdbcSandboxLease,
    strategy: Option<Arc<dyn SandboxLockStrategy>>,
}

impl std::fmt::Debug for NativeLockGuardLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLockGuardLease")
            .field("key", &self.inner.key())
            .field("acquired", &self.is_acquired())
            .finish()
    }
}

impl NativeLockGuardLease {
    fn acquired(
        key: SandboxIsolationKey,
        lock: LockLease,
        strategy: Arc<dyn SandboxLockStrategy>,
    ) -> Self {
        Self {
            inner: JdbcSandboxLease::acquired(key, lock),
            strategy: Some(strategy),
        }
    }

    /// A lease that holds no lock (acquisition failed or the session could not be opened).
    pub fn noop() -> Self {
        Self {
            inner: JdbcSandboxLease::noop(),
            strategy: None,
        }
    }

    /// Whether the native lock was actually acquired.
    pub fn is_acquired(&self) -> bool {
        self.inner.is_acquired()
    }

    /// The isolation key this lease was requested for.
    pub fn key(&self) -> &SandboxIsolationKey {
        self.inner.key()
    }

    /// Explicitly releases the native lock. Idempotent; the exclusive session is closed when
    /// the lease itself is dropped.
    pub fn release(&mut self) {
        self.inner.release();
    }

    /// The underlying harness-shaped lease (key-only view; it carries no release hook).
    pub fn sandbox_lease(&self) -> &agentscope_harness::agent::sandbox::SandboxLease {
        self.inner.sandbox_lease()
    }
}

impl Drop for NativeLockGuardLease {
    fn drop(&mut self) {
        // Release the server-side lock first, then close the exclusive session (whose
        // `SessionScopedLockSession::drop` is the safety net for anything still held).
        self.inner.release();
        let _ = self.strategy.take();
    }
}

// ---------------------------------------------------------------------------
//  Host assembly: one guard, a fresh exclusive session per acquisition
// ---------------------------------------------------------------------------

/// [`SandboxLockStrategy`] that opens a **fresh** exclusive session per acquisition.
///
/// [`JdbcSandboxExecutionGuard`] owns a single `Arc<dyn SandboxLockStrategy>` for its whole
/// lifetime, while a native advisory lock needs a *new* session per lease. This adapter is the
/// bridge: it keeps the provider and opens the session lazily inside
/// [`SandboxLockStrategy::try_enter_async`], so the guard — and therefore the
/// `sandbox_execution_guard` slot produced by
/// [`crate::JdbcDistributedStore::to_harness_store`] — can be assembled **before** any
/// connection exists.
///
/// This is the assembly point the earlier revisions were missing: without it,
/// `ExecutorNativeLockProvider` had no production caller at all.
pub(crate) struct NativeProviderStrategy {
    provider: Arc<dyn NativeLockSessionProvider>,
    flavor: NativeLockFlavor,
}

impl std::fmt::Debug for NativeProviderStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeProviderStrategy")
            .field("flavor", &self.flavor)
            .finish_non_exhaustive()
    }
}

impl NativeProviderStrategy {
    pub(crate) fn new(
        provider: Arc<dyn NativeLockSessionProvider>,
        flavor: NativeLockFlavor,
    ) -> Self {
        Self { provider, flavor }
    }
}

#[async_trait::async_trait]
impl SandboxLockStrategy for NativeProviderStrategy {
    /// Single-attempt acquisition is impossible here: the session has to be opened first, and
    /// opening one is asynchronous. Callers use [`Self::try_enter_async`].
    fn try_enter_once(
        &self,
        lock_name: &str,
        attempt_timeout: Duration,
    ) -> Result<LockLease, LockOutcome> {
        let _ = (lock_name, attempt_timeout);
        Err(LockOutcome::Failed(
            "native advisory locks must be acquired through try_enter_async: opening a \
             dedicated session is asynchronous"
                .to_string(),
        ))
    }

    async fn try_enter_async(
        &self,
        lock_name: &str,
        timeout_seconds: u64,
    ) -> Result<LockLease, LockOutcome> {
        let session = self
            .provider
            .open()
            .await
            .map_err(|error| LockOutcome::Failed(error.to_string()))?;
        let scoped: Arc<dyn NativeLockSession> = Arc::from(Box::new(SessionScopedLockSession::new(
            session,
        ))
            as Box<dyn NativeLockSession>);
        let strategy = self.flavor.strategy(scoped.clone());
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match strategy.try_enter_once(lock_name, remaining) {
                Ok(mut lease) => {
                    // The lease must own both halves: the dialect lease (releases the
                    // server-side lock) and the session view (closing it releases anything
                    // still held, mirroring the Java `lease.close()`).
                    let view = scoped.clone();
                    // `release` is an `FnMut`, so the session view is taken out of an
                    // `Option` on the first call instead of being moved by the closure.
                    let mut view = Some(view);
                    return Ok(LockLease::new(lock_name.to_string(), move || {
                        lease.release();
                        view.take();
                    }));
                }
                Err(LockOutcome::Acquired) | Err(LockOutcome::TimedOut) => {}
                Err(LockOutcome::Failed(message)) => return Err(LockOutcome::Failed(message)),
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Dropping `scoped` closes the session and releases whatever it holds.
                return Err(LockOutcome::TimedOut);
            }
            tokio::time::sleep(DEFAULT_LOCK_POLL_INTERVAL.min(remaining)).await;
        }
    }
}

impl JdbcSandboxExecutionGuard {
    /// Creates a guard that acquires **native** advisory locks (`GET_LOCK` /
    /// `pg_try_advisory_lock`) through `executor`'s
    /// [`crate::backend::JdbcExecutor::open_native_lock_session`].
    ///
    /// This is the minimal host assembly point for the native-lock path: the resulting guard is
    /// an ordinary [`JdbcSandboxExecutionGuard`], so it drops straight into
    /// [`crate::JdbcDistributedStore::with_native_lock_guard`], `Arc<dyn SandboxGuard>` and the
    /// harness `sandbox_execution_guard` slot. A backend without advisory locks (SQLite)
    /// reports [`crate::backend::JdbcError::Unsupported`] per acquisition; the guard then
    /// degrades to a no-op lease and logs it, exactly like a contended lock.
    pub fn with_native_lock(executor: SharedExecutor, flavor: NativeLockFlavor) -> Self {
        let provider: Arc<dyn NativeLockSessionProvider> =
            Arc::new(ExecutorNativeLockProvider::new(executor));
        Self::new(Arc::new(NativeProviderStrategy::new(provider, flavor)))
    }
}
