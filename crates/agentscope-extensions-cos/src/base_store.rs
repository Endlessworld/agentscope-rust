//! Tencent Cloud COS backed [`BaseStore`], mirroring
//! `io.agentscope.extensions.cos.CosBaseStore`.
//!
//! Object key layout:
//!
//! ```text
//! {prefix}{ns0}/{ns1}/.../{key}.json     — item data
//! {prefix}{ns0}/{ns1}/.../{key}.version  — version counter
//! ```
//!
//! **CAS semantics are harness-defined.** `version` starts at 1 and increments on
//! every successful write; `put_if_version(ns, key, value, 0)` means "write only if
//! the key does not yet exist". Crypto: the write path is serialized per store via
//! an async mutex so the read-check-write sequence in `put_if_version` is atomic
//! within a process (matching Java's reliance on single-writer sequential calls).

use std::sync::Arc;

use agentscope_harness::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};
use tokio::sync::Mutex as AsyncMutex;

use crate::layout::{
    namespace_prefix, normalize_prefix, strip_leading_slashes, DEFAULT_STORE_PREFIX,
    JSON_SUFFIX, VERSION_SUFFIX,
};
use crate::object_store::SharedObjectStore;
use crate::{CosError, Result};

/// Cos-backed `BaseStore` implementing the harness CAS/version contract.
#[derive(Clone)]
pub struct CosStore {
    object_store: SharedObjectStore,
    key_prefix: String,
    write_lock: Arc<AsyncMutex<()>>,
}

impl std::fmt::Debug for CosStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosStore")
            .field("key_prefix", &self.key_prefix)
            .finish_non_exhaustive()
    }
}

impl CosStore {
    pub fn builder() -> CosStoreBuilder {
        CosStoreBuilder::default()
    }

    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    fn data_key(&self, namespace: &[String], key: &str) -> String {
        format!(
            "{}{}{}",
            namespace_prefix(&self.key_prefix, namespace),
            strip_leading_slashes(key),
            JSON_SUFFIX
        )
    }

    fn version_key(&self, namespace: &[String], key: &str) -> String {
        format!(
            "{}{}{}",
            namespace_prefix(&self.key_prefix, namespace),
            strip_leading_slashes(key),
            VERSION_SUFFIX
        )
    }

    /// Read the version counter; a missing or malformed counter reads as `0`.
    async fn read_version(&self, version_key: &str) -> u64 {
        match self.object_store.get_string(version_key).await {
            Ok(Some(content)) => content.trim().parse::<u64>().unwrap_or(0),
            _ => 0,
        }
    }

    /// Runs a future to completion from synchronous code.
    ///
    /// Works in any of three situations: (1) already inside a multi-threaded Tokio
    /// runtime (uses `block_in_place` + `block_on` so the runtime is not stalled),
    /// (2) inside a current-thread runtime, or (3) outside any runtime (spins up a
    /// temporary runtime). Used only by the synchronous [`BaseStore`] shim.
    fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future + Send,
        F::Output: Send,
    {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(move || handle.block_on(fut))
            }
            // Cannot nest `block_on` on a current-thread runtime; run the future on a
            // dedicated thread with its own runtime.
            Ok(_) => std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("build current-thread runtime")
                            .block_on(fut)
                    })
                    .join()
                    .expect("blocking bridge thread panicked")
            }),
            Err(_) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build temporary runtime")
                .block_on(fut),
        }
    }
}

/// Builder for [`CosStore`].
#[derive(Default)]
pub struct CosStoreBuilder {
    object_store: Option<SharedObjectStore>,
    key_prefix: Option<String>,
}

impl CosStoreBuilder {
    pub fn object_store(mut self, store: SharedObjectStore) -> Self {
        self.object_store = Some(store);
        self
    }

    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Some(prefix.into());
        self
    }

    pub fn build(self) -> Result<CosStore> {
        let object_store = self
            .object_store
            .ok_or_else(|| CosError::Config("object_store must not be null".to_string()))?;
        Ok(CosStore {
            object_store,
            key_prefix: normalize_prefix(self.key_prefix.as_deref(), DEFAULT_STORE_PREFIX),
            write_lock: Arc::new(AsyncMutex::new(())),
        })
    }
}

impl BaseStore for CosStore {
    fn get(&self, namespace: &[String], key: &str) -> Option<StoreItem> {
        let data_key = self.data_key(namespace, key);
        let version_key = self.version_key(namespace, key);
        self.block_on(async move {
            let json = self.object_store.get_string(&data_key).await.ok()??;
            let value: StoreValue = serde_json::from_str(&json).ok()?;
            let version = self.read_version(&version_key).await;
            Some(StoreItem::new(key, value, version))
        })
    }

    fn put(&self, namespace: &[String], key: &str, value: StoreValue) {
        let data_key = self.data_key(namespace, key);
        let version_key = self.version_key(namespace, key);
        self.block_on(async move {
            let _guard = self.write_lock.lock().await;
            if let Ok(json) = serde_json::to_string(&value) {
                let _ = self.object_store.put_string(&data_key, &json).await;
                let current = self.read_version(&version_key).await;
                let _ = self
                    .object_store
                    .put_string(&version_key, &(current + 1).to_string())
                    .await;
            }
        });
    }

    fn put_if_version(
        &self,
        namespace: &[String],
        key: &str,
        value: StoreValue,
        expected_version: u64,
    ) -> bool {
        let data_key = self.data_key(namespace, key);
        let version_key = self.version_key(namespace, key);
        self.block_on(async move {
            let _guard = self.write_lock.lock().await;
            let current = self.read_version(&version_key).await;
            if current != expected_version {
                return false;
            }
            match serde_json::to_string(&value) {
                Ok(json) => {
                    if self.object_store.put_string(&data_key, &json).await.is_err() {
                        return false;
                    }
                    let _ = self
                        .object_store
                        .put_string(&version_key, &(current + 1).to_string())
                        .await;
                    true
                }
                Err(_) => false,
            }
        })
    }

    fn search(&self, namespace: &[String], limit: usize, offset: usize) -> Vec<StoreItem> {
        let ns = namespace.to_vec();
        let prefix = namespace_prefix(&self.key_prefix, &ns);
        self.block_on(async move {
            let mut data_keys = match self.object_store.list_prefix(&prefix).await {
                Ok(keys) => keys
                    .into_iter()
                    .filter(|k| k.ends_with(JSON_SUFFIX) && !k.ends_with(VERSION_SUFFIX))
                    .collect::<Vec<_>>(),
                Err(_) => return Vec::new(),
            };
            data_keys.sort();

            let start = offset.min(data_keys.len());
            let end = start.saturating_add(limit).min(data_keys.len());
            let page = &data_keys[start..end];

            let mut items = Vec::with_capacity(page.len());
            for data_key in page {
                let item_key = data_key[prefix.len()..data_key.len() - JSON_SUFFIX.len()].to_string();
                if let Ok(Some(json)) = self.object_store.get_string(data_key).await {
                    if let Ok(value) = serde_json::from_str::<StoreValue>(&json) {
                        let vk = format!(
                            "{}{}",
                            &data_key[..data_key.len() - JSON_SUFFIX.len()],
                            VERSION_SUFFIX
                        );
                        let version = self.read_version(&vk).await;
                        items.push(StoreItem::new(item_key, value, version));
                    }
                }
            }
            items
        })
    }

    fn delete(&self, namespace: &[String], key: &str) {
        let data_key = self.data_key(namespace, key);
        let version_key = self.version_key(namespace, key);
        self.block_on(async move {
            let _guard = self.write_lock.lock().await;
            let _ = self.object_store.delete(&data_key).await;
            if self.object_store.exists(&version_key).await.unwrap_or(false) {
                let _ = self.object_store.delete(&version_key).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::InMemoryCosObjectStore;

    fn ns() -> Vec<String> {
        vec!["sessions".to_string(), "s1".to_string(), "fs".to_string()]
    }

    fn value(content: &str) -> StoreValue {
        let mut m = StoreValue::new();
        m.insert(
            "content".to_string(),
            serde_json::Value::String(content.to_string()),
        );
        m
    }

    fn store() -> CosStore {
        CosStore::builder()
            .object_store(InMemoryCosObjectStore::new().shared())
            .build()
            .unwrap()
    }

    #[test]
    fn put_get_increments_version() {
        let s = store();
        assert!(s.get(&ns(), "a.txt").is_none());
        s.put(&ns(), "a.txt", value("one"));
        assert_eq!(s.get(&ns(), "a.txt").unwrap().version, 1);
        s.put(&ns(), "a.txt", value("two"));
        let item = s.get(&ns(), "a.txt").unwrap();
        assert_eq!(item.version, 2);
        assert_eq!(item.value.get("content").unwrap(), &serde_json::json!("two"));
    }

    #[test]
    fn put_if_version_cas() {
        let s = store();
        // Version 0 means "create only if absent".
        assert!(s.put_if_version(&ns(), "k", value("v1"), 0));
        assert!(!s.put_if_version(&ns(), "k", value("v2"), 0));
        assert_eq!(s.get(&ns(), "k").unwrap().version, 1);
        assert!(s.put_if_version(&ns(), "k", value("v2"), 1));
        assert_eq!(s.get(&ns(), "k").unwrap().version, 2);
        assert!(!s.put_if_version(&ns(), "k", value("v3"), 1));
    }

    #[test]
    fn search_paginates_ordered() {
        let s = store();
        for name in ["c.txt", "a.txt", "b.txt"] {
            s.put(&ns(), name, value(name));
        }
        let all = s.search(&ns(), 10, 0);
        let keys: Vec<_> = all.iter().map(|i| i.key.clone()).collect();
        assert_eq!(keys, vec!["a.txt", "b.txt", "c.txt"]);
        let page = s.search(&ns(), 1, 1);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].key, "b.txt");
        assert_eq!(s.search(&ns(), 10, 5).len(), 0);
    }

    #[test]
    fn delete_removes_item() {
        let s = store();
        s.put(&ns(), "a.txt", value("one"));
        s.delete(&ns(), "a.txt");
        assert!(s.get(&ns(), "a.txt").is_none());
        // Deleting a missing key is a no-op.
        s.delete(&ns(), "a.txt");
    }

    #[test]
    fn namespaces_are_isolated() {
        let s = store();
        let a = vec!["ns".to_string(), "a".to_string()];
        let b = vec!["ns".to_string(), "b".to_string()];
        s.put(&a, "k", value("in-a"));
        assert!(s.get(&b, "k").is_none());
        assert_eq!(s.get(&a, "k").unwrap().version, 1);
    }
}
