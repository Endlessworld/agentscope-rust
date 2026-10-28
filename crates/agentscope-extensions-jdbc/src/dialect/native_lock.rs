//! Native (server-scoped) advisory-lock strategies for MySQL and PostgreSQL,
//! mirroring the `tryEnter` overrides on `MysqlDialect` / `PostgresDialect`.
//!
//! ## Why a session seam instead of the shared executor
//!
//! Java acquires a **dedicated `Connection` per lease**: `GET_LOCK` and
//! `pg_try_advisory_lock` are *session*-scoped, so the lock is released by
//! `close()`ing that connection. Rust's [`crate::backend::JdbcExecutor`] is a shared
//! single-connection seam and cannot represent that ownership — re-acquiring on the same
//! connection would succeed (PostgreSQL advisory locks are even re-entrant) and silently
//! break mutual exclusion. The native strategies therefore take a [`NativeLockSession`]
//! seam: real adapters open a dedicated connection per acquisition, offline test adapters
//! emulate the acquire/release bookkeeping.
//!
//! [`SandboxLockStrategy`] is implemented here (not on the dialect types) because a
//! dialect carries no connection; build one of these over an already-open session and hand
//! it to [`crate::JdbcSandboxExecutionGuard::new`].
//!
//! ## Wiring a provider (production path)
//!
//! [`crate::sandbox::NativeLockGuard`] closes the loop: per acquisition it calls
//! [`NativeLockSessionProvider::open`] (driver crates implement it over their own pool), wraps
//! the result in [`SessionScopedLockSession`], builds the matching strategy and polls it via
//! [`SandboxLockStrategy::try_enter_async`]. A driver crate that can open exclusive connections only needs to override
//! [`crate::backend::JdbcExecutor::open_native_lock_session`] —
//! [`ExecutorNativeLockProvider`] then plugs that executor into the guard. Backends without
//! native locks (SQLite) answer [`crate::backend::JdbcError::Unsupported`] and callers stay on
//! the portable [`crate::sandbox::TableLockStrategy`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::lock_strategy::{LockLease, LockOutcome, SandboxLockStrategy};
use crate::backend::SharedExecutor;

/// Maximum lock-name length accepted by MySQL `GET_LOCK`; longer names are truncated and
/// suffixed with a SHA-256 digest (mirrors `MysqlDialect.MAX_LOCK_NAME_LENGTH`).
pub const MYSQL_MAX_LOCK_NAME_LENGTH: usize = 64;
/// Number of hex digest characters appended when normalizing a long MySQL lock name.
pub const MYSQL_LOCK_NAME_HASH_HEX_LENGTH: usize = 32;
/// Poll interval used while retrying a non-blocking acquire (mirrors the Java
/// PostgreSQL `TimeUnit.MILLISECONDS.sleep(100)`).
pub const NATIVE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// MySQL acquire probe. The first `?` is the lock name, the second the timeout in seconds.
pub const MYSQL_GET_LOCK_SQL: &str = "SELECT GET_LOCK(?, ?)";
/// MySQL release probe.
pub const MYSQL_RELEASE_LOCK_SQL: &str = "SELECT RELEASE_LOCK(?)";
/// PostgreSQL non-blocking acquire probe (the `?` is the 64-bit advisory-lock key).
pub const PG_TRY_ADVISORY_LOCK_SQL: &str = "SELECT pg_try_advisory_lock(?)";
/// PostgreSQL release probe.
pub const PG_ADVISORY_UNLOCK_SQL: &str = "SELECT pg_advisory_unlock(?)";

/// Hex-encodes the SHA-256 digest of `value`.
fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Normalizes a lock name to MySQL's 64-character limit.
///
/// Names within the limit are returned unchanged. Longer names are truncated and suffixed
/// with a SHA-256 hex digest so two distinct long names can never collide — a collision
/// would let one agent's `RELEASE_LOCK` release another agent's lock (mirrors
/// `MysqlDialect.normalizeLockName`, which replaces a 32-bit `String.hashCode` fold for
/// exactly that reason).
///
/// Truncation is measured in Unicode scalar values where Java measures UTF-16 code units;
/// composed lock names are ASCII in practice, so the two agree.
pub fn normalize_lock_name(lock_name: &str) -> String {
    let chars: Vec<char> = lock_name.chars().collect();
    if chars.len() <= MYSQL_MAX_LOCK_NAME_LENGTH {
        return lock_name.to_string();
    }
    let hash = sha256_hex(lock_name);
    let prefix_length = MYSQL_MAX_LOCK_NAME_LENGTH - 1 - MYSQL_LOCK_NAME_HASH_HEX_LENGTH;
    let prefix: String = chars[..prefix_length].iter().collect();
    format!("{prefix}:{}", &hash[..MYSQL_LOCK_NAME_HASH_HEX_LENGTH])
}

/// Maps a lock name to a 64-bit PostgreSQL advisory-lock key by folding the first 8 bytes
/// of its SHA-256 digest (mirrors `PostgresDialect.composeLockKey`).
///
/// Distinct names collide with probability ~2^-32, matching the legacy PostgreSQL
/// module's MurmurHash3 width.
pub fn postgres_lock_key(lock_name: &str) -> i64 {
    let digest = Sha256::digest(lock_name.as_bytes());
    let mut key: i64 = 0;
    for byte in digest.iter().take(8) {
        key = (key << 8) | (*byte as i64);
    }
    key
}

/// Failure modes of a native lock operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeLockError {
    /// The acquire probe timed out.
    TimedOut,
    /// The underlying session reported a failure.
    Failed(String),
}

impl std::fmt::Display for NativeLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeLockError::TimedOut => write!(f, "timed out waiting for native lock"),
            NativeLockError::Failed(msg) => write!(f, "native lock failed: {msg}"),
        }
    }
}

impl std::error::Error for NativeLockError {}

/// A dedicated database session capable of holding a server-scoped advisory lock.
///
/// `lock_key` is the **dialect-specific lock identity**, already mapped by the strategy:
/// the normalized lock name for MySQL, the decimal advisory-lock key for PostgreSQL.
/// Real adapters open one connection per acquisition (the lock dies with the connection);
/// test adapters emulate the bookkeeping in memory.
pub trait NativeLockSession: Send + Sync {
    /// Tries to acquire `lock_key`. `Ok(true)` when acquired, `Ok(false)` when the lock is
    /// held by someone else.
    ///
    /// `timeout` is an **upper bound for this single attempt**, not for the whole
    /// acquisition: [`SandboxLockStrategy::try_enter_async`] retries until the deadline
    /// expires. Blocking-capable sessions (MySQL `GET_LOCK(name, timeout)`) should pass it
    /// to the server; non-blocking ones (PostgreSQL `pg_try_advisory_lock`) ignore it and
    /// return immediately, which is exactly the Java behaviour of both dialects.
    fn try_acquire(&self, lock_key: &str, timeout: Duration) -> Result<bool, NativeLockError>;

    /// Releases `lock_key`.
    fn release(&self, lock_key: &str) -> Result<(), NativeLockError>;

    /// Whether `lock_key` is currently held.
    fn is_held(&self, lock_key: &str) -> Result<bool, NativeLockError>;
}

/// Boxed future returned by [`NativeLockSessionProvider::open`].
///
/// Opening a dedicated connection is asynchronous for real drivers, so the *provider* seam
/// is async. The strategy seam is async as well (see
/// [`SandboxLockStrategy::try_enter_async`]), so a session it already owns is polled without
/// blocking the runtime.
pub type NativeLockSessionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn NativeLockSession>, NativeLockError>> + Send + 'a>>;

/// Opens a fresh, **exclusive** [`NativeLockSession`] per call.
///
/// This is the injectable half of the native-lock path: a driver crate implements it over
/// its own connection pool (one pooled connection per acquisition — the exact counterpart of
/// the Java `getDataSource().getConnection()` + `close()` pair), while this crate keeps the
/// lock semantics. Implementations **must not** hand out a shared connection: `GET_LOCK` and
/// `pg_try_advisory_lock` are session-scoped (PostgreSQL is even re-entrant), so a shared
/// connection would silently let two guards hold the same lock.
pub trait NativeLockSessionProvider: Send + Sync {
    /// Opens an exclusive session, or reports why it could not be opened.
    fn open(&self) -> NativeLockSessionFuture<'_>;
}

/// Exclusive-session view handed back by a [`NativeLockSessionProvider`].
///
/// Wraps the provider-owned session, forwards the [`NativeLockSession`] operations, and
/// remembers which keys it holds so dropping the view releases them explicitly before the
/// underlying connection is dropped — mirroring the Java lease `close()` (which issues
/// `RELEASE_LOCK` / `pg_advisory_unlock` and then closes the connection as a safety net).
pub struct SessionScopedLockSession {
    inner: Box<dyn NativeLockSession>,
    held: Mutex<Vec<String>>,
}

impl std::fmt::Debug for SessionScopedLockSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionScopedLockSession")
            .field("held", &self.held_keys())
            .finish_non_exhaustive()
    }
}

impl SessionScopedLockSession {
    /// Wraps an exclusive session.
    pub fn new(inner: Box<dyn NativeLockSession>) -> Self {
        Self {
            inner,
            held: Mutex::new(Vec::new()),
        }
    }

    /// Keys currently held through this view (insertion order, deduplicated).
    pub fn held_keys(&self) -> Vec<String> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record_hold(&self, lock_key: &str) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !held.iter().any(|k| k == lock_key) {
            held.push(lock_key.to_string());
        }
    }

    fn forget_hold(&self, lock_key: &str) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        held.retain(|k| k != lock_key);
    }
}

impl NativeLockSession for SessionScopedLockSession {
    fn try_acquire(&self, lock_key: &str, timeout: Duration) -> Result<bool, NativeLockError> {
        let acquired = self.inner.try_acquire(lock_key, timeout)?;
        if acquired {
            self.record_hold(lock_key);
        }
        Ok(acquired)
    }

    fn release(&self, lock_key: &str) -> Result<(), NativeLockError> {
        self.inner.release(lock_key)?;
        self.forget_hold(lock_key);
        Ok(())
    }

    fn is_held(&self, lock_key: &str) -> Result<bool, NativeLockError> {
        self.inner.is_held(lock_key)
    }
}

impl Drop for SessionScopedLockSession {
    fn drop(&mut self) {
        // Closing the Java connection releases session-scoped locks implicitly; do it
        // explicitly so the release is deterministic (and observable) before the drop.
        let held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for lock_key in held {
            let _ = self.inner.release(&lock_key);
        }
    }
}

/// [`NativeLockSessionProvider`] over a [`crate::backend::JdbcExecutor`] that can open
/// native lock sessions ([`crate::backend::JdbcExecutor::open_native_lock_session`]).
///
/// The thin production adapter: a MySQL/PostgreSQL driver crate implements
/// `open_native_lock_session` on its executor (one dedicated connection per call) and this
/// provider plugs it into [`crate::sandbox::NativeLockGuard`].
#[derive(Debug, Clone)]
pub struct ExecutorNativeLockProvider {
    executor: SharedExecutor,
}

impl ExecutorNativeLockProvider {
    /// Creates a provider backed by `executor`.
    pub fn new(executor: SharedExecutor) -> Self {
        Self { executor }
    }
}

impl NativeLockSessionProvider for ExecutorNativeLockProvider {
    fn open(&self) -> NativeLockSessionFuture<'_> {
        let executor = self.executor.clone();
        Box::pin(async move {
            executor
                .open_native_lock_session()
                .map_err(|e| NativeLockError::Failed(e.to_string()))
        })
    }
}

/// Shared body for the two native strategies: map the composed name to the dialect's lock
/// identity, perform a **single** acquisition attempt, and wrap success in a self-releasing
/// [`LockLease`].
///
/// Nothing sleeps here. The retry loop lives one layer up in
/// [`SandboxLockStrategy::try_enter_async`] (it sleeps on the task with `tokio::time::sleep`)
/// — a synchronous poll loop had to sleep on the calling thread, which stalls a Tokio worker
/// whenever the guard is acquired from async code.
///
/// `Ok(false)` (lock held elsewhere) and [`NativeLockError::TimedOut`] map to
/// [`LockOutcome::TimedOut`] so the caller can retry; a hard session failure maps to
/// [`LockOutcome::Failed`] and must not be retried into a timeout.
fn try_enter_once(
    session: &Arc<dyn NativeLockSession>,
    lock_key: String,
    attempt_timeout: Duration,
) -> Result<LockLease, LockOutcome> {
    match session.try_acquire(&lock_key, attempt_timeout) {
        Ok(true) => {
            let release_session = session.clone();
            let lease_name = lock_key.clone();
            let release_key = lock_key;
            Ok(LockLease::new(lease_name, move || {
                let _ = release_session.release(&release_key);
            }))
        }
        Ok(false) | Err(NativeLockError::TimedOut) => Err(LockOutcome::TimedOut),
        // Backend failures are surfaced, not downgraded (Java throws here).
        Err(NativeLockError::Failed(message)) => Err(LockOutcome::Failed(message)),
    }
}

/// MySQL `GET_LOCK()`-backed [`SandboxLockStrategy`], mirroring `MysqlDialect.tryEnter`.
pub struct MysqlNativeLockStrategy {
    session: Arc<dyn NativeLockSession>,
}

impl std::fmt::Debug for MysqlNativeLockStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlNativeLockStrategy")
            .finish_non_exhaustive()
    }
}

impl MysqlNativeLockStrategy {
    /// Creates the strategy over a dedicated-session seam.
    pub fn new(session: Arc<dyn NativeLockSession>) -> Self {
        Self { session }
    }
}

impl SandboxLockStrategy for MysqlNativeLockStrategy {
    fn try_enter_once(
        &self,
        lock_name: &str,
        attempt_timeout: Duration,
    ) -> Result<LockLease, LockOutcome> {
        try_enter_once(
            &self.session,
            normalize_lock_name(lock_name),
            attempt_timeout,
        )
    }

    fn release(&self, lock_name: &str) {
        let _ = self.session.release(&normalize_lock_name(lock_name));
    }

    fn is_held(&self, lock_name: &str) -> bool {
        self.session
            .is_held(&normalize_lock_name(lock_name))
            .unwrap_or(false)
    }
}

/// PostgreSQL `pg_try_advisory_lock()`-backed [`SandboxLockStrategy`], mirroring
/// `PostgresDialect.tryEnter` (poll until the deadline, then time out).
pub struct PostgresNativeLockStrategy {
    session: Arc<dyn NativeLockSession>,
}

impl std::fmt::Debug for PostgresNativeLockStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresNativeLockStrategy")
            .finish_non_exhaustive()
    }
}

impl PostgresNativeLockStrategy {
    /// Creates the strategy over a dedicated-session seam.
    pub fn new(session: Arc<dyn NativeLockSession>) -> Self {
        Self { session }
    }
}

impl SandboxLockStrategy for PostgresNativeLockStrategy {
    fn try_enter_once(
        &self,
        lock_name: &str,
        attempt_timeout: Duration,
    ) -> Result<LockLease, LockOutcome> {
        try_enter_once(
            &self.session,
            postgres_lock_key(lock_name).to_string(),
            attempt_timeout,
        )
    }

    fn release(&self, lock_name: &str) {
        let _ = self
            .session
            .release(&postgres_lock_key(lock_name).to_string());
    }

    fn is_held(&self, lock_name: &str) -> bool {
        self.session
            .is_held(&postgres_lock_key(lock_name).to_string())
            .unwrap_or(false)
    }
}
