//! Session journal backed by the workspace's distributed [`BaseStore`].
//!
//! Mirrors `StoreSessionStorage`: the workspace store already owns a resolved namespace, so
//! the journal appends a private partition to it and reuses the store's conditional write.
//! Only stores that declare [`BaseStore::supports_atomic_session_storage`] may back a
//! journal — the fencing guarantee is the whole point of the narrow storage capability.

use std::collections::BTreeSet;
use std::sync::Arc;

use agentscope_core::session::{session_error, AtomicSessionStorage, StorageValue};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

use crate::filesystem::remote::store::{BaseStore, StoreValue};

/// Private namespace segment appended to the workspace namespace.
const JOURNAL_NAMESPACE: &str = "__agentscope_session_log_v1__";

/// Page size used when listing keys; matches the Java implementation.
const LIST_PAGE_SIZE: usize = 256;

/// Atomic session storage over a workspace store.
pub struct StoreSessionStorage {
    store: Arc<dyn BaseStore>,
    namespace: Vec<String>,
}

impl std::fmt::Debug for StoreSessionStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreSessionStorage")
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl StoreSessionStorage {
    /// Creates journal storage under `namespace`, appending the private journal partition.
    ///
    /// Fails when the store does not declare an atomic compare-and-set: journal fencing
    /// cannot be emulated on top of a last-write-wins store.
    pub fn new(store: Arc<dyn BaseStore>, namespace: Vec<String>) -> crate::Result<Self> {
        if !store.supports_atomic_session_storage() {
            return Err(session_error(format!(
                "Workspace store does not declare atomic session storage: {}",
                std::any::type_name_of_val(store.as_ref())
            )));
        }
        let mut scoped = namespace;
        scoped.push(JOURNAL_NAMESPACE.to_string());
        Ok(Self {
            store,
            namespace: scoped,
        })
    }

    /// The journal namespace actually used for reads and writes.
    pub fn namespace(&self) -> &[String] {
        &self.namespace
    }
}

#[async_trait]
impl AtomicSessionStorage for StoreSessionStorage {
    async fn read(&self, path: &str) -> crate::Result<Option<StorageValue>> {
        let Some(item) = self.store.get(&self.namespace, path) else {
            return Ok(None);
        };
        if item.version < 1 {
            return Err(session_error(
                "Workspace store returned an unversioned journal object",
            ));
        }
        let Some(serde_json::Value::String(encoded)) = item.value.get("bytes") else {
            return Err(session_error("Invalid journal object"));
        };
        let bytes = BASE64
            .decode(encoded)
            .map_err(|_| session_error("Invalid journal object"))?;
        Ok(Some(StorageValue::new(item.version, bytes)))
    }

    async fn compare_and_set(
        &self,
        path: &str,
        expected_version: u64,
        bytes: &[u8],
    ) -> crate::Result<bool> {
        let mut value = StoreValue::new();
        value.insert(
            "bytes".to_string(),
            serde_json::Value::String(BASE64.encode(bytes)),
        );
        // A `false` here is a lost race, not a failure: the caller re-reads and retries.
        Ok(self
            .store
            .put_if_version(&self.namespace, path, value, expected_version))
    }

    async fn list_paths(&self, prefix: &str) -> crate::Result<Vec<String>> {
        let mut paths = BTreeSet::new();
        let mut offset = 0usize;
        loop {
            let page = self.store.search(&self.namespace, LIST_PAGE_SIZE, offset);
            for item in &page {
                if item.key.starts_with(prefix) {
                    paths.insert(item.key.clone());
                }
            }
            if page.len() < LIST_PAGE_SIZE {
                return Ok(paths.into_iter().collect());
            }
            offset += LIST_PAGE_SIZE;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::remote::store::{InMemoryStore, StoreItem};

    fn namespace() -> Vec<String> {
        vec!["sessions".to_string(), "alice".to_string()]
    }

    /// A store that keeps the store's real CAS but declines to declare it atomic, which is
    /// what any non-opted-in backend looks like to the journal.
    struct UndeclaredStore {
        inner: InMemoryStore,
    }

    impl UndeclaredStore {
        fn new() -> Self {
            Self {
                inner: InMemoryStore::new(),
            }
        }
    }

    impl BaseStore for UndeclaredStore {
        fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
            self.inner.get(namespace, key)
        }

        fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
            self.inner.put(namespace, key, value)
        }

        fn put_if_version(
            &self,
            namespace: &[String],
            key: &str,
            value: StoreValue,
            expected_version: u64,
        ) -> bool {
            self.inner
                .put_if_version(namespace, key, value, expected_version)
        }

        fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
            self.inner.search(namespace, limit, offset)
        }

        fn delete(&self, namespace: &[String], key: &str) {
            self.inner.delete(namespace, key)
        }
    }

    fn storage(store: Arc<InMemoryStore>) -> StoreSessionStorage {
        StoreSessionStorage::new(store, namespace()).unwrap()
    }

    #[test]
    fn the_journal_appends_a_private_namespace() {
        let storage = storage(Arc::new(InMemoryStore::new()));
        assert_eq!(
            storage.namespace().to_vec(),
            vec![
                "sessions".to_string(),
                "alice".to_string(),
                "__agentscope_session_log_v1__".to_string(),
            ]
        );
    }

    #[test]
    fn a_store_without_the_declaration_is_rejected() {
        let store: Arc<dyn BaseStore> = Arc::new(UndeclaredStore::new());
        assert!(!store.supports_atomic_session_storage());
        let error = StoreSessionStorage::new(store, namespace()).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("does not declare atomic session storage"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn storage_round_trips_and_enforces_versions() {
        let store = Arc::new(InMemoryStore::new());
        let storage = storage(Arc::clone(&store));

        assert!(storage.read("s1/head.json").await.unwrap().is_none());
        assert!(storage
            .compare_and_set("s1/head.json", 0, b"first")
            .await
            .unwrap());
        let value = storage.read("s1/head.json").await.unwrap().unwrap();
        assert_eq!(value.bytes, b"first".to_vec());
        assert_eq!(value.version, 1);

        // A stale expectation reports a lost race rather than an error.
        assert!(!storage
            .compare_and_set("s1/head.json", 0, b"stale")
            .await
            .unwrap());
        assert_eq!(
            storage.read("s1/head.json").await.unwrap().unwrap().bytes,
            b"first".to_vec()
        );

        assert!(storage
            .compare_and_set("s1/head.json", 1, b"second")
            .await
            .unwrap());
        let value = storage.read("s1/head.json").await.unwrap().unwrap();
        assert_eq!(value.bytes, b"second".to_vec());
        assert_eq!(value.version, 2);
    }

    #[tokio::test]
    async fn payloads_are_base64_encoded_in_the_store() {
        let store = Arc::new(InMemoryStore::new());
        let storage = storage(Arc::clone(&store));
        storage
            .compare_and_set("s1/a", 0, &[0xff, 0x00])
            .await
            .unwrap();

        let scoped = storage.namespace().to_vec();
        let item = store.get(&scoped, "s1/a").unwrap();
        assert_eq!(
            item.value.get("bytes"),
            Some(&serde_json::Value::String("/wA=".to_string()))
        );
    }

    #[tokio::test]
    async fn invalid_journal_objects_are_rejected() {
        let store = Arc::new(InMemoryStore::new());
        let storage = storage(Arc::clone(&store));

        // A foreign value without the `bytes` field.
        let mut value = StoreValue::new();
        value.insert("content".to_string(), serde_json::Value::String("x".into()));
        store.put(storage.namespace(), "bad/value", value);
        let error = storage.read("bad/value").await.unwrap_err();
        assert!(error.to_string().contains("Invalid journal object"));

        // A `bytes` field that is not base64 text.
        let mut value = StoreValue::new();
        value.insert("bytes".to_string(), serde_json::Value::String("%%%".into()));
        store.put(storage.namespace(), "bad/base64", value);
        let error = storage.read("bad/base64").await.unwrap_err();
        assert!(error.to_string().contains("Invalid journal object"));
    }

    #[tokio::test]
    async fn list_paths_filters_prefixes_and_sorts() {
        let store = Arc::new(InMemoryStore::new());
        let storage = storage(Arc::clone(&store));
        for path in ["s2/b", "s1/b", "s1/a", "other/c"] {
            storage.compare_and_set(path, 0, b"x").await.unwrap();
        }
        assert_eq!(
            storage.list_paths("s1/").await.unwrap(),
            vec!["s1/a".to_string(), "s1/b".to_string()]
        );
        // A prefix that matches nothing is an empty list, not an error.
        assert!(storage.list_paths("missing/").await.unwrap().is_empty());
    }
}
