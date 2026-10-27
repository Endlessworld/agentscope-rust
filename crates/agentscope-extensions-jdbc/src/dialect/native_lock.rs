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
//! dialect carries no connection; build one of these and hand it to
//! [`crate::JdbcSandboxExecutionGuard::new`].

use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::lock_strategy::{LockLease, LockOutcome, SandboxLockStrategy};

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
    /// Tries to acquire `lock_key`, waiting up to `timeout`. `Ok(true)` when acquired.
    fn try_acquire(&self, lock_key: &str, timeout: Duration) -> Result<bool, NativeLockError>;

    /// Releases `lock_key`.
    fn release(&self, lock_key: &str) -> Result<(), NativeLockError>;

    /// Whether `lock_key` is currently held.
    fn is_held(&self, lock_key: &str) -> Result<bool, NativeLockError>;
}

/// Shared body for the two native strategies: map the composed name to the dialect's
/// lock identity, acquire, and wrap the result in a self-releasing [`LockLease`].
fn enter(
    session: &Arc<dyn NativeLockSession>,
    lock_key: String,
    timeout_seconds: u64,
) -> Result<LockLease, LockOutcome> {
    match session.try_acquire(&lock_key, Duration::from_secs(timeout_seconds)) {
        Ok(true) => {
            let release_session = session.clone();
            let release_key = lock_key.clone();
            Ok(LockLease::new(lock_key, move || {
                let _ = release_session.release(&release_key);
            }))
        }
        Ok(false) | Err(NativeLockError::TimedOut) | Err(NativeLockError::Failed(_)) => {
            Err(LockOutcome::TimedOut)
        }
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
    fn try_enter(&self, lock_name: &str, timeout_seconds: u64) -> Result<LockLease, LockOutcome> {
        enter(
            &self.session,
            normalize_lock_name(lock_name),
            timeout_seconds,
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
    fn try_enter(&self, lock_name: &str, timeout_seconds: u64) -> Result<LockLease, LockOutcome> {
        enter(
            &self.session,
            postgres_lock_key(lock_name).to_string(),
            timeout_seconds,
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
