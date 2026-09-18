//! Tests for [`OssStore`] (`BaseStore`), mirroring `OssBaseStoreTest`.

mod common;

use std::sync::Arc;

use agentscope_extensions_oss::{InMemoryOssClient, OssClient, OssError, OssStore};
use agentscope_harness::filesystem::BaseStore;

fn store(client: Arc<InMemoryOssClient>) -> OssStore {
    OssStore::builder()
        .client(client)
        .bucket_name("test-bucket")
        .key_prefix("test/store/")
        .build()
        .unwrap()
}

fn ns() -> Vec<String> {
    vec!["ns1".to_string()]
}

fn value() -> agentscope_harness::filesystem::remote::store::StoreValue {
    let mut m = serde_json::Map::new();
    m.insert("foo".to_string(), serde_json::json!("bar"));
    m
}

#[test]
fn builder_rejects_missing_client() {
    assert!(matches!(
        OssStore::builder().bucket_name("b").build().unwrap_err(),
        OssError::MissingClient
    ));
}

#[test]
fn builder_rejects_blank_bucket() {
    assert!(matches!(
        OssStore::builder()
            .client(common::mem_client())
            .bucket_name("")
            .build()
            .unwrap_err(),
        OssError::BlankBucket
    ));
}

#[test]
fn put_writes_data_and_version() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.put(&ns(), "my-key", value());
    assert!(client.exists("test/store/ns1/my-key.json").unwrap());
    assert_eq!(
        client.get("test/store/ns1/my-key.version").unwrap(),
        Some(b"1".to_vec())
    );
}

#[test]
fn get_returns_none_when_missing() {
    let s = store(common::mem_client());
    assert!(s.get(&ns(), "my-key").is_none());
}

#[test]
fn get_returns_item_with_version() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.put(&ns(), "my-key", value());
    s.put(&ns(), "my-key", value());
    s.put(&ns(), "my-key", value());

    let item = s.get(&ns(), "my-key").unwrap();
    assert_eq!(item.key, "my-key");
    assert_eq!(item.value.get("foo"), Some(&serde_json::json!("bar")));
    assert_eq!(item.version, 3);
}

#[test]
fn put_if_version_returns_false_on_mismatch() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.put(&ns(), "my-key", value()); // version -> 1
    let ok = s.put_if_version(&ns(), "my-key", value(), 3);
    assert!(!ok);
    // version unchanged
    assert_eq!(s.get(&ns(), "my-key").unwrap().version, 1);
}

#[test]
fn put_if_version_returns_true_on_match() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.put(&ns(), "my-key", value()); // version -> 1
    let ok = s.put_if_version(&ns(), "my-key", value(), 1);
    assert!(ok);
    assert_eq!(s.get(&ns(), "my-key").unwrap().version, 2);
}

#[test]
fn put_if_version_zero_creates_only_when_absent() {
    let client = common::mem_client();
    let s = store(client.clone());
    // First write with expected 0 succeeds.
    assert!(s.put_if_version(&ns(), "fresh", value(), 0));
    // Second write with expected 0 fails because object now exists.
    assert!(!s.put_if_version(&ns(), "fresh", value(), 0));
}

#[test]
fn search_paginates_and_orders() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.put(&ns(), "a", value());
    s.put(&ns(), "b", value());
    s.put(&ns(), "c", value());

    let all = s.search(&ns(), 10, 0);
    let keys: Vec<_> = all.iter().map(|i| i.key.clone()).collect();
    assert_eq!(keys, vec!["a", "b", "c"]);

    let page = s.search(&ns(), 1, 1);
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].key, "b");
}

#[test]
fn search_excludes_version_objects() {
    let client = common::mem_client();
    let s = store(client);
    s.put(&ns(), "a", value());
    let items = s.search(&ns(), 10, 0);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "a");
}

#[test]
fn delete_removes_data_and_version() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.put(&ns(), "my-key", value());
    s.delete(&ns(), "my-key");
    assert!(!client.exists("test/store/ns1/my-key.json").unwrap());
    assert!(!client.exists("test/store/ns1/my-key.version").unwrap());
    assert!(s.get(&ns(), "my-key").is_none());
}

#[test]
fn put_strips_leading_slash_from_key() {
    let client = common::mem_client();
    let s = store(client.clone());
    s.put(&ns(), "/my-key", value());
    assert!(client.exists("test/store/ns1/my-key.json").unwrap());
    let item = s.get(&ns(), "/my-key").unwrap();
    assert_eq!(item.key, "/my-key");
}

#[test]
fn search_scoped_to_namespace() {
    let client = common::mem_client();
    let s = store(client);
    s.put(&["ns1".to_string()], "a", value());
    s.put(&["ns2".to_string()], "b", value());
    assert_eq!(s.search(&["ns1".to_string()], 10, 0).len(), 1);
    assert_eq!(s.search(&["ns2".to_string()], 10, 0).len(), 1);
}

#[allow(unused_imports)]
use common::as_dyn as _as_dyn;
