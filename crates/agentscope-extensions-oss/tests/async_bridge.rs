//! OSS-01 regression: the stores must be usable from inside a Tokio runtime.
//!
//! `OssClient` is synchronous while `S3OssClient` bridges to the async `rust-s3`
//! crate. Before the executor seam existed, `S3OssClient` called
//! `Runtime::block_on` on the *caller's* thread. `AgentStateStore::save/get` are
//! `async fn` bodies that call the blocking client inline, so any
//! `#[tokio::test]` using the S3 backend panicked with
//! "Cannot start a runtime from within a runtime". These cases drive the real
//! `S3OssClient` against `httpmock`, so they fail (panic) on the pre-fix
//! implementation and pass once every request is executed on the client's own
//! worker thread.
//!
//! Both runtime flavours are covered: `#[tokio::test]` defaults to a
//! current-thread runtime (where `block_in_place` would also panic) and one case
//! uses a multi-thread runtime.

#![cfg(feature = "s3")]

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;
use agentscope_extensions_oss::{OssAgentStateStore, OssClient, OssStore, S3OssClient};
use agentscope_harness::filesystem::BaseStore;
use httpmock::prelude::*;

const BUCKET: &str = "test-bucket";
const STATE_PREFIX: &str = "agentscope/state/";
const STORE_PREFIX: &str = "agentscope/store/";

fn test_client(server: &MockServer) -> Arc<dyn OssClient> {
    Arc::new(
        S3OssClient::builder()
            .bucket(BUCKET)
            .region("us-east-1")
            .endpoint(server.base_url())
            .access_key("test-access-key")
            .secret_key("test-secret-key")
            .env_credentials(false)
            .build()
            .expect("S3OssClient should build against the mock endpoint"),
    )
}

fn state_store(server: &MockServer) -> OssAgentStateStore {
    OssAgentStateStore::builder()
        .client(test_client(server))
        .bucket_name(BUCKET)
        .key_prefix(STATE_PREFIX)
        .build()
        .expect("state store should build")
}

fn base_store(server: &MockServer) -> OssStore {
    OssStore::builder()
        .client(test_client(server))
        .bucket_name(BUCKET)
        .key_prefix(STORE_PREFIX)
        .build()
        .expect("base store should build")
}

fn ns() -> Vec<String> {
    vec!["agents".to_string(), "tools".to_string()]
}

fn value() -> agentscope_harness::filesystem::remote::store::StoreValue {
    let mut m = serde_json::Map::new();
    m.insert("foo".to_string(), serde_json::json!("bar"));
    m
}

fn no_such_key() -> String {
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
     <Error><Code>NoSuchKey</Code><Message>missing</Message></Error>"
        .to_string()
}

fn list_xml(prefix: &str, keys: &[&str]) -> String {
    let contents = keys
        .iter()
        .map(|key| {
            format!(
                "<Contents>\
                   <Key>{key}</Key>\
                   <LastModified>2026-10-31T00:00:00.000Z</LastModified>\
                   <ETag>\"etag\"</ETag>\
                   <Size>1</Size>\
                   <StorageClass>STANDARD</StorageClass>\
                 </Contents>"
            )
        })
        .collect::<String>();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
           <Name>{BUCKET}</Name>\
           <Prefix>{prefix}</Prefix>\
           <KeyCount>{}</KeyCount>\
           <MaxKeys>1000</MaxKeys>\
           <IsTruncated>false</IsTruncated>\
           {contents}\
         </ListBucketResult>",
        keys.len()
    )
}

#[tokio::test]
async fn state_store_save_and_get_from_a_tokio_runtime_do_not_panic() {
    // `start_async` / `mock_async` / `assert_*_async` are mandatory here: their
    // blocking twins create and `block_on` their own runtime, which panics inside
    // an async test (the very failure mode under test).
    let server = MockServer::start_async().await;
    let object_path = format!("/{BUCKET}/{STATE_PREFIX}u1/s1/k.json");
    let payload = r#"{"count":7}"#;

    let put_mock = server
        .mock_async(|when, then| {
            when.method(PUT)
                .path(object_path.clone())
                .body_contains("count");
            then.status(200);
        })
        .await;
    let get_mock = server
        .mock_async(|when, then| {
            when.method(GET).path(object_path.clone());
            then.status(200)
                .header("content-type", "application/json")
                .body(payload);
        })
        .await;

    let store = state_store(&server);

    // Pre-fix: `save` is an `async fn` calling the blocking client inline, and
    // `S3OssClient` used `block_on` on the very thread polling this future.
    store
        .save(Some("u1"), "s1", "k", serde_json::json!({ "count": 7 }))
        .await;
    let round_tripped = store.get(Some("u1"), "s1", "k").await;

    assert_eq!(round_tripped, Some(serde_json::json!({ "count": 7 })));
    put_mock.assert_hits_async(1).await;
    get_mock.assert_hits_async(1).await;
    // Dropping the store inside the runtime must not panic either (the client
    // joins its worker thread instead of dropping a runtime in place).
    drop(store);
}

#[tokio::test]
async fn base_store_put_from_a_tokio_runtime_does_not_panic() {
    let server = MockServer::start_async().await;
    let data_path = format!("/{BUCKET}/{STORE_PREFIX}agents/tools/item.json");
    let version_path = format!("/{BUCKET}/{STORE_PREFIX}agents/tools/item.version");

    // A fresh key: the `.version` sidecar does not exist yet.
    let version_read = server
        .mock_async(|when, then| {
            when.method(GET).path(version_path.clone());
            then.status(404).body(no_such_key());
        })
        .await;
    let data_put = server
        .mock_async(|when, then| {
            when.method(PUT)
                .path(data_path.clone())
                .body_contains("foo");
            then.status(200);
        })
        .await;
    let version_put = server
        .mock_async(|when, then| {
            when.method(PUT).path(version_path.clone()).body("1");
            then.status(200);
        })
        .await;

    let store = base_store(&server);

    // Pre-fix this synchronously blocking `put` also panicked, because the future
    // was polled from inside the test's runtime thread.
    store.put(&ns(), "item", value());

    version_read.assert_hits_async(1).await;
    data_put.assert_hits_async(1).await;
    version_put.assert_hits_async(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_store_search_from_a_multi_thread_runtime_does_not_panic() {
    let server = MockServer::start_async().await;
    let namespace_prefix = format!("{STORE_PREFIX}agents/tools/");
    let item_key = format!("{namespace_prefix}item.json");
    let data_path = format!("/{BUCKET}/{item_key}");
    let version_path = format!("/{BUCKET}/{namespace_prefix}item.version");

    let list_mock = server
        .mock_async(|when, then| {
            when.method(GET)
                .path(format!("/{BUCKET}/"))
                .query_param("list-type", "2")
                .query_param("prefix", namespace_prefix.clone())
                // `search(limit = 10, offset = 0)` only asks for the keys it needs
                // (`remaining items × 2`, since each item is `{key}.json` + `{key}.version`)
                // instead of listing the namespace.
                .query_param("max-keys", "20");
            then.status(200)
                .header("content-type", "application/xml")
                .body(list_xml(&namespace_prefix, &[item_key.as_str()]));
        })
        .await;
    let data_mock = server
        .mock_async(|when, then| {
            when.method(GET).path(data_path.clone());
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"foo":"bar"}"#);
        })
        .await;
    let version_mock = server
        .mock_async(|when, then| {
            when.method(GET).path(version_path.clone());
            then.status(200).body("1");
        })
        .await;

    let store = base_store(&server);
    let items = store.search(&ns(), 10, 0);

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "item");
    assert_eq!(items[0].version, 1);
    assert_eq!(items[0].value.get("foo"), Some(&serde_json::json!("bar")));

    list_mock.assert_hits_async(1).await;
    data_mock.assert_hits_async(1).await;
    version_mock.assert_hits_async(1).await;
}
