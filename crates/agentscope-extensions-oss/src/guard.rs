//! OSS-backed sandbox execution guard.
//!
//! The Java OSS module does not define its own sandbox execution guard: the
//! harness `SandboxExecutionGuard` (an in-process isolation-key lease counter) is
//! used as-is. Per the裁定 in EXTENSIONS_GAP_TRACKER §2.1, storage modules must
//! not invent a parallel `SandboxExecutionGuard` type; instead we re-export the
//! harness guard under the module-specific alias and provide a thin
//! OSS-flavoured constructor so callers can wire it consistently with the other
//! OSS components.
//!
//! [`OssSandboxExecutionGuard`] is therefore a transparent wrapper that delegates
//! to the harness guard, preserving exact `try_enter`/`release`/`is_held`
//! semantics (CAS/lease semantics are owned by the harness).

use std::sync::Arc;

use agentscope_harness::agent::sandbox::{
    SandboxExecutionGuard, SandboxGuard, SandboxIsolationKey, SandboxLease,
};

/// OSS module alias for the harness [`SandboxExecutionGuard`].
///
/// Mirrors the Java module layout (`OssDistributedStore` exposes a
/// `SandboxExecutionGuard`) without redefining the guard's semantics.
#[derive(Debug, Clone, Default)]
pub struct OssSandboxExecutionGuard {
    inner: Arc<SandboxExecutionGuard>,
}

impl OssSandboxExecutionGuard {
    /// Creates a new guard.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SandboxExecutionGuard::new()),
        }
    }

    /// Creates a guard from an existing harness guard (shared handle).
    pub fn from_harness(guard: Arc<SandboxExecutionGuard>) -> Self {
        Self { inner: guard }
    }

    /// Returns the underlying harness guard.
    pub fn inner(&self) -> &Arc<SandboxExecutionGuard> {
        &self.inner
    }

    /// Tries to acquire a lease for the given isolation key.
    pub fn try_enter(&self, key: SandboxIsolationKey) -> SandboxLease {
        self.inner.try_enter(key)
    }

    /// Releases a lease for the given isolation key.
    pub fn release(&self, key: &SandboxIsolationKey) {
        self.inner.release(key)
    }

    /// Returns whether the given key is currently held.
    pub fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        self.inner.is_held(key)
    }

    /// Returns the number of distinct active leases.
    pub fn active_count(&self) -> usize {
        self.inner.active_count()
    }
}

#[async_trait::async_trait]
impl SandboxGuard for OssSandboxExecutionGuard {
    async fn try_enter(&self, key: SandboxIsolationKey) -> SandboxLease {
        OssSandboxExecutionGuard::try_enter(self, key)
    }

    async fn release(&self, key: &SandboxIsolationKey) {
        OssSandboxExecutionGuard::release(self, key)
    }

    fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        OssSandboxExecutionGuard::is_held(self, key)
    }

    fn active_count(&self) -> usize {
        OssSandboxExecutionGuard::active_count(self)
    }

    // `is_distributed()` keeps the trait default (`false`): this wrapper delegates to the
    // process-local harness `SandboxExecutionGuard`. OSS offers no server-side lock, so the
    // guard cannot serialise across processes.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_delegates_to_the_harness_guard() {
        let guard = OssSandboxExecutionGuard::new();
        let key = SandboxIsolationKey::new("user", "u1");
        assert!(!guard.is_held(&key));
        let lease = guard.try_enter(key.clone());
        assert!(!lease.is_noop());
        assert!(guard.is_held(&key));
        guard.release(&key);
        assert!(!guard.is_held(&key));
    }

    /// OSS delegates to the in-process harness guard: `is_distributed()` must stay `false`
    /// so a host does not assume cross-process sandbox serialisation.
    #[test]
    fn guard_is_not_distributed() {
        let guard = OssSandboxExecutionGuard::new();
        assert!(!SandboxGuard::is_distributed(&guard));
        let object: Arc<dyn SandboxGuard> = Arc::new(OssSandboxExecutionGuard::new());
        assert!(!object.is_distributed());
    }
}
