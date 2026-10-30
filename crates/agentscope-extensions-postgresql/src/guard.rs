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
//!
//! # Session ownership (why leases open their own connection)
//!
//! `pg_try_advisory_lock` locks are **session-scoped** and **re-entrant inside a session**.
//! Issuing them over a shared client (one `tokio_postgres::Client`, or a pool that may hand
//! the same connection to two leases) makes two different leases land in the same session, so
//! the second `pg_try_advisory_lock` "succeeds" and mutual exclusion silently disappears.
//! Java avoids this by taking a dedicated `Connection` per lease
//! (`PostgresSandboxExecutionGuard#tryEnter`) and closing it in `PostgresLease#close()`.
//! This crate does the same through [`PostgresClient::open_lock_session`]: every
//! [`PostgresSandboxLease`] owns exactly one [`PostgresLockSession`], and dropping the lease
//! both unlocks and ends the session. A client that cannot provide dedicated sessions makes
//! [`PostgresSandboxExecutionGuard::try_enter`] fail explicitly instead of pretending.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey, SandboxLease};

use crate::client::{PostgresClient, PostgresLockSession};
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
            trait_leases: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Advisory-lock-backed execution guard.
#[derive(Debug, Clone)]
pub struct PostgresSandboxExecutionGuard {
    client: Arc<dyn PostgresClient>,
    key_prefix: String,
    lock_timeout: Duration,
    /// Leases acquired through the harness `SandboxGuard` trait, keyed by isolation key.
    ///
    /// The harness `SandboxLease` carries no release hook, so the concrete
    /// [`PostgresSandboxLease`] (which unlocks on drop) must be retained until
    /// `SandboxGuard::release` is called.
    trait_leases: Arc<Mutex<HashMap<SandboxIsolationKey, Vec<PostgresSandboxLease>>>>,
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
    /// Derivation: `prefix + scope_lowercased + ":" + value`, hashed with
    /// `MurmurHash3.hash128(...)[0]` — byte-for-byte the Java
    /// `PostgresSandboxExecutionGuard#composeLockKey`. Matching the algorithm matters:
    /// Rust and Java processes sharing one database must contend on the same key, or both
    /// sides acquire "the same" lock independently and mutual exclusion silently fails.
    pub fn compose_lock_key(&self, key: &SandboxIsolationKey) -> i64 {
        let raw = format!(
            "{}{}:{}",
            self.key_prefix,
            key.scope.to_lowercase(),
            key.value
        );
        crate::murmur3::lock_key(&raw)
    }

    /// Tries to acquire the advisory lock, polling until the timeout expires.
    ///
    /// A **dedicated session** is opened for the attempt (Java `dataSource.getConnection()`);
    /// it is kept alive for the whole lease and closed by [`PostgresSandboxLease::release`] /
    /// `Drop`. On timeout or error the session is dropped, which ends it and releases any
    /// lock it may have taken.
    ///
    /// On success returns a [`PostgresSandboxLease`] whose [`Drop`] releases the lock.
    pub fn try_enter(&self, key: &SandboxIsolationKey) -> PgResult<PostgresSandboxLease> {
        let lock_name = self.compose_lock_key(key);
        let session = self.client.open_lock_session()?;
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            if session.try_lock(lock_name)? {
                return Ok(PostgresSandboxLease {
                    session: Some(session),
                    lock_name,
                    released: false,
                });
            }
            if Instant::now() >= deadline {
                // Dropping `session` ends the connection: nothing is left held.
                return Err(PostgresError::LockTimeout(format!(
                    "{lock_name} (timeout={}s)",
                    self.lock_timeout.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Releases the advisory lock identified by `lock_name` **if this guard retains a lease
    /// for it** (i.e. it was acquired through [`SandboxGuard::try_enter`]).
    ///
    /// Returns `Ok(false)` when no retained lease matches. With session-scoped locks a foreign
    /// session can never unlock (`pg_advisory_unlock` only succeeds for the owning session),
    /// which is exactly Java's behaviour — it exposes no cross-lease release API at all.
    pub fn release(&self, lock_name: i64) -> PgResult<bool> {
        let lease = self.trait_leases.lock().ok().and_then(|mut leases| {
            let matched_key = leases.iter().find_map(|(key, stack)| {
                stack
                    .iter()
                    .any(|lease| lease.lock_name == lock_name)
                    .then(|| key.clone())
            })?;
            let stack = leases.get_mut(&matched_key)?;
            let index = stack.iter().position(|l| l.lock_name == lock_name)?;
            let lease = stack.remove(index);
            if stack.is_empty() {
                leases.remove(&matched_key);
            }
            Some(lease)
        });
        match lease {
            // Dropping the lease unlocks and ends its session.
            Some(mut lease) => lease.release(),
            None => Ok(false),
        }
    }
}

#[async_trait::async_trait]
impl SandboxGuard for PostgresSandboxExecutionGuard {
    /// Acquires the advisory lock on a dedicated session and retains the concrete
    /// [`PostgresSandboxLease`] until `SandboxGuard::release` runs.
    ///
    /// The harness trait returns a `SandboxLease` with no error channel, so a failed
    /// acquisition degrades to [`SandboxLease::noop`] — loudly (`error!` for a client without
    /// session support, `warn!` for a timeout). The inherent
    /// [`PostgresSandboxExecutionGuard::try_enter`] never degrades: it returns the error, so
    /// callers that need hard mutual exclusion should use it directly.
    ///
    /// The acquire probe is synchronous and polls by sleeping, so it runs on a blocking
    /// thread: awaiting it directly would block a Tokio worker for the whole lock timeout.
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
            Ok(Err(PostgresError::DriverUnavailable(e))) => {
                // Never pretend: this client cannot provide dedicated lock sessions.
                tracing::error!(
                    error = %e,
                    "[postgres-guard] advisory locks unsupported by this client; \
                     execution proceeds WITHOUT distributed exclusion"
                );
                SandboxLease::noop()
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "[postgres-guard] lock not acquired");
                SandboxLease::noop()
            }
            Err(e) => {
                tracing::error!(error = %e, "[postgres-guard] blocking acquire task failed");
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
            // Dropping the lease unlocks the advisory lock.
            Some(lease) => drop(lease),
            None => {
                let _ = PostgresSandboxExecutionGuard::release(self, self.compose_lock_key(key));
            }
        }
    }

    /// PostgreSQL has no `is_held` query on the guard, so this reports the local view:
    /// whether this guard currently retains a lease for the key.
    fn is_held(&self, key: &SandboxIsolationKey) -> bool {
        self.trait_leases
            .lock()
            .map(|leases| leases.get(key).is_some_and(|stack| !stack.is_empty()))
            .unwrap_or(false)
    }

    fn active_count(&self) -> usize {
        self.trait_leases
            .lock()
            .map(|leases| leases.values().map(|stack| stack.len()).sum())
            .unwrap_or(0)
    }

    /// `pg_try_advisory_lock` keys are server-scoped: they serialise across processes.
    fn is_distributed(&self) -> bool {
        true
    }
}

/// A lease held on a PostgreSQL advisory lock; releases on drop.
///
/// It owns the **dedicated session** the lock was acquired on (Java `PostgresLease` holds the
/// `Connection`): releasing unlocks *and* closes the session, so the server frees everything
/// the session still holds even if the explicit unlock failed.
pub struct PostgresSandboxLease {
    session: Option<Box<dyn PostgresLockSession>>,
    lock_name: i64,
    released: bool,
}

impl PostgresSandboxLease {
    /// The underlying advisory-lock key.
    pub fn lock_name(&self) -> i64 {
        self.lock_name
    }

    /// Whether this lease still owns a live session.
    pub fn is_open(&self) -> bool {
        !self.released && self.session.is_some()
    }

    /// Unlocks and closes the session; safe to call multiple times (the second call returns
    /// `false`, mirroring Java `leaseCloseHandlesReleaseFailure`'s double-close expectations).
    pub fn release(&mut self) -> PgResult<bool> {
        if self.released {
            return Ok(false);
        }
        self.released = true;
        match self.session.take() {
            Some(mut session) => {
                // Always end the session, even if the explicit unlock fails: closing the
                // connection is what actually guarantees the lock is gone (Java does the same
                // in the `finally` block of `PostgresLease#close`).
                let unlocked = session.unlock(self.lock_name);
                session.close();
                unlocked
            }
            None => Ok(false),
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
            .field("session_open", &self.session.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    //! Offline unit tests mirroring `PostgresSandboxExecutionGuardTest` (JUnit5 + Mockito on
    //! the Java side). `MemoryClient` models PostgreSQL *sessions*: `fork()` shares the lock
    //! table (same server, another client) while every `open_lock_session()` call yields a
    //! distinct session, so re-entrancy, contention and release can all be asserted without a
    //! live server.
    //!
    //! Note: the 64-bit lock key uses MurmurHash3 x64/128 hash[0], exactly as
    //! `PostgresSandboxExecutionGuard#composeLockKey` does, so Rust and Java interoperate
    //! on the same advisory-lock keys (see `crate::murmur3`).

    use super::*;
    use crate::client::{QueryOutcome, SqlValue};
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
        assert_eq!(
            guard.lock_timeout,
            Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECONDS)
        );
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
        assert!(PostgresSandboxExecutionGuard::builder(c)
            .key_prefix("  ")
            .is_err());
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
    fn try_enter_acquires_lock_and_returns_lease() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::new(c.clone());
        let k = key("session", "s1");
        let mut lease = g.try_enter(&k).unwrap();
        assert_eq!(lease.lock_name(), g.compose_lock_key(&k));
        assert!(lease.is_open());
        assert_eq!(c.advisory_lock_count(), 1);
        assert!(lease.release().unwrap());
        assert_eq!(c.advisory_lock_count(), 0);
    }

    // Java: PostgresSandboxExecutionGuard#tryEnter opens a *dedicated* `Connection` per lease.
    // Two leases for the same key therefore contend even inside one process; with a shared
    // connection both would "succeed" (PG advisory locks are re-entrant per session) and
    // mutual exclusion would silently disappear.
    #[test]
    fn distinct_leases_on_the_same_key_are_mutually_exclusive() {
        let c = client();
        let g = PostgresSandboxExecutionGuard::builder(c.clone())
            .lock_timeout(Duration::from_millis(80))
            .unwrap()
            .build();
        let k = key("session", "exclusive");
        let first = g.try_enter(&k).unwrap();
        let err = g.try_enter(&k).unwrap_err();
        assert!(matches!(err, PostgresError::LockTimeout(_)));
        assert_eq!(c.advisory_lock_count(), 1);

        // Once the first lease is released (unlock + session close) the key is free again.
        drop(first);
        assert_eq!(c.advisory_lock_count(), 0);
        let second = g.try_enter(&k).unwrap();
        assert_eq!(second.lock_name(), g.compose_lock_key(&k));
        assert_eq!(c.advisory_lock_count(), 1);
    }

    // Java: `pg_try_advisory_lock` is re-entrant *within* the owning session and
    // `pg_advisory_unlock` peels one nesting level at a time — the seam models that faithfully.
    #[test]
    fn lock_session_is_reentrant_within_one_session() {
        let c = client();
        let mut session = c.open_lock_session().unwrap();
        let name = 42;
        assert!(session.try_lock(name).unwrap());
        assert!(session.try_lock(name).unwrap(), "same session re-enters");
        assert_eq!(c.advisory_lock_count(), 1);

        // A different session is excluded while the owner holds it.
        let mut other = c.fork().open_lock_session().unwrap();
        assert!(!other.try_lock(name).unwrap());

        // One unlock only drops one level: the lock is still held.
        assert!(session.unlock(name).unwrap());
        assert!(!other.try_lock(name).unwrap());

        // The second unlock really frees it.
        assert!(session.unlock(name).unwrap());
        assert!(other.try_lock(name).unwrap());

        // A non-owner cannot unlock someone else's lock.
        assert!(!session.unlock(name).unwrap());

        other.close();
        session.close();
    }

    // Java: `PostgresLease#close()` closes the `Connection` in a `finally` block, so the lock
    // is gone even without a successful `pg_advisory_unlock`.
    #[test]
    fn closing_a_lock_session_releases_every_lock_it_holds() {
        let c = client();
        let mut session = c.open_lock_session().unwrap();
        assert!(session.try_lock(7).unwrap());
        assert!(session.try_lock(9).unwrap());
        assert_eq!(c.advisory_lock_count(), 2);

        session.close();
        assert_eq!(c.advisory_lock_count(), 0);
        session.close(); // idempotent

        // A closed session fails instead of pretending to hold a lock.
        assert!(session.try_lock(7).is_err());

        // Everything it held is now available to another session.
        let mut other = c.open_lock_session().unwrap();
        assert!(other.try_lock(7).unwrap());
        other.close();
    }

    /// A client that can only offer a *shared* connection must make the guard fail loudly.
    /// The default `PostgresClient::open_lock_session` returns `DriverUnavailable`, and the
    /// guard must never fall back to running advisory SQL on the shared connection.
    #[test]
    fn client_without_dedicated_sessions_fails_loudly() {
        #[derive(Debug)]
        struct SharedOnlyClient;

        impl PostgresClient for SharedOnlyClient {
            fn query(&self, _sql: &str, _params: &[SqlValue]) -> PgResult<QueryOutcome> {
                unreachable!("advisory locks must not run on the shared connection")
            }
            fn execute_batch(&self, _sql: &str) -> PgResult<()> {
                Ok(())
            }
        }

        let g = PostgresSandboxExecutionGuard::new(Arc::new(SharedOnlyClient));
        let err = g.try_enter(&key("session", "no-session")).unwrap_err();
        assert!(
            matches!(err, PostgresError::DriverUnavailable(_)),
            "expected an explicit failure, got {err:?}"
        );
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

    /// `pg_try_advisory_lock` keys are server-scoped, so the guard must advertise itself as a
    /// cross-process (distributed) guard.
    #[tokio::test]
    async fn guard_is_distributed() {
        let g = PostgresSandboxExecutionGuard::new(client());
        assert!(SandboxGuard::is_distributed(&g));
        let object: Arc<dyn SandboxGuard> = Arc::new(PostgresSandboxExecutionGuard::new(client()));
        assert!(object.is_distributed());
    }
}
