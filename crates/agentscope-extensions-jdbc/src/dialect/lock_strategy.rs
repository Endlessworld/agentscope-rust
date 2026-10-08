//! Distributed lock contract, mirroring `SandboxLockStrategy`.
//!
//! The portable implementation is [`crate::sandbox::TableLockStrategy`] (INSERT/DELETE with
//! polling). Databases with native advisory locks are served by the strategies in
//! [`crate::dialect::native_lock`] — [`crate::MysqlNativeLockStrategy`] (`GET_LOCK`) and
//! [`crate::PostgresNativeLockStrategy`] (`pg_try_advisory_lock`). Those are separate types
//! rather than dialect overrides because the lock is session-scoped and a dialect carries no
//! connection (see the `native_lock` module docs).

use std::time::Duration;

/// Outcome of a lock acquisition attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    /// The lock was acquired.
    Acquired,
    /// The lock could not be acquired before the timeout expired.
    TimedOut,
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
pub trait SandboxLockStrategy: Send + Sync {
    /// Acquires the named lock, blocking until available or the timeout expires.
    ///
    /// Returns `Ok(lease)` when acquired, or `Err(LockOutcome::TimedOut)` when the
    /// timeout expired. Backends that cannot block (e.g. a lock table) poll internally.
    fn try_enter(
        &self,
        lock_name: &str,
        timeout_seconds: u64,
    ) -> Result<LockLease, LockOutcome>;

    /// Convenience wrapper accepting a [`Duration`] timeout.
    fn try_enter_for(
        &self,
        lock_name: &str,
        timeout: Duration,
    ) -> Result<LockLease, LockOutcome> {
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
