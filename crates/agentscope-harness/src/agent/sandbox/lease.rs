//! Sandbox lease for execution isolation.
use super::key::SandboxIsolationKey;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Guard lease for execution isolation.
///
/// When a `SandboxExecutionGuard` is configured, the manager acquires an execution
/// `SandboxLease` before sandbox resume/create for isolation keys that are present. The
/// lease is carried by the `SandboxAcquireResult` and closed by the caller
/// (`SandboxLifecycleMiddleware`) after `SandboxManager::release`, ensuring the full
/// call window is covered.
///
/// # Release contract (drop does **not** release)
///
/// [`SandboxGuard::release`](super::SandboxGuard::release) is `async`, so it cannot run
/// from `Drop`. Dropping a lease therefore never touches the guard: the owner must hand
/// the acquire result back to
/// [`SandboxManager::release`](super::SandboxManager::release), which awaits the guard
/// release. (The former empty `Drop` impl was a no-op, so every isolation-keyed
/// acquisition kept its lock until the lease expired.)
///
/// [`SandboxLease::mark_released`] makes the release exactly-once: a re-entrant guard
/// counts leases per key, so releasing twice would drop another caller's hold.
pub struct SandboxLease {
    /// The isolation key this lease is for.
    key: SandboxIsolationKey,

    /// Whether this is a no-op lease.
    noop: bool,

    /// Whether the guard release for this lease has already been performed.
    released: Arc<AtomicBool>,
}

impl SandboxLease {
    /// Creates a new lease for the given isolation key.
    pub fn new(key: SandboxIsolationKey) -> Self {
        Self {
            key,
            noop: false,
            released: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Creates a no-op lease (no guard was configured).
    pub fn noop() -> Self {
        Self {
            key: SandboxIsolationKey::new("noop", "noop"),
            noop: true,
            released: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns the isolation key.
    pub fn key(&self) -> &SandboxIsolationKey {
        &self.key
    }

    /// Returns true if this is a no-op lease.
    pub fn is_noop(&self) -> bool {
        self.noop
    }

    /// Claims the single release this lease is entitled to.
    ///
    /// Returns `true` for the first caller only, so
    /// [`SandboxManager::release`](super::SandboxManager::release) is idempotent and a
    /// second call can never release a re-entrant guard hold twice.
    pub fn mark_released(&self) -> bool {
        !self.released.swap(true, Ordering::SeqCst)
    }

    /// Whether the guard release for this lease has already been performed.
    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for SandboxLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxLease")
            .field("key", &self.key)
            .field("noop", &self.noop)
            .field("released", &self.is_released())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_lease_creation() {
        let key = SandboxIsolationKey::new("user", "user123");
        let lease = SandboxLease::new(key.clone());

        assert_eq!(lease.key(), &key);
        assert!(!lease.is_noop());
    }

    #[test]
    fn test_sandbox_lease_noop() {
        let lease = SandboxLease::noop();

        assert!(lease.is_noop());
        assert_eq!(lease.key().scope, "noop");
        assert_eq!(lease.key().value, "noop");
    }

    /// Dropping a lease must not claim the release: only an explicit `mark_released`
    /// (issued by `SandboxManager::release`) may claim it, and only once.
    #[test]
    fn drop_does_not_release_and_mark_released_is_exactly_once() {
        let key = SandboxIsolationKey::new("user", "u1");
        let lease = SandboxLease::new(key.clone());

        drop(SandboxLease::new(key));
        assert!(!lease.is_released(), "drop must not release the guard");

        assert!(lease.mark_released(), "the first release claims the lease");
        assert!(lease.is_released());
        assert!(!lease.mark_released(), "the second release is a no-op");
    }
}
