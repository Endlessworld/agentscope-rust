//! Tests for [`OssSandboxExecutionGuard`] and [`OssDistributedStore`].

mod common;

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;
use agentscope_extensions_oss::{
    OssAgentStateStore, OssClient, OssDistributedStore, OssSandboxExecutionGuard, OssStore,
};
use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use agentscope_harness::filesystem::BaseStore;

#[test]
fn guard_delegates_to_harness_semantics() {
    let guard = OssSandboxExecutionGuard::new();
    let key = SandboxIsolationKey::new("session", "s1");
    assert!(!guard.is_held(&key));

    let _lease = guard.try_enter(key.clone());
    assert!(guard.is_held(&key));
    assert_eq!(guard.active_count(), 1);

    guard.release(&key);
    assert!(!guard.is_held(&key));
}

#[test]
fn guard_reentrant_access_increments_then_releases() {
    let guard = OssSandboxExecutionGuard::new();
    let key = SandboxIsolationKey::new("user", "u1");
    let _a = guard.try_enter(key.clone());
    let _b = guard.try_enter(key.clone());
    assert!(guard.is_held(&key));
    guard.release(&key);
    assert!(guard.is_held(&key));
    guard.release(&key);
    assert!(!guard.is_held(&key));
}

#[test]
fn distributed_store_rejects_blank_bucket() {
    let err = OssDistributedStore::create(common::mem_client(), "  ", None).unwrap_err();
    assert!(matches!(
        err,
        agentscope_extensions_oss::OssError::BlankBucket
    ));
}

#[tokio::test]
async fn distributed_store_composes_components_with_sub_prefixes() {
    let client = common::mem_client();
    let ds = OssDistributedStore::create(client.clone(), "bucket", Some("agentscope/")).unwrap();

    let state: OssAgentStateStore = ds.oss_agent_state_store().unwrap();
    state
        .save(Some("alice"), "s1", "k", serde_json::json!(1))
        .await;
    assert!(client.exists("agentscope/state/alice/s1/k.json").unwrap());

    let base: OssStore = ds.oss_base_store().unwrap();
    assert_eq!(base.key_prefix(), "agentscope/store/");

    let spec = ds.oss_snapshot_spec().unwrap();
    assert_eq!(spec.client().key_prefix(), "agentscope/snapshot/");

    let guard = ds.sandbox_execution_guard();
    assert_eq!(guard.active_count(), 0);
}

#[tokio::test]
async fn distributed_store_builds_harness_distributed_store() {
    let client = common::mem_client();
    let ds = OssDistributedStore::create(client, "bucket", None).unwrap();
    let harness_ds = ds.distributed_store().unwrap();
    assert!(harness_ds.agent_state_store.is_some());
}

#[tokio::test]
async fn distributed_store_base_store_is_usable_as_dyn() {
    let client = common::mem_client();
    let ds = OssDistributedStore::create(client, "bucket", None).unwrap();
    let store: Arc<dyn BaseStore> = Arc::new(ds.oss_base_store().unwrap());
    let mut m = serde_json::Map::new();
    m.insert("k".to_string(), serde_json::json!(1));
    store.put(&["ns".to_string()], "item", m);
    assert!(store.get(&["ns".to_string()], "item").is_some());
}

#[allow(unused_imports)]
use common::as_dyn as _as_dyn;
