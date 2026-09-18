//! Test-only in-memory [`LockBackend`] implementation.
//!
//! Emulates MySQL `GET_LOCK()` / `RELEASE_LOCK()` hold/release bookkeeping so the
//! contention and timeout behaviour of [`super::MysqlSandboxExecutionGuard`] can be
//! exercised without a live MySQL server. Compiled only under `cfg(test)`.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use crate::error::MysqlResult;

use super::LockBackend;

/// In-memory advisory-lock backend.
#[derive(Default)]
pub struct InMemoryLockBackend {
    held: Mutex<HashSet<String>>,
}

impl InMemoryLockBackend {
    /// Number of currently-held locks.
    pub fn held_count(&self) -> usize {
        self.held.lock().unwrap().len()
    }
}

impl LockBackend for InMemoryLockBackend {
    fn try_acquire(&self, lock_name: &str, _timeout: Duration) -> MysqlResult<bool> {
        let mut held = self.held.lock().unwrap();
        // Emulate MySQL's server-scoped semantics: a lock is held until released.
        // Contention (already held) reports failure immediately, matching GET_LOCK
        // returning 0 within a zero-effort in-memory model.
        Ok(held.insert(lock_name.to_string()))
    }

    fn release_lock(&self, lock_name: &str) -> MysqlResult<bool> {
        let mut held = self.held.lock().unwrap();
        Ok(held.remove(lock_name))
    }

    fn is_held(&self, lock_name: &str) -> MysqlResult<bool> {
        Ok(self.held.lock().unwrap().contains(lock_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_roundtrip() {
        let b = InMemoryLockBackend::default();
        assert!(b.try_acquire("l", Duration::from_secs(1)).unwrap());
        assert!(!b.try_acquire("l", Duration::from_secs(1)).unwrap());
        assert!(b.is_held("l").unwrap());
        assert!(b.release_lock("l").unwrap());
        assert!(!b.is_held("l").unwrap());
        assert!(!b.release_lock("l").unwrap());
    }
}
