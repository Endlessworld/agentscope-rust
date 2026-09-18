//! Async object-store abstraction plus an in-memory backend.
//!
//! The Java implementation talks directly to `COSClient`. The Rust port depends on
//! no Tencent SDK; instead every component is written against the [`CosObjectStore`]
//! trait, so a real SDK adapter can be plugged in later (tracked as a follow-up) and
//! tests run against [`InMemoryCosObjectStore`].

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::{CosError, Result};

/// Minimal async object-store surface required by the COS components.
///
/// This mirrors the subset of `COSClient` operations actually used by the Java
/// module (`getObjectMetadata`, `getObject`, `putObject`, `deleteObject`,
/// `listObjects` with a marker).
#[async_trait]
#[allow(clippy::len_without_is_empty)]
pub trait CosObjectStore: Send + Sync + std::fmt::Debug {
    /// Returns the object bytes, or `None` when the object does not exist.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Writes (creates or replaces) an object.
    async fn put(&self, key: &str, data: &[u8]) -> Result<()>;

    /// Deletes an object; a missing object is not an error.
    async fn delete(&self, key: &str) -> Result<()>;

    /// Returns whether an object exists.
    async fn exists(&self, key: &str) -> Result<bool>;

    /// The number of stored objects (used to distinguish an empty bucket).
    async fn len(&self) -> usize;

    /// Lists all object keys starting with `prefix`, sorted lexicographically.
    ///
    /// This corresponds to Java's paginated `listAllKeys` helper, flattened into a
    /// single call.
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>>;

    // ---- convenience helpers (UTF-8 text) ----

    /// Reads an object as UTF-8 text; `None` when the object is absent.
    async fn get_string(&self, key: &str) -> Result<Option<String>> {
        match self.get(key).await? {
            Some(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|e| {
                CosError::Serialization(format!("object {key} is not valid UTF-8: {e}"))
            })?)),
            None => Ok(None),
        }
    }

    /// Writes a UTF-8 string object.
    async fn put_string(&self, key: &str, content: &str) -> Result<()> {
        self.put(key, content.as_bytes()).await
    }
}

/// Shared handle to a [`CosObjectStore`].
pub type SharedObjectStore = Arc<dyn CosObjectStore>;

/// Thread-safe, process-local [`CosObjectStore`] used for testing and local
/// development. Keys are kept sorted so list results are deterministic.
#[derive(Debug, Default)]
pub struct InMemoryCosObjectStore {
    objects: RwLock<BTreeMap<String, Vec<u8>>>,
}

impl InMemoryCosObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps this backend in an `Arc` typed as a [`SharedObjectStore`].
    pub fn shared(self) -> SharedObjectStore {
        Arc::new(self)
    }
}

#[async_trait]
impl CosObjectStore for InMemoryCosObjectStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.objects.read().await.get(key).cloned())
    }

    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.objects.write().await.insert(key.to_string(), data.to_vec());
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.objects.write().await.remove(key);
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.objects.read().await.contains_key(key))
    }

    async fn len(&self) -> usize {
        self.objects.read().await.len()
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .objects
            .read()
            .await
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_store_roundtrip() {
        let store = InMemoryCosObjectStore::new();
        assert!(store.get("missing").await.unwrap().is_none());
        store.put_string("a/1.txt", "hello").await.unwrap();
        assert!(store.exists("a/1.txt").await.unwrap());
        assert_eq!(store.get_string("a/1.txt").await.unwrap().unwrap(), "hello");
        store.put_string("a/2.txt", "world").await.unwrap();
        store.put_string("b/3.txt", "x").await.unwrap();
        assert_eq!(store.list_prefix("a/").await.unwrap(), vec!["a/1.txt", "a/2.txt"]);
        store.delete("a/1.txt").await.unwrap();
        assert!(!store.exists("a/1.txt").await.unwrap());
        assert_eq!(store.len().await, 2);
    }
}
