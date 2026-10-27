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
