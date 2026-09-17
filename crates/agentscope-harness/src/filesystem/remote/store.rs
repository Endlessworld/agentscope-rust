//! Namespace-based key-value store abstractions, mirroring
//! `io.agentscope.harness.agent.filesystem.remote.store.*`.
//!
//! Items are organized by namespaces (hierarchical path-like tuples) and identified
//! by a key within each namespace. Every successful write increments the item's
//! version, enabling optimistic concurrency control via [`BaseStore::put_if_version`].

use std::collections::HashMap;
use std::sync::Mutex;

use agentscope_core::agent::RuntimeContext;

/// A single item stored in a [`BaseStore`], stored as a string-keyed map.
pub type StoreValue = serde_json::Map<String, serde_json::Value>;

/// A single item retrieved from a [`BaseStore`].
///
/// `version` starts at 1 and increments on every successful write. A value of `0`
/// means the version is unknown (e.g. items returned by legacy implementations).
#[derive(Debug, Clone, PartialEq)]
pub struct StoreItem {
    pub key: String,
    pub value: StoreValue,
    pub version: u64,
}

impl StoreItem {
    pub fn new(key: impl Into<String>, value: StoreValue, version: u64) -> Self {
        Self { key: key.into(), value, version }
    }
}

/// Abstract interface for a namespace-based key-value store.
pub trait BaseStore: Send + Sync {
    /// Gets a single item by namespace and key.
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem>;

    /// Stores or updates an item (last-write-wins).
    fn put(&self, namespace: &[String], key: &str, value: StoreValue);

    /// Conditional update (compare-and-swap): stores the item only when the current
    /// stored version matches `expected_version`. A value of `0` means "write only if
    /// the key does not yet exist".
    ///
    /// The default implementation always returns `false`; backends supporting
    /// distributed deployments should override it with a proper atomic check.
    fn put_if_version(
        &self,
        _namespace: &[String],
        _key: &str,
        _value: StoreValue,
        _expected_version: u64,
    ) -> bool {
        false
    }

    /// Searches for items within a namespace with pagination, ordered by key.
    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem>;

    /// Deletes an item by namespace and key.
    fn delete(&self, namespace: &[String], key: &str);
}

/// Produces a namespace tuple for [`BaseStore`] operations at call time.
///
/// Unlike a static namespace, a `NamespaceFactory` is invoked on every store
/// operation, allowing the namespace to vary based on the per-call [`RuntimeContext`].
pub trait NamespaceFactory: Send + Sync {
    fn get_namespace(&self, runtime_context: &RuntimeContext) -> Vec<String>;
}

/// A [`NamespaceFactory`] returning a fixed namespace tuple.
#[derive(Debug, Clone)]
pub struct StaticNamespace(pub Vec<String>);

impl NamespaceFactory for StaticNamespace {
    fn get_namespace(&self, _runtime_context: &RuntimeContext) -> Vec<String> {
        self.0.clone()
    }
}

/// A [`NamespaceFactory`] returning the default single-segment `["filesystem"]`.
#[derive(Debug, Clone, Default)]
pub struct DefaultNamespace;

impl NamespaceFactory for DefaultNamespace {
    fn get_namespace(&self, _runtime_context: &RuntimeContext) -> Vec<String> {
        vec!["filesystem".to_string()]
    }
}

/// Thread-safe in-memory implementation of [`BaseStore`].
///
/// Items are keyed by the concatenation of namespace components and the item key,
/// separated by `'\0'`, enabling prefix-based searches.
#[derive(Default)]
pub struct InMemoryStore {
    store: Mutex<HashMap<String, StoreItem>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of items currently stored.
    pub fn size(&self) -> usize {
        self.store.lock().unwrap().len()
    }

    /// Removes all items from the store.
    pub fn clear(&self) {
        self.store.lock().unwrap().clear();
    }
}

fn namespace_prefix(namespace: &[String]) -> String {
    let mut sb = String::new();
    for component in namespace {
        sb.push_str(component);
        sb.push('\0');
    }
    sb
}

fn compound_key(namespace: &[String], key: &str) -> String {
    format!("{}{}", namespace_prefix(namespace), key)
}

impl BaseStore for InMemoryStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        self.store.lock().unwrap().get(&compound_key(namespace, key)).cloned()
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        let ck = compound_key(namespace, key);
        let mut guard = self.store.lock().unwrap();
        let next_version = guard.get(&ck).map_or(1, |existing| existing.version + 1);
        guard.insert(ck, StoreItem::new(key, value, next_version));
    }

    fn put_if_version(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> bool {
        let ck = compound_key(namespace, key);
        let mut guard = self.store.lock().unwrap();
        let current_version = guard.get(&ck).map_or(0, |existing| existing.version);
        if current_version != expected_version {
            return false;
        }
        guard.insert(ck, StoreItem::new(key, value, current_version + 1));
        true
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        let prefix = namespace_prefix(namespace);
        let guard = self.store.lock().unwrap();
        let mut matches: Vec<StoreItem> = guard
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| v.clone())
            .collect();
        matches.sort_by(|a, b| a.key.cmp(&b.key));
        let start = offset.min(matches.len());
        let end = start.saturating_add(limit).min(matches.len());
        matches[start..end].to_vec()
    }

    fn delete(&self, namespace: &[String], key: &str) {
        self.store.lock().unwrap().remove(&compound_key(namespace, key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> Vec<String> {
        vec!["sessions".to_string(), "s1".to_string(), "filesystem".to_string()]
    }

    fn value(content: &str) -> StoreValue {
        let mut m = StoreValue::new();
        m.insert("content".to_string(), serde_json::Value::String(content.to_string()));
        m
    }

    #[test]
    fn put_assigns_increasing_versions() {
        let store = InMemoryStore::new();
        store.put(&ns(), "a.txt", value("one"));
        assert_eq!(store.get(&ns(), "a.txt").unwrap().version, 1);
        store.put(&ns(), "a.txt", value("two"));
        let item = store.get(&ns(), "a.txt").unwrap();
        assert_eq!(item.version, 2);
        assert_eq!(item.value.get("content").unwrap(), &serde_json::json!("two"));
    }

    #[test]
    fn put_if_version_create_if_absent() {
        let store = InMemoryStore::new();
        assert!(store.put_if_version(&ns(), "a.txt", value("one"), 0));
        assert!(!store.put_if_version(&ns(), "a.txt", value("two"), 0));
        assert_eq!(
            store.get(&ns(), "a.txt").unwrap().value.get("content").unwrap(),
            &serde_json::json!("one")
        );
        assert!(store.put_if_version(&ns(), "a.txt", value("three"), 1));
        assert_eq!(store.get(&ns(), "a.txt").unwrap().version, 2);
    }

    #[test]
    fn search_is_scoped_paginated_and_sorted() {
        let store = InMemoryStore::new();
        store.put(&ns(), "c.txt", value("c"));
        store.put(&ns(), "a.txt", value("a"));
        store.put(&ns(), "b.txt", value("b"));
        store.put(&vec!["other".to_string()], "x.txt", value("x"));
        let page = store.search(&ns(), 2, 0);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].key, "a.txt");
        assert_eq!(page[1].key, "b.txt");
        let page2 = store.search(&ns(), 2, 2);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].key, "c.txt");
    }

    #[test]
    fn delete_removes_only_target_key() {
        let store = InMemoryStore::new();
        store.put(&ns(), "a.txt", value("a"));
        store.put(&ns(), "b.txt", value("b"));
        store.delete(&ns(), "a.txt");
        assert!(store.get(&ns(), "a.txt").is_none());
        assert!(store.get(&ns(), "b.txt").is_some());
        assert_eq!(store.size(), 1);
    }
}
