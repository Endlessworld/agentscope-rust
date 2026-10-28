//! End-to-end native advisory-lock tests: provider → exclusive session → strategy → guard.
//!
//! Everything is offline. The fake backend emulates the parts of `GET_LOCK` /
//! `pg_try_advisory_lock` that matter for the guard contract: a lock is owned by the session
//! that acquired it, another session cannot take it, and closing the session releases it.
//! The fake can also refuse the first N attempts, which is how the deadline **polling**
//! requirement is asserted (a one-shot implementation fails those cases).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope_extensions_jdbc::backend::{
    in_memory_executor, ColumnValue, JdbcError, JdbcExecutor, JdbcResult, RusqliteExecutor,
    SharedExecutor,
};
use agentscope_extensions_jdbc::dialect::{
    normalize_lock_name, postgres_lock_key, BoundSql, ExecutorNativeLockProvider, NativeLockError,
    NativeLockSession, NativeLockSessionFuture, NativeLockSessionProvider,
    SessionScopedLockSession, MYSQL_MAX_LOCK_NAME_LENGTH, NATIVE_LOCK_POLL_INTERVAL,
};
use agentscope_extensions_jdbc::sandbox::{
    NativeLockFlavor, NativeLockGuard, ProviderBackedGuard, DEFAULT_KEY_PREFIX,
    DEFAULT_LOCK_TIMEOUT_SECONDS,
};
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use agentscope_harness::agent::IsolationScope;

// ---------------------------------------------------------------------------
//  Fake backend: session-scoped lock bookkeeping (no database)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    /// `(session id, lock key)` pairs currently held.
    held: Vec<(usize, String)>,
    /// Keys released through `RELEASE_LOCK` / `pg_advisory_unlock`.
    released: Vec<String>,
    /// Number of acquire attempts per key.
    attempts: Vec<String>,
}

/// Offline stand-in for a MySQL/PostgreSQL server.
#[derive(Default)]
struct FakeBackend {
    inner: Mutex<Inner>,
    opens: AtomicUsize,
    closes: AtomicUsize,
    /// How many upcoming acquire attempts must report `Ok(false)`.
    deny_attempts: AtomicUsize,
    /// Whether [`FakeProvider::open`] should fail.
    open_fails: AtomicBool,
    /// Whether every acquire probe should fail at the session level (`Err`) instead of
    /// reporting the lock as busy.
    probe_fails: AtomicBool,
    /// Whether the same session may acquire the same key twice (PostgreSQL semantics).
    reentrant: AtomicBool,
    next_session_id: AtomicUsize,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn deny_first(&self, attempts: usize) {
        self.deny_attempts.store(attempts, Ordering::SeqCst);
    }

    fn set_open_fails(&self) {
        self.open_fails.store(true, Ordering::SeqCst);
    }

    fn set_probe_fails(&self) {
        self.probe_fails.store(true, Ordering::SeqCst);
    }

    fn set_reentrant(&self) {
        self.reentrant.store(true, Ordering::SeqCst);
    }

    fn attempts(&self) -> Vec<String> {
        self.inner.lock().unwrap().attempts.clone()
    }

    fn attempt_count(&self) -> usize {
        self.inner.lock().unwrap().attempts.len()
    }

    fn released(&self) -> Vec<String> {
        self.inner.lock().unwrap().released.clone()
    }

    fn is_held(&self, key: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .held
            .iter()
            .any(|(_, k)| k == key)
    }

    fn held_count(&self) -> usize {
        self.inner.lock().unwrap().held.len()
    }

    fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    fn closes(&self) -> usize {
        self.closes.load(Ordering::SeqCst)
    }
}

/// One exclusive session; closing it (drop) releases everything it held.
struct FakeSession {
    backend: Arc<FakeBackend>,
    id: usize,
}

impl NativeLockSession for FakeSession {
    fn try_acquire(&self, lock_key: &str, _timeout: Duration) -> Result<bool, NativeLockError> {
        let mut inner = self.backend.inner.lock().unwrap();
        inner.attempts.push(lock_key.to_string());
        if self.backend.probe_fails.load(Ordering::SeqCst) {
            return Err(NativeLockError::Failed(
                "simulated probe failure".to_string(),
            ));
        }
        if self.backend.deny_attempts.load(Ordering::SeqCst) > 0 {
            self.backend.deny_attempts.fetch_sub(1, Ordering::SeqCst);
            return Ok(false);
        }
        let reentrant = self.backend.reentrant.load(Ordering::SeqCst);
        let held_by_session = inner
            .held
            .iter()
            .any(|(sid, k)| *sid == self.id && k == lock_key);
        let held_by_other = inner
            .held
            .iter()
            .any(|(sid, k)| *sid != self.id && k == lock_key);
        if held_by_session {
            return Ok(reentrant);
        }
        if held_by_other {
            return Ok(false);
        }
        inner.held.push((self.id, lock_key.to_string()));
        Ok(true)
    }

    fn release(&self, lock_key: &str) -> Result<(), NativeLockError> {
        let mut inner = self.backend.inner.lock().unwrap();
        inner.released.push(lock_key.to_string());
        if let Some(pos) = inner
            .held
            .iter()
            .position(|(sid, k)| *sid == self.id && k == lock_key)
        {
            inner.held.remove(pos);
        }
        Ok(())
    }

    fn is_held(&self, lock_key: &str) -> Result<bool, NativeLockError> {
        Ok(self
            .backend
            .inner
            .lock()
            .unwrap()
            .held
            .iter()
            .any(|(_, k)| k == lock_key))
    }
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        // Closing a connection releases its session-scoped locks.
        let mut inner = self.backend.inner.lock().unwrap();
        inner.held.retain(|(sid, _)| *sid != self.id);
        drop(inner);
        self.backend.closes.fetch_add(1, Ordering::SeqCst);
    }
}

/// A provider that hands out a fake exclusive session per call.
struct FakeProvider {
    backend: Arc<FakeBackend>,
}

impl FakeProvider {
    fn new(backend: Arc<FakeBackend>) -> Self {
        Self { backend }
    }
}

impl NativeLockSessionProvider for FakeProvider {
    fn open(&self) -> NativeLockSessionFuture<'_> {
        Box::pin(async move {
            if self.backend.open_fails.load(Ordering::SeqCst) {
                return Err(NativeLockError::Failed(
                    "pool exhausted (simulated)".to_string(),
                ));
            }
            let id = self.backend.next_session_id.fetch_add(1, Ordering::SeqCst);
            self.backend.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeSession {
                backend: self.backend.clone(),
                id,
            }) as Box<dyn NativeLockSession>)
        })
    }
}

fn key(scope: IsolationScope, agent: &str) -> SandboxIsolationKey {
    SandboxIsolationKey::resolve(Some(&scope), None, agent).unwrap()
}

// ---------------------------------------------------------------------------
//  Acquire through a provider-opened exclusive session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mysql_guard_acquires_the_normalized_lock_name_through_a_provider() {
    let backend = FakeBackend::new();
    let guard = NativeLockGuard::mysql(FakeProvider::new(backend.clone()));

    // A long key exercises the MySQL 64-character normalization on the real path.
    let isolation_key = key(IsolationScope::Agent, &"a".repeat(200));
    let composed = guard.compose_lock_name(&isolation_key);
    assert_eq!(
        composed,
        format!("{DEFAULT_KEY_PREFIX}agent:{}", "a".repeat(200))
    );
    assert!(composed.chars().count() > MYSQL_MAX_LOCK_NAME_LENGTH);

    let lease = guard.try_enter(isolation_key.clone()).await;
    assert!(lease.is_acquired());
    assert_eq!(backend.opens(), 1);
    let expected = normalize_lock_name(&composed);
    assert_eq!(backend.attempts(), vec![expected.clone()]);
    assert_eq!(expected.chars().count(), MYSQL_MAX_LOCK_NAME_LENGTH);
    assert!(backend.is_held(&expected));
}

#[tokio::test]
async fn postgres_guard_acquires_the_decimal_advisory_key_through_a_provider() {
    let backend = FakeBackend::new();
    let guard = NativeLockGuard::postgres(FakeProvider::new(backend.clone()));

    let isolation_key = key(IsolationScope::Agent, "agent-1");
    let composed = guard.compose_lock_name(&isolation_key);
    let expected = postgres_lock_key(&composed).to_string();

    let lease = guard.try_enter(isolation_key).await;
    assert!(lease.is_acquired());
    assert_eq!(guard.flavor(), NativeLockFlavor::Postgres);
    assert_eq!(backend.attempts(), vec![expected.clone()]);
    assert!(backend.is_held(&expected));
}

// ---------------------------------------------------------------------------
//  Polling (Java: PostgresDialect loops with a 100ms sleep until the deadline)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_attempt_failure_is_retried_until_it_succeeds() {
    let backend = FakeBackend::new();
    backend.deny_first(1);
    let guard =
        NativeLockGuard::postgres(FakeProvider::new(backend.clone())).with_timeout_seconds(5);
    let isolation_key = key(IsolationScope::Agent, "agent-1");
    let lock_key = postgres_lock_key(&guard.compose_lock_name(&isolation_key)).to_string();

    let started = Instant::now();
    let lease = guard.try_enter(isolation_key).await;

    assert!(
        lease.is_acquired(),
        "the second attempt must acquire the lock"
    );
    assert_eq!(backend.attempts(), vec![lock_key.clone(), lock_key.clone()]);
    // The retry waited out one poll interval instead of spinning.
    assert!(
        started.elapsed() >= NATIVE_LOCK_POLL_INTERVAL / 2,
        "elapsed {:?}",
        started.elapsed()
    );
    assert!(backend.is_held(&lock_key));
}

#[tokio::test]
async fn unavailable_lock_polls_until_the_deadline_then_times_out() {
    let backend = FakeBackend::new();
    backend.deny_first(10_000);
    let guard = NativeLockGuard::mysql(FakeProvider::new(backend.clone())).with_timeout_seconds(1);

    let lease = guard.try_enter(key(IsolationScope::Global, "busy")).await;

    assert!(
        !lease.is_acquired(),
        "a busy lock must degrade to a no-op lease"
    );
    // ~1s / 100ms poll interval -> several attempts; a one-shot implementation makes exactly 1.
    assert!(
        backend.attempt_count() >= 5,
        "expected polling, got {} attempt(s)",
        backend.attempt_count()
    );
    // The failed acquisition must not leak the exclusive session.
    assert_eq!(backend.opens(), 1);
    assert_eq!(backend.closes(), 1);
    assert_eq!(backend.held_count(), 0);
}

#[tokio::test]
async fn zero_timeout_makes_a_single_attempt() {
    let backend = FakeBackend::new();
    backend.deny_first(10_000);
    let guard = NativeLockGuard::mysql(FakeProvider::new(backend.clone())).with_timeout_seconds(0);

    let lease = guard.try_enter(key(IsolationScope::Global, "busy")).await;

    assert!(!lease.is_acquired());
    // A deadline that has already expired cannot be polled — one probe, no sleeping.
    assert_eq!(backend.attempt_count(), 1);
    assert_eq!(backend.closes(), 1);
}

// ---------------------------------------------------------------------------
//  Release / drop / re-acquire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn releasing_the_lease_releases_the_lock() {
    let backend = FakeBackend::new();
    let guard = NativeLockGuard::mysql(FakeProvider::new(backend.clone()));
    let isolation_key = key(IsolationScope::Agent, "agent-1");
    let lock_key = guard.compose_lock_name(&isolation_key);

    let mut lease = guard.try_enter(isolation_key).await;
    assert!(lease.is_acquired());
    assert!(backend.is_held(&lock_key));

    lease.release();
    assert!(!backend.is_held(&lock_key));
    assert_eq!(backend.released(), vec![lock_key.clone()]);
    assert_eq!(
        backend.closes(),
        0,
        "the session closes when the lease drops"
    );

    // Idempotent: a second release issues no second RELEASE_LOCK.
    lease.release();
    assert_eq!(backend.released(), vec![lock_key]);
}

#[tokio::test]
async fn dropping_the_lease_releases_the_lock_and_closes_the_session() {
    let backend = FakeBackend::new();
    let guard = NativeLockGuard::postgres(FakeProvider::new(backend.clone()));
    let isolation_key = key(IsolationScope::Agent, "agent-1");
    let lock_key = postgres_lock_key(&guard.compose_lock_name(&isolation_key)).to_string();

    {
        let lease = guard.try_enter(isolation_key).await;
        assert!(lease.is_acquired());
        assert!(backend.is_held(&lock_key));
    }

    assert!(
        !backend.is_held(&lock_key),
        "dropping the lease releases the lock"
    );
    assert_eq!(backend.released(), vec![lock_key]);
    assert_eq!(
        backend.closes(),
        1,
        "dropping the lease closes the exclusive session"
    );
    assert_eq!(backend.held_count(), 0);
}

#[tokio::test]
async fn acquire_release_acquire_succeeds_through_the_guard() {
    let backend = FakeBackend::new();
    let guard = NativeLockGuard::mysql(FakeProvider::new(backend.clone()));
    let isolation_key = key(IsolationScope::Agent, "agent-1");

    for round in 0..3 {
        let lease = guard.try_enter(isolation_key.clone()).await;
        assert!(lease.is_acquired(), "round {round} must acquire");
        assert!(backend.is_held(&guard.compose_lock_name(&isolation_key)));
        drop(lease);
        assert!(!backend.is_held(&guard.compose_lock_name(&isolation_key)));
    }
    assert_eq!(backend.opens(), 3, "one exclusive session per lease");
    assert_eq!(backend.closes(), 3);
}

#[tokio::test]
async fn a_second_guard_cannot_hold_the_same_lock() {
    // Mutual exclusion is the whole point of the exclusive-session contract: a shared
    // connection (PostgreSQL re-entrant!) would let this succeed.
    let backend = FakeBackend::new();
    let first =
        NativeLockGuard::postgres(FakeProvider::new(backend.clone())).with_timeout_seconds(0);
    let second =
        NativeLockGuard::postgres(FakeProvider::new(backend.clone())).with_timeout_seconds(0);
    let isolation_key = key(IsolationScope::Agent, "agent-1");

    let held = first.try_enter(isolation_key.clone()).await;
    assert!(held.is_acquired());

    let blocked = second.try_enter(isolation_key.clone()).await;
    assert!(
        !blocked.is_acquired(),
        "the lock is held by another session"
    );

    drop(held);
    let third = second.try_enter(isolation_key).await;
    assert!(
        third.is_acquired(),
        "once released, another session may acquire"
    );
}

// ---------------------------------------------------------------------------
//  SessionScopedLockSession (PostgreSQL same-session re-entrancy)
// ---------------------------------------------------------------------------

#[test]
fn session_scoped_view_forwards_and_dedups_reentrant_acquires() {
    let backend = FakeBackend::new();
    backend.set_reentrant();
    let session = FakeSession {
        backend: backend.clone(),
        id: 0,
    };
    let scoped = SessionScopedLockSession::new(Box::new(session));

    // PostgreSQL advisory locks are re-entrant on the same session.
    assert!(scoped.try_acquire("k", Duration::ZERO).unwrap());
    assert!(scoped.try_acquire("k", Duration::ZERO).unwrap());
    assert_eq!(scoped.held_keys(), vec!["k".to_string()]);
    assert!(scoped.is_held("k").unwrap());

    scoped.release("k").unwrap();
    assert!(scoped.held_keys().is_empty());
    assert!(!backend.is_held("k"));
    assert_eq!(backend.released(), vec!["k".to_string()]);
}

#[test]
fn dropping_the_session_view_releases_what_it_still_holds() {
    let backend = FakeBackend::new();
    let session = FakeSession {
        backend: backend.clone(),
        id: 7,
    };
    {
        let scoped = SessionScopedLockSession::new(Box::new(session));
        assert!(scoped.try_acquire("a", Duration::ZERO).unwrap());
        assert!(scoped.try_acquire("b", Duration::ZERO).unwrap());
        assert_eq!(scoped.held_keys().len(), 2);
        assert!(backend.is_held("a") && backend.is_held("b"));
    }
    // Explicit releases first (deterministic), then the connection close on drop.
    assert_eq!(backend.released(), vec!["a".to_string(), "b".to_string()]);
    assert_eq!(backend.held_count(), 0);
    assert_eq!(backend.closes(), 1);
}

// ---------------------------------------------------------------------------
//  Backend seam: JdbcExecutor::open_native_lock_session
// ---------------------------------------------------------------------------

#[test]
fn rusqlite_executor_reports_unsupported_native_lock_sessions() {
    let executor = RusqliteExecutor::in_memory().expect("in-memory sqlite");
    match executor.open_native_lock_session() {
        Err(JdbcError::Unsupported(message)) => {
            assert!(message.contains("SQLite"), "{message}");
            assert!(message.contains("advisory"), "{message}");
            assert!(message.contains("MySQL/PostgreSQL"), "{message}");
        }
        Err(other) => panic!("expected Unsupported, got {other:?}"),
        Ok(_) => panic!("SQLite must not hand out a native advisory-lock session"),
    }
}

/// An executor that keeps the trait's default [`JdbcExecutor::open_native_lock_session`].
#[derive(Debug)]
struct PlainExecutor;

impl JdbcExecutor for PlainExecutor {
    fn execute(&self, _bound: &BoundSql) -> JdbcResult<usize> {
        Err(JdbcError::Unsupported("plain executor".to_string()))
    }

    fn query(&self, _bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        Err(JdbcError::Unsupported("plain executor".to_string()))
    }
}

#[test]
fn default_executor_reports_unsupported_with_its_own_type_name() {
    match PlainExecutor.open_native_lock_session() {
        Err(JdbcError::Unsupported(message)) => {
            assert!(message.contains("PlainExecutor"), "{message}");
            assert!(
                message.contains("exclusive native advisory-lock sessions"),
                "{message}"
            );
        }
        Err(other) => panic!("expected Unsupported, got {other:?}"),
        Ok(_) => panic!("the default must not hand out a native lock session"),
    }
}

#[tokio::test]
async fn executor_backed_provider_turns_unsupported_into_a_failed_session() {
    let executor: SharedExecutor = in_memory_executor().expect("in-memory sqlite");
    let provider = ExecutorNativeLockProvider::new(executor);
    match provider.open().await {
        Err(NativeLockError::Failed(message)) => {
            assert!(message.contains("unsupported"), "{message}");
            assert!(message.contains("SQLite"), "{message}");
        }
        Ok(_) => panic!("SQLite must not hand out a native lock session"),
        Err(other) => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn guard_over_a_sqlite_executor_degrades_to_a_noop_lease() {
    let executor: SharedExecutor = in_memory_executor().expect("in-memory sqlite");
    let guard = NativeLockGuard::from_executor(executor, NativeLockFlavor::Mysql);

    let lease = guard.try_enter(key(IsolationScope::Agent, "agent-1")).await;
    assert!(!lease.is_acquired());
}

// ---------------------------------------------------------------------------
//  Provider failure / naming parity with JdbcSandboxExecutionGuard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_open_failure_returns_a_noop_lease() {
    let backend = FakeBackend::new();
    backend.set_open_fails();
    let guard = NativeLockGuard::postgres(FakeProvider::new(backend.clone()));

    let lease = guard.try_enter(key(IsolationScope::Agent, "agent-1")).await;
    assert!(!lease.is_acquired());
    assert_eq!(backend.opens(), 0, "no session was opened");
    assert_eq!(backend.closes(), 0, "nothing to close");
}

// ---------------------------------------------------------------------------
//  Backend failures are errors, not "unlocked" (M3)
// ---------------------------------------------------------------------------

/// A provider that cannot open a session is a **backend failure**: the checked acquisition
/// must report it and hand back no lease at all.
#[tokio::test]
async fn provider_open_failure_is_reported_as_an_error() {
    let backend = FakeBackend::new();
    backend.set_open_fails();
    let guard = NativeLockGuard::postgres(FakeProvider::new(backend.clone()));

    match guard
        .try_enter_checked(key(IsolationScope::Agent, "agent-1"))
        .await
    {
        Err(NativeLockError::Failed(message)) => {
            assert!(message.contains("pool exhausted"), "{message}");
        }
        Ok(lease) => panic!(
            "a backend failure must not degrade to a lease (acquired={})",
            lease.is_acquired()
        ),
        Err(other) => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(backend.opens(), 0);
}

/// A session whose probe always errors must surface as `Err` after exactly one attempt: the
/// retry loop must not grind a broken backend down into a timeout/no-op lease.
#[tokio::test]
async fn always_failing_probe_is_reported_as_an_error() {
    let backend = FakeBackend::new();
    backend.set_probe_fails();
    let guard = NativeLockGuard::mysql(FakeProvider::new(backend.clone())).with_timeout_seconds(5);

    match guard
        .try_enter_checked(key(IsolationScope::Global, "broken"))
        .await
    {
        Err(NativeLockError::Failed(message)) => {
            assert!(message.contains("simulated probe failure"), "{message}");
        }
        Ok(lease) => panic!(
            "a failing lock backend must not degrade to a lease (acquired={})",
            lease.is_acquired()
        ),
        Err(other) => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        backend.attempt_count(),
        1,
        "a failed probe is not retried: only a deadline expiry degrades"
    );
    assert_eq!(backend.closes(), 1, "the exclusive session is closed");
    assert_eq!(backend.held_count(), 0, "no lock was leaked");
}

/// "Deny N times, then succeed": polling must still work through the checked path.
#[tokio::test]
async fn checked_acquisition_polls_until_the_lock_is_free() {
    let backend = FakeBackend::new();
    backend.deny_first(2);
    let guard =
        NativeLockGuard::postgres(FakeProvider::new(backend.clone())).with_timeout_seconds(5);

    let lease = guard
        .try_enter_checked(key(IsolationScope::Agent, "agent-1"))
        .await
        .expect("third attempt acquires");
    assert!(lease.is_acquired());
    assert_eq!(backend.attempt_count(), 3);
}

#[test]
fn guard_composes_the_same_lock_names_as_the_jdbc_guard() {
    use agentscope_extensions_jdbc::sandbox::JdbcSandboxExecutionGuard;

    let backend = FakeBackend::new();
    let native = NativeLockGuard::mysql(FakeProvider::new(backend));
    let portable = JdbcSandboxExecutionGuard::new(Arc::new(
        agentscope_extensions_jdbc::sandbox::TableLockStrategy::new(
            in_memory_executor().unwrap(),
            Arc::new(agentscope_extensions_jdbc::dialect::SqliteDialect::default()),
        )
        .unwrap(),
    ));

    let isolation_key = key(IsolationScope::Agent, "agent-1");
    assert_eq!(
        native.compose_lock_name(&isolation_key),
        portable.compose_lock_name(&isolation_key)
    );
    assert_eq!(native.lock_timeout_seconds(), DEFAULT_LOCK_TIMEOUT_SECONDS);

    let custom = NativeLockGuard::postgres(FakeProvider::new(FakeBackend::new()))
        .with_key_prefix("x:")
        .expect("non-blank prefix");
    assert_eq!(custom.compose_lock_name(&isolation_key), "x:agent:agent-1");
    assert!(custom.with_key_prefix("  ").is_err());
}

#[test]
fn provider_backed_guard_alias_is_the_same_type() {
    let guard: ProviderBackedGuard<FakeProvider> = NativeLockGuard::new(
        FakeProvider::new(FakeBackend::new()),
        NativeLockFlavor::Mysql,
    );
    assert_eq!(guard.flavor(), NativeLockFlavor::Mysql);
    assert_eq!(guard.provider().backend.opens(), 0);
}
