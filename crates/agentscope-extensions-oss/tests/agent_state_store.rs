//! Tests for [`OssAgentStateStore`], mirroring `OssAgentStateStoreTest`.

mod common;

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;
use agentscope_extensions_oss::{InMemoryOssClient, OssAgentStateStore, OssClient, OssError};

fn store(client: Arc<InMemoryOssClient>) -> OssAgentStateStore {
    OssAgentStateStore::builder()
        .client(client)
        .bucket_name("test-bucket")
        .key_prefix("test/state/")
        .build()
        .unwrap()
}

#[test]
fn builder_rejects_missing_client() {
    let err = OssAgentStateStore::builder()
        .bucket_name("b")
        .build()
        .unwrap_err();
    assert!(matches!(err, OssError::MissingClient));
}

#[test]
fn builder_rejects_blank_bucket() {
    let err = OssAgentStateStore::builder()
        .client(common::mem_client())
        .bucket_name("   ")
        .build()
        .unwrap_err();
    assert!(matches!(err, OssError::BlankBucket));
}

#[tokio::test]
async fn save_single_state_uses_expected_object_key() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.save(Some("alice"), "s1", "agent_state", serde_json::json!({"hello": 1}))
        .await;

    assert!(client
        .exists("test/state/alice/s1/agent_state.json")
        .unwrap());
}

#[tokio::test]
async fn null_user_uses_anon() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.save(None, "s1", "agent_state", serde_json::json!("data")).await;
    assert!(client
        .exists("test/state/__anon__/s1/agent_state.json")
        .unwrap());
}

#[tokio::test]
async fn get_single_state_roundtrip() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.save(Some("alice"), "s1", "k", serde_json::json!({"a": 1})).await;
    let got = s.get(Some("alice"), "s1", "k").await;
    assert_eq!(got, Some(serde_json::json!({"a": 1})));
}

#[tokio::test]
async fn get_single_state_returns_none_when_missing() {
    let s = store(common::mem_client());
    assert_eq!(s.get(Some("alice"), "s1", "agent_state").await, None);
}

#[tokio::test]
async fn save_list_and_get_list_roundtrip() {
    let client = common::mem_client();
    let s = store(client.clone());
    let values = vec![serde_json::json!({"i": 0}), serde_json::json!({"i": 1})];
    s.save_list(Some("alice"), "s1", "msgs", &values).await;

    assert!(client.exists("test/state/alice/s1/msgs.list.json").unwrap());
    assert!(client.exists("test/state/alice/s1/msgs.list.hash").unwrap());

    let got = s.get_list(Some("alice"), "s1", "msgs").await;
    assert_eq!(got, values);
}

#[tokio::test]
async fn get_list_empty_when_missing() {
    let s = store(common::mem_client());
    assert!(s.get_list(Some("alice"), "s1", "none").await.is_empty());
}

#[tokio::test]
async fn save_list_append_rewrites_when_count_grows() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.save_list(Some("alice"), "s1", "msgs", &[serde_json::json!(1)])
        .await;
    s.save_list(
        Some("alice"),
        "s1",
        "msgs",
        &[serde_json::json!(1), serde_json::json!(2)],
    )
    .await;
    let got = s.get_list(Some("alice"), "s1", "msgs").await;
    assert_eq!(got.len(), 2);
}

#[tokio::test]
async fn exists_false_when_no_objects() {
    let s = store(common::mem_client());
    assert!(!s.exists(Some("alice"), "s1").await);
}

#[tokio::test]
async fn exists_true_when_objects_present() {
    let client = common::mem_client();
    let s = store(client);
    s.save(Some("alice"), "s1", "agent_state", serde_json::json!(1))
        .await;
    assert!(s.exists(Some("alice"), "s1").await);
}

#[tokio::test]
async fn list_session_ids_extracts_ids() {
    let client = common::mem_client();
    let s = store(client);
    s.save(Some("alice"), "sess-a", "k", serde_json::json!(1)).await;
    s.save(Some("alice"), "sess-b", "k", serde_json::json!(1)).await;
    let mut ids = s.list_session_ids(Some("alice")).await;
    ids.sort();
    assert_eq!(ids, vec!["sess-a".to_string(), "sess-b".to_string()]);
}

#[tokio::test]
async fn delete_key_removes_state_and_list_objects() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.save(Some("alice"), "s1", "k", serde_json::json!(1)).await;
    s.save_list(Some("alice"), "s1", "k", &[serde_json::json!(1)])
        .await;

    // AgentStateStore only defines session-level delete; emulate key delete by
    // checking session delete below.
    s.delete(Some("alice"), "s1").await;
    assert!(!s.exists(Some("alice"), "s1").await);
    assert!(client.is_empty());
}

#[tokio::test]
async fn close_shuts_down_client() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.close();
    assert!(client.is_shut_down());
}

#[tokio::test]
async fn key_prefix_is_normalized_with_trailing_slash() {
    let s = OssAgentStateStore::builder()
        .client(common::mem_client())
        .bucket_name("b")
        .key_prefix("/test/state")
        .build()
        .unwrap();
    assert_eq!(s.key_prefix(), "test/state/");
}

// Keep Arc/OssClient imports used on all platforms.
#[allow(unused_imports)]
use common::as_dyn as _as_dyn;
