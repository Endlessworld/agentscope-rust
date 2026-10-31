//! OSS-02 / OSS-03 regression: the stores' compare-and-set must be a real CAS.
//!
//! `InMemoryOssClient` implements the conditional-write seam
//! ([`OssClient::put_conditional`]), so it stands in for a backend that evaluates
//! `If-Match` / `If-None-Match` server side. Before the seam existed both stores did a
//! read-check-write: two writers could both "win", `supports_atomic_session_storage()` was
//! `false` (so the session journal refused the store) and `supports_versioning()` was
//! `false` (so the state checkpoint never saw a conflict).
//!
//! The last case pins the opposite direction: a client without conditional writes must
//! still keep the Java read-check-write behaviour **and** must not claim atomicity.

mod common;

use std::sync::Arc;

use agentscope_core::session::AtomicSessionStorage;
use agentscope_core::state::store::AgentStateStore;
use agentscope_core::state::UNVERSIONED;
use agentscope_extensions_oss::{
    InMemoryOssClient, OssAgentStateStore, OssClient, OssListPage, OssResult, OssStore,
    PutCondition, PutOutcome,
};
use agentscope_harness::agent::session::StoreSessionStorage;
use agentscope_harness::filesystem::remote::store::{BaseStore, StoreValue};

fn ns() -> Vec<String> {
    vec!["ns1".to_string()]
}

fn value(text: &str) -> StoreValue {
    let mut map = serde_json::Map::new();
    map.insert("foo".to_string(), serde_json::json!(text));
    map
}

fn state_store(client: Arc<dyn OssClient>) -> OssAgentStateStore {
    OssAgentStateStore::builder()
        .client(client)
        .bucket_name("test-bucket")
        .key_prefix("test/state/")
        .build()
        .unwrap()
}

fn base_store(client: Arc<dyn OssClient>) -> OssStore {
    OssStore::builder()
        .client(client)
        .bucket_name("test-bucket")
        .key_prefix("test/store/")
        .build()
        .unwrap()
}

/// A client that implements only the mandatory object API — i.e. exactly what a
/// third-party backend looked like before the conditional-write seam existed.
#[derive(Debug)]
struct PlainClient(InMemoryOssClient);

impl OssClient for PlainClient {
    fn put(&self, object_key: &str, content: &[u8]) -> OssResult<()> {
        self.0.put(object_key, content)
    }

    fn get(&self, object_key: &str) -> OssResult<Option<Vec<u8>>> {
        self.0.get(object_key)
    }

    fn exists(&self, object_key: &str) -> OssResult<bool> {
        self.0.exists(object_key)
    }

    fn delete(&self, object_key: &str) -> OssResult<()> {
        self.0.delete(object_key)
    }

    fn list_page(
        &self,
        prefix: &str,
        max_keys: usize,
        continuation_token: Option<&str>,
    ) -> OssResult<OssListPage> {
        self.0.list_page(prefix, max_keys, continuation_token)
    }
}

#[test]
fn in_memory_client_rejects_stale_preconditions() {
    let client = InMemoryOssClient::new();
    client.put("k", b"a").unwrap();
    let etag = client.get_with_etag("k").unwrap().expect("object").etag;

    assert!(matches!(
        client
            .put_conditional("k", b"b", &PutCondition::IfMatch(etag.clone()))
            .unwrap(),
        PutOutcome::Written { .. }
    ));
    // The ETag is now stale: the same token must not win twice.
    assert_eq!(
        client
            .put_conditional("k", b"c", &PutCondition::IfMatch(etag))
            .unwrap(),
        PutOutcome::PreconditionFailed
    );
    assert_eq!(
        client
            .put_conditional("k", b"d", &PutCondition::IfNoneMatchAny)
            .unwrap(),
        PutOutcome::PreconditionFailed
    );
    assert_eq!(client.get("k").unwrap(), Some(b"b".to_vec()));
}

#[tokio::test]
async fn state_store_save_if_version_is_a_real_cas() {
    let client = common::mem_client();
    let store = state_store(common::as_dyn(&client));

    assert!(AgentStateStore::supports_versioning(&store));

    // Absent key on a versioning backend reports version 0.
    let absent = AgentStateStore::get_versioned(&store, Some("alice"), "s1", "k").await;
    assert_eq!(absent.value, None);
    assert_eq!(absent.version, 0);

    let created = AgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "k",
        serde_json::json!({"turn": 1}),
        0,
    )
    .await;
    assert_ne!(created, UNVERSIONED, "create-if-absent must commit");

    let loaded = AgentStateStore::get_versioned(&store, Some("alice"), "s1", "k").await;
    assert_eq!(loaded.value, Some(serde_json::json!({"turn": 1})));
    assert_eq!(loaded.version, created);

    // A stale expected version loses and must not touch the stored value.
    let stale = AgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "k",
        serde_json::json!({"turn": 99}),
        0,
    )
    .await;
    assert_eq!(stale, UNVERSIONED);
    assert_eq!(
        AgentStateStore::get(&store, Some("alice"), "s1", "k").await,
        Some(serde_json::json!({"turn": 1}))
    );

    // The current version wins and advances the token.
    let updated = AgentStateStore::save_if_version(
        &store,
        Some("alice"),
        "s1",
        "k",
        serde_json::json!({"turn": 2}),
        loaded.version,
    )
    .await;
    assert_ne!(updated, UNVERSIONED);
    assert_ne!(updated, loaded.version);
    assert_eq!(
        AgentStateStore::get_versioned(&store, Some("alice"), "s1", "k")
            .await
            .version,
        updated
    );
}

#[tokio::test]
async fn concurrent_state_writers_at_the_same_version_cannot_both_commit() {
    let client = common::mem_client();
    let store = state_store(common::as_dyn(&client));
    let base =
        AgentStateStore::save_if_version(&store, None, "s1", "k", serde_json::json!(0), 0).await;
    assert_ne!(base, UNVERSIONED);

    let mut handles = Vec::new();
    for writer in 0..4u64 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(AgentStateStore::save_if_version(
                &store,
                None,
                "s1",
                "k",
                serde_json::json!(writer + 1),
                base,
            ))
        }));
    }
    let winners = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|version| *version != UNVERSIONED)
        .count();

    assert_eq!(winners, 1, "exactly one writer may commit per version");
}

#[tokio::test]
async fn oss_store_declares_atomic_session_storage_and_backs_the_journal() {
    let client = common::mem_client();
    assert!(OssClient::supports_conditional_writes(client.as_ref()));

    let store = Arc::new(base_store(common::as_dyn(&client)));
    assert!(store.supports_atomic_session_storage());

    // The harness journal refuses non-atomic stores, so constructing it is the assertion.
    let journal = StoreSessionStorage::new(store, vec!["ws".to_string()]).unwrap();
    assert!(journal.compare_and_set("k", 0, b"one").await.unwrap());
    let read = journal.read("k").await.unwrap().expect("value");
    assert_eq!(read.bytes, b"one".to_vec());
    assert_eq!(read.version, 1);

    assert!(!journal.compare_and_set("k", 0, b"stale").await.unwrap());
    assert!(journal.compare_and_set("k", 1, b"two").await.unwrap());
    assert_eq!(
        journal.read("k").await.unwrap().expect("value").bytes,
        b"two".to_vec()
    );
}

#[tokio::test]
async fn concurrent_base_store_writers_at_the_same_version_cannot_both_commit() {
    let client = common::mem_client();
    let store = base_store(common::as_dyn(&client));
    assert!(store.put_if_version(&ns(), "created", value("first"), 0));
    let version = store.get(&ns(), "created").unwrap().version;
    assert_eq!(version, 1);

    let mut handles = Vec::new();
    for writer in 0..4 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            store.put_if_version(&ns(), "created", value(&format!("{writer}")), version)
        }));
    }
    let winners = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|written| *written)
        .count();

    assert_eq!(winners, 1, "exactly one writer may commit per version");
}

#[tokio::test]
async fn client_without_conditional_writes_keeps_the_read_check_write_behaviour() {
    let client = Arc::new(PlainClient(InMemoryOssClient::new()));
    assert!(!OssClient::supports_conditional_writes(client.as_ref()));

    // Base store: no atomicity claim, but the Java CAS-shaped write still works.
    let store = base_store(client.clone());
    assert!(!store.supports_atomic_session_storage());
    assert!(store.put_if_version(&ns(), "k", value("first"), 0));
    assert!(!store.put_if_version(&ns(), "k", value("stale"), 0));
    assert!(store.put_if_version(&ns(), "k", value("second"), 1));
    assert_eq!(store.get(&ns(), "k").expect("item").value, value("second"));

    // State store: no versioning claim, `save_if_version` degrades to a plain write.
    let state = state_store(client);
    assert!(!AgentStateStore::supports_versioning(&state));
    let loaded = AgentStateStore::get_versioned(&state, None, "s1", "k").await;
    assert_eq!(loaded.version, UNVERSIONED);
    assert_eq!(
        AgentStateStore::save_if_version(&state, None, "s1", "k", serde_json::json!(1), 7).await,
        UNVERSIONED
    );
    assert_eq!(
        AgentStateStore::get(&state, None, "s1", "k").await,
        Some(serde_json::json!(1))
    );
}
