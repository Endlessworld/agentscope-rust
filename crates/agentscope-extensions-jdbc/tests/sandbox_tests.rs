//! `JdbcSandboxExecutionGuard` offline tests, mirroring the Java
//! `JdbcSandboxExecutionGuardH2Test` and `JdbcSandboxExecutionGuardSqliteTest`.
//!
//! Backend: in-memory SQLite (`RusqliteExecutor::in_memory()`) with `SqliteDialect` — the Rust
//! analogue of Java's H2/SQLite pair. These cases exercise the portable table lock; the
//! native MySQL/PostgreSQL strategies are covered in `native_lock_tests.rs`.
//!
//! Semantics difference (see §3.4): the Java guard *throws* `InterruptedException` when the lock
//! cannot be acquired before the timeout; the Rust guard returns a no-op lease
//! (`JdbcSandboxLease::noop()`, `is_acquired() == false`) instead, because harness
//! `SandboxLease` has no throwing surface.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use agentscope_extensions_jdbc::dialect::vendor::SqliteDialect;
use agentscope_extensions_jdbc::dialect::AbstractJdbcDialect;
use agentscope_extensions_jdbc::sandbox::{
    JdbcSandboxExecutionGuard, DEFAULT_KEY_PREFIX, DEFAULT_LOCK_TIMEOUT_SECONDS,
};
use agentscope_extensions_jdbc::RusqliteExecutor;
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use agentscope_harness::agent::IsolationScope;

fn new_guard() -> JdbcSandboxExecutionGuard {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    JdbcSandboxExecutionGuard::table_lock(executor, dialect).unwrap()
}

fn new_guard_with_timeout(seconds: u64) -> JdbcSandboxExecutionGuard {
    new_guard().with_timeout_seconds(seconds)
}

fn key(scope: IsolationScope, agent: &str) -> SandboxIsolationKey {
    SandboxIsolationKey::resolve(Some(&scope), None, agent).unwrap()
}

// ---------------------------------------------------------------------------
//  Constants (Java: JdbcSandboxExecutionGuard.Builder.DEFAULT_*)
// ---------------------------------------------------------------------------

#[test]
fn default_constants_match_java() {
    // Java: DEFAULT_KEY_PREFIX="agentscope:sandbox:lock:",
    //       DEFAULT_LOCK_TIMEOUT_SECONDS=1800 (30 minutes)
    assert_eq!(DEFAULT_KEY_PREFIX, "agentscope:sandbox:lock:");
    assert_eq!(DEFAULT_LOCK_TIMEOUT_SECONDS, 1800);
}

#[test]
fn default_guard_uses_java_key_prefix_and_timeout() {
    // Java: JdbcSandboxExecutionGuard.Builder defaults
    let guard = new_guard();
    let composed = guard.compose_lock_name(&key(IsolationScope::Agent, "agent-1"));
    assert_eq!(composed, "agentscope:sandbox:lock:agent:agent-1");
}

// ---------------------------------------------------------------------------
//  Java: JdbcSandboxExecutionGuardH2Test
// ---------------------------------------------------------------------------

#[test]
fn acquire_and_release_lock() {
    // Java: JdbcSandboxExecutionGuardH2Test.acquireAndReleaseLock
    let guard = new_guard();
    let isolation_key = key(IsolationScope::Global, "test-agent");

    let mut lease = guard.try_enter(isolation_key);
    assert!(lease.is_acquired(), "the first acquisition must succeed");
    lease.release();
}

#[test]
fn second_acquire_after_release() {
    // Java: JdbcSandboxExecutionGuardH2Test.secondAcquireAfterRelease
    let guard = new_guard();
    let isolation_key = key(IsolationScope::Agent, "agent-1");

    let mut lease1 = guard.try_enter(isolation_key.clone());
    assert!(lease1.is_acquired());
    lease1.release();

    let mut lease2 = guard.try_enter(isolation_key);
    assert!(
        lease2.is_acquired(),
        "the lock must be re-acquirable after release"
    );
    lease2.release();
}

#[test]
fn lock_is_held_until_released() {
    // Java: JdbcSandboxExecutionGuardH2Test.acquireAndReleaseLock (lock lifetime)
    let guard = new_guard();
    let isolation_key = key(IsolationScope::Agent, "lifetime-agent");

    assert!(!guard.is_held(&isolation_key));
    let mut lease = guard.try_enter(isolation_key.clone());
    assert!(
        guard.is_held(&isolation_key),
        "the lock must be visible while held"
    );
    lease.release();
    assert!(
        !guard.is_held(&isolation_key),
        "the lock must vanish after release"
    );
}

#[test]
fn dropping_the_lease_releases_the_lock() {
    // Java: TableLease.close() deletes the lock row; Rust releases on Drop.
    let guard = new_guard();
    let isolation_key = key(IsolationScope::Agent, "drop-agent");

    {
        let lease = guard.try_enter(isolation_key.clone());
        assert!(lease.is_acquired());
        assert!(guard.is_held(&isolation_key));
    }
    assert!(!guard.is_held(&isolation_key), "Drop must release the lock");
}

#[test]
fn lock_provides_mutual_exclusion_across_threads() {
    // Java: JdbcSandboxExecutionGuardH2Test.lockProvidesMutualExclusion
    let guard = Arc::new(new_guard());
    let isolation_key = key(IsolationScope::Agent, "mutex-agent");
    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let num_threads = 5usize;

    thread::scope(|scope| {
        for _ in 0..num_threads {
            let guard = guard.clone();
            let isolation_key = isolation_key.clone();
            let concurrent = concurrent.clone();
            let max_concurrent = max_concurrent.clone();
            scope.spawn(move || {
                let mut lease = guard.try_enter(isolation_key);
                if lease.is_acquired() {
                    let current = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(current, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(50));
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                }
                lease.release();
            });
        }
    });

    assert_eq!(
        max_concurrent.load(Ordering::SeqCst),
        1,
        "max concurrent holders must be 1"
    );
}

#[test]
fn try_enter_keeps_polling_until_the_deadline_but_never_overshoots_it() {
    // JDBC-08: the retry waits 50/100/200/... ms (capped at 1s) instead of a fixed 50ms, so
    // the deadline still bounds the whole acquisition (no wait past it beyond the cap).
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    let holder = JdbcSandboxExecutionGuard::table_lock(executor.clone(), dialect.clone())
        .unwrap()
        .with_timeout_seconds(0);
    let contender = JdbcSandboxExecutionGuard::table_lock(executor, dialect)
        .unwrap()
        .with_timeout_seconds(1);

    let isolation_key = key(IsolationScope::Agent, "backoff-agent");
    let held = holder.try_enter(isolation_key.clone());
    assert!(held.is_acquired());

    let started = Instant::now();
    let mut timed_out = contender.try_enter(isolation_key);
    let elapsed = started.elapsed();

    assert!(!timed_out.is_acquired(), "a held lock must time out");
    assert!(
        elapsed >= Duration::from_secs(1),
        "the contender kept retrying until the deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2) + Duration::from_millis(500),
        "the backoff must be clamped to the remaining budget: {elapsed:?}"
    );
    timed_out.release();
    drop(held);
}

#[test]
fn try_enter_times_out_when_lock_is_held() {
    // Java: JdbcSandboxExecutionGuardH2Test.tryEnterTimesOutWhenHeld
    // (Java throws InterruptedException; Rust returns a no-op lease — see module docs.)
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    // A second lock table would be a different table; share executor+dialect so both guards
    // contend on the same lock table.
    let holder = JdbcSandboxExecutionGuard::table_lock(executor.clone(), dialect.clone())
        .unwrap()
        .with_timeout_seconds(5);
    let contender = JdbcSandboxExecutionGuard::table_lock(executor, dialect)
        .unwrap()
        .with_timeout_seconds(0);

    let isolation_key = key(IsolationScope::Agent, "held-agent");
    let held = holder.try_enter(isolation_key.clone());
    assert!(held.is_acquired());

    let mut timed_out = contender.try_enter(isolation_key.clone());
    assert!(
        !timed_out.is_acquired(),
        "the contender must not acquire a held lock"
    );
    timed_out.release();
    drop(held);
}

// ---------------------------------------------------------------------------
//  Java: JdbcSandboxExecutionGuardSqliteTest
// ---------------------------------------------------------------------------

#[test]
fn sqlite_acquire_and_release_lock() {
    // Java: JdbcSandboxExecutionGuardSqliteTest.acquireAndReleaseLock
    let guard = new_guard_with_timeout(0);
    let isolation_key = key(IsolationScope::Global, "sqlite-agent");

    let mut lease = guard.try_enter(isolation_key);
    assert!(lease.is_acquired());
    lease.release();
}

#[test]
fn sqlite_duplicate_lock_is_contention_not_an_error() {
    // Java: JdbcSandboxExecutionGuardSqliteTest.duplicateLockTimesOut
    // SQLite has no SQLSTATE and words the PK violation like every other constraint failure;
    // its extended result code (`SQLITE_CONSTRAINT_PRIMARYKEY` = 1555) is what
    // `is_unique_violation` keys on, so the second attempt polls instead of aborting.
    let guard = new_guard_with_timeout(0);
    let isolation_key = key(IsolationScope::Agent, "sqlite-mutex");

    let mut lease1 = guard.try_enter(isolation_key.clone());
    assert!(lease1.is_acquired());

    let mut contender = guard.try_enter(isolation_key.clone());
    assert!(
        !contender.is_acquired(),
        "duplicate acquisition must time out, not error"
    );
    contender.release();

    lease1.release();
    assert!(!guard.is_held(&isolation_key));
}

// ---------------------------------------------------------------------------
//  Builder surface (Java: JdbcSandboxExecutionGuard.Builder)
// ---------------------------------------------------------------------------

#[test]
fn with_key_prefix_appends_missing_colon() {
    // Java: Builder.keyPrefix appends ':' when absent
    let guard = new_guard().with_key_prefix("myapp").unwrap();
    assert_eq!(
        guard.compose_lock_name(&key(IsolationScope::Global, "ignored")),
        "myapp:global:global"
    );

    // An explicit trailing colon is preserved as-is.
    let guard = new_guard().with_key_prefix("myapp:").unwrap();
    assert_eq!(
        guard.compose_lock_name(&key(IsolationScope::Global, "ignored")),
        "myapp:global:global"
    );
}

#[test]
fn with_key_prefix_rejects_blank() {
    // Java: Builder.keyPrefix throws IllegalArgumentException when blank
    assert!(new_guard().with_key_prefix("").is_err());
    assert!(new_guard().with_key_prefix("   ").is_err());
}

#[test]
fn explicit_release_via_guard_deletes_the_lock() {
    // Java: SandboxLockStrategy.release(lockName) / AbstractJdbcDialect table lock DELETE
    let guard = new_guard();
    let isolation_key = key(IsolationScope::Agent, "explicit-release");

    let mut lease = guard.try_enter(isolation_key.clone());
    assert!(lease.is_acquired());
    assert!(guard.is_held(&isolation_key));

    guard.release(&isolation_key);
    assert!(!guard.is_held(&isolation_key));

    // The lease's own release stays idempotent after an explicit guard release.
    lease.release();
}
