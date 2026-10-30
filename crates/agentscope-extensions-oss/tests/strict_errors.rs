//! Strict (error-propagating) twins of the trait methods, plus accessor coverage.
//!
//! The core/harness traits (`AgentStateStore`, `BaseStore`) have no error channel, so Java's
//! "throw `RuntimeException` on every backend failure" cannot be expressed there. Each store
//! therefore exposes `*_strict` inherent methods; these tests pin down both halves:
//! the trait methods degrade quietly (never fabricate data), the strict twins surface the
//! failure, and "absent" still reads as `Ok(None)` / `Ok(vec![])` rather than an error.

mod common;

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;
use agentscope_extensions_oss::{
    OssAgentStateStore, OssClient, OssDistributedStore, OssError, OssListPage,
    OssRemoteSnapshotClient, OssResult, OssSandboxExecutionGuard, OssStore,
};
use agentscope_harness::agent::sandbox::{
    RemoteSnapshotClient, SandboxExecutionGuard, SandboxIsolationKey,
};
use agentscope_harness::filesystem::remote::store::{BaseStore, StoreValue};
use serde_json::json;

/// A backend that fails every operation.
#[derive(Debug)]
struct FailingClient;

impl OssClient for FailingClient {
    fn put(&self, _object_key: &str, _content: &[u8]) -> OssResult<()> {
        Err(OssError::Backend("put boom".into()))
    }

    fn get(&self, _object_key: &str) -> OssResult<Option<Vec<u8>>> {
        Err(OssError::Backend("get boom".into()))
    }

    fn exists(&self, _object_key: &str) -> OssResult<bool> {
        Err(OssError::Backend("exists boom".into()))
    }

    fn delete(&self, _object_key: &str) -> OssResult<()> {
        Err(OssError::Backend("delete boom".into()))
    }

    fn list_page(
        &self,
        _prefix: &str,
        _max_keys: usize,
        _continuation_token: Option<&str>,
    ) -> OssResult<OssListPage> {
        Err(OssError::Backend("list boom".into()))
    }
}

fn state_store(client: Arc<dyn OssClient>) -> OssAgentStateStore {
    OssAgentStateStore::builder()
        .client(client)
        .bucket_name("bucket")
        .build()
        .unwrap()
}

fn base_store(client: Arc<dyn OssClient>) -> OssStore {
    OssStore::builder()
        .client(client)
        .bucket_name("bucket")
        .build()
        .unwrap()
}

fn ns() -> Vec<String> {
    vec!["ns".to_string()]
}

fn value() -> StoreValue {
    json!({ "a": 1 }).as_object().unwrap().clone()
}

#[tokio::test]
async fn state_strict_variants_surface_backend_failures() {
    let store = state_store(Arc::new(FailingClient));

    assert!(store.get_strict(None, "s1", "k").await.is_err());
    assert!(store.get_list_strict(None, "s1", "k").await.is_err());
    assert!(store.save_strict(None, "s1", "k", json!(1)).await.is_err());
    assert!(store
        .save_list_strict(None, "s1", "k", &[json!(1)])
        .await
        .is_err());
    assert!(store.exists_strict(None, "s1").await.is_err());
    assert!(store.delete_session_strict(None, "s1").await.is_err());
    assert!(store.list_session_ids_strict(None).await.is_err());
}

#[tokio::test]
async fn state_trait_methods_degrade_to_empty_without_fabricating_data() {
    let store = state_store(Arc::new(FailingClient));

    assert!(store.get(None, "s1", "k").await.is_none());
    assert!(store.get_list(None, "s1", "k").await.is_empty());
    assert!(!store.exists(None, "s1").await);
    assert!(store.list_session_ids(None).await.is_empty());
    // Neither call may panic or invent a value on a failing backend.
    store.save(None, "s1", "k", json!(1)).await;
    store.save_list(None, "s1", "k", &[json!(1)]).await;
    store.delete(None, "s1").await;
}

#[tokio::test]
async fn state_absent_values_are_ok_none_not_errors() {
    let store = state_store(common::mem_client());

    assert_eq!(store.get_strict(None, "s1", "k").await.unwrap(), None);
    assert!(store
        .get_list_strict(None, "s1", "k")
        .await
        .unwrap()
        .is_empty());
    assert!(!store.exists_strict(None, "s1").await.unwrap());
    assert!(store.list_session_ids_strict(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn corrupt_state_payload_is_visible_only_through_the_strict_variant() {
    let client = common::mem_client();
    client
        .put("agentscope/state/__anon__/s1/k.json", b"{not-json")
        .unwrap();
    let store = state_store(common::as_dyn(&client));

    assert!(store.get(None, "s1", "k").await.is_none());
    assert!(matches!(
        store.get_strict(None, "s1", "k").await,
        Err(OssError::Json(_))
    ));
}

#[tokio::test]
async fn state_save_list_rewrites_when_the_hash_sidecar_is_missing() {
    let client = common::mem_client();
    // Stale list object without a `.list.hash` sidecar (legacy write / tampering).
    client
        .put("agentscope/state/__anon__/s1/log.list.json", b"[1,2,3]")
        .unwrap();
    let store = state_store(common::as_dyn(&client));

    // `existing_count` must be read from the list object itself (Java `existingCount`);
    // otherwise a shrunken/emptied list would never be rewritten.
    store
        .save_list_strict(None, "s1", "log", &[json!(9)])
        .await
        .unwrap();
    assert_eq!(
        store.get_list_strict(None, "s1", "log").await.unwrap(),
        vec![json!(9)]
    );
    store
        .save_list_strict(None, "s1", "log", &[])
        .await
        .unwrap();
    assert!(store
        .get_list_strict(None, "s1", "log")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn state_store_keeps_the_core_defaults_for_versioning() {
    // Documented design decision (tracker §8.4): `OssClient` has no conditional-put seam and
    // Java `OssAgentStateStore` does not override the versioned API either, so the core
    // defaults must stay in force: no optimistic concurrency, `save_if_version` degrades to an
    // unconditional write and reports `UNVERSIONED`.
    let client = common::mem_client();
    let store = state_store(common::as_dyn(&client));
    assert!(!AgentStateStore::supports_versioning(&store));
    assert_eq!(
        AgentStateStore::get_versioned(&store, None, "s1", "k")
            .await
            .version,
        agentscope_core::state::UNVERSIONED
    );
    assert_eq!(
        AgentStateStore::save_if_version(&store, None, "s1", "k", json!(1), 7).await,
        agentscope_core::state::UNVERSIONED
    );
    // The degraded write still lands (last-writer-wins), like Java.
    assert_eq!(store.get(None, "s1", "k").await, Some(json!(1)));
}

#[tokio::test]
async fn trait_delete_key_removes_only_the_targeted_key() {
    let client = common::mem_client();
    let store = state_store(common::as_dyn(&client));
    AgentStateStore::save(&store, Some("alice"), "s1", "k", json!(1)).await;
    AgentStateStore::save(&store, Some("alice"), "s1", "keep", json!(2)).await;

    AgentStateStore::delete_key(&store, Some("alice"), "s1", "k")
        .await
        .unwrap();

    assert_eq!(
        AgentStateStore::get(&store, Some("alice"), "s1", "k").await,
        None
    );
    assert_eq!(
        AgentStateStore::get(&store, Some("alice"), "s1", "keep").await,
        Some(json!(2))
    );
    assert!(!client
        .exists("agentscope/state/alice/s1/k.json")
        .unwrap());
    assert!(!client
        .exists("agentscope/state/alice/s1/k.list.json")
        .unwrap());
    assert!(!client
        .exists("agentscope/state/alice/s1/k.list.hash")
        .unwrap());

    // The trait method keeps the error channel: a blank session id is reported, not ignored
    // (the default trait impl would have reported "backend does not support per-key delete").
    let err = AgentStateStore::delete_key(&store, Some("alice"), "  ", "k")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("OSS per-key delete failed"), "{err}");
}

#[test]
fn base_store_strict_variants_surface_backend_failures() {
    let store = base_store(Arc::new(FailingClient));

    assert!(store.get_strict(&ns(), "k").is_err());
    assert!(store.put_strict(&ns(), "k", value()).is_err());
    assert!(store.put_if_version_strict(&ns(), "k", value(), 0).is_err());
    assert!(store.delete_strict(&ns(), "k").is_err());
    assert!(store.search_strict(&ns(), 10, 0).is_err());

    // Trait methods degrade quietly (log only), never reporting a successful CAS.
    assert!(store.get(&ns(), "k").is_none());
    assert!(!store.put_if_version(&ns(), "k", value(), 0));
    assert!(store.search(&ns(), 10, 0).is_empty());
    store.put(&ns(), "k", value());
    store.delete(&ns(), "k");
}

#[test]
fn unparsable_version_object_is_a_backend_error_in_strict_mode() {
    let client = common::mem_client();
    client
        .put("agentscope/store/ns/k.json", br#"{"a":1}"#)
        .unwrap();
    client
        .put("agentscope/store/ns/k.version", b"not-a-number")
        .unwrap();
    let store = base_store(common::as_dyn(&client));

    // Java `Long.parseLong` throws here; the trait path must stay quiet.
    assert!(store.get(&ns(), "k").is_none());
    assert!(matches!(
        store.get_strict(&ns(), "k"),
        Err(OssError::Backend(_))
    ));
}

#[test]
fn accessors_expose_the_configured_bucket_and_prefix() {
    let client = common::mem_client();

    let state = OssAgentStateStore::builder()
        .client(common::as_dyn(&client))
        .bucket_name("bucket")
        .key_prefix("custom")
        .build()
        .unwrap();
    assert_eq!(state.bucket_name(), "bucket");
    assert_eq!(state.key_prefix(), "custom/");

    let store = OssStore::builder()
        .client(common::as_dyn(&client))
        .bucket_name("bucket")
        .key_prefix("kv")
        .build()
        .unwrap();
    assert_eq!(store.bucket_name(), "bucket");
    assert_eq!(store.key_prefix(), "kv/");

    let snap = OssRemoteSnapshotClient::new(common::as_dyn(&client), "bucket", Some("snap"))
        .unwrap();
    assert_eq!(snap.bucket_name(), "bucket");
    assert_eq!(snap.key_prefix(), "snap/");

    let composed = OssDistributedStore::create(common::as_dyn(&client), "bucket", Some("root"))
        .unwrap();
    assert_eq!(composed.bucket_name(), "bucket");
    assert_eq!(composed.key_prefix(), "root/");
}

#[test]
fn guard_wraps_an_existing_harness_guard() {
    let inner = Arc::new(SandboxExecutionGuard::new());
    let guard = OssSandboxExecutionGuard::from_harness(inner.clone());
    assert!(Arc::ptr_eq(guard.inner(), &inner));

    let key = SandboxIsolationKey::new("user", "u-1");
    let lease = guard.try_enter(key.clone());
    assert!(!lease.is_noop());
    assert!(inner.is_held(&key));
    assert_eq!(guard.active_count(), 1);

    guard.release(&key);
    assert!(!inner.is_held(&key));
    assert_eq!(guard.active_count(), 0);
}

#[tokio::test]
async fn snapshot_download_distinguishes_missing_from_backend_failure() {
    // Missing object → the typed `SnapshotNotFound` variant (Java `FileNotFoundException`).
    let client = common::mem_client();
    let snap = OssRemoteSnapshotClient::new(common::as_dyn(&client), "bucket", None).unwrap();
    let missing = snap.download("absent").await;
    assert!(missing.is_err());
    assert!(missing
        .unwrap_err()
        .to_string()
        .contains("snapshot not found in OSS"));

    let failing = OssRemoteSnapshotClient::new(Arc::new(FailingClient), "bucket", None).unwrap();
    let err = failing.download("any").await;
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("download failed"));
}
