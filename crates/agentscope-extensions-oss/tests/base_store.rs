//! Tests for [`OssStore`] (`BaseStore`), mirroring `OssBaseStoreTest`.

mod common;

use std::sync::Arc;

use agentscope_extensions_oss::{
    InMemoryOssClient, OssClient, OssError, OssListPage, OssObjectSummary, OssResult, OssStore,
};
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

#[test]
fn search_spans_multiple_pages() {
    // Five items behind a two-key page cap: `search` must aggregate every page
    // before applying limit/offset (real OSS always paginates).
    let client = Arc::new(InMemoryOssClient::with_page_size(2));
    let s = store(client);
    for key in ["a", "b", "c", "d", "e"] {
        s.put(&ns(), key, value());
    }

    let keys: Vec<String> = s
        .search(&ns(), 10, 0)
        .into_iter()
        .map(|i| i.key)
        .collect();
    assert_eq!(keys, vec!["a", "b", "c", "d", "e"]);

    // Offset paging still applies on top of the aggregated list.
    let page: Vec<String> = s
        .search(&ns(), 2, 3)
        .into_iter()
        .map(|i| i.key)
        .collect();
    assert_eq!(page, vec!["d", "e"]);
}

/// Client whose listing violates the `prefix` contract by returning a key from
/// another namespace.
///
/// Java `OssBaseStore.search` uses an unchecked
/// `substring(prefix.length(), …)`, so such a key throws; the Rust port must
/// surface an error rather than fabricating an item key.
#[derive(Debug, Default)]
struct OutsidePrefixClient;

impl OssClient for OutsidePrefixClient {
    fn put(&self, _object_key: &str, _content: &[u8]) -> OssResult<()> {
        Ok(())
    }

    fn get(&self, _object_key: &str) -> OssResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn exists(&self, _object_key: &str) -> OssResult<bool> {
        Ok(false)
    }

    fn delete(&self, _object_key: &str) -> OssResult<()> {
        Ok(())
    }

    fn list_page(
        &self,
        _prefix: &str,
        _max_keys: usize,
        _continuation_token: Option<&str>,
    ) -> OssResult<OssListPage> {
        Ok(OssListPage {
            objects: vec![OssObjectSummary {
                key: "test/store/ns2/rogue.json".to_string(),
            }],
            truncated: false,
            next_token: None,
        })
    }
}

/// Client whose listing always fails.
#[derive(Debug, Default)]
struct FailingListClient;

impl OssClient for FailingListClient {
    fn put(&self, _object_key: &str, _content: &[u8]) -> OssResult<()> {
        Ok(())
    }

    fn get(&self, _object_key: &str) -> OssResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn exists(&self, _object_key: &str) -> OssResult<bool> {
        Ok(false)
    }

    fn delete(&self, _object_key: &str) -> OssResult<()> {
        Ok(())
    }

    fn list_page(
        &self,
        _prefix: &str,
        _max_keys: usize,
        _continuation_token: Option<&str>,
    ) -> OssResult<OssListPage> {
        Err(OssError::Backend("boom".to_string()))
    }
}

fn store_with(client: Arc<dyn OssClient>) -> OssStore {
    OssStore::builder()
        .client(client)
        .bucket_name("test-bucket")
        .key_prefix("test/store/")
        .build()
        .unwrap()
}

#[test]
fn search_strict_errors_when_listed_key_is_outside_namespace_prefix() {
    let s = store_with(Arc::new(OutsidePrefixClient));

    match s.search_strict(&ns(), 10, 0) {
        Err(OssError::Backend(message)) => {
            assert!(message.contains("test/store/ns2/rogue.json"), "{message}");
            assert!(message.contains("test/store/ns1/"), "{message}");
        }
        other => panic!("expected OssError::Backend, got {other:?}"),
    }

    // The trait method cannot report errors, but it must not return an item
    // whose key is the whole object key (the old `unwrap_or(data_key)` bug).
    assert!(s.search(&ns(), 10, 0).is_empty());
}

#[test]
fn search_strict_propagates_listing_errors() {
    let s = store_with(Arc::new(FailingListClient));

    match s.search_strict(&ns(), 10, 0) {
        Err(OssError::Backend(message)) => assert_eq!(message, "boom"),
        other => panic!("expected OssError::Backend, got {other:?}"),
    }
    assert!(s.search(&ns(), 10, 0).is_empty());
}

#[test]
fn search_strict_rejects_malformed_item_json() {
    let client = common::mem_client();
    client
        .put("test/store/ns1/broken.json", b"not-json")
        .unwrap();
    let s = store(client);

    assert!(matches!(
        s.search_strict(&ns(), 10, 0),
        Err(OssError::Json(_))
    ));
    assert!(s.search(&ns(), 10, 0).is_empty());
}
