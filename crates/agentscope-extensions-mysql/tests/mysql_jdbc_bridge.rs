//! Live integration tests for the JDBC façade bridge (`feature = "jdbc-bridge"`).
//!
//! Gated on `AGENTSCOPE_TEST_MYSQL_URL` (unset ⇒ print a skip note and return; never
//! `#[ignore]`), so `cargo test -p agentscope-extensions-mysql` stays green without a server.
//!
//! ```text
//! $env:AGENTSCOPE_TEST_MYSQL_URL = 'mysql://root:secret@127.0.0.1:3306/agentscope_it'
//! cargo test -p agentscope-extensions-mysql --features jdbc-bridge --test mysql_jdbc_bridge
//! ```
//!
//! The URL **must** name a database: the jdbc MySQL dialect resolves its tables through
//! `DATABASE()`. Every key is unique per run and the schema is created with
//! `CREATE TABLE IF NOT EXISTS`, so the same database can be reused.
#![cfg(feature = "jdbc-bridge")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agentscope_core::state::AgentStateStore as _;
use agentscope_extensions_mysql::jdbc_bridge::MysqlJdbcStoreBuilder;
use agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec;
use agentscope_harness::agent::sandbox::{SandboxGuard, SandboxIsolationKey};
use agentscope_harness::filesystem::remote::store::BaseStore;
use serde_json::json;

const URL_ENV: &str = "AGENTSCOPE_TEST_MYSQL_URL";
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns the configured URL, or `None` when the environment gate is closed.
fn url() -> Option<String> {
    match std::env::var(URL_ENV) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// Prints the skip note and returns `true` when the test should stop.
fn skip() -> bool {
    if url().is_some() {
        return false;
    }
    eprintln!("skipping: set {URL_ENV} to run the JDBC bridge integration tests");
    true
}

fn unique(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}-{millis}-{seq}")
}

#[tokio::test]
async fn harness_bundle_carries_every_component() {
    if skip() {
        return;
    }
    let store = MysqlJdbcStoreBuilder::new(url().expect("checked above"))
        .isolation_scope(agentscope_harness::agent::IsolationScope::User)
        .build()
        .expect("connect to MySQL through the bridge");
    let harness = store.to_harness_distributed_store();
    assert!(harness.agent_state_store.is_some());
    assert!(harness.base_store.is_some());
    assert!(harness.sandbox_snapshot_spec.is_some());
    assert!(harness.sandbox_execution_guard.is_some());

    let state = harness.agent_state_store.as_ref().expect("state store");
    let session = unique("bridge-session");
    state
        .save(Some("user1"), &session, "key", json!({"v": 1}))
        .await;
    let loaded = state
        .get(Some("user1"), &session, "key")
        .await
        .expect("the state must round-trip");
    assert_eq!(loaded.get("v"), Some(&json!(1)));

    let base = harness.base_store.as_ref().expect("base store");
    let namespace = vec![unique("bridge-ns")];
    let mut value = serde_json::Map::new();
    value.insert("kv".to_string(), json!(2));
    assert!(base.put_if_version(&namespace, "k", value, 0));
    assert_eq!(
        base.get(&namespace, "k")
            .expect("item")
            .value
            .get("kv"),
        Some(&json!(2))
    );

    let spec = harness
        .sandbox_snapshot_spec
        .as_ref()
        .expect("snapshot spec");
    let snapshot = spec.build(unique("bridge-snap").as_str()).expect("build");
    // Non UTF-8 bytes: the LONGBLOB must decode as `ColumnValue::Blob`, not as text.
    snapshot.persist(&[0x1f, 0x8b, 0xff, 0xfe]).await.expect("persist");
    assert!(snapshot.is_restorable().await.expect("restorable"));
}

#[tokio::test]
async fn native_lock_guard_is_the_default() {
    if skip() {
        return;
    }
    let store = MysqlJdbcStoreBuilder::new(url().expect("checked above"))
        .build()
        .expect("connect to MySQL through the bridge");
    let harness = store.to_harness_distributed_store();
    let guard = harness
        .sandbox_execution_guard
        .as_ref()
        .expect("the bridge projects a guard");
    assert!(
        guard.is_distributed(),
        "the default guard must be the native GET_LOCK one"
    );

    let long_value = format!("agent-{}", "y".repeat(120));
    let key = SandboxIsolationKey::new("user", long_value.as_str());
    let lease = guard.try_enter(key.clone()).await;
    assert!(!lease.is_noop(), "GET_LOCK must succeed on a live server");
    guard.release(&key).await;
}

#[tokio::test]
async fn reopening_with_auto_create_disabled_verifies_the_existing_schema() {
    if skip() {
        return;
    }
    let url = url().expect("checked above");
    MysqlJdbcStoreBuilder::new(url.as_str())
        .build()
        .expect("the first open creates the schema");

    // Java: DialectBuilderTableGroupsTest.reopenWithAutoCreateFalse — verification passes
    // because the tables exist; nothing is dropped or re-created.
    let reopened = MysqlJdbcStoreBuilder::new(url)
        .initialize_schema(false)
        .build()
        .expect("re-opening an existing schema with auto-create off must succeed");
    let state = reopened.agent_state_store();
    let session = unique("bridge-reopen");
    state
        .save(Some("user1"), &session, "key", json!({"reopened": true}))
        .await;
    assert!(state.get(Some("user1"), &session, "key").await.is_some());
}
