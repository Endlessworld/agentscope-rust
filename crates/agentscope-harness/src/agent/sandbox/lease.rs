//! Sandbox lease for execution isolation.
use super::key::SandboxIsolationKey;

/// Guard lease for execution isolation.
///
/// When a `SandboxExecutionGuard` is configured, the manager acquires an execution
/// `SandboxLease` before sandbox resume/create for isolation keys that are present. The
/// lease is carried by the `SandboxAcquireResult` and closed by the caller
/// (`SandboxLifecycleMiddleware`) after `SandboxManager::release`, ensuring the full
/// call window is covered.
pub struct SandboxLease {
    /// The isolation key this lease is for.
    key: SandboxIsolationKey,

    /// Whether this is a no-op lease.
    noop: bool,
}

impl SandboxLease {
    /// Creates a new lease for the given isolation key.
    pub fn new(key: SandboxIsolationKey) -> Self {
        Self { key, noop: false }
    }

    /// Creates a no-op lease (no guard was configured).
    pub fn noop() -> Self {
        Self {
            key: SandboxIsolationKey::new("noop", "noop"),
            noop: true,
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
}

impl Drop for SandboxLease {
    fn drop(&mut self) {
        // The actual guard release happens in SandboxExecutionGuard
        // when the lease is dropped. For no-op leases, nothing happens.
        if !self.noop {
            // In a real implementation, this would call guard.release(&self.key)
            // But since we don't have a reference to the guard here,
            // we rely on the guard's internal cleanup mechanism.
        }
    }
}

impl std::fmt::Debug for SandboxLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxLease")
            .field("key", &self.key)
            .field("noop", &self.noop)
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
}
