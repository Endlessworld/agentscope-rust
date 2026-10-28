//! MySQL named-lock based sandbox execution guard, mirroring
//! `io.agentscope.extensions.mysql.sandbox.JdbcSandboxExecutionGuard`.
//!
//! The Java implementation serialises sandbox execution for a given
//! [`SandboxIsolationKey`] using MySQL's server-scoped `GET_LOCK()` / `RELEASE_LOCK()`
//! advisory locks. Each acquired lock is carried by a `SandboxLease` whose `close()`
//! releases the underlying connection (implicitly releasing the lock).
//!
//! ## Harness note
//! In the Rust harness, [`SandboxExecutionGuard`] and [`SandboxLease`] are **concrete
//! structs, not traits** (`agentscope_harness::agent::sandbox::guard`). There is
//! therefore no trait for this crate to `impl`; instead `MysqlSandboxExecutionGuard`
//! mirrors the Java guard's public API (`try_enter` / `release` / `is_held`) over a
//! [`LockBackend`] so lock semantics are real and testable without a live MySQL server.
//! See `docs/extensions/MYSQL__GAP_TRACKER.md` §4.2 for the noted cross-module item.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey, SandboxLease};

use crate::error::{MysqlError, MysqlResult};

/// Default lock-name prefix, mirroring `DEFAULT_KEY_PREFIX`.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:sandbox:lock:";

/// A held distributed lock. Dropping it releases the lock (mirroring Java's
/// `SandboxLease.close()` calling `RELEASE_LOCK()` and closing the connection).
pub struct MysqlLockLease {
    backend: Arc<dyn LockBackend>,
    lock_name: String,
    released: bool,
    /// Shared local lease tally, decremented on release/drop so the guard's
    /// `active_count` stays consistent with the backend.
    active: Arc<Mutex<HashMap<String, usize>>>,
}

impl MysqlLockLease {
    /// The composed lock name held by this lease.
    pub fn lock_name(&self) -> &str {
        &self.lock_name
    }

    /// Whether this lease has already been released.
    pub fn is_released(&self) -> bool {
        self.released
    }

    /// Explicitly releases the lock. Idempotent.
    pub fn release(&mut self) {
        if !self.released {
            let _ = self.backend.release_lock(&self.lock_name);
            Self::decrement_active(&self.active, &self.lock_name);
            self.released = true;
        }
    }

    /// Decrements the shared local lease tally for `lock_name`, removing it when the
    /// count reaches zero.
    fn decrement_active(active: &Arc<Mutex<HashMap<String, usize>>>, lock_name: &str) {
        let mut map = active.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(count) = map.get_mut(lock_name) {
            if *count > 1 {
                *count -= 1;
            } else {
                map.remove(lock_name);
            }
        }
    }
}

impl Drop for MysqlLockLease {
    fn drop(&mut self) {
        self.release();
    }
}

impl std::fmt::Debug for MysqlLockLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlLockLease")
            .field("lock_name", &self.lock_name)
            .field("released", &self.released)
            .finish()
    }
}

/// Low-level advisory-lock surface used by [`MysqlSandboxExecutionGuard`].
///
/// Real adapters issue `SELECT GET_LOCK(?, ?)` / `SELECT RELEASE_LOCK(?)` on a MySQL
/// connection; test adapters emulate the same acquire/release bookkeeping.
pub trait LockBackend: Send + Sync {
    /// Tries to acquire `lock_name`, waiting up to `timeout`. Returns `true` on success
    /// (`GET_LOCK` returned 1), `false` on timeout (`GET_LOCK` returned 0).
    fn try_acquire(&self, lock_name: &str, timeout: Duration) -> MysqlResult<bool>;

    /// Releases `lock_name`. Returns whether the lock was held (`RELEASE_LOCK` != 0).
    fn release_lock(&self, lock_name: &str) -> MysqlResult<bool>;

    /// Whether `lock_name` is currently held.
    fn is_held(&self, lock_name: &str) -> MysqlResult<bool>;
}

/// MySQL `GET_LOCK()`-backed sandbox execution guard, mirroring
/// `JdbcSandboxExecutionGuard`.
#[derive(Clone)]
pub struct MysqlSandboxExecutionGuard {
    backend: Arc<dyn LockBackend>,
    key_prefix: String,
    lock_timeout: Duration,
    /// Locally tracked leases, used by [`MysqlSandboxExecutionGuard::active_count`].
    active: Arc<Mutex<HashMap<String, usize>>>,
    /// Concrete leases acquired through the harness `SandboxGuard` trait, keyed by
    /// isolation key.
    ///
    /// The harness `SandboxLease` carries no release hook, so the [`MysqlLockLease`]
    /// (which releases the named lock on drop) must be retained until
    /// `SandboxGuard::release` is called.
    trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<MysqlLockLease>>>>,
}

impl std::fmt::Debug for MysqlSandboxExecutionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlSandboxExecutionGuard")
            .field("key_prefix", &self.key_prefix)
            .field("lock_timeout", &self.lock_timeout)
            .finish()
    }
}

impl MysqlSandboxExecutionGuard {
    /// Creates a builder over the given lock backend.
    pub fn builder(backend: Arc<dyn LockBackend>) -> MysqlSandboxExecutionGuardBuilder {
        MysqlSandboxExecutionGuardBuilder {
            backend,
            key_prefix: DEFAULT_KEY_PREFIX.to_string(),
            lock_timeout: Duration::from_secs(30),
        }
    }

    /// Composes the MySQL lock name for an isolation key.
    ///
    /// Mirrors `JdbcSandboxExecutionGuard.composeLockName`: the scope is lower-cased and
    /// joined with the value and optional sub-key.
    pub fn compose_lock_name(&self, key: &SandboxIsolationKey) -> String {
        let mut name = format!(
            "{}{}:{}",
            self.key_prefix,
            key.scope.to_ascii_lowercase(),
            key.value
        );
        if let Some(sub) = &key.sub_key {
            name.push(':');
            name.push_str(sub);
        }
        name
    }

    /// Tries to acquire a lease for the given isolation key.
    ///
    /// Returns a [`MysqlLockLease`] on success. Returns
    /// [`MysqlError::LockTimeout`] when `GET_LOCK` times out, mirroring the Java guard's
    /// `InterruptedException`.
    pub fn try_enter(&self, key: &SandboxIsolationKey) -> MysqlResult<MysqlLockLease> {
        let lock_name = self.compose_lock_name(key);
        if self.backend.try_acquire(&lock_name, self.lock_timeout)? {
            *self
                .active
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(lock_name.clone())
                .or_insert(0) += 1;
            Ok(MysqlLockLease {
                backend: Arc::clone(&self.backend),
                lock_name,
                released: false,
                active: Arc::clone(&self.active),
            })
        } else {
            Err(MysqlError::LockTimeout(format!(
                "{lock_name} (timeout={}s)",
                self.lock_timeout.as_secs()
            )))
        }
    }

    /// Releases a lease held for the given isolation key (by composed name).
    pub fn release(&self, key: &SandboxIsolationKey) {
        let lock_name = self.compose_lock_name(key);
        let _ = self.backend.release_lock(&lock_name);
        MysqlLockLease::decrement_active(&self.active, &lock_name);
    }

    /// Whether the guard currently holds a lock for the given isolation key.
    pub fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        let lock_name = self.compose_lock_name(key);
        self.backend.is_held(&lock_name).unwrap_or(false)
    }

    /// Number of active local leases.
    pub fn active_count(&self) -> usize {
        self.active.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

#[async_trait::async_trait]
impl SandboxGuard for MysqlSandboxExecutionGuard {
    /// Acquires the named lock and retains the concrete [`MysqlLockLease`] until
    /// [`SandboxGuard::release`] runs.
    ///
    /// The backend seam is synchronous (`GET_LOCK(name, timeout)` waits *server-side* for up
    /// to `lock_timeout`), so the acquisition runs on a blocking thread: awaiting it directly
    /// would stall a Tokio worker for the whole timeout.
    ///
    /// A timed-out acquisition degrades to a no-op lease, mirroring the JDBC guard's
    /// fallback (the caller then runs unprotected rather than deadlocking).
    async fn try_enter(&self, key: SandboxIsolationKey) -> SandboxLease {
        let this = self.clone();
        let acquire_key = key.clone();
        let acquired = tokio::task::spawn_blocking(move || this.try_enter(&acquire_key)).await;
        match acquired {
            Ok(Ok(lease)) => {
                if let Ok(mut leases) = self.trait_leases.lock() {
                    leases.entry(key.clone()).or_default().push(lease);
                }
                SandboxLease::new(key)
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "[mysql-guard] lock not acquired");
                SandboxLease::noop()
            }
            Err(e) => {
                tracing::error!(error = %e, "[mysql-guard] blocking acquire task failed");
                SandboxLease::noop()
            }
        }
    }

    async fn release(&self, key: &SandboxIsolationKey) {
        let dropped =
            self.trait_leases
                .lock()
                .ok()
                .and_then(|mut leases| match leases.get_mut(key) {
                    Some(stack) => {
                        let lease = stack.pop();
                        if stack.is_empty() {
                            leases.remove(key);
                        }
                        lease
                    }
                    None => None,
                });
        match dropped {
            // Dropping the lease releases the named lock.
            Some(lease) => drop(lease),
            None => MysqlSandboxExecutionGuard::release(self, key),
        }
    }

    fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        MysqlSandboxExecutionGuard::is_held(self, key)
    }

    fn active_count(&self) -> usize {
        MysqlSandboxExecutionGuard::active_count(self)
    }

    /// `GET_LOCK` locks are server-scoped: they serialise across processes.
    fn is_distributed(&self) -> bool {
        true
    }
}

/// Builder for [`MysqlSandboxExecutionGuard`], mirroring `JdbcSandboxExecutionGuard.Builder`.
pub struct MysqlSandboxExecutionGuardBuilder {
    backend: Arc<dyn LockBackend>,
    key_prefix: String,
    lock_timeout: Duration,
}

impl MysqlSandboxExecutionGuardBuilder {
    /// Overrides the lock-name prefix (default: [`DEFAULT_KEY_PREFIX`]).
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = prefix.into();
        self
    }

    /// Overrides the lock acquisition timeout (default: 30s).
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    /// Overrides the lock timeout in seconds.
    pub fn lock_timeout_seconds(self, seconds: u64) -> Self {
        self.lock_timeout(Duration::from_secs(seconds))
    }

    /// Builds the guard.
    pub fn build(self) -> MysqlSandboxExecutionGuard {
        MysqlSandboxExecutionGuard {
            backend: self.backend,
            key_prefix: self.key_prefix,
            lock_timeout: self.lock_timeout,
            active: Arc::new(Mutex::new(HashMap::new())),
            trait_leases: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[cfg(test)]
pub mod testing;

#[cfg(test)]
mod tests {
    use super::testing::InMemoryLockBackend;
    use super::*;

    fn guard() -> MysqlSandboxExecutionGuard {
        MysqlSandboxExecutionGuard::builder(Arc::new(InMemoryLockBackend::default())).build()
    }

    #[test]
    fn compose_lock_name_includes_prefix_and_scope() {
        let g = guard();
        let key = SandboxIsolationKey::new("User", "u1");
        assert_eq!(g.compose_lock_name(&key), "agentscope:sandbox:lock:user:u1");
    }

    #[test]
    fn compose_lock_name_includes_sub_key() {
        let g = guard();
        let key = SandboxIsolationKey::with_sub_key("session", "s1", "agent");
        assert_eq!(
            g.compose_lock_name(&key),
            "agentscope:sandbox:lock:session:s1:agent"
        );
    }

    #[test]
    fn try_enter_and_release() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        assert!(!g.is_held(&key));
        {
            let lease = g.try_enter(&key).expect("acquire");
            assert!(g.is_held(&key));
            assert_eq!(g.active_count(), 1);
            assert!(!lease.is_released());
        }
        // Lease drop releases the lock.
        assert!(!g.is_held(&key));
        assert_eq!(g.active_count(), 0);
    }

    #[test]
    fn contention_times_out() {
        let guard = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let _held = guard.try_enter(&key).expect("first acquire");
        let err = guard.try_enter(&key).unwrap_err();
        assert!(matches!(err, MysqlError::LockTimeout(_)));
    }

    #[test]
    fn explicit_release_is_idempotent() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let mut lease = g.try_enter(&key).expect("acquire");
        lease.release();
        lease.release();
        assert!(lease.is_released());
        assert!(!g.is_held(&key));
    }

    #[test]
    fn release_by_key_frees_lock() {
        let g = guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let lease = g.try_enter(&key).expect("acquire");
        g.release(&key);
        assert!(!g.is_held(&key));
        assert_eq!(g.active_count(), 0);
        drop(lease);
    }

    #[test]
    fn custom_prefix_and_timeout() {
        let g = MysqlSandboxExecutionGuard::builder(Arc::new(InMemoryLockBackend::default()))
            .key_prefix("app:lock:")
            .lock_timeout_seconds(5)
            .build();
        let key = SandboxIsolationKey::new("Session", "s1");
        assert_eq!(g.compose_lock_name(&key), "app:lock:session:s1");
    }

    /// `GET_LOCK` names are server-scoped, so the guard must advertise itself as a
    /// cross-process (distributed) guard.
    #[tokio::test]
    async fn guard_is_distributed() {
        let g = guard();
        assert!(SandboxGuard::is_distributed(&g));
        let object: Arc<dyn SandboxGuard> = Arc::new(guard());
        assert!(object.is_distributed());
    }
}
