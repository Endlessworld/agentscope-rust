//! Live **standalone** Redis integration tests, gated on `AGENTSCOPE_TEST_REDIS_URL`.
//!
//! The crate's unit tests drive in-memory doubles (see `src/test_support`), and
//! `tests/redis_cluster_integration.rs` covers a real cluster. Nothing exercised a real
//! single node, which is where the following production-only behaviour lives:
//!
//! * the `SAVE_SCRIPT` Lua CAS really runs on the server (`EVAL`) instead of being simulated;
//! * `save_list` / `get_list` really heal a stale `:_hash` marker (a crash between the list
//!   rewrite and the marker write leaves the two out of sync);
//! * `find_keys_by_pattern` really drives the `SCAN` cursor over several round-trips;
//! * `redis::aio::ConnectionManager` really reconnects after its socket dies;
//! * `SET NX PX` / the token-CAS `DEL` script really serialise a lease across clients.
//!
//! ```text
//! $env:AGENTSCOPE_TEST_REDIS_URL = 'redis://127.0.0.1:6379'
//! cargo test -p agentscope-extensions-redis --test redis_integration -- --test-threads=1
//! ```
//!
//! When the variable is unset the tests skip themselves (printing a note) instead of being
//! `#[ignore]`d, matching the cluster suite.
//!
//! ## Why there is no Sentinel case
//!
//! `RedisConnection` models exactly two topologies: one node (`connect`, which also covers a
//! Sentinel *primary* you were handed the address of) and a Redis Cluster (`connect_cluster`,
//! `CLUSTER SLOTS` discovery). It never talks to Sentinel itself, so there is no Sentinel
//! discovery code path to prove — and this machine has no Sentinel deployment (`1 master +
//! replicas + 3 sentinels`) to point a test at. The gap stays recorded in the module tracker
//! (`§4.2`) rather than being papered over with a green no-op.
//!
//! ## Isolation
//!
//! Every test owns a fresh `it:<uuid>:<name>:` key prefix and deletes only its own keys at the
//! end. `FLUSHDB` / `FLUSHALL` are never issued: the container is shared with other suites.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope_core::state::AgentStateStore as _;
use agentscope_extensions_redis::state::adapter::RedisClientAdapter;
use agentscope_extensions_redis::state::RedisAgentStateStore;
use agentscope_extensions_redis::{RedisConnection, RedisSandboxExecutionGuard};
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use serde_json::json;

// ---------------------------------------------------------------------------
// env gate + helpers
// ---------------------------------------------------------------------------

fn redis_url() -> Option<String> {
    match std::env::var("AGENTSCOPE_TEST_REDIS_URL") {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

fn skip() -> bool {
    if redis_url().is_some() {
        return false;
    }
    eprintln!("skipping: set AGENTSCOPE_TEST_REDIS_URL to run the live standalone Redis tests");
    true
}

/// A key prefix owned exclusively by one test — no test ever touches another's keys.
fn prefix_for(name: &str) -> String {
    format!("it:{}:{name}:", uuid::Uuid::new_v4())
}

async fn live_connection() -> RedisConnection {
    let connection = RedisConnection::connect(&redis_url().unwrap())
        .await
        .expect("connect to the live standalone Redis");
    assert!(
        !connection.is_cluster_scan(),
        "AGENTSCOPE_TEST_REDIS_URL must point at a standalone node"
    );
    connection
}

fn store(connection: &RedisConnection, key_prefix: &str) -> RedisAgentStateStore {
    let adapter: Arc<dyn RedisClientAdapter> = Arc::new(connection.clone_shared());
    RedisAgentStateStore::builder()
        .client(adapter)
        .key_prefix(key_prefix)
        .build()
        .expect("build the state store")
}

async fn scan_keys(connection: &RedisConnection, pattern: &str) -> Vec<String> {
    RedisClientAdapter::find_keys_by_pattern(connection, pattern)
        .await
        .expect("SCAN must succeed")
}

async fn raw_get(connection: &RedisConnection, key: &str) -> Option<String> {
    RedisClientAdapter::get(connection, key)
        .await
        .expect("GET must succeed")
}

async fn raw_set(connection: &RedisConnection, key: &str, value: &str) {
    RedisClientAdapter::set(connection, key, value)
        .await
        .expect("SET must succeed");
}

async fn raw_list_len(connection: &RedisConnection, key: &str) -> i64 {
    RedisClientAdapter::get_list_length(connection, key)
        .await
        .expect("LLEN must succeed")
}

async fn raw_set_members(connection: &RedisConnection, key: &str) -> Vec<String> {
    let mut members = RedisClientAdapter::get_set_members(connection, key)
        .await
        .expect("SMEMBERS must succeed");
    members.sort();
    members
}

/// Issues one `SCAN` round-trip straight through the connection manager, bypassing the
/// adapter, so a test can independently observe the server-side cursor.
async fn raw_scan_once(
    connection: &RedisConnection,
    cursor: u64,
    pattern: &str,
    count: usize,
) -> (u64, Vec<String>) {
    let mut manager = connection.manager().clone();
    let mut cmd = redis::cmd("SCAN");
    cmd.arg(cursor)
        .arg("MATCH")
        .arg(pattern)
        .arg("COUNT")
        .arg(count);
    let (next, keys): (String, Vec<String>) = cmd
        .query_async(&mut manager)
        .await
        .expect("SCAN must succeed");
    (next.parse().unwrap_or(0), keys)
}

/// `CLIENT ID` of the connection the manager is currently using.
async fn client_id(connection: &RedisConnection) -> i64 {
    let mut manager = connection.manager().clone();
    redis::cmd("CLIENT")
        .arg("ID")
        .query_async(&mut manager)
        .await
        .expect("CLIENT ID must succeed")
}

/// Remaining TTL (ms) of a key, i.e. proof that the lease was written with `PX`.
async fn raw_pttl(connection: &RedisConnection, key: &str) -> i64 {
    let mut manager = connection.manager().clone();
    redis::cmd("PTTL")
        .arg(key)
        .query_async(&mut manager)
        .await
        .expect("PTTL must succeed")
}

/// Deletes every key of `key_prefix`, in batches, leaving the rest of the database alone.
async fn cleanup(connection: &RedisConnection, key_prefix: &str) {
    let keys = scan_keys(connection, &format!("{key_prefix}*")).await;
    for chunk in keys.chunks(64) {
        RedisClientAdapter::delete_keys(connection, chunk)
            .await
            .expect("cleanup DEL must succeed");
    }
}

// ---------------------------------------------------------------------------
// 1. real Lua CAS
// ---------------------------------------------------------------------------

/// `save_if_version` is the single most dangerous piece of the migration: the whole
/// create-if-absent / optimistic-lock contract is implemented in `SAVE_SCRIPT`, which the
/// unit tests only ever run against a scripted double. Here the server executes it.
#[tokio::test]
async fn save_if_version_runs_the_real_cas_lua_script() {
    if skip() {
        return;
    }
    let connection = live_connection().await;
    let prefix = prefix_for("cas");
    let store = store(&connection, &prefix);
    let payload_key = format!("{prefix}alice/s1:agent_state");
    let version_key = format!("{payload_key}:ver");
    let keys_key = format!("{prefix}alice/s1:_keys");

    // expected == 0 on a missing key -> create-if-absent, returning version 1.
    let created = RedisAgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "agent_state",
        &json!({"step": 1}),
        0,
    )
    .await
    .expect("the create-if-absent EVAL must succeed");
    assert_eq!(created, 1, "create-if-absent must return version 1");

    // The *script* (not the caller) wrote payload + version + session index.
    assert_eq!(
        raw_get(&connection, &payload_key).await.as_deref(),
        Some(r#"{"step":1}"#),
        "the script stores the raw JSON payload"
    );
    assert_eq!(
        raw_get(&connection, &version_key).await.as_deref(),
        Some("1"),
        "the script writes the version counter itself"
    );
    assert_eq!(
        raw_set_members(&connection, &keys_key).await,
        vec!["agent_state".to_string()],
        "the script SADDs the state key into the session index"
    );

    // expected == 0 again: the key exists with version 1, so this must conflict.
    let conflict = RedisAgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "agent_state",
        &json!({"step": 2}),
        0,
    )
    .await
    .expect("a conflict is reported as 0, not as an error");
    assert_eq!(
        conflict, 0,
        "create-if-absent must not overwrite an existing value"
    );
    assert_eq!(
        raw_get(&connection, &payload_key).await.as_deref(),
        Some(r#"{"step":1}"#),
        "a losing CAS leaves the payload untouched"
    );
    assert_eq!(
        raw_get(&connection, &version_key).await.as_deref(),
        Some("1"),
        "a losing CAS leaves the version untouched"
    );

    // Matching version -> the script bumps to 2.
    let bumped = RedisAgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "agent_state",
        &json!({"step": 2}),
        1,
    )
    .await
    .expect("the CAS EVAL must succeed");
    assert_eq!(bumped, 2, "a matching expected version bumps the counter");
    assert_eq!(
        raw_get(&connection, &payload_key).await.as_deref(),
        Some(r#"{"step":2}"#)
    );
    assert_eq!(
        raw_get(&connection, &version_key).await.as_deref(),
        Some("2")
    );

    // Stale expected version -> conflict, nothing written.
    let stale = RedisAgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "agent_state",
        &json!({"step": 3}),
        1,
    )
    .await
    .expect("the CAS EVAL must succeed");
    assert_eq!(stale, 0, "a stale expected version must lose");
    assert_eq!(
        raw_get(&connection, &payload_key).await.as_deref(),
        Some(r#"{"step":2}"#)
    );
    assert_eq!(
        raw_get(&connection, &version_key).await.as_deref(),
        Some("2")
    );

    // The read side agrees with what the script wrote.
    let versioned = store
        .get_versioned(Some("alice"), "s1", "agent_state")
        .await
        .expect("get_versioned");
    assert_eq!(versioned.version, 2);
    assert_eq!(versioned.value, Some(json!({"step": 2})));

    // `save` (unconditional) keeps working against the same script.
    store
        .save(Some("alice"), "s1", "agent_state", json!({"step": 99}))
        .await;
    assert_eq!(
        raw_get(&connection, &version_key).await.as_deref(),
        Some("3"),
        "an unconditional save bumps without a version check"
    );

    cleanup(&connection, &prefix).await;
    assert!(
        scan_keys(&connection, &format!("{prefix}*"))
            .await
            .is_empty(),
        "the test removes every key it created"
    );
}

/// Eight writers race on the same key with the same `expected_version`; the server-side
/// script must let exactly one win. A client-side "read, compare, write" would let several
/// through, so this asserts the migration did not quietly replace `EVAL` with read-modify-write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cas_writers_serialise_on_the_server() {
    if skip() {
        return;
    }
    let connection = live_connection().await;
    let prefix = prefix_for("cas-race");
    let store = Arc::new(store(&connection, &prefix));

    let seeded =
        RedisAgentStateStore::save_if_version(&store, Some("alice"), "s1", "counter", &json!(0), 0)
            .await
            .expect("seed the key");
    assert_eq!(seeded, 1);

    let mut handles = Vec::new();
    for writer in 0..8u64 {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            RedisAgentStateStore::save_if_version(
                &store,
                Some("alice"),
                "s1",
                "counter",
                &json!(writer + 1),
                1,
            )
            .await
        }));
    }

    let mut winners = Vec::new();
    for handle in handles {
        match handle.await.expect("the writer task must not panic") {
            Ok(0) => {}
            Ok(version) => winners.push(version),
            Err(e) => panic!("the CAS EVAL must not fail: {e}"),
        }
    }
    assert_eq!(
        winners,
        vec![2],
        "exactly one of eight racing writers may win the CAS"
    );
    assert_eq!(
        raw_get(&connection, &format!("{prefix}alice/s1:counter:ver"))
            .await
            .as_deref(),
        Some("2"),
        "the version advanced exactly once"
    );

    cleanup(&connection, &prefix).await;
}

// ---------------------------------------------------------------------------
// 2. list `:_hash` self-healing
// ---------------------------------------------------------------------------

/// `save_list` keeps a sampled `:_hash` marker next to the list and only appends when the
/// marker still matches the stored prefix. If a writer died between the list rewrite and the
/// marker write (or the data predates the marker) the two disagree, and the next write must
/// *fully* rewrite instead of appending — otherwise items get duplicated or resurrected.
#[tokio::test]
async fn save_list_self_heals_a_stale_hash_marker_and_legacy_payloads() {
    if skip() {
        return;
    }
    let connection = live_connection().await;
    let prefix = prefix_for("list");
    let store = store(&connection, &prefix);
    let list_key = format!("{prefix}alice/s2:items:list");
    let hash_key = format!("{list_key}:_hash");

    store
        .save_list(
            Some("alice"),
            "s2",
            "items",
            &[json!(1), json!(2), json!(3)],
        )
        .await;
    assert_eq!(
        store.get_list(Some("alice"), "s2", "items").await,
        vec![json!(1), json!(2), json!(3)]
    );
    assert_eq!(raw_list_len(&connection, &list_key).await, 3);
    let healthy_hash = raw_get(&connection, &hash_key)
        .await
        .expect("the first write stores a :_hash marker");
    assert!(!healthy_hash.is_empty());

    // Simulate "the list was rewritten but the marker write was lost".
    raw_set(&connection, &hash_key, "stale-marker").await;
    store
        .save_list(
            Some("alice"),
            "s2",
            "items",
            &[json!(1), json!(2), json!(3)],
        )
        .await;
    assert_eq!(
        store.get_list(Some("alice"), "s2", "items").await,
        vec![json!(1), json!(2), json!(3)],
        "the stale marker must trigger a full rewrite that reads back correctly"
    );
    assert_eq!(
        raw_list_len(&connection, &list_key).await,
        3,
        "a full rewrite must replace the list, not append to it"
    );
    assert_eq!(
        raw_get(&connection, &hash_key).await.as_deref(),
        Some(healthy_hash.as_str()),
        "the rewrite repairs the marker"
    );

    // Legacy payload written before the marker existed (`_hash` absent, list non-empty).
    RedisClientAdapter::delete_keys(&connection, &[list_key.clone(), hash_key.clone()])
        .await
        .expect("clear the list for the legacy case");
    RedisClientAdapter::right_push_list(&connection, &list_key, r#"{"legacy":1}"#)
        .await
        .expect("seed legacy item 1");
    RedisClientAdapter::right_push_list(&connection, &list_key, r#"{"legacy":2}"#)
        .await
        .expect("seed legacy item 2");
    assert!(
        raw_get(&connection, &hash_key).await.is_none(),
        "legacy data has no marker"
    );

    store
        .save_list(Some("alice"), "s2", "items", &[json!(9), json!(10)])
        .await;
    assert_eq!(
        store.get_list(Some("alice"), "s2", "items").await,
        vec![json!(9), json!(10)],
        "unverifiable legacy data is replaced, never appended to"
    );
    assert_eq!(raw_list_len(&connection, &list_key).await, 2);

    // Healthy prefix: an append must only push the suffix (no rewrite, no duplication).
    store
        .save_list(
            Some("alice"),
            "s2",
            "items",
            &[json!(9), json!(10), json!(11)],
        )
        .await;
    assert_eq!(
        store.get_list(Some("alice"), "s2", "items").await,
        vec![json!(9), json!(10), json!(11)]
    );
    assert_eq!(raw_list_len(&connection, &list_key).await, 3);

    cleanup(&connection, &prefix).await;
    assert!(
        scan_keys(&connection, &format!("{prefix}*"))
            .await
            .is_empty(),
        "list, marker and index are all removed"
    );
}

// ---------------------------------------------------------------------------
// 3. real SCAN cursor
// ---------------------------------------------------------------------------

/// `find_keys_by_pattern` must drive `SCAN` to completion. 300 keys with the production
/// `COUNT 100` needs several round-trips, so an implementation that fired a single `SCAN`
/// (or fell back to `KEYS` on one page) would come back short. The reference walk below uses
/// the very same cursor protocol but is written inside the test, so the expectation does not
/// come from the code under test.
#[tokio::test]
async fn find_keys_by_pattern_walks_the_real_scan_cursor_over_several_pages() {
    if skip() {
        return;
    }
    let connection = live_connection().await;
    let prefix = prefix_for("scan");
    let target_prefix = format!("{prefix}page:");
    let pattern = format!("{target_prefix}*");

    const KEY_COUNT: u32 = 300;
    for index in 0..KEY_COUNT {
        raw_set(&connection, &format!("{target_prefix}{index:04}"), "v").await;
    }

    // One round-trip is demonstrably not enough for this data set.
    let (first_cursor, first_page) = raw_scan_once(&connection, 0, &pattern, 100).await;
    assert_ne!(
        first_cursor, 0,
        "{KEY_COUNT} keys must span more than one SCAN round-trip at COUNT 100"
    );
    assert!(first_page.len() < KEY_COUNT as usize);

    // Independent reference walk.
    let mut walked: HashSet<String> = HashSet::new();
    let mut cursor = 0u64;
    let mut rounds = 0usize;
    loop {
        let (next, keys) = raw_scan_once(&connection, cursor, &pattern, 100).await;
        walked.extend(keys);
        rounds += 1;
        assert!(rounds < 256, "the reference SCAN walk did not terminate");
        if next == 0 {
            break;
        }
        cursor = next;
    }
    assert_eq!(
        walked.len(),
        KEY_COUNT as usize,
        "the reference walk sees every key"
    );
    assert!(rounds > 1, "the reference walk needed {rounds} rounds");

    // The adapter must produce the same key set: complete and deduplicated.
    let found = scan_keys(&connection, &pattern).await;
    let mut unique: HashSet<String> = found.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        KEY_COUNT as usize,
        "find_keys_by_pattern lost or duplicated keys"
    );
    for index in 0..KEY_COUNT {
        assert!(
            unique.remove(&format!("{target_prefix}{index:04}")),
            "missing key {index}"
        );
    }

    // A decoy right next to the prefix is not matched by the glob.
    raw_set(&connection, &format!("{prefix}decoy"), "v").await;
    let found = scan_keys(&connection, &pattern).await;
    assert_eq!(
        found.len(),
        KEY_COUNT as usize,
        "the MATCH glob stays exact"
    );
    assert!(!found.iter().any(|key| key.ends_with("decoy")));

    cleanup(&connection, &prefix).await;
    assert!(
        scan_keys(&connection, &format!("{prefix}*"))
            .await
            .is_empty(),
        "all {KEY_COUNT} keys plus the decoy are removed"
    );
}

// ---------------------------------------------------------------------------
// 4. ConnectionManager reconnect
// ---------------------------------------------------------------------------

/// `RedisConnection::connect` wraps a `redis::aio::ConnectionManager`, which is supposed to
/// reconnect transparently after the socket dies (`CLIENT KILL` here). Nothing in the crate
/// verified that, yet the whole store depends on it: a dead socket would otherwise surface as
/// permanent `IO error` failures instead of one retried command.
#[tokio::test]
async fn connection_manager_reconnects_after_the_socket_is_killed() {
    if skip() {
        return;
    }
    let connection = live_connection().await;
    let prefix = prefix_for("reconnect");
    let key = format!("{prefix}socket");

    raw_set(&connection, &key, "before").await;
    let old_id = client_id(&connection).await;

    // Kill that exact connection from a *different* client.
    let admin = live_connection().await;
    let mut manager = admin.manager().clone();
    let killed: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(old_id)
        .query_async(&mut manager)
        .await
        .expect("CLIENT KILL must succeed");
    assert_eq!(
        killed, 1,
        "the manager's connection {old_id} must have been killed"
    );

    // The next commands may fail once while the manager notices the dead socket; it must
    // recover without a new `connect`.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut attempts = 0usize;
    loop {
        attempts += 1;
        let write = RedisClientAdapter::set(&connection, &key, "after").await;
        if write.is_ok() && raw_get(&connection, &key).await.as_deref() == Some("after") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the connection manager did not reconnect after CLIENT KILL (last write: {write:?})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(attempts <= 20, "reconnect took {attempts} attempts");

    let new_id = client_id(&connection).await;
    assert_ne!(
        new_id, old_id,
        "the manager must have opened a fresh connection, not reused the killed one"
    );

    cleanup(&connection, &prefix).await;
}

// ---------------------------------------------------------------------------
// 5. distributed lease: SET NX PX + token CAS
// ---------------------------------------------------------------------------

/// The guard is a distributed mutual exclusion primitive: acquisition is `SET NX PX` (so a
/// second client loses) and release is a Lua `GET`-compare-`DEL` (so a client whose lease
/// expired cannot delete the new owner's lock). Both directions are asserted against the
/// server, plus the `PTTL` proof that the lease really expires on its own.
#[tokio::test]
async fn distributed_lease_uses_set_nx_and_a_token_cas_release() {
    if skip() {
        return;
    }
    let connection = live_connection().await;
    let prefix = prefix_for("lock");
    let guard = RedisSandboxExecutionGuard::builder(&connection)
        .key_prefix(format!("{prefix}lease:"))
        .lease_ttl(Duration::from_secs(30))
        .retry_interval(Duration::from_millis(50))
        .build();
    let isolation_key = SandboxIsolationKey::new("session", "s1");
    let redis_key = format!("{prefix}lease:session:s1");

    // Acquire: the lock value is the lease token, and the TTL proves `PX` was used.
    let lease = guard
        .try_enter(&isolation_key)
        .await
        .expect("SET NX must acquire a free slot");
    assert_eq!(
        raw_get(&connection, &redis_key).await.as_deref(),
        Some(lease.token()),
        "the lock stores the lease token"
    );
    let ttl = raw_pttl(&connection, &redis_key).await;
    assert!(
        (0..=30_000).contains(&ttl),
        "the lease must carry a PX expiry, got {ttl}"
    );
    assert!(guard.is_held(&isolation_key).await);

    // Contention: a second acquisition against the live lock cannot succeed. `try_enter`
    // retries forever, so bound it with a timeout instead of hanging the suite.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), guard.try_enter(&isolation_key))
            .await
            .is_err(),
        "a held lock must not be handed out twice"
    );

    // Success branch of the token CAS: the owner releases, and only once.
    assert!(lease.release().await, "the owner's token releases the lock");
    assert!(!guard.is_held(&isolation_key).await);
    assert!(
        !lease.release().await,
        "release is idempotent and never re-runs the script"
    );

    // Failure branch: the TTL expired and somebody else owns the key now, so the stale
    // token's `DEL` must be a no-op.
    let stale = guard
        .try_enter(&isolation_key)
        .await
        .expect("the slot is free again");
    raw_set(&connection, &redis_key, "another-owner-token").await;
    assert!(
        !stale.release().await,
        "a token that no longer matches must not delete the lock"
    );
    assert_eq!(
        raw_get(&connection, &redis_key).await.as_deref(),
        Some("another-owner-token"),
        "the new owner's lock survives the stale release"
    );

    // Once the foreign lock is gone the guard acquires again (the retry loop terminates).
    RedisClientAdapter::delete_keys(&connection, &[redis_key.clone()])
        .await
        .expect("drop the simulated foreign lock");
    let regained = tokio::time::timeout(Duration::from_secs(5), guard.try_enter(&isolation_key))
        .await
        .expect("the retry loop must observe the freed slot")
        .expect("and acquire it");
    assert!(regained.release().await);

    cleanup(&connection, &prefix).await;
    assert!(
        scan_keys(&connection, &format!("{prefix}*"))
            .await
            .is_empty(),
        "no lease keys are left behind"
    );
}
