//! PostgreSQL advisory-lock-based sandbox execution guard.
//!
//! **Harness note:** unlike the Java side, the harness `SandboxExecutionGuard` is a
//! concrete in-process struct (`agentscope_harness::agent::sandbox::SandboxExecutionGuard`),
//! not a trait. This crate therefore provides a standalone
//! [`PostgresSandboxExecutionGuard`] mirroring the same API surface
//! (`try_enter` / `release`), backed by PostgreSQL advisory locks, so the
//! distributed guard can be used wherever a process-local one would be. See the module
//! gap tracker §4.2 for the harness abstraction follow-up.
//!
//! Mirrors the Java `PostgresSandboxExecutionGuard`: a 64-bit lock key is derived from
//! the isolation key via a hash, acquired with `pg_try_advisory_lock` (polling until the
//! timeout expires) and released with `pg_advisory_unlock`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_harness::agent::sandbox::SandboxIsolationKey;

use crate::client::{Cell, PostgresClient, QueryOutcome, SqlValue};
use crate::error::{PgResult, PostgresError};

/// Default lock-key prefix, matching the Java `DEFAULT_KEY_PREFIX`.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:sandbox:lock:";
/// Default timeout, matching the Java `DEFAULT_LOCK_TIMEOUT_SECONDS` (30 minutes).
pub const DEFAULT_LOCK_TIMEOUT_SECONDS: u64 = 1800;

/// Builder for [`PostgresSandboxExecutionGuard`].
#[derive(Debug, Clone)]
pub struct PostgresSandboxExecutionGuardBuilder {
    client: Arc<dyn PostgresClient>,
    key_prefix: String,
    lock_timeout: Duration,
}

impl PostgresSandboxExecutionGuardBuilder {
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self {
            client,
            key_prefix: DEFAULT_KEY_PREFIX.to_string(),
            lock_timeout: Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECONDS),
        }
    }

    /// Sets the lock-key prefix; a trailing `:` is appended when absent.
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> PgResult<Self> {
        let p = prefix.into();
        if p.trim().is_empty() {
            return Err(PostgresError::InvalidIdentifier(
                "keyPrefix must not be blank".into(),
            ));
        }
        self.key_prefix = if p.ends_with(':') { p } else { format!("{p}:") };
        Ok(self)
    }

    /// Sets the lock acquisition timeout.
    pub fn lock_timeout(mut self, timeout: Duration) -> PgResult<Self> {
        if timeout.is_zero() {
            return Err(PostgresError::Query("timeout must be positive".into()));
        }
        self.lock_timeout = timeout;
        Ok(self)
    }

    pub fn build(self) -> PostgresSandboxExecutionGuard {
        PostgresSandboxExecutionGuard {
            client: self.client,
            key_prefix: self.key_prefix,
            lock_timeout: self.lock_timeout,
        }
    }
}

/// Advisory-lock-backed execution guard.
#[derive(Debug, Clone)]
pub struct PostgresSandboxExecutionGuard {
    client: Arc<dyn PostgresClient>,
    key_prefix: String,
    lock_timeout: Duration,
}

impl PostgresSandboxExecutionGuard {
    /// Creates a builder (mirrors Java `PostgresSandboxExecutionGuard.builder(ds)`).
    pub fn builder(client: Arc<dyn PostgresClient>) -> PostgresSandboxExecutionGuardBuilder {
        PostgresSandboxExecutionGuardBuilder::new(client)
    }

    /// Builds a guard with default settings.
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self::builder(client).build()
    }

    /// Computes the 64-bit advisory-lock key for an isolation key.
    ///
    /// Derivation: `prefix + scope_lowercased + ":" + value`, hashed to `i64`.
    pub fn compose_lock_key(&self, key: &SandboxIsolationKey) -> i64 {
        use std::hash::{Hash, Hasher};
        let raw = format!(
            "{}{}:{}",
            self.key_prefix,
            key.scope.to_lowercase(),
            key.value
        );
        let mut h = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut h);
        h.finish() as i64
    }

    /// Tries to acquire the advisory lock, polling until the timeout expires.
    ///
    /// On success returns a [`PostgresSandboxLease`] whose [`Drop`] releases the lock.
    pub fn try_enter(&self, key: &SandboxIsolationKey) -> PgResult<PostgresSandboxLease> {
        let lock_name = self.compose_lock_key(key);
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            if self.try_lock_once(lock_name)? {
                return Ok(PostgresSandboxLease {
                    client: Arc::clone(&self.client),
                    lock_name,
                    released: false,
                });
            }
            if Instant::now() >= deadline {
                return Err(PostgresError::LockTimeout(format!(
                    "{lock_name} (timeout={}s)",
                    self.lock_timeout.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn try_lock_once(&self, lock_name: i64) -> PgResult<bool> {
        let sql = "SELECT pg_try_advisory_lock($1)";
        let params = [SqlValue::BigInt(lock_name)];
        match self.client.query(sql, &params)? {
            QueryOutcome::Rows(rows) => Ok(rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_bool)
                .unwrap_or(false)),
            QueryOutcome::Affected(_) => Ok(false),
        }
    }

    /// Releases a previously-acquired lock by key.
    pub fn release(&self, lock_name: i64) -> PgResult<bool> {
        let sql = "SELECT pg_advisory_unlock($1)";
        let params = [SqlValue::BigInt(lock_name)];
        match self.client.query(sql, &params)? {
            QueryOutcome::Rows(rows) => Ok(rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_bool)
                .unwrap_or(false)),
            QueryOutcome::Affected(_) => Ok(false),
        }
    }
}

/// A lease held on a PostgreSQL advisory lock; releases on drop.
pub struct PostgresSandboxLease {
    client: Arc<dyn PostgresClient>,
    lock_name: i64,
    released: bool,
}

impl PostgresSandboxLease {
    /// The underlying advisory-lock key.
    pub fn lock_name(&self) -> i64 {
        self.lock_name
    }

    /// Releases the lock; safe to call multiple times.
    pub fn release(&mut self) -> PgResult<bool> {
        if self.released {
            return Ok(false);
        }
        self.released = true;
        let sql = "SELECT pg_advisory_unlock($1)";
        let params = [SqlValue::BigInt(self.lock_name)];
        match self.client.query(sql, &params)? {
            QueryOutcome::Rows(rows) => Ok(rows
                .first()
                .and_then(|r| r.first())
                .and_then(Cell::as_bool)
                .unwrap_or(false)),
            QueryOutcome::Affected(_) => Ok(false),
        }
    }
}

impl Drop for PostgresSandboxLease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl std::fmt::Debug for PostgresSandboxLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresSandboxLease")
            .field("lock_name", &self.lock_name)
            .field("released", &self.released)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    //! Offline unit tests mirroring `PostgresSandboxExecutionGuardTest` (JUnit5 + Mockito on
    //! the Java side). `MemoryClient` models a PG session per client: `fork()` yields a
    //! second "connection" over the same lock table, so re-entrancy, contention and release
    //! can all be asserted without a live server.
    //!
    //! Note: the 64-bit lock key uses `DefaultHasher` where Java uses MurmurHash3, so the
    //! two languages do not interoperate — see the gap tracker §3.3.

    use super::*;
    use crate::memory::MemoryClient;

    fn client() -> Arc<MemoryClient> {
        Arc::new(MemoryClient::new())
    }

    fn key(scope: &str, value: &str) -> SandboxIsolationKey {
        SandboxIsolationKey::new(scope, value)
    }

    // Java: PostgresSandboxExecutionGuardTest#builderWithDefaultsCreatesGuard
    #[test]
    fn builder_defaults_expose_documented_constants() {
        assert_eq!(DEFAULT_KEY_PREFIX, "agentscope:sandbox:lock:");
        assert_eq!(DEFAULT_LOCK_TIMEOUT_SECONDS, 1800);
        let c = client();
        let guard = PostgresSandboxExecutionGuard::new(c);
        assert_eq!(guard.key_prefix, DEFAULT_KEY_PREFIX);
        assert_eq!(guard.lock_timeout, Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECONDS));
    }

    // Java: PostgresSandboxExecutionGuardTest#builderWithCustomKeyPrefix,
    //       #builderWithKeyPrefixAlreadyEndingWithColon
    #[test]
    fn builder_normalises_key_prefix() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::builder(c.clone())
            .key_prefix("custom")
            .unwrap()
            .build();
        assert_eq!(g.key_prefix, "custom:");
        let g2 = PostgresSandboxExecutionGuard::builder(c.clone())
            .key_prefix("custom:")
            .unwrap()
            .build();
        assert_eq!(g2.key_prefix, "custom:");

        // Java: #builderRejectsBlankKeyPrefix
        assert!(PostgresSandboxExecutionGuard::builder(c).key_prefix("  ").is_err());
    }

    // Java: PostgresSandboxExecutionGuardTest#builderWithCustomTimeout,
    //       #builderRejectsNonPositiveTimeout
    #[test]
    fn builder_validates_timeout() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::builder(c.clone())
            .lock_timeout(Duration::from_millis(250))
            .unwrap()
            .build();
        assert_eq!(g.lock_timeout, Duration::from_millis(250));
        assert!(PostgresSandboxExecutionGuard::builder(c)
            .lock_timeout(Duration::ZERO)
            .is_err());
    }

    // Java: composeLockKey — same key is deterministic; scope is lowercased; the prefix
    // participates in the hash.
    #[test]
    fn compose_lock_key_is_deterministic_and_prefix_sensitive() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::new(c.clone());
        let k = key("User", "u1");
        let a = g.compose_lock_key(&k);
        let b = g.compose_lock_key(&key("user", "u1"));
        assert_eq!(a, b, "scope must be lowercased before hashing");
        assert_ne!(a, g.compose_lock_key(&key("user", "u2")));
        assert_ne!(a, g.compose_lock_key(&key("agent", "u1")));

        let other = PostgresSandboxExecutionGuard::builder(c)
            .key_prefix("other")
            .unwrap()
            .build();
        assert_ne!(a, other.compose_lock_key(&k));
    }

    // Java: PostgresSandboxExecutionGuardTest#tryEnterAcquiresLockAndReturnsLease
    #[test]
    fn try_enter_acquires_and_reenters_same_connection() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::new(c.clone());
        let k = key("session", "s1");
        let lease = g.try_enter(&k).unwrap();
        assert_eq!(c.advisory_lock_count(), 1);
        // PG advisory locks are re-entrant per session.
        let lease2 = g.try_enter(&k).unwrap();
        assert_eq!(lease.lock_name(), lease2.lock_name());
        assert_eq!(g.release(lease.lock_name()).unwrap(), true);
        drop(lease);
        drop(lease2);
    }

    // Java: PostgresSandboxExecutionGuardTest#tryEnterTimesOutWhenLockNeverAcquired
    #[test]
    fn try_enter_times_out_when_another_connection_holds_the_lock() {
        let c = client();
        let other = Arc::new(c.fork());
        let k = key("session", "s2");

        // Hold the lock on a different "connection".
        let holder = PostgresSandboxExecutionGuard::new(other.clone());
        let held = holder.try_enter(&k).unwrap();

        let g = PostgresSandboxExecutionGuard::builder(c.clone())
            .lock_timeout(Duration::from_millis(120))
            .unwrap()
            .build();
        let err = g.try_enter(&k).unwrap_err();
        assert!(matches!(err, PostgresError::LockTimeout(_)));

        // After the holder releases, the lock becomes available.
        drop(held);
        let lease = g.try_enter(&k).unwrap();
        assert_eq!(lease.lock_name(), g.compose_lock_key(&k));
    }

    // Java: PostgresSandboxExecutionGuardTest#tryEnterPollsUntilLockAcquired (the loop polls
    // under the timeout; after the holder releases we observe a successful acquisition).
    #[test]
    fn try_enter_succeeds_after_contention_clears() {
        let c = client();
        let other = Arc::new(c.fork());
        let k = key("user", "contended");

        let holder = PostgresSandboxExecutionGuard::new(other);
        let held = holder.try_enter(&k).unwrap();
        assert!(held.released == false);

        let g = PostgresSandboxExecutionGuard::new(c.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(held);
        })
        .join()
        .unwrap();

        let lease = g.try_enter(&k).unwrap();
        // The fork released it; only this connection now holds the lock.
        assert_eq!(c.advisory_lock_count(), 1);
        assert_eq!(lease.lock_name(), g.compose_lock_key(&k));
    }

    // Java: PostgresSandboxExecutionGuardTest#leaseCloseReleasesLockAndClosesConnection
    #[test]
    fn lease_drop_releases_lock() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::new(c.clone());
        let k = key("session", "s3");
        {
            let _lease = g.try_enter(&k).unwrap();
            assert_eq!(c.advisory_lock_count(), 1);
        }
        assert_eq!(c.advisory_lock_count(), 0);
        // A subsequent acquisition succeeds.
        let _lease = g.try_enter(&k).unwrap();
    }

    // Java: PostgresSandboxExecutionGuardTest#leaseCloseHandlesReleaseFailure (double release
    // is a no-op returning false rather than an error).
    #[test]
    fn lease_release_is_idempotent() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::new(c.clone());
        let k = key("session", "s4");
        let mut lease = g.try_enter(&k).unwrap();
        assert!(lease.release().unwrap());
        assert!(!lease.release().unwrap());
        assert_eq!(c.advisory_lock_count(), 0);
    }

    // Java: PostgresSandboxExecutionGuardTest#tryEnterHandlesInnerSQLException (Rust: an
    // unrecognised SQL surface would surface as `PostgresError::Query`; the guard translates
    // lock results, and `release` reports the current lock owner).
    #[test]
    fn release_by_key_reports_owner_mismatch() {
        let c = client();
        let other = Arc::new(c.fork());
        let g = PostgresSandboxExecutionGuard::new(c.clone());
        let k = key("session", "s5");
        let holder = PostgresSandboxExecutionGuard::new(other);
        let held = holder.try_enter(&k).unwrap();
        // A different connection cannot release the holder's lock.
        assert!(!g.release(held.lock_name()).unwrap());
        drop(held);
        assert_eq!(c.advisory_lock_count(), 0);
    }

    // Java: PostgresSandboxExecutionGuardTest#builderRejectsNullDataSource (Rust: the analogue
    // is an invalid isolation-key/prefix, already covered; this asserts distinct keys are
    // independently lockable).
    #[test]
    fn distinct_keys_are_independently_lockable() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::new(c.clone());
        let a = key("session", "a");
        let b = key("session", "b");
        let la = g.try_enter(&a).unwrap();
        let lb = g.try_enter(&b).unwrap();
        assert_ne!(la.lock_name(), lb.lock_name());
        assert_eq!(c.advisory_lock_count(), 2);
    }
}
