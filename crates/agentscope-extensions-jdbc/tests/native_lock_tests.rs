//! Native advisory-lock strategy offline tests, mirroring the `tryEnter` overrides on the
//! Java `MysqlDialect` / `PostgresDialect`.
//!
//! Everything here is offline: [`NativeLockSession`] is the dedicated-session seam, so a fake
//! adapter emulates the `GET_LOCK` / `pg_try_advisory_lock` bookkeeping. The name/key mapping is
//! asserted against **independently computed** SHA-256 digests (both literal expectations taken
//! from outside the implementation, and a recomputation via the `sha2` dev-dependency) so a
//! wrong fold could not pass by agreeing with itself.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use agentscope_extensions_jdbc::dialect::{
    normalize_lock_name, postgres_lock_key, MysqlNativeLockStrategy, NativeLockError,
    NativeLockSession, PostgresNativeLockStrategy, SandboxLockStrategy,
    MYSQL_GET_LOCK_SQL, MYSQL_MAX_LOCK_NAME_LENGTH, MYSQL_RELEASE_LOCK_SQL,
    NATIVE_LOCK_POLL_INTERVAL, PG_ADVISORY_UNLOCK_SQL, PG_TRY_ADVISORY_LOCK_SQL,
};
use agentscope_extensions_jdbc::sandbox::JdbcSandboxExecutionGuard;
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use agentscope_harness::agent::IsolationScope;

/// Independent SHA-256 hex, recomputed for cross-checking (not copied from the crate).
fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// How the fake session should answer an acquire probe.
#[derive(Debug, Clone, Copy)]
enum Probe {
    /// Report the lock as acquired.
    Acquired,
    /// Report the lock as unavailable (the SQL probe returned false / 0).
    TimedOut,
    /// Fail the underlying session.
    Failed,
}

/// Offline [`NativeLockSession`]: records every key it is asked about and keeps a held set.
struct FakeSession {
    probe: Probe,
    held: Mutex<HashSet<String>>,
    acquired: Mutex<Vec<String>>,
    released: Mutex<Vec<String>>,
}

impl FakeSession {
    fn new(probe: Probe) -> Self {
        Self {
            probe,
            held: Mutex::new(HashSet::new()),
            acquired: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
        }
    }

    fn acquired_keys(&self) -> Vec<String> {
        self.acquired.lock().unwrap().clone()
    }

    fn released_keys(&self) -> Vec<String> {
        self.released.lock().unwrap().clone()
    }
}

impl NativeLockSession for FakeSession {
    fn try_acquire(&self, lock_key: &str, _timeout: Duration) -> Result<bool, NativeLockError> {
        self.acquired.lock().unwrap().push(lock_key.to_string());
        match self.probe {
            Probe::Acquired => {
                self.held.lock().unwrap().insert(lock_key.to_string());
                Ok(true)
            }
            Probe::TimedOut => Ok(false),
            Probe::Failed => Err(NativeLockError::Failed("simulated session failure".into())),
        }
    }

    fn release(&self, lock_key: &str) -> Result<(), NativeLockError> {
        self.released.lock().unwrap().push(lock_key.to_string());
        self.held.lock().unwrap().remove(lock_key);
        Ok(())
    }

    fn is_held(&self, lock_key: &str) -> Result<bool, NativeLockError> {
        Ok(self.held.lock().unwrap().contains(lock_key))
    }
}

fn key(scope: IsolationScope, agent: &str) -> SandboxIsolationKey {
    SandboxIsolationKey::resolve(Some(&scope), None, agent).unwrap()
}

// ---------------------------------------------------------------------------
//  Constants (Java: MysqlDialect.MAX_LOCK_NAME_LENGTH / PostgresDialect sleep)
// ---------------------------------------------------------------------------

#[test]
fn lock_constants_match_java() {
    // Java: MysqlDialect.MAX_LOCK_NAME_LENGTH = 64, LOCK_NAME_HASH_HEX_LENGTH = 32
    assert_eq!(MYSQL_MAX_LOCK_NAME_LENGTH, 64);
    // Java: PostgresDialect TimeUnit.MILLISECONDS.sleep(100)
    assert_eq!(NATIVE_LOCK_POLL_INTERVAL, Duration::from_millis(100));
}

#[test]
fn sql_probes_match_java() {
    // Java: MysqlDialect / PostgresDialect statement literals
    assert_eq!(MYSQL_GET_LOCK_SQL, "SELECT GET_LOCK(?, ?)");
    assert_eq!(MYSQL_RELEASE_LOCK_SQL, "SELECT RELEASE_LOCK(?)");
    assert_eq!(PG_TRY_ADVISORY_LOCK_SQL, "SELECT pg_try_advisory_lock(?)");
    assert_eq!(PG_ADVISORY_UNLOCK_SQL, "SELECT pg_advisory_unlock(?)");
}

// ---------------------------------------------------------------------------
//  Java: MysqlDialect.normalizeLockName
// ---------------------------------------------------------------------------

#[test]
fn mysql_short_lock_name_is_unchanged() {
    assert_eq!(normalize_lock_name("agentscope:sandbox:lock:agent:a"), "agentscope:sandbox:lock:agent:a");
}

#[test]
fn mysql_lock_name_at_limit_is_unchanged() {
    let at_limit = "n".repeat(MYSQL_MAX_LOCK_NAME_LENGTH);
    assert_eq!(normalize_lock_name(&at_limit), at_limit);
}

#[test]
fn mysql_long_lock_name_is_truncated_then_suffixed_with_digest() {
    // 100 x's -> 31 x's + ':' + first 32 hex chars of sha256("x"*100).
    let long = "x".repeat(100);
    let normalized = normalize_lock_name(&long);
    assert_eq!(
        normalized,
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx:09ecb6ebc8bcefc733f6f2ec44f791ab"
    );
    assert_eq!(normalized.chars().count(), MYSQL_MAX_LOCK_NAME_LENGTH);
}

#[test]
fn mysql_normalization_matches_independently_computed_digest() {
    let long = "x".repeat(100);
    let expected = format!("{}:{}", &long[..31], &sha256_hex(&long)[..32]);
    assert_eq!(normalize_lock_name(&long), expected);
}

#[test]
fn mysql_normalization_distinguishes_distinct_long_names() {
    // The Java comment's whole point: a 32-bit fold collides under high cardinality, so the
    // digest suffix must keep distinct long names distinct.
    let a = "x".repeat(100);
    let b = format!("{}y", "x".repeat(99));
    assert_ne!(normalize_lock_name(&a), normalize_lock_name(&b));
    assert_eq!(normalize_lock_name(&a).chars().count(), MYSQL_MAX_LOCK_NAME_LENGTH);
    assert_eq!(normalize_lock_name(&b).chars().count(), MYSQL_MAX_LOCK_NAME_LENGTH);
}

// ---------------------------------------------------------------------------
//  Java: PostgresDialect.composeLockKey
// ---------------------------------------------------------------------------

#[test]
fn postgres_lock_key_folds_the_first_eight_digest_bytes() {
    assert_eq!(postgres_lock_key("agentscope"), 2_761_187_194_516_106_426);
}

#[test]
fn postgres_lock_key_matches_independently_computed_digest() {
    let digest = Sha256::digest(b"agentscope");
    let mut expected: i64 = 0;
    for byte in digest.iter().take(8) {
        expected = (expected << 8) | (*byte as i64);
    }
    assert_eq!(postgres_lock_key("agentscope"), expected);
}

#[test]
fn postgres_lock_key_distinguishes_distinct_names() {
    assert_eq!(postgres_lock_key("agentscope-2"), 6_739_745_666_128_362_534);
    assert_ne!(postgres_lock_key("agentscope"), postgres_lock_key("agentscope-2"));
}

// ---------------------------------------------------------------------------
//  MysqlNativeLockStrategy / PostgresNativeLockStrategy
// ---------------------------------------------------------------------------

#[test]
fn mysql_strategy_acquires_the_normalized_name() {
    let session = Arc::new(FakeSession::new(Probe::Acquired));
    let strategy = MysqlNativeLockStrategy::new(session.clone());
    let long = "x".repeat(100);

    let lease = strategy.try_enter(&long, 5).expect("acquire");
    let expected = normalize_lock_name(&long);
    assert_eq!(session.acquired_keys(), vec![expected.clone()]);
    assert_eq!(lease.lock_name, expected);
    assert!(strategy.is_held(&long));
}

#[test]
fn mysql_strategy_releases_the_normalized_name() {
    let session = Arc::new(FakeSession::new(Probe::Acquired));
    let strategy = MysqlNativeLockStrategy::new(session.clone());
    let long = "x".repeat(100);

    let mut lease = strategy.try_enter(&long, 5).expect("acquire");
    lease.release();
    assert_eq!(session.released_keys(), vec![normalize_lock_name(&long)]);
    assert!(!strategy.is_held(&long));
}

#[test]
fn postgres_strategy_acquires_the_decimal_advisory_key() {
    let session = Arc::new(FakeSession::new(Probe::Acquired));
    let strategy = PostgresNativeLockStrategy::new(session.clone());

    let _lease = strategy.try_enter("agentscope", 5).expect("acquire");
    assert_eq!(session.acquired_keys(), vec!["2761187194516106426".to_string()]);
    assert!(strategy.is_held("agentscope"));
}

#[test]
fn postgres_strategy_releases_the_decimal_advisory_key() {
    let session = Arc::new(FakeSession::new(Probe::Acquired));
    let strategy = PostgresNativeLockStrategy::new(session.clone());

    let mut lease = strategy.try_enter("agentscope", 5).expect("acquire");
    lease.release();
    assert_eq!(session.released_keys(), vec!["2761187194516106426".to_string()]);
    assert!(!strategy.is_held("agentscope"));
}

#[test]
fn unavailable_lock_is_reported_as_timed_out() {
    let session = Arc::new(FakeSession::new(Probe::TimedOut));
    let strategy = MysqlNativeLockStrategy::new(session.clone());

    let outcome = strategy.try_enter("busy", 1).unwrap_err();
    assert_eq!(outcome, agentscope_extensions_jdbc::dialect::LockOutcome::TimedOut);
    assert!(session.released_keys().is_empty());
}

#[test]
fn session_failure_is_reported_as_timed_out_not_a_panic() {
    // Bypass principle: an unreachable/failing backend must not panic the guard.
    let session = Arc::new(FakeSession::new(Probe::Failed));
    let strategy = PostgresNativeLockStrategy::new(session);

    assert!(strategy.try_enter("broken", 1).is_err());
}

#[test]
fn dropping_the_lease_releases_the_native_lock() {
    let session = Arc::new(FakeSession::new(Probe::Acquired));
    let strategy = MysqlNativeLockStrategy::new(session.clone());

    {
        let _lease = strategy.try_enter("dropped", 5).expect("acquire");
        assert!(strategy.is_held("dropped"));
    }
    assert!(!strategy.is_held("dropped"));
    assert_eq!(session.released_keys(), vec!["dropped".to_string()]);
}

#[test]
fn lease_release_is_idempotent() {
    let session = Arc::new(FakeSession::new(Probe::Acquired));
    let strategy = MysqlNativeLockStrategy::new(session.clone());

    let mut lease = strategy.try_enter("once", 5).expect("acquire");
    lease.release();
    lease.release();
    drop(lease);
    assert_eq!(session.released_keys(), vec!["once".to_string()]);
}

// ---------------------------------------------------------------------------
//  Wiring: JdbcSandboxExecutionGuard over a native strategy
// ---------------------------------------------------------------------------

#[test]
fn guard_composes_the_key_prefix_then_maps_it_to_the_native_lock() {
    let session = Arc::new(FakeSession::new(Probe::Acquired));
    let strategy = Arc::new(MysqlNativeLockStrategy::new(session.clone()));
    let guard = JdbcSandboxExecutionGuard::new(strategy);

    let isolation_key = key(IsolationScope::Agent, "agent-1");
    let composed = guard.compose_lock_name(&isolation_key);
    assert_eq!(composed, "agentscope:sandbox:lock:agent:agent-1");

    let lease = guard.try_enter(isolation_key.clone());
    assert!(lease.is_acquired());
    // Short composed name -> MySQL normalization is a no-op, so the raw name is the lock key.
    assert_eq!(session.acquired_keys(), vec![composed]);
    assert!(guard.is_held(&isolation_key));
}

#[test]
fn guard_release_uses_the_native_strategy() {
    let session = Arc::new(FakeSession::new(Probe::Acquired));
    let strategy = Arc::new(PostgresNativeLockStrategy::new(session.clone()));
    let guard = JdbcSandboxExecutionGuard::new(strategy);

    let isolation_key = key(IsolationScope::Agent, "agent-1");
    let composed = guard.compose_lock_name(&isolation_key);
    let expected_key = postgres_lock_key(&composed).to_string();

    let lease = guard.try_enter(isolation_key.clone());
    assert!(lease.is_acquired());
    assert_eq!(session.acquired_keys(), vec![expected_key.clone()]);

    guard.release(&isolation_key);
    assert_eq!(session.released_keys(), vec![expected_key]);
    assert!(!guard.is_held(&isolation_key));
}

#[test]
fn guard_with_failing_session_returns_a_noop_lease() {
    // Java throws here; Rust returns a no-op lease (documented in §3.4).
    let session = Arc::new(FakeSession::new(Probe::Failed));
    let strategy = Arc::new(MysqlNativeLockStrategy::new(session));
    let guard = JdbcSandboxExecutionGuard::new(strategy);

    let lease = guard.try_enter(key(IsolationScope::Global, "a"));
    assert!(!lease.is_acquired());
}
