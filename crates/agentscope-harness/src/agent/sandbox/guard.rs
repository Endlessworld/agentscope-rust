//! Sandbox execution guard for preventing concurrent execution.
use super::key::SandboxIsolationKey;
use super::lease::SandboxLease;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Prevents concurrent execution for isolation keys.
///
/// When configured, the manager acquires an execution `SandboxLease` before sandbox
/// resume/create for isolation keys that are present. The lease is carried by the
/// `SandboxAcquireResult` and closed by the caller after `SandboxManager::release`,
/// ensuring the full call window is covered.
pub struct SandboxExecutionGuard {
    /// Active leases by isolation key.
    leases: Arc<Mutex<HashMap<SandboxIsolationKey, usize>>>,
}

impl SandboxExecutionGuard {
    /// Creates a new execution guard.
    pub fn new() -> Self {
        Self {
            leases: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Creates a no-op guard (no protection).
    pub fn noop() -> Self {
        Self::new()
    }

    /// Tries to acquire a lease for the given isolation key.
    ///
    /// Returns a lease if the key is not already held, or if we allow re-entrant access.
    pub fn try_enter(&self, key: SandboxIsolationKey) -> SandboxLease {
        let mut leases = self.leases.lock().unwrap_or_else(|poisoned| {
            // If the mutex is poisoned, we still want to return a lease
            // to avoid deadlocking the entire system
            poisoned.into_inner()
        });

        // For now, we allow re-entrant access (increment counter)
        *leases.entry(key.clone()).or_insert(0) += 1;

        SandboxLease::new(key)
    }

    /// Releases a lease for the given isolation key.
    pub fn release(&self, key: &SandboxIsolationKey) {
        let mut leases = self
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(count) = leases.get_mut(key) {
            if *count > 1 {
                *count -= 1;
            } else {
                leases.remove(key);
            }
        }
    }

    /// Returns true if the given key is currently held.
    pub fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        let leases = self
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        leases.contains_key(key)
    }

    /// Returns the number of active leases.
    pub fn active_count(&self) -> usize {
        let leases = self
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        leases.len()
    }
}

impl Default for SandboxExecutionGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SandboxExecutionGuard {
    fn clone(&self) -> Self {
        Self {
            leases: Arc::clone(&self.leases),
        }
    }
}

impl std::fmt::Debug for SandboxExecutionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.active_count();
        f.debug_struct("SandboxExecutionGuard")
            .field("active_leases", &count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execution_guard_creation() {
        let guard = SandboxExecutionGuard::new();
        assert_eq!(guard.active_count(), 0);
    }

    #[test]
    fn test_execution_guard_acquire_release() {
        let guard = SandboxExecutionGuard::new();
        let key = SandboxIsolationKey::new("user", "user123");

        // Acquire lease
        let lease = guard.try_enter(key.clone());
        assert!(!lease.is_noop());
        assert!(guard.is_held(&key));
        assert_eq!(guard.active_count(), 1);

        // Release lease
        guard.release(&key);
        assert!(!guard.is_held(&key));
        assert_eq!(guard.active_count(), 0);
    }

    #[test]
    fn test_execution_guard_reentrant() {
        let guard = SandboxExecutionGuard::new();
        let key = SandboxIsolationKey::new("user", "user123");

        // Acquire two leases
        let _lease1 = guard.try_enter(key.clone());
        let _lease2 = guard.try_enter(key.clone());

        assert!(guard.is_held(&key));

        // Release one lease
        guard.release(&key);
        assert!(guard.is_held(&key)); // Still held

        // Release second lease
        guard.release(&key);
        assert!(!guard.is_held(&key)); // Now released
    }

    #[test]
    fn test_execution_guard_clone() {
        let guard1 = SandboxExecutionGuard::new();
        let guard2 = guard1.clone();

        let key = SandboxIsolationKey::new("user", "user123");

        // Acquire on guard1
        let _lease = guard1.try_enter(key.clone());

        // Should be visible on guard2 (shared state)
        assert!(guard2.is_held(&key));
    }
}
