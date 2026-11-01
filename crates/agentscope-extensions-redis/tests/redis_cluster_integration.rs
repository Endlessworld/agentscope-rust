//! Live Redis **Cluster** integration tests, gated on `AGENTSCOPE_TEST_REDIS_CLUSTER_URL`.
//!
//! The unit tests cover `MOVED` / `ASK` handling offline (scripted transport in
//! `state/redirect.rs`); these tests prove the production path against a real cluster:
//! keys land on different primaries, so a command that starts on the seed node has to be
//! redirected before it can succeed.
//!
//! ```text
//! $env:AGENTSCOPE_TEST_REDIS_CLUSTER_URL = 'redis://127.0.0.1:7000'
//! cargo test -p agentscope-extensions-redis --test redis_cluster_integration
//! ```
//!
//! When the variable is unset the tests skip themselves (printing a note) instead of being
//! `#[ignore]`d, so an environment that has a cluster always exercises them.

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::state::AgentStateStore as _;
use agentscope_extensions_redis::sandbox::RedisSandboxExecutionGuard;
use agentscope_extensions_redis::snapshot::RedisRemoteSnapshotClient;
use agentscope_extensions_redis::state::adapter::RedisClientAdapter;
use agentscope_extensions_redis::state::connection::RedisConnection;
use agentscope_extensions_redis::state::{KeyLayoutVersion, RedisAgentStateStore};
use agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient;
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use serde_json::json;

fn cluster_url() -> Option<String> {
    match std::env::var("AGENTSCOPE_TEST_REDIS_CLUSTER_URL") {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

fn skip() -> bool {
    if cluster_url().is_some() {
        return false;
    }
    eprintln!(
        "skipping: set AGENTSCOPE_TEST_REDIS_CLUSTER_URL to run the live Redis Cluster tests"
    );
    true
}

fn key_prefix() -> String {
    format!("agentscope:it:cluster:{}:", std::process::id())
}

async fn store(url: &str) -> RedisAgentStateStore {
    let connection = RedisConnection::connect_cluster(url)
        .await
        .expect("connect to the Redis Cluster seed");
    assert!(connection.is_cluster_scan(), "cluster topology expected");
    let adapter: Arc<dyn RedisClientAdapter> = Arc::new(connection);
    RedisAgentStateStore::builder()
        .client(adapter)
        .key_prefix(key_prefix())
        // V1 wraps the session in a `{user/session}` hash tag so every key of a session
        // shares one slot — required for the multi-key Lua scripts.
        .key_layout_version(KeyLayoutVersion::V1)
        .build()
        .expect("build the state store")
}

/// Many sessions spread over many slots: each `save`/`get` pair starts on the seed node and
/// must follow the `MOVED` reply (and then hit the cache) to succeed.
#[tokio::test]
async fn keyed_commands_follow_slot_redirects() {
    if skip() {
        return;
    }
    let store = store(&cluster_url().unwrap()).await;

    for i in 0..64u64 {
        let session = format!("session-{i}");
        let version = store
            .save_if_version(Some("cluster-it"), &session, "counter", &json!(i), 0)
            .await
            .expect("create-if-absent must succeed after routing");
        assert!(version >= 1, "unexpected version {version}");
        assert_eq!(
            store.get(Some("cluster-it"), &session, "counter").await,
            Some(json!(i)),
            "read back routed session {session}"
        );
    }

    let ids = store.list_session_ids(Some("cluster-it")).await;
    assert_eq!(ids.len(), 64, "every routed session is enumerable");

    for i in 0..64u64 {
        store.delete(Some("cluster-it"), &format!("session-{i}")).await;
    }
    assert!(
        store.list_session_ids(Some("cluster-it")).await.is_empty(),
        "cleanup removes every session"
    );
}

/// Lists and set keys (the `_keys` index and the list payload) live in the same hash-tag
/// slot, so the multi-key Lua script must still work after a redirect.
#[tokio::test]
async fn lists_and_key_index_stay_in_one_slot() {
    if skip() {
        return;
    }
    let store = store(&cluster_url().unwrap()).await;

    for i in 0..32u64 {
        let session = format!("list-{i}");
        store
            .save_list(Some("cluster-it"), &session, "items", &[json!(1), json!(2), json!(3)])
            .await;
        assert_eq!(
            store.get_list(Some("cluster-it"), &session, "items").await,
            vec![json!(1), json!(2), json!(3)],
            "list round-trip for {session}"
        );
        store.delete(Some("cluster-it"), &session).await;
    }
}

/// REDIS-01: snapshot storage and the sandbox lease do not go through the state store's
/// adapter but through the `RedisCommands` seam, which used to be built straight from the
/// **seed** connection. Both now carry the connection's `ClusterRouter`, so a key whose slot
/// is owned by another primary is routed (and redirected) instead of failing with `MOVED`.
#[tokio::test]
async fn snapshots_and_leases_reach_non_seed_slots() {
    if skip() {
        return;
    }
    let connection = RedisConnection::connect_cluster(&cluster_url().unwrap())
        .await
        .expect("connect to the Redis Cluster seed");
    let prefix = format!("{}route:", key_prefix());

    // Snapshot payloads across many ids: most land on a primary other than the seed.
    let snapshots =
        RedisRemoteSnapshotClient::new(&connection, &format!("{prefix}snap:"), Some(600));
    for i in 0..32u32 {
        let id = format!("snap-{i}");
        let payload = vec![i as u8; 32];
        snapshots
            .upload(&id, &payload)
            .await
            .expect("upload must follow the slot owner");
        assert_eq!(
            snapshots.download(&id).await.expect("download routed"),
            payload,
            "round-trip for {id}"
        );
        assert!(snapshots.exists(&id).await.expect("exists routed"));
    }

    // The distributed lease is `SET NX PX` plus a Lua CAS release on the lock key.
    let guard = RedisSandboxExecutionGuard::builder(&connection)
        .key_prefix(format!("{prefix}lock:"))
        .lease_ttl(Duration::from_secs(60))
        .retry_interval(Duration::from_millis(50))
        .build();
    for i in 0..16u32 {
        let key = SandboxIsolationKey::new("session", format!("s{i}"));
        let lease = tokio::time::timeout(Duration::from_secs(10), guard.try_enter(&key))
            .await
            .expect("SET NX PX must not spin on a MOVED slot")
            .expect("acquire the routed lease");
        assert!(guard.probe_is_held(&key).await.expect("EXISTS probe"));
        assert!(lease.release().await, "the token CAS release is routed too");
        assert!(!guard.probe_is_held(&key).await.expect("EXISTS probe"));
    }

    // Cleanup: `SCAN` fans out over every primary.
    let keys = RedisClientAdapter::find_keys_by_pattern(&connection, &format!("{prefix}*"))
        .await
        .expect("scan every primary");
    assert!(!keys.is_empty(), "the routed keys are enumerable");
    for key in keys {
        RedisClientAdapter::delete_keys(&connection, &[key])
            .await
            .expect("delete one routed key");
    }
    assert!(
        RedisClientAdapter::find_keys_by_pattern(&connection, &format!("{prefix}*"))
            .await
            .expect("scan every primary")
            .is_empty(),
        "cleanup removes every routed key"
    );
}