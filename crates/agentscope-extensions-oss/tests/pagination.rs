//! Multi-page listing coverage for [`InMemoryOssClient`] and for the stores that
//! walk listings through [`OssClient::list_all`].
//!
//! Real OSS/S3 backends cap a page (1000 keys for OSS) and hand out a
//! continuation token, so every listing path must work across page boundaries.
//! `InMemoryOssClient::with_page_size` emulates that cap offline.

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;
use agentscope_extensions_oss::{InMemoryOssClient, OssAgentStateStore, OssClient, OssListPage};

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
