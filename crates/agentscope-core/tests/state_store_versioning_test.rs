//! Versioning / CAS behaviour of the core `AgentStateStore` implementations,
//! mirroring the Java `AgentStateStore.saveIfVersion` / `getVersioned` contract.

use agentscope_core::state::{
    AgentStateStore, InMemoryAgentStateStore, JsonFileAgentStateStore, UNVERSIONED,
};
use serde_json::json;

#[tokio::test]
async fn in_memory_supports_versioning_and_tracks_versions() {
    let store = InMemoryAgentStateStore::new();
    assert!(store.supports_versioning());

    let absent = store.get_versioned(Some("u"), "s", "k").await;
    assert!(!absent.is_present());
    assert_eq!(absent.version, 0);

    assert_eq!(
        store
            .save_if_version(Some("u"), "s", "k", json!(1), 0)
            .await,
        1
    );
    let loaded = store.get_versioned(Some("u"), "s", "k").await;
    assert_eq!(loaded.value, Some(json!(1)));
    assert_eq!(loaded.version, 1);
}

#[tokio::test]
async fn in_memory_cas_conflict_returns_unversioned_without_writing() {
    let store = InMemoryAgentStateStore::new();
    assert_eq!(
        store.save_if_version(None, "s", "k", json!("a"), 0).await,
        1
    );
    assert_eq!(
        store.save_if_version(None, "s", "k", json!("b"), 0).await,
        UNVERSIONED
    );
    assert_eq!(store.get(None, "s", "k").await, Some(json!("a")));

    assert_eq!(
        store.save_if_version(None, "s", "k", json!("c"), 1).await,
        2
    );
    assert_eq!(store.get(None, "s", "k").await, Some(json!("c")));
}

#[tokio::test]
async fn in_memory_unconditional_write_returns_its_own_version() {
    let store = InMemoryAgentStateStore::new();
    assert_eq!(
        store
            .save_if_version(None, "s", "k", json!(1), UNVERSIONED)
            .await,
        1
    );
    assert_eq!(
        store
            .save_if_version(None, "s", "k", json!(2), UNVERSIONED)
            .await,
        2
    );
    assert_eq!(store.get_versioned(None, "s", "k").await.version, 2);
}

#[tokio::test]
async fn in_memory_plain_save_bumps_version() {
    let store = InMemoryAgentStateStore::new();
    store.save(None, "s", "k", json!(1)).await;
    assert_eq!(store.get_versioned(None, "s", "k").await.version, 1);
}

#[tokio::test]
async fn json_file_store_degrades_to_unconditional_writes() {
    let dir = std::env::temp_dir().join(format!(
        "agentscope-state-store-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = JsonFileAgentStateStore::new(&dir);
    assert!(!store.supports_versioning());
    assert_eq!(
        store.save_if_version(None, "s", "k", json!(1), 0).await,
        UNVERSIONED
    );
    assert_eq!(store.get(None, "s", "k").await, Some(json!(1)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn in_memory_delete_key_removes_only_that_key() {
    let store = InMemoryAgentStateStore::new();
    store.save(Some("u"), "s", "a", json!(1)).await;
    store
        .save_list(Some("u"), "s", "b", &[json!(1), json!(2)])
        .await;

    store.delete_key(Some("u"), "s", "a").await.unwrap();

    assert_eq!(store.get(Some("u"), "s", "a").await, None);
    // Version bookkeeping is dropped together with the value, so a re-create starts at 0.
    assert_eq!(store.get_versioned(Some("u"), "s", "a").await.version, 0);
    // Other keys and other sessions are untouched.
    assert_eq!(
        store.get_list(Some("u"), "s", "b").await,
        vec![json!(1), json!(2)]
    );
    assert!(store.exists(Some("u"), "s").await);
}

/// A store that implements only the required trait surface, so the **default** `try_*`
/// methods are the ones under test.
#[derive(Default)]
struct PlainStore {
    values: std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>,
}

#[async_trait::async_trait]
impl AgentStateStore for PlainStore {
    async fn save(&self, _u: Option<&str>, session: &str, key: &str, value: serde_json::Value) {
        self.values
            .lock()
            .unwrap()
            .insert(format!("{session}/{key}"), value);
    }

    async fn save_list(&self, _u: Option<&str>, _s: &str, _k: &str, _v: &[serde_json::Value]) {}

    async fn get(&self, _u: Option<&str>, session: &str, key: &str) -> Option<serde_json::Value> {
        self.values
            .lock()
            .unwrap()
            .get(&format!("{session}/{key}"))
            .cloned()
    }

    async fn get_list(&self, _u: Option<&str>, _s: &str, _k: &str) -> Vec<serde_json::Value> {
        Vec::new()
    }

    async fn exists(&self, _u: Option<&str>, _s: &str) -> bool {
        false
    }

    async fn delete(&self, _u: Option<&str>, _s: &str) {}

    async fn list_session_ids(&self, _u: Option<&str>) -> Vec<String> {
        Vec::new()
    }
}

#[tokio::test]
async fn default_try_methods_delegate_to_the_infallible_surface() {
    // The `try_*` family must not silently change behaviour for backends that keep the
    // infallible contract: the defaults delegate to `get_versioned` / `save_if_version`.
    let store = PlainStore::default();

    let absent = store.try_get_versioned(None, "s", "k").await.unwrap();
    assert!(!absent.is_present());
    assert_eq!(absent.version, UNVERSIONED);

    // A non-versioning backend writes unconditionally and reports `UNVERSIONED`.
    assert_eq!(
        store
            .try_save_if_version(None, "s", "k", json!(1), 0)
            .await
            .unwrap(),
        UNVERSIONED
    );
    let loaded = store.try_get_versioned(None, "s", "k").await.unwrap();
    assert_eq!(loaded.value, Some(json!(1)));
    assert_eq!(loaded.version, UNVERSIONED);
}

#[tokio::test]
async fn in_memory_try_methods_reject_a_blank_session_id() {
    // The infallible methods drop a blank session id on the floor (`let _ = require_...`);
    // the fallible surface must report it instead of writing to the `""` session.
    let store = InMemoryAgentStateStore::new();
    assert!(store.try_get_versioned(None, "  ", "k").await.is_err());
    assert!(store
        .try_save_if_version(None, "  ", "k", json!(1), 0)
        .await
        .is_err());
    assert!(!store.exists(None, "").await);
}

#[tokio::test]
async fn json_file_try_get_versioned_reports_a_corrupt_payload() {
    // `get_versioned` collapses a parse failure to "nothing stored"; the fallible variant
    // must surface it so a caller does not overwrite a state it failed to read.
    let dir = std::env::temp_dir().join(format!(
        "agentscope-state-store-try-read-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = JsonFileAgentStateStore::new(&dir);
    let session_dir = dir.join("__anon__").join("s");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("k.json"), "{not json").unwrap();

    assert!(store.get_versioned(None, "s", "k").await.value.is_none());
    assert!(store.try_get_versioned(None, "s", "k").await.is_err());

    // A missing key is still an ordinary absence, not a failure.
    let missing = store.try_get_versioned(None, "s", "other").await.unwrap();
    assert!(!missing.is_present());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn json_file_try_save_if_version_reports_a_failed_write() {
    // A write that never lands must not look like a lost CAS race (`UNVERSIONED`) or like
    // an ordinary `save` that succeeds.
    let dir = std::env::temp_dir().join(format!(
        "agentscope-state-store-try-write-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = JsonFileAgentStateStore::new(&dir);
    // Occupy the session directory path with a regular file so every write fails.
    let user_dir = dir.join("__anon__");
    std::fs::create_dir_all(&user_dir).unwrap();
    std::fs::write(user_dir.join("s"), "blocked").unwrap();

    store.save(None, "s", "k", json!(1)).await;
    assert_eq!(store.get(None, "s", "k").await, None);
    assert!(store
        .try_save_if_version(None, "s", "k", json!(1), 0)
        .await
        .is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn json_file_store_reports_per_key_delete_as_unsupported() {
    // The trait default must fail loudly rather than pretend the value was removed.
    let dir = std::env::temp_dir().join(format!(
        "agentscope-state-store-delete-key-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = JsonFileAgentStateStore::new(&dir);
    store.save(None, "s", "k", json!(1)).await;

    assert!(store.delete_key(None, "s", "k").await.is_err());
    assert_eq!(store.get(None, "s", "k").await, Some(json!(1)));
    let _ = std::fs::remove_dir_all(&dir);
}
