//! Sandbox execution guard for preventing concurrent execution.
use super::key::SandboxIsolationKey;
use super::lease::SandboxLease;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Pluggable execution-guard contract, mirroring the Java `SandboxExecutionGuard`
/// `@FunctionalInterface`.
///
/// The concrete [`SandboxExecutionGuard`] below is the in-process default (re-entrant
/// counting only). Distributed backends (JDBC / Redis / Mongo / COS / ...) implement this
/// trait so `SandboxManager` can serialise sandbox acquisition across processes instead of
/// being limited to the local process.
///
/// ## Why `try_enter` / `release` are async
///
/// A distributed guard has to poll an external lock store (`SET NX PX`, `GET_LOCK`,
/// `pg_try_advisory_lock`, ...), and that poll sleeps between attempts. When the contract
/// was synchronous, implementations had to bridge with `futures::executor::block_on` —
/// which spins or deadlocks when the acquisition is itself driven from a Tokio task
/// (the only production call site is `SandboxManager::acquire`). Making both the acquire
/// and the release `async` lets every backend just `await tokio::time::sleep`.
///
/// [`SandboxGuard::is_held`] stays synchronous: it is a diagnostics probe (single
/// round-trip, no polling) and is never part of the acquire/release path. Callers that can
/// `await` should use [`SandboxGuard::try_is_held`] instead — the async seam a distributed
/// backend overrides when its probe needs a real round-trip.
///
/// ## Fail-closed contract
///
/// [`SandboxGuard::try_enter`] is **fallible**: when the lease cannot be acquired (lock
/// store unreachable, acquisition timed out, backend unsupported) the implementation
/// **must** return `Err`. It must never hand back [`SandboxLease::noop`] and let the
/// caller keep going — that silently drops cross-process mutual exclusion, which is the
/// exact bug this contract exists to prevent. `SandboxManager::acquire` propagates the
/// error with `?`, so the sandbox is neither resumed nor created under a lost lock.
#[async_trait::async_trait]
pub trait SandboxGuard: Send + Sync + std::fmt::Debug {
    /// Tries to acquire a lease for the given isolation key.
    ///
    /// `Ok(lease)` means the caller holds the isolation key. `Err(_)` means acquisition
    /// failed and the caller **must abort** (fail-closed); returning a no-op lease here is
    /// a contract violation.
    async fn try_enter(
        &self,
        key: SandboxIsolationKey,
    ) -> agentscope_core::error::Result<SandboxLease>;

    /// Releases a lease for the given isolation key.
    ///
    /// This must be awaited by [`SandboxManager::release`](super::SandboxManager::release):
    /// dropping a [`SandboxLease`] never releases the guard.
    async fn release(&self, key: &SandboxIsolationKey);

    /// Returns true if the given key is currently held.
    fn is_held(&self, key: &SandboxIsolationKey) -> bool;

    /// Async diagnostics probe: is `key` currently held?
    ///
    /// [`is_held`](Self::is_held) stays synchronous for the existing backends, but a
    /// distributed guard can only answer by round-tripping to its lock store. Bridging that
    /// with `futures::executor::block_on` blocks a Tokio worker and can deadlock on a
    /// `current_thread` runtime, so backends whose probe needs an `await` override this
    /// method and callers that can `await` should ask through here. The default delegates to
    /// [`is_held`](Self::is_held).
    async fn try_is_held(&self, key: &SandboxIsolationKey) -> agentscope_core::error::Result<bool> {
        Ok(self.is_held(key))
    }

    /// Number of active leases (diagnostics; distributed guards may return 0).
    fn active_count(&self) -> usize {
        0
    }

    /// Whether this guard serialises across processes.
    ///
    /// `false` (the default) means the guard is process-local: the harness default
    /// [`SandboxExecutionGuard`], and the object-store guards that delegate to it. Storage
    /// backends that take a real distributed lock (JDBC / MySQL / PostgreSQL / Redis /
    /// MongoDB) override this to `true`. Hosts that project a
    /// [`DistributedStore`](crate::agent::DistributedStore) must consult this flag: the
    /// `sandbox_execution_guard` slot is typed `Arc<dyn SandboxGuard>` and may hold either
    /// flavour.
    fn is_distributed(&self) -> bool {
        false
    }
}

#[async_trait::async_trait]
impl SandboxGuard for SandboxExecutionGuard {
    /// The in-process guard cannot fail: re-entrant counting is local state, so it always
    /// yields `Ok(lease)`. Object-store guards that delegate here inherit that.
    async fn try_enter(
        &self,
        key: SandboxIsolationKey,
    ) -> agentscope_core::error::Result<SandboxLease> {
        Ok(SandboxExecutionGuard::try_enter(self, key))
    }

    async fn release(&self, key: &SandboxIsolationKey) {
        SandboxExecutionGuard::release(self, key)
    }

    fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        SandboxExecutionGuard::is_held(self, key)
    }

    fn active_count(&self) -> usize {
        SandboxExecutionGuard::active_count(self)
    }
}

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

    /// A distributed guard is a plain `Arc<dyn SandboxGuard>`; the trait must be object safe
    /// and dispatch to the injected implementation rather than the in-process default.
    #[tokio::test]
    async fn injected_guard_drives_the_trait_object_path() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug, Default)]
        struct CountingGuard {
            enters: AtomicUsize,
            releases: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl SandboxGuard for CountingGuard {
            async fn try_enter(
                &self,
                key: SandboxIsolationKey,
            ) -> agentscope_core::error::Result<SandboxLease> {
                self.enters.fetch_add(1, Ordering::SeqCst);
                Ok(SandboxLease::new(key))
            }
            async fn release(&self, _key: &SandboxIsolationKey) {
                self.releases.fetch_add(1, Ordering::SeqCst);
            }
            fn is_held(&self, _key: &SandboxIsolationKey) -> bool {
                false
            }
        }

        let inner = Arc::new(CountingGuard::default());
        let guard: Arc<dyn SandboxGuard> = inner.clone();
        let key = SandboxIsolationKey::new("user", "u1");
        let _lease = guard.try_enter(key.clone()).await.expect("lease");
        guard.release(&key).await;

        assert_eq!(inner.enters.load(Ordering::SeqCst), 1);
        assert_eq!(inner.releases.load(Ordering::SeqCst), 1);
    }

    /// The in-process default is not a distributed guard; storage backends override this so
    /// hosts can tell the two apart through the `Arc<dyn SandboxGuard>` slot.
    #[tokio::test]
    async fn in_process_guard_is_not_distributed() {
        let guard = SandboxExecutionGuard::new();
        assert!(!guard.is_distributed());
        let object: Arc<dyn SandboxGuard> = Arc::new(SandboxExecutionGuard::new());
        assert!(!object.is_distributed());
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

    /// A guard that only implements the synchronous probe: the async seam must delegate to it.
    #[derive(Debug)]
    struct SyncOnlyProbeGuard {
        held: bool,
    }

    #[async_trait::async_trait]
    impl SandboxGuard for SyncOnlyProbeGuard {
        async fn try_enter(
            &self,
            key: SandboxIsolationKey,
        ) -> agentscope_core::error::Result<SandboxLease> {
            Ok(SandboxLease::new(key))
        }

        async fn release(&self, _key: &SandboxIsolationKey) {}

        fn is_held(&self, key: &SandboxIsolationKey) -> bool {
            let _ = key;
            self.held
        }
    }

    /// A distributed guard whose probe needs a round-trip: it overrides the async seam and
    /// reports the backend failure instead of blocking a Tokio worker on `block_on`.
    #[derive(Debug)]
    struct FailingAsyncProbeGuard;

    #[async_trait::async_trait]
    impl SandboxGuard for FailingAsyncProbeGuard {
        async fn try_enter(
            &self,
            key: SandboxIsolationKey,
        ) -> agentscope_core::error::Result<SandboxLease> {
            Ok(SandboxLease::new(key))
        }

        async fn release(&self, _key: &SandboxIsolationKey) {}

        fn is_held(&self, _key: &SandboxIsolationKey) -> bool {
            unreachable!("the async probe overrides the synchronous one")
        }

        async fn try_is_held(
            &self,
            _key: &SandboxIsolationKey,
        ) -> agentscope_core::error::Result<bool> {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            Err(agentscope_core::error::Error::Other(
                "lock store unreachable".to_string(),
            ))
        }
    }

    /// The added seam is opt-in: a backend that only knows the local probe keeps working and
    /// its answer reaches the caller through `try_is_held`.
    #[tokio::test]
    async fn try_is_held_defaults_to_the_sync_probe() {
        let key = SandboxIsolationKey::new("user", "u1");
        assert!(SyncOnlyProbeGuard { held: true }
            .try_is_held(&key)
            .await
            .unwrap());
        assert!(!SyncOnlyProbeGuard { held: false }
            .try_is_held(&key)
            .await
            .unwrap());

        // Object safe through `Arc<dyn SandboxGuard>`, like the manager's guard slot.
        let object: Arc<dyn SandboxGuard> = Arc::new(SyncOnlyProbeGuard { held: true });
        assert!(object.try_is_held(&key).await.unwrap());
    }

    /// The override must be reached through the trait object, and it must be able to `await`
    /// inside a single-threaded runtime — a `block_on` probe deadlocks there.
    #[tokio::test(flavor = "current_thread")]
    async fn try_is_held_reaches_an_async_override_on_a_current_thread_runtime() {
        let key = SandboxIsolationKey::new("user", "u1");
        let guard: Arc<dyn SandboxGuard> = Arc::new(FailingAsyncProbeGuard);
        let error = guard.try_is_held(&key).await.unwrap_err();
        assert!(
            format!("{error}").contains("lock store unreachable"),
            "{error}"
        );
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
