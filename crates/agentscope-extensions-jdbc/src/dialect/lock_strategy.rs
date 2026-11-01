//! Distributed lock contract, mirroring `SandboxLockStrategy`.
//!
//! The portable implementation is [`crate::sandbox::TableLockStrategy`] (INSERT/DELETE with
//! polling). Databases with native advisory locks are served by the strategies in
//! [`crate::dialect::native_lock`] — [`crate::MysqlNativeLockStrategy`] (`GET_LOCK`) and
//! [`crate::PostgresNativeLockStrategy`] (`pg_try_advisory_lock`). Those are separate types
//! rather than dialect overrides because the lock is session-scoped and a dialect carries no
//! connection (see the `native_lock` module docs).

use std::time::{Duration, Instant};

/// Initial interval between two acquisition attempts of [`SandboxLockStrategy::try_enter_async`].
///
/// Mirrors Java `AbstractJdbcDialect.INITIAL_BACKOFF_MS`. The interval doubles after every
/// failed attempt up to [`MAX_LOCK_POLL_INTERVAL`], so a contended lock is probed with
/// 50/100/200/400/800/1000ms waits instead of a fixed interval. Sleeping happens on the async
/// task (`tokio::time::sleep`), never on the calling worker thread.
pub const DEFAULT_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Upper bound for the retry interval, mirroring Java `MAX_BACKOFF_MS`.
pub const MAX_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// Next retry interval: `min(current * 2, MAX_LOCK_POLL_INTERVAL)` (Java
/// `backoff = Math.min(backoff * 2, MAX_BACKOFF_MS)`).
pub fn next_lock_poll_interval(current: Duration) -> Duration {
    (current * 2).min(MAX_LOCK_POLL_INTERVAL)
}

/// Outcome of a lock acquisition attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOutcome {
    /// The lock was acquired.
    Acquired,
    /// The lock could not be acquired before the timeout expired.
    ///
    /// Only a **deadline expiry** (or "someone else holds it") maps here: the guard degrades
    /// to a no-op lease.
    TimedOut,
    /// The backend reported a failure (unreachable server, unsupported statement, ...).
    ///
    /// Backend failures must never be folded into [`LockOutcome::TimedOut`]: a broken lock
    /// store is not the same thing as a contended lock, and downgrading it would let two
    /// processes run unprotected while pretending the guard worked.
    Failed(String),
}

/// Handle to a held distributed lock.
///
/// Dropping the lease (or explicitly calling [`LockLease::release`]) releases the lock.
/// The concrete release mechanism is backend-specific and captured by the closure.
pub struct LockLease {
    /// The name of the lock held.
    pub lock_name: String,
    /// Releases the underlying lock. Idempotent.
    release: Box<dyn FnMut() + Send>,
    released: bool,
}

impl LockLease {
    /// Creates a lease with the given name and release callback.
    pub fn new(lock_name: impl Into<String>, release: impl FnMut() + Send + 'static) -> Self {
        Self {
            lock_name: lock_name.into(),
            release: Box::new(release),
            released: false,
        }
    }

    /// Explicitly releases the lock. Idempotent.
    pub fn release(&mut self) {
        if !self.released {
            (self.release)();
            self.released = true;
        }
    }
}

impl std::fmt::Debug for LockLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockLease")
            .field("lock_name", &self.lock_name)
            .field("released", &self.released)
            .finish()
    }
}

impl Drop for LockLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Blocking-until-available or timeout lock acquisition.
///
/// ## Sync vs async
///
/// [`Self::try_enter_once`] performs a **single** attempt and never sleeps. The polling loop
/// lives in [`Self::try_enter_async`], which sleeps with `tokio::time::sleep` — a sync
/// polling loop would have to sleep on the calling thread, which is exactly what stalls a
/// Tokio worker (and previously forced `std::thread::sleep` inside the acquire path).
///
/// [`Self::try_enter`] is the sync compatibility entry point: it performs one attempt only,
/// so existing synchronous callers keep working without blocking a runtime. It stays
/// available for strategies (like [`crate::sandbox::TableLockStrategy`]) that must offer a
/// blocking flavour to non-async callers.
#[async_trait::async_trait]
pub trait SandboxLockStrategy: Send + Sync {
    /// Performs a single acquisition attempt.
    ///
    /// `attempt_timeout` bounds **this attempt only** (a blocking-capable backend such as the
    /// MySQL `GET_LOCK` may wait that long server-side); the whole acquisition is bounded by
    /// [`Self::try_enter_async`]'s deadline.
    ///
    /// Returns `Ok(lease)` when acquired, `Err(LockOutcome::TimedOut)` when the lock is held
    /// by someone else, and `Err(LockOutcome::Failed(_))` when the backend failed.
    fn try_enter_once(
        &self,
        lock_name: &str,
        attempt_timeout: Duration,
    ) -> Result<LockLease, LockOutcome>;

    /// Acquires the named lock, retrying until the deadline expires.
    ///
    /// Sleeps between attempts with `tokio::time::sleep`. Deadline expiry yields
    /// `Err(LockOutcome::TimedOut)`; a backend failure short-circuits with
    /// `Err(LockOutcome::Failed(_))` and is never retried into a timeout.
    async fn try_enter_async(
        &self,
        lock_name: &str,
        timeout_seconds: u64,
    ) -> Result<LockLease, LockOutcome> {
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        let mut interval = DEFAULT_LOCK_POLL_INTERVAL;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.try_enter_once(lock_name, remaining) {
                Ok(lease) => return Ok(lease),
                // `Acquired` as an error value is a legacy sentinel: treat it as "retry".
                Err(LockOutcome::Acquired) | Err(LockOutcome::TimedOut) => {}
                Err(LockOutcome::Failed(message)) => {
                    return Err(LockOutcome::Failed(message));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LockOutcome::TimedOut);
            }
            tokio::time::sleep(interval.min(remaining)).await;
            interval = next_lock_poll_interval(interval);
        }
    }

    /// Synchronous single-attempt acquisition (no sleeping).
    ///
    /// Returns `Ok(lease)` when acquired, or `Err(LockOutcome::TimedOut)` when the lock is
    /// unavailable / the timeout is already exhausted. Backend failures surface as
    /// `Err(LockOutcome::Failed(_))`. Callers that can await should use
    /// [`Self::try_enter_async`] to get the retry loop.
    fn try_enter(&self, lock_name: &str, timeout_seconds: u64) -> Result<LockLease, LockOutcome> {
        self.try_enter_once(lock_name, Duration::from_secs(timeout_seconds))
    }

    /// Convenience wrapper accepting a [`Duration`] timeout.
    fn try_enter_for(&self, lock_name: &str, timeout: Duration) -> Result<LockLease, LockOutcome> {
        self.try_enter(lock_name, timeout.as_secs())
    }

    /// Releases a named lock previously acquired via [`Self::try_enter`]. Idempotent.
    ///
    /// The portable table strategy deletes the lock row here; strategies whose leases
    /// release on drop may keep the default no-op.
    fn release(&self, lock_name: &str) {
        let _ = lock_name;
    }

    /// Reports whether the named lock is currently held. Defaults to `false` for
    /// strategies without a cheap probe.
    fn is_held(&self, lock_name: &str) -> bool {
        let _ = lock_name;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_50ms_and_caps_at_one_second() {
        // Java: AbstractJdbcDialect table lock (`backoff = min(backoff * 2, MAX)`).
        let mut interval = DEFAULT_LOCK_POLL_INTERVAL;
        let mut seen = Vec::new();
        for _ in 0..7 {
            seen.push(interval.as_millis());
            interval = next_lock_poll_interval(interval);
        }
        assert_eq!(seen, vec![50, 100, 200, 400, 800, 1000, 1000]);
        // At or past the cap it stays capped.
        assert_eq!(
            next_lock_poll_interval(MAX_LOCK_POLL_INTERVAL),
            MAX_LOCK_POLL_INTERVAL
        );
        assert_eq!(
            next_lock_poll_interval(Duration::from_millis(900)),
            MAX_LOCK_POLL_INTERVAL
        );
    }
}
