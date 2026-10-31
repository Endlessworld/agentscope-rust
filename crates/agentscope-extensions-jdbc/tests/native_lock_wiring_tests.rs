//! Host-assembly tests for the native advisory-lock guard.
//!
//! These tests exercise the **production wiring**, not an internal detail:
//! `JdbcDistributedStore::with_native_lock_guard(...)` → `to_harness_store()` →
//! `Arc<dyn SandboxGuard>` → `try_enter` / `release` → `ExecutorNativeLockProvider` →
//! `JdbcExecutor::open_native_lock_session` → advisory-lock SQL. Everything is offline: the
//! executor/session fake stands in for MySQL/PostgreSQL.
//!
//! Java counterparts: `JdbcSandboxExecutionGuard` + `MysqlDialect.tryEnter` /
//! `PostgresDialect.tryEnter` (one `Connection` per lease, `close()` releases).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_extensions_jdbc::backend::{
    in_memory_executor, ColumnValue, JdbcError, JdbcExecutor, JdbcResult, SharedExecutor,
};
use agentscope_extensions_jdbc::dialect::{
    detect_dialect, postgres_lock_key, BoundSql, NativeLockError, NativeLockSession,
};
use agentscope_extensions_jdbc::sandbox::{
    compose_lock_name, JdbcSandboxExecutionGuard, NativeLockFlavor, DEFAULT_KEY_PREFIX,
};
use agentscope_extensions_jdbc::JdbcDistributedStore;
use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey};
use agentscope_harness::agent::IsolationScope;

// ---------------------------------------------------------------------------
//  Fake backend: session-scoped advisory locks (no database)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct LockBackend {
    /// `(session id, lock key)` pairs currently held.
    held: Mutex<Vec<(usize, String)>>,
    attempts: Mutex<Vec<String>>,
    released: Mutex<Vec<String>>,
    opens: AtomicUsize,
    closes: AtomicUsize,
    busy: AtomicBool,
    open_fails: AtomicBool,
}

impl LockBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn attempts(&self) -> Vec<String> {
        self.attempts.lock().unwrap().clone()
    }

    fn released(&self) -> Vec<String> {
        self.released.lock().unwrap().clone()
    }

    fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    fn closes(&self) -> usize {
        self.closes.load(Ordering::SeqCst)
    }

    fn is_held(&self, key: &str) -> bool {
        self.held.lock().unwrap().iter().any(|(_, held)| held == key)
    }

    fn set_busy(&self) {
        self.busy.store(true, Ordering::SeqCst);
    }

    fn set_open_fails(&self) {
        self.open_fails.store(true, Ordering::SeqCst);
    }
}

/// One exclusive session; closing it releases everything it still holds.
struct FakeSession {
    backend: Arc<LockBackend>,
    id: usize,
}

impl NativeLockSession for FakeSession {
    fn try_acquire(&self, lock_key: &str, _timeout: Duration) -> Result<bool, NativeLockError> {
        self.backend
            .attempts
            .lock()
            .unwrap()
            .push(lock_key.to_string());
        if self.backend.busy.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let mut held = self.backend.held.lock().unwrap();
        if held.iter().any(|(_, key)| key == lock_key) {
            return Ok(false);
        }
        held.push((self.id, lock_key.to_string()));
        Ok(true)
    }

    fn release(&self, lock_key: &str) -> Result<(), NativeLockError> {
        self.backend
            .released
            .lock()
            .unwrap()
            .push(lock_key.to_string());
        self.backend
            .held
            .lock()
            .unwrap()
            .retain(|(_, key)| key != lock_key);
        Ok(())
    }

    fn is_held(&self, lock_key: &str) -> Result<bool, NativeLockError> {
        Ok(self.backend.is_held(lock_key))
    }
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        // Closing the connection releases every advisory lock it still holds.
        self.backend.closes.fetch_add(1, Ordering::SeqCst);
        self.backend
            .held
            .lock()
            .unwrap()
            .retain(|(session, _)| *session != self.id);
    }
}

/// An executor that can open native lock sessions but implements no SQL (schema creation is
/// skipped by the tests, so nothing else is ever asked of it).
#[derive(Debug)]
struct FakeExecutor {
    backend: Arc<LockBackend>,
}

impl JdbcExecutor for FakeExecutor {
    fn execute(&self, _bound: &BoundSql) -> JdbcResult<usize> {
        Ok(1)
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        // The store probes for its tables on construction; answer "present" so the
        // schema verification passes without a real database.
        let sql = bound.sql.to_ascii_uppercase();
        if sql.contains("INFORMATION_SCHEMA") || sql.contains("WHERE 1 = 0") {
            return Ok(vec![vec![ColumnValue::Int(1)]]);
        }
        Ok(Vec::new())
    }

    fn open_native_lock_session(&self) -> JdbcResult<Box<dyn NativeLockSession>> {
        if self.backend.open_fails.load(Ordering::SeqCst) {
            return Err(JdbcError::Unsupported(
                "this backend has no advisory locks".to_string(),
            ));
        }
        let id = self.backend.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeSession {
            backend: self.backend.clone(),
            id,
        }))
    }
}

fn native_guard(
    backend: &Arc<LockBackend>,
    flavor: NativeLockFlavor,
) -> JdbcSandboxExecutionGuard {
    let executor: SharedExecutor = Arc::new(FakeExecutor {
        backend: backend.clone(),
    });
    JdbcSandboxExecutionGuard::with_native_lock(executor, flavor)
}

fn native_store(backend: &Arc<LockBackend>, flavor: NativeLockFlavor) -> JdbcDistributedStore {
    let executor: SharedExecutor = Arc::new(FakeExecutor {
        backend: backend.clone(),
    });
    JdbcDistributedStore::create_with_dialect_and_schema(
        executor,
        detect_dialect("PostgreSQL"),
        // The fake executor implements no SQL, so schema creation/verification is skipped;
        // this test only exercises the guard projection.
        false,
    )
    .expect("the store assembles against the fake executor")
    .with_native_lock_guard(flavor)
}

fn key() -> SandboxIsolationKey {
    SandboxIsolationKey::resolve(Some(&IsolationScope::Global), None, "agent-1")
        .expect("a global isolation key")
}

// ---------------------------------------------------------------------------
//  Guard assembly: acquire / release through a real executor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_guard_acquires_a_native_lock_through_the_executor() {
    let backend = LockBackend::new();
    let guard = native_guard(&backend, NativeLockFlavor::Postgres);
    let isolation = key();
    let expected = postgres_lock_key(&guard.compose_lock_name(&isolation)).to_string();

    let lease = SandboxGuard::try_enter(&guard, isolation.clone())
        .await
        .expect("lease");
    assert!(!lease.is_noop(), "the lease must hold a real advisory lock");
    assert_eq!(backend.opens(), 1);
    assert_eq!(backend.attempts(), vec![expected.clone()]);
    assert!(backend.is_held(&expected));

    SandboxGuard::release(&guard, &isolation).await;
    assert_eq!(backend.released(), vec![expected.clone()]);
    assert!(!backend.is_held(&expected));
    assert_eq!(backend.closes(), 1, "the session is closed on release");
}

#[tokio::test]
async fn every_acquisition_opens_a_new_session() {
    let backend = LockBackend::new();
    let guard = native_guard(&backend, NativeLockFlavor::Postgres);

    for _ in 0..3 {
        let lease = SandboxGuard::try_enter(&guard, key()).await.expect("lease");
        assert!(!lease.is_noop());
        SandboxGuard::release(&guard, &key()).await;
    }

    // Session-scoped locks require one connection per lease (a shared one would let two
    // guards hold the same lock).
    assert_eq!(backend.opens(), 3);
    assert_eq!(backend.closes(), 3);
}

#[tokio::test]
async fn the_mysql_flavor_uses_the_composed_lock_name() {
    let backend = LockBackend::new();
    let guard = native_guard(&backend, NativeLockFlavor::Mysql);
    let isolation = key();
    let composed = guard.compose_lock_name(&isolation);

    let lease = SandboxGuard::try_enter(&guard, isolation.clone())
        .await
        .expect("lease");
    assert!(!lease.is_noop());
    assert_eq!(backend.attempts(), vec![composed.clone()]);
    assert!(composed.starts_with(DEFAULT_KEY_PREFIX));
    SandboxGuard::release(&guard, &isolation).await;
}

// ---------------------------------------------------------------------------
//  Store projection: the harness `sandbox_execution_guard` slot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_distributed_store_projects_a_native_lock_guard() {
    let backend = LockBackend::new();
    let store = native_store(&backend, NativeLockFlavor::Postgres);

    // Same naming as the portable table lock it replaces.
    assert_eq!(
        store
            .native_lock_guard(NativeLockFlavor::Postgres)
            .compose_lock_name(&key()),
        compose_lock_name(DEFAULT_KEY_PREFIX, &key())
    );

    let harness = store.to_harness_store();
    let guard = harness
        .sandbox_execution_guard
        .expect("the harness store carries the execution guard");
    assert!(guard.is_distributed());

    let isolation = key();
    let lease = guard.try_enter(isolation.clone()).await.expect("lease");
    assert!(!lease.is_noop());
    assert_eq!(backend.opens(), 1);

    guard.release(&isolation).await;
    assert_eq!(backend.closes(), 1);
}

#[tokio::test]
async fn a_backend_without_advisory_locks_is_an_error_not_a_noop_lease() {
    let store = JdbcDistributedStore::create(in_memory_executor().unwrap(), "SQLite")
        .expect("sqlite store")
        .with_native_lock_guard(NativeLockFlavor::Mysql);
    let harness = store.to_harness_store();
    let guard = harness
        .sandbox_execution_guard
        .expect("the harness store carries the execution guard");

    // Fail-closed: SQLite has no advisory locks, so the guard cannot claim the isolation
    // key. It must report the failure instead of handing back a no-op lease.
    let error = guard
        .try_enter(key())
        .await
        .expect_err("SQLite has no advisory locks: the guard must not claim the lock");
    assert!(
        format!("{error}").contains("advisory lock"),
        "the backend limitation must be reported: {error}"
    );
}

// ---------------------------------------------------------------------------
//  Failure semantics: a broken backend is not a contended lock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_backend_failure_surfaces_as_an_error_not_a_timeout() {
    let backend = LockBackend::new();
    backend.set_open_fails();
    let guard = native_guard(&backend, NativeLockFlavor::Postgres);

    match guard.try_enter_async(key()).await {
        Err(NativeLockError::Failed(message)) => {
            assert!(message.contains("advisory locks"), "{message}");
        }
        Err(other) => panic!("expected Failed, got {other:?}"),
        Ok(lease) => panic!(
            "a broken lock backend must never look like an acquired lock (noop={})",
            lease.is_acquired()
        ),
    }
}

#[tokio::test]
async fn a_contended_lock_times_out_to_a_noop_lease() {
    let backend = LockBackend::new();
    backend.set_busy();
    let guard = native_guard(&backend, NativeLockFlavor::Mysql).with_timeout_seconds(0);

    let lease = SandboxGuard::try_enter(&guard, key()).await;
    assert!(
        lease.is_err(),
        "an exhausted deadline is a failed acquisition on the harness path"
    );
    // An exhausted deadline makes exactly one attempt (no spinning).
    assert_eq!(backend.attempts().len(), 1);
    assert_eq!(backend.closes(), 1);
}

