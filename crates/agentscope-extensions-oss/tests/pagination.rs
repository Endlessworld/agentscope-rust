//! Multi-page listing coverage for [`InMemoryOssClient`] and for the stores that
//! walk listings through [`OssClient::list_all`].
//!
//! Real OSS/S3 backends cap a page (1000 keys for OSS) and hand out a
//! continuation token, so every listing path must work across page boundaries.
//! `InMemoryOssClient::with_page_size` emulates that cap offline.

use std::sync::{Arc, Mutex};

use agentscope_core::state::AgentStateStore;
use agentscope_extensions_oss::{
    InMemoryOssClient, OssAgentStateStore, OssClient, OssError, OssListPage, OssResult, OssStore,
};
use agentscope_harness::filesystem::remote::store::{BaseStore, StoreValue};
use serde_json::json;

fn page_keys(page: &OssListPage) -> Vec<String> {
    page.objects.iter().map(|o| o.key.clone()).collect()
}

fn seeded(page_size: usize, keys: &[&str]) -> Arc<InMemoryOssClient> {
    let client = Arc::new(InMemoryOssClient::with_page_size(page_size));
    for key in keys {
        client.put(key, b"v").unwrap();
    }
    client
}

#[test]
fn page_size_is_exposed_and_clamped_to_at_least_one() {
    assert_eq!(InMemoryOssClient::new().page_size(), None);
    assert_eq!(InMemoryOssClient::with_page_size(0).page_size(), Some(1));
    assert_eq!(InMemoryOssClient::with_page_size(2).page_size(), Some(2));
}

#[test]
fn list_page_returns_first_page_and_continuation_token() {
    let client = seeded(2, &["p/a", "p/b", "p/c", "p/d", "p/e"]);
    let page = client.list_page("p/", 1000, None).unwrap();
    assert_eq!(page_keys(&page), vec!["p/a", "p/b"]);
    assert!(page.truncated);
    assert_eq!(page.next_token.as_deref(), Some("p/b"));
}

#[test]
fn list_page_resumes_after_continuation_token() {
    let client = seeded(2, &["p/a", "p/b", "p/c", "p/d", "p/e"]);
    let page = client.list_page("p/", 1000, Some("p/b")).unwrap();
    assert_eq!(page_keys(&page), vec!["p/c", "p/d"]);
    assert!(page.truncated);
    assert_eq!(page.next_token.as_deref(), Some("p/d"));
}

#[test]
fn list_page_last_page_is_not_truncated() {
    let client = seeded(2, &["p/a", "p/b", "p/c", "p/d", "p/e"]);
    let page = client.list_page("p/", 1000, Some("p/d")).unwrap();
    assert_eq!(page_keys(&page), vec!["p/e"]);
    assert!(!page.truncated);
    assert!(page.next_token.is_none());
}

#[test]
fn list_page_exact_boundary_last_page_is_not_truncated() {
    // 4 keys with a 2-key cap: the second page ends exactly on the last key and
    // must not claim `truncated` (otherwise `list_all` would issue one extra
    // empty request).
    let client = seeded(2, &["p/a", "p/b", "p/c", "p/d"]);
    let first = client.list_page("p/", 10, None).unwrap();
    assert_eq!(page_keys(&first), vec!["p/a", "p/b"]);
    assert!(first.truncated);

    let second = client
        .list_page("p/", 10, first.next_token.as_deref())
        .unwrap();
    assert_eq!(page_keys(&second), vec!["p/c", "p/d"]);
    assert!(!second.truncated);
    assert!(second.next_token.is_none());
}

#[test]
fn list_page_without_matches_returns_empty_page() {
    let client = seeded(2, &["p/a"]);
    let page = client.list_page("missing/", 10, None).unwrap();
    assert!(page.objects.is_empty());
    assert!(!page.truncated);
    assert!(page.next_token.is_none());
}

#[test]
fn list_page_token_past_last_key_returns_empty_page() {
    let client = seeded(2, &["p/a", "p/b"]);
    let page = client.list_page("p/", 10, Some("p/z")).unwrap();
    assert!(page.objects.is_empty());
    assert!(!page.truncated);
    assert!(page.next_token.is_none());
}

#[test]
fn list_page_zero_max_keys_returns_empty_untruncated_page() {
    let client = seeded(2, &["p/a"]);
    let page = client.list_page("p/", 0, None).unwrap();
    assert!(page.objects.is_empty());
    assert!(!page.truncated);
    assert!(page.next_token.is_none());
}

#[test]
fn list_page_honours_caller_max_keys_below_page_cap() {
    let client = seeded(5, &["p/a", "p/b", "p/c"]);
    let page = client.list_page("p/", 2, None).unwrap();
    assert_eq!(page_keys(&page), vec!["p/a", "p/b"]);
    assert!(page.truncated);
    assert_eq!(page.next_token.as_deref(), Some("p/b"));
}

#[test]
fn list_all_aggregates_every_page() {
    let client = seeded(2, &["p/a", "p/b", "p/c", "p/d", "p/e"]);
    assert_eq!(
        client.list_all("p/").unwrap(),
        vec!["p/a", "p/b", "p/c", "p/d", "p/e"]
    );
}

#[test]
fn list_all_returns_empty_vec_for_missing_prefix() {
    let client = seeded(2, &["p/a"]);
    assert!(client.list_all("other/").unwrap().is_empty());
}

fn state_store(client: Arc<InMemoryOssClient>) -> OssAgentStateStore {
    OssAgentStateStore::builder()
        .client(client)
        .bucket_name("test-bucket")
        .key_prefix("test/state/")
        .build()
        .unwrap()
}

#[tokio::test]
async fn state_store_list_session_ids_spans_pages() {
    // Two objects per session with a 2-key page cap: session ids only surface
    // when every page is visited.
    let client = Arc::new(InMemoryOssClient::with_page_size(2));
    let store = state_store(client.clone());
    for session in ["s1", "s2", "s3"] {
        store
            .save(Some("alice"), session, "k", serde_json::json!(1))
            .await;
        store
            .save_list(Some("alice"), session, "m", &[serde_json::json!(1)])
            .await;
    }

    let mut ids = store.list_session_ids(Some("alice")).await;
    ids.sort();
    assert_eq!(ids, vec!["s1", "s2", "s3"]);
}

#[tokio::test]
async fn state_store_session_delete_spans_pages() {
    let client = Arc::new(InMemoryOssClient::with_page_size(2));
    let store = state_store(client.clone());
    store
        .save(Some("alice"), "s1", "a", serde_json::json!(1))
        .await;
    store
        .save(Some("alice"), "s1", "b", serde_json::json!(2))
        .await;
    store
        .save_list(Some("alice"), "s1", "c", &[serde_json::json!(3)])
        .await;
    store
        .save(Some("alice"), "s2", "a", serde_json::json!(4))
        .await;

    store.delete(Some("alice"), "s1").await;

    assert!(!store.exists(Some("alice"), "s1").await);
    assert!(client.list_all("test/state/alice/s1/").unwrap().is_empty());
    assert_eq!(
        client.list_all("test/state/alice/s2/").unwrap(),
        vec!["test/state/alice/s2/a.json"]
    );
}

fn ns() -> Vec<String> {
    vec!["ns1".to_string()]
}

fn value() -> StoreValue {
    json!({ "a": 1 }).as_object().unwrap().clone()
}

/// Forwards to an in-memory backend and records the `max_keys` of every listing request.
#[derive(Debug)]
struct CountingClient {
    inner: Arc<InMemoryOssClient>,
    max_keys: Mutex<Vec<usize>>,
}

impl CountingClient {
    fn new(inner: Arc<InMemoryOssClient>) -> Self {
        Self {
            inner,
            max_keys: Mutex::new(Vec::new()),
        }
    }

    fn list_requests(&self) -> Vec<usize> {
        self.max_keys.lock().unwrap().clone()
    }
}

impl OssClient for CountingClient {
    fn put(&self, object_key: &str, content: &[u8]) -> OssResult<()> {
        self.inner.put(object_key, content)
    }

    fn get(&self, object_key: &str) -> OssResult<Option<Vec<u8>>> {
        self.inner.get(object_key)
    }

    fn exists(&self, object_key: &str) -> OssResult<bool> {
        self.inner.exists(object_key)
    }

    fn delete(&self, object_key: &str) -> OssResult<()> {
        self.inner.delete(object_key)
    }

    fn list_page(
        &self,
        prefix: &str,
        max_keys: usize,
        continuation_token: Option<&str>,
    ) -> OssResult<OssListPage> {
        self.max_keys.lock().unwrap().push(max_keys);
        self.inner.list_page(prefix, max_keys, continuation_token)
    }
}

fn base_store(client: Arc<dyn OssClient>) -> OssStore {
    OssStore::builder()
        .client(client)
        .bucket_name("test-bucket")
        .key_prefix("test/store/")
        .build()
        .unwrap()
}

/// Seeds `items` base-store items (`2 * items` objects) behind `page_size` (when set).
fn seeded_store(page_size: Option<usize>, items: usize) -> (Arc<CountingClient>, OssStore) {
    let inner = match page_size {
        Some(page_size) => Arc::new(InMemoryOssClient::with_page_size(page_size)),
        None => Arc::new(InMemoryOssClient::new()),
    };
    let counting = Arc::new(CountingClient::new(inner));
    let store = base_store(counting.clone());
    for index in 0..items {
        store.put(&ns(), &format!("k{index:02}"), value());
    }
    (counting, store)
}

#[test]
fn search_asks_only_for_the_requested_page() {
    // 60 items = 120 objects: listing the whole namespace would ask for (and keep) far
    // more keys than a 10-item page needs.
    let (client, store) = seeded_store(None, 60);

    let keys: Vec<String> = store
        .search(&ns(), 10, 0)
        .into_iter()
        .map(|item| item.key)
        .collect();
    assert_eq!(keys, (0..10).map(|i| format!("k{i:02}")).collect::<Vec<_>>());
    assert_eq!(client.list_requests(), vec![20]);
}

#[test]
fn search_follows_tokens_until_the_offset_page_is_complete() {
    // A 40-key page cap: the offset page is only reachable by following continuation
    // tokens, and the walk stops at `offset + limit` items instead of listing the tail.
    let (client, store) = seeded_store(Some(40), 60);

    let keys: Vec<String> = store
        .search(&ns(), 10, 40)
        .into_iter()
        .map(|item| item.key)
        .collect();
    assert_eq!(keys, (40..50).map(|i| format!("k{i:02}")).collect::<Vec<_>>());

    let requests = client.list_requests();
    assert_eq!(requests, vec![100, 60, 20]);
    // 120 objects exist; with the 40-key page cap the walk only reads 100 of them.
    let fetched: usize = requests.iter().map(|max_keys| (*max_keys).min(40)).sum();
    assert!(fetched < 120, "{requests:?}");
}

/// Forwards everything to in-memory storage but refuses per-key `delete`, so a session
/// delete must go through the batched [`OssClient::delete_many`].
#[derive(Debug)]
struct BatchDeleteOnlyClient {
    inner: Arc<InMemoryOssClient>,
    batches: Mutex<Vec<usize>>,
}

impl OssClient for BatchDeleteOnlyClient {
    fn put(&self, object_key: &str, content: &[u8]) -> OssResult<()> {
        self.inner.put(object_key, content)
    }

    fn get(&self, object_key: &str) -> OssResult<Option<Vec<u8>>> {
        self.inner.get(object_key)
    }

    fn exists(&self, object_key: &str) -> OssResult<bool> {
        self.inner.exists(object_key)
    }

    fn delete(&self, _object_key: &str) -> OssResult<()> {
        Err(OssError::Backend(
            "per-key delete must not be used".to_string(),
        ))
    }

    fn list_page(
        &self,
        prefix: &str,
        max_keys: usize,
        continuation_token: Option<&str>,
    ) -> OssResult<OssListPage> {
        self.inner.list_page(prefix, max_keys, continuation_token)
    }

    fn delete_many(&self, object_keys: &[String]) -> OssResult<()> {
        self.batches.lock().unwrap().push(object_keys.len());
        for object_key in object_keys {
            self.inner.delete(object_key)?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn session_delete_uses_one_batched_request() {
    let inner = Arc::new(InMemoryOssClient::new());
    let client = Arc::new(BatchDeleteOnlyClient {
        inner: Arc::clone(&inner),
        batches: Mutex::new(Vec::new()),
    });
    let store = OssAgentStateStore::builder()
        .client(client.clone())
        .bucket_name("test-bucket")
        .key_prefix("test/state/")
        .build()
        .unwrap();

    store
        .save(Some("alice"), "s1", "a", serde_json::json!(1))
        .await;
    store
        .save(Some("alice"), "s1", "b", serde_json::json!(2))
        .await;
    store
        .save_list(Some("alice"), "s1", "c", &[serde_json::json!(3)])
        .await;

    store.delete(Some("alice"), "s1").await;

    // a.json + b.json + c.list.json + c.list.hash in a single batch.
    assert_eq!(*client.batches.lock().unwrap(), vec![4]);
    assert!(inner.is_empty());
}
