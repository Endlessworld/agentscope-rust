//! Sandbox execution guard for preventing concurrent execution.
use super::key::SandboxIsolationKey;
use super::lease::SandboxLease;
use agentscope_core::error::{Error, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default upper bound for waiting on a key another task is executing under.
///
/// A guard wait is a *mutual-exclusion* wait, so it must be bounded: an unbounded wait
/// inside a turn would hang the whole run instead of failing the acquisition.
const DEFAULT_WAIT: Duration = Duration::from_secs(120);

/// Per-key state: the gate that serialises holders plus the live holder count.
struct KeyState {
    /// Async gate; a holder owns the `OwnedMutexGuard` for the whole call window.
    gate: Arc<tokio::sync::Mutex<()>>,
    /// Live leases (admission counting, `is_held` / `active_count` diagnostics).
    holders: usize,
}

/// Owned per-key gate hold; dropping it frees the key for the next waiter.
///
/// Lives inside [`SandboxLease`] so the mutual exclusion covers exactly the lease's
/// lifetime, even when a caller forgets to `release`.
#[allow(dead_code)] // holding the guard is the whole point; its value is never read
pub(super) struct GateHold(pub(super) tokio::sync::OwnedMutexGuard<()>);

/// Pluggable execution-guard contract, mirroring the Java `SandboxExecutionGuard`
/// `@FunctionalInterface`.
///
/// The concrete [`SandboxExecutionGuard`] below is the in-process default: it serialises
/// isolation keys **inside one process** with a per-key async gate. Distributed backends
/// (JDBC / Redis / Mongo / COS / ...) implement this trait so `SandboxManager` can serialise
/// sandbox acquisition across processes instead of being limited to the local process.
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
    /// Serialises the isolation key in-process.
    ///
    /// The await is the point of the contract: two `SandboxManager::acquire` windows on one
    /// key must not overlap (the second would resume/mutate/persist over the first), so the
    /// second caller waits for the gate and keeps waiting until the first lease is dropped.
    /// The wait is bounded by [`SandboxExecutionGuard::with_wait_timeout`] (120s by
    /// default); a timeout is an `Err`, i.e. fail-closed abort of the acquisition.
    async fn try_enter(&self, key: SandboxIsolationKey) -> Result<SandboxLease> {
        SandboxExecutionGuard::enter_serialised(self, key).await
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
///
/// The async [`SandboxGuard`] path — the one `SandboxManager` uses — **serialises** a key:
/// a second acquisition waits on the per-key gate until the first lease is dropped. The
/// synchronous [`try_enter`](Self::try_enter) entry point keeps the historical admission
/// counting only (re-entrant, never waits) because callers of it cannot `await`; it is the
/// extension wrappers' entry point and does not provide mutual exclusion.
pub struct SandboxExecutionGuard {
    /// Per-key gate + live lease count.
    keys: Arc<Mutex<HashMap<SandboxIsolationKey, KeyState>>>,

    /// `false` for [`SandboxExecutionGuard::noop`], which admits without a lease.
    enabled: bool,

    /// Upper bound for the async gate wait.
    wait: Duration,
}

impl SandboxExecutionGuard {
    /// Creates a new execution guard that serialises every isolation key.
    pub fn new() -> Self {
        Self {
            keys: Arc::new(Mutex::new(HashMap::new())),
            enabled: true,
            wait: DEFAULT_WAIT,
        }
    }

    /// Creates a no-op guard (no protection).
    ///
    /// Distinct from [`new`](Self::new) on purpose: an explicit no-op admits every caller
    /// and hands back [`SandboxLease::noop`], so a host that wants no exclusion says so.
    pub fn noop() -> Self {
        Self {
            keys: Arc::new(Mutex::new(HashMap::new())),
            enabled: false,
            wait: DEFAULT_WAIT,
        }
    }

    /// Overrides how long the async acquisition waits for a key another task holds.
    pub fn with_wait_timeout(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    fn keys(&self) -> std::sync::MutexGuard<'_, HashMap<SandboxIsolationKey, KeyState>> {
        self.keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Increments the live-lease count for `key` (creating the gate when absent).
    fn admit(&self, key: &SandboxIsolationKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut keys = self.keys();
        let entry = keys.entry(key.clone()).or_insert_with(|| KeyState {
            gate: Arc::new(tokio::sync::Mutex::new(())),
            holders: 0,
        });
        entry.holders += 1;
        Arc::clone(&entry.gate)
    }

    /// Drops every idle key (no holder, no waiter) so the map cannot grow without bound.
    ///
    /// A waiter holds a clone of the gate, so `Arc::strong_count == 1` proves nobody is
    /// queued on it — pruning then can never let a fresh gate race a woken waiter.
    fn prune_idle(&self) {
        let mut keys = self.keys();
        keys.retain(|_, state| state.holders > 0 || Arc::strong_count(&state.gate) > 1);
    }

    /// Async acquisition: waits for the per-key gate, then hands the hold to the lease.
    async fn enter_serialised(&self, key: SandboxIsolationKey) -> Result<SandboxLease> {
        if !self.enabled {
            return Ok(SandboxLease::noop());
        }
        self.prune_idle();
        let gate = self.admit(&key);
        let wait = self.wait;
        let held = match tokio::time::timeout(wait, gate.lock_owned()).await {
            Ok(held) => held,
            Err(_) => {
                // The admission was counted before the wait: take it back before failing,
                // otherwise the key stays "held" with no holder.
                self.release(&key);
                return Err(Error::Other(format!(
                    "sandbox execution guard timed out after {}s waiting for scope {key}",
                    wait.as_secs()
                )));
            }
        };
        Ok(SandboxLease::with_gate(key, GateHold(held)))
    }

    /// Tries to acquire a lease for the given isolation key.
    ///
    /// Synchronous admission only: the caller cannot await, so it never blocks and the key
    /// is not mutually exclusive against a concurrent task. [`SandboxGuard::try_enter`] is
    /// the serialising entry point.
    pub fn try_enter(&self, key: SandboxIsolationKey) -> SandboxLease {
        if !self.enabled {
            return SandboxLease::noop();
        }
        self.admit(&key);
        SandboxLease::new(key)
    }

    /// Releases a lease for the given isolation key.
    pub fn release(&self, key: &SandboxIsolationKey) {
        let mut keys = self.keys();
        if let Some(state) = keys.get_mut(key) {
            state.holders = state.holders.saturating_sub(1);
            if state.holders == 0 && Arc::strong_count(&state.gate) == 1 {
                keys.remove(key);
            }
        }
    }

    /// Returns true if the given key is currently held.
    pub fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        let keys = self.keys();
        keys.get(key)
            .map(|state| state.holders > 0)
            .unwrap_or(false)
    }

    /// Returns the number of active leases.
    pub fn active_count(&self) -> usize {
        let keys = self.keys();
        keys.values().filter(|state| state.holders > 0).count()
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
            keys: Arc::clone(&self.keys),
            enabled: self.enabled,
            wait: self.wait,
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

    /// The async path is the one `SandboxManager` uses: a second acquisition on the same
    /// key must wait until the first lease is dropped instead of resuming/mutating the same
    /// sandbox concurrently (the write-after-write bug).
    #[tokio::test]
    async fn concurrent_entries_on_one_key_are_serialised() {
        let guard = Arc::new(SandboxExecutionGuard::new());
        let key = SandboxIsolationKey::new("session", "s1");

        let first = SandboxGuard::try_enter(&*guard, key.clone())
            .await
            .expect("first lease");
        let second = {
            let guard = Arc::clone(&guard);
            let key = key.clone();
            tokio::spawn(async move { SandboxGuard::try_enter(&*guard, key).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !second.is_finished(),
            "the second entry must wait for the first lease"
        );

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("second entry after release")
            .expect("join")
            .expect("second lease");
        assert!(!second.is_noop());
        drop(second);
    }

    /// The wait is bounded: a holder that never releases fails the second acquisition
    /// (fail-closed) instead of blocking the turn forever.
    #[tokio::test]
    async fn a_waiting_entry_times_out_and_does_not_keep_the_key_held() {
        let guard = SandboxExecutionGuard::new().with_wait_timeout(Duration::from_millis(30));
        let key = SandboxIsolationKey::new("session", "s2");

        let first = SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect("first lease");
        let error = SandboxGuard::try_enter(&guard, key.clone())
            .await
            .unwrap_err();
        assert!(format!("{error}").contains("timed out"), "{error}");
        assert_eq!(
            guard.active_count(),
            1,
            "the failed wait must not leave an admission behind"
        );

        drop(first);
        // `SandboxManager::release` always releases before the lease is dropped.
        SandboxGuard::release(&guard, &key).await;
        assert!(!guard.is_held(&key), "a failed wait must not leak the key");
        // The key is free again once the holder is gone.
        drop(
            SandboxGuard::try_enter(&guard, key.clone())
                .await
                .expect("re-acquire"),
        );
    }

    /// `noop()` is a real no-op now: it admits without exclusion and hands back a no-op
    /// lease, so a host that wants no guard is explicit about it.
    #[tokio::test]
    async fn a_noop_guard_does_not_serialise() {
        let guard = SandboxExecutionGuard::noop();
        let key = SandboxIsolationKey::new("session", "s3");

        let first = SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect("lease");
        assert!(first.is_noop());
        let second = SandboxGuard::try_enter(&guard, key.clone())
            .await
            .expect("lease");
        assert!(second.is_noop());
        assert!(!guard.is_held(&key));
        assert_eq!(guard.active_count(), 0);
    }
}
