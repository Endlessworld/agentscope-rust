//! `JdbcStore` offline integration tests, mirroring the Java `JdbcStoreH2Test`.
//!
//! Backend: in-memory SQLite (`RusqliteExecutor::in_memory()`) with `SqliteDialect` — the Rust
//! analogue of Java's H2 + `H2Dialect` pairing. No external database or network is used.

use std::sync::Arc;

use agentscope_extensions_jdbc::dialect::vendor::SqliteDialect;
use agentscope_extensions_jdbc::dialect::AbstractJdbcDialect;
use agentscope_extensions_jdbc::{BaseStore, JdbcStore, RusqliteExecutor, StoreValue};

fn new_store() -> JdbcStore {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> = Arc::new(SqliteDialect::new());
    JdbcStore::with_schema(executor, dialect).unwrap()
}

fn value(pairs: &[(&str, serde_json::Value)]) -> StoreValue {
    let mut map = StoreValue::new();
    for (k, v) in pairs {
        map.insert((*k).to_string(), v.clone());
    }
    map
}

fn ns(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

// ---------------------------------------------------------------------------
//  Java: JdbcStoreH2Test
// ---------------------------------------------------------------------------

#[test]
fn put_and_get_round_trip() {
    // Java: JdbcStoreH2Test.putAndGetRoundTrip
    let store = new_store();
    let namespace = ns(&["agents", "my-agent"]);
    store.put(
        &namespace,
        "item1",
        value(&[
            ("key", serde_json::json!("value")),
            ("count", serde_json::json!(42)),
        ]),
    );

    let result = store.get(&namespace, "item1").expect("item must exist");
    assert_eq!(result.key, "item1");
    assert_eq!(result.value.get("key"), Some(&serde_json::json!("value")));
    assert_eq!(result.value.get("count"), Some(&serde_json::json!(42)));
    assert_eq!(result.version, 1);
}

#[test]
fn get_returns_null_for_missing_key() {
    // Java: JdbcStoreH2Test.getReturnsNullForMissingKey
    let store = new_store();
    assert!(store.get(&ns(&["ns"]), "nonexistent").is_none());
}

#[test]
fn put_updates_version_on_overwrite() {
    // Java: JdbcStoreH2Test.putUpdatesVersion
    let store = new_store();
    let namespace = ns(&["ns"]);
    store.put(&namespace, "k", value(&[("v", serde_json::json!(1))]));
    store.put(&namespace, "k", value(&[("v", serde_json::json!(2))]));

    let result = store.get(&namespace, "k").unwrap();
    assert_eq!(result.version, 2);
    assert_eq!(result.value.get("v"), Some(&serde_json::json!(2)));
}

#[test]
fn put_if_version_create_new() {
    // Java: JdbcStoreH2Test.putIfVersionCreateNew
    let store = new_store();
    assert!(store.put_if_version(&ns(&["ns"]), "k", value(&[("v", serde_json::json!(1))]), 0));
    assert_eq!(store.get(&ns(&["ns"]), "k").unwrap().version, 1);
}

#[test]
fn put_if_version_create_fails_on_existing() {
    // Java: JdbcStoreH2Test.putIfVersionCreateFailsOnExisting
    let store = new_store();
    assert!(store.put_if_version(&ns(&["ns"]), "k", value(&[("v", serde_json::json!(1))]), 0));
    assert!(!store.put_if_version(&ns(&["ns"]), "k", value(&[("v", serde_json::json!(2))]), 0));
}

#[test]
fn put_if_version_matching_succeeds() {
    // Java: JdbcStoreH2Test.putIfVersionMatchingSucceeds
    let store = new_store();
    let namespace = ns(&["ns"]);
    store.put(&namespace, "k", value(&[("v", serde_json::json!(1))]));
    let version = store.get(&namespace, "k").unwrap().version;

    assert!(store.put_if_version(
        &namespace,
        "k",
        value(&[("v", serde_json::json!(2))]),
        version
    ));
    assert_eq!(store.get(&namespace, "k").unwrap().version, 2);
}

#[test]
fn put_if_version_stale_fails() {
    // Java: JdbcStoreH2Test.putIfVersionStaleFails
    let store = new_store();
    let namespace = ns(&["ns"]);
    store.put(&namespace, "k", value(&[("v", serde_json::json!(1))]));
    store.put(&namespace, "k", value(&[("v", serde_json::json!(2))])); // version now 2

    assert!(!store.put_if_version(&namespace, "k", value(&[("v", serde_json::json!(3))]), 1));
}

#[test]
fn delete_removes_item() {
    // Java: JdbcStoreH2Test.deleteRemovesItem
    let store = new_store();
    let namespace = ns(&["ns"]);
    store.put(&namespace, "k", value(&[("v", serde_json::json!(1))]));
    store.delete(&namespace, "k");
    assert!(store.get(&namespace, "k").is_none());
}

#[test]
fn search_returns_items_in_namespace_with_sub_namespaces() {
    // Java: JdbcStoreH2Test.searchReturnsItemsWithPagination
    let store = new_store();
    let parent = ns(&["parent"]);
    store.put(&parent, "b", value(&[("v", serde_json::json!(2))]));
    store.put(&parent, "a", value(&[("v", serde_json::json!(1))]));
    store.put(&parent, "c", value(&[("v", serde_json::json!(3))]));
    store.put(
        &ns(&["parent", "child"]),
        "d",
        value(&[("v", serde_json::json!(4))]),
    );

    let results = store.search(&parent, 10, 0);
    assert_eq!(results.len(), 4, "should include items from sub-namespaces");
    assert_eq!(results[0].key, "a", "should be ordered by key");
}

#[test]
fn search_respects_limit_and_offset() {
    // Java: JdbcStoreH2Test.searchRespectsLimitOffset
    let store = new_store();
    let namespace = ns(&["ns"]);
    for i in 0..5 {
        store.put(
            &namespace,
            &format!("key{i}"),
            value(&[("v", serde_json::json!(i))]),
        );
    }

    let page = store.search(&namespace, 2, 1);
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].key, "key1");
    assert_eq!(page[1].key, "key2");
}

// ---------------------------------------------------------------------------
//  Extra coverage for the CAS three-branch contract and namespace encoding
// ---------------------------------------------------------------------------

#[test]
fn put_if_version_requires_exact_row_count_for_update() {
    // Java: JdbcStore.putIfVersion(update) returns `executeUpdate() == 1`.
    // A single logical key can only match one row, so a correct CAS always reports 1.
    let store = new_store();
    let namespace = ns(&["ns"]);
    store.put(&namespace, "k", value(&[("v", serde_json::json!(1))]));
    assert!(store.put_if_version(&namespace, "k", value(&[("v", serde_json::json!(2))]), 1));
    // The next CAS against version 1 must now fail (row is at version 2).
    assert!(!store.put_if_version(&namespace, "k", value(&[("v", serde_json::json!(3))]), 1));
    assert_eq!(
        store.get(&namespace, "k").unwrap().value.get("v"),
        Some(&serde_json::json!(2))
    );
}

#[test]
fn namespaces_are_isolated_by_unit_separator() {
    // Java: JdbcStore.namespacePath joins with the 0x1F unit separator and appends it to the
    // last segment, so prefix searches do not leak across similarly-named sibling namespaces.
    let store = new_store();
    store.put(
        &ns(&["parent"]),
        "own",
        value(&[("v", serde_json::json!(1))]),
    );
    store.put(
        &ns(&["parent-long"]),
        "other",
        value(&[("v", serde_json::json!(2))]),
    );

    let results = store.search(&ns(&["parent"]), 10, 0);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].key, "own");
}

#[test]
fn blank_key_is_rejected() {
    // Java: JdbcStore.validateKey rejects null/empty keys.
    let store = new_store();
    assert!(store.get(&ns(&["ns"]), "").is_none());
    assert!(!store.put_if_version(&ns(&["ns"]), "", value(&[("v", serde_json::json!(1))]), 0));
}

#[test]
fn search_with_zero_limit_returns_empty() {
    // Java: JdbcStore.search returns List.of() when limit <= 0.
    let store = new_store();
    store.put(&ns(&["ns"]), "k", value(&[("v", serde_json::json!(1))]));
    assert!(store.search(&ns(&["ns"]), 0, 0).is_empty());
}
