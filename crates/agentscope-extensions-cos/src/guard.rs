//! Tencent Cloud COS-adjacent [`SandboxExecutionGuard`].
//!
//! **Design note.** In Java, `com.qcloud.cos.COSClient` was passed around by every
//! component. The Rust port keeps the guard in-crate and reuses the harness
//! [`SandboxExecutionGuard`] lease semantics: `try_enter`/`release`/`is_held`.
//! This wrapper adds the COS-object-store-backed coordination seam so callers can
//! gate sandbox work on the same distributed store they use for state.
//!
//! The harness guard is process-local; the distributed coordination over the object
//! store is strategy-deferred (see `COS__GAP_TRACKER.md` §4.2). The type is a thin,
//! labelled newtype so the distributed-store composition has a concrete guard to
//! hand out without re-inventing the trait.

use std::sync::Arc;

use agentscope_harness::agent::sandbox::{
    SandboxExecutionGuard as HarnessExecutionGuard, SandboxGuard, SandboxIsolationKey, SandboxLease,
};

use crate::object_store::SharedObjectStore;

/// COS-backed sandbox execution guard.
///
/// Wraps the harness [`HarnessExecutionGuard`] and retains the shared object store
/// for (future) distributed coordination. Implements the same
/// `try_enter` / `release` / `is_held` surface as the harness guard so it can be
/// dropped into the same call sites.
#[derive(Clone)]
pub struct CosSandboxExecutionGuard {
    object_store: SharedObjectStore,
    inner: HarnessExecutionGuard,
}

impl std::fmt::Debug for CosSandboxExecutionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosSandboxExecutionGuard")
            .field("active_leases", &self.inner.active_count())
            .finish_non_exhaustive()
    }
}

impl CosSandboxExecutionGuard {
    /// Creates a new guard backed by the given object store.
    pub fn new(object_store: SharedObjectStore) -> Self {
        Self {
            object_store,
            inner: HarnessExecutionGuard::new(),
        }
    }

    /// Creates a no-op guard (no protection) that still carries an object store.
    pub fn noop(object_store: SharedObjectStore) -> Self {
        Self {
            object_store,
            inner: HarnessExecutionGuard::noop(),
        }
    }

    /// The shared object store backing this guard.
    pub fn object_store(&self) -> &SharedObjectStore {
        &self.object_store
    }

    /// Tries to acquire a lease for the given isolation key.
    pub fn try_enter(&self, key: SandboxIsolationKey) -> SandboxLease {
        self.inner.try_enter(key)
    }

    /// Releases a lease for the given isolation key.
    pub fn release(&self, key: &SandboxIsolationKey) {
        self.inner.release(key)
    }

    /// Whether the given key is currently held.
    pub fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        self.inner.is_held(key)
    }

    /// Number of active leases.
    pub fn active_count(&self) -> usize {
        self.inner.active_count()
    }

    /// Returns the wrapped harness guard (cloned handle).
    pub fn inner(&self) -> HarnessExecutionGuard {
        self.inner.clone()
    }
}

#[async_trait::async_trait]
impl SandboxGuard for CosSandboxExecutionGuard {
    async fn try_enter(&self, key: SandboxIsolationKey) -> SandboxLease {
        CosSandboxExecutionGuard::try_enter(self, key)
    }

    async fn release(&self, key: &SandboxIsolationKey) {
        CosSandboxExecutionGuard::release(self, key)
    }

    fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        CosSandboxExecutionGuard::is_held(self, key)
    }

    fn active_count(&self) -> usize {
        CosSandboxExecutionGuard::active_count(self)
    }

    // `is_distributed()` keeps the trait default (`false`): this guard wraps the
    // process-local harness `SandboxExecutionGuard`, so it serialises only inside one
    // process. COS has no server-side lock primitive to build a distributed guard on.
}

impl Default for CosSandboxExecutionGuard {
    fn default() -> Self {
        Self::new(Arc::new(crate::object_store::InMemoryCosObjectStore::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::InMemoryCosObjectStore;

    fn guard() -> CosSandboxExecutionGuard {
        CosSandboxExecutionGuard::new(InMemoryCosObjectStore::new().shared())
    }

    #[test]
    fn enter_release_lease_lifecycle() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        assert!(!g.is_held(&key));
        let lease = g.try_enter(key.clone());
        assert!(!lease.is_noop());
        assert!(g.is_held(&key));
        assert_eq!(g.active_count(), 1);
        g.release(&key);
        assert!(!g.is_held(&key));
        assert_eq!(g.active_count(), 0);
    }

    #[test]
    fn reentrant_leases() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let _l1 = g.try_enter(key.clone());
        let _l2 = g.try_enter(key.clone());
        g.release(&key);
        assert!(g.is_held(&key));
        g.release(&key);
        assert!(!g.is_held(&key));
    }

    #[test]
    fn clone_shares_state() {
        let g = guard();
        let g2 = g.clone();
        let key = SandboxIsolationKey::new("session", "s1");
        let _l = g.try_enter(key.clone());
        assert!(g2.is_held(&key));
    }

    /// COS delegates to the in-process harness guard: the trait object must report
    /// `is_distributed() == false` so a host does not assume cross-process locking.
    #[tokio::test]
    async fn guard_is_not_distributed() {
        let g = guard();
        assert!(!SandboxGuard::is_distributed(&g));
        let object: Arc<dyn SandboxGuard> = Arc::new(guard());
        assert!(!object.is_distributed());
    }
}
