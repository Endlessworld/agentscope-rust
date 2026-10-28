//! Async object-store abstraction, an in-memory backend, and the real COS client.
//!
//! The Java implementation talks directly to `COSClient`. The Rust port keeps every
//! component written against the [`CosObjectStore`] trait, so:
//!
//! * [`InMemoryCosObjectStore`] serves tests and local development,
//! * [`crate::CosHttpObjectStore`] is the real Tencent Cloud COS backend
//!   (`reqwest` + hand-written `q-sign-algorithm=sha1` signing, see
//!   [`crate::signature`]).
//!
//! The trait models the *semantics* the upper layers need instead of mirroring
//! `COSClient` call-for-call: [`CosObjectStore::list_page`] exposes the paginated
//! listing (continuation token / prefix / max-keys) that COS returns, and
//! [`CosObjectStore::delete_many`] exposes batch deletion (Java `deleteObjects`).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::{CosError, Result};

/// Page size used when a caller does not cap it (COS caps `max-keys` at 1000).
pub const DEFAULT_LIST_PAGE_SIZE: usize = 1000;

/// One page of a bucket listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectListPage {
    /// Object keys in this page, in the order the service returned them
    /// (lexicographic for COS).
    pub keys: Vec<String>,
    /// Token to pass as `continuation_token` for the next page; `None` when the
    /// listing is complete.
    pub next_continuation_token: Option<String>,
}

impl ObjectListPage {
    /// Whether another page exists.
    pub fn is_truncated(&self) -> bool {
        self.next_continuation_token.is_some()
    }
}

/// Minimal async object-store surface required by the COS components.
///
/// This mirrors the subset of `COSClient` operations actually used by the Java
/// module: `getObjectMetadata`, `getObject`, `putObject`, `deleteObject`,
/// `deleteObjects` and `listObjects` (marker / continuation token pagination).
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
    ///
    /// The default implementation lists the whole bucket; backends that can answer
    /// more cheaply should override it.
    async fn len(&self) -> usize {
        self.list_prefix("")
            .await
            .map(|keys| keys.len())
            .unwrap_or(0)
    }

    /// Lists all object keys starting with `prefix`, sorted lexicographically.
    ///
    /// This corresponds to Java's paginated `listAllKeys` helper, flattened into a
    /// single call.
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>>;

    /// Lists **one page** of object keys under `prefix`.
    ///
    /// `continuation_token` resumes after the token returned by the previous page;
    /// `max_keys` is clamped by the backend (COS caps it at 1000, `0` means
    /// "backend default").
    ///
    /// The default implementation paginates over [`Self::list_prefix`].
    async fn list_page(
        &self,
        prefix: &str,
        continuation_token: Option<&str>,
        max_keys: usize,
    ) -> Result<ObjectListPage> {
        let max_keys = if max_keys == 0 {
            DEFAULT_LIST_PAGE_SIZE
        } else {
            max_keys
        };
        let all = self.list_prefix(prefix).await?;
        // Continuation tokens are opaque; the in-memory contract is "resume after
        // this key", which matches COS `continuation-token` semantics.
        let start = match continuation_token {
            Some(token) => all
                .iter()
                .position(|key| key.as_str() > token)
                .unwrap_or(all.len()),
            None => 0,
        };
        let page: Vec<String> = all[start..].iter().take(max_keys).cloned().collect();
        let next_continuation_token = if start + page.len() < all.len() {
            page.last().cloned()
        } else {
            None
        };
        Ok(ObjectListPage {
            keys: page,
            next_continuation_token,
        })
    }

    /// Deletes several objects, treating missing keys as no-ops.
    ///
    /// Mirrors Java's `deleteObjects` batch call. The default implementation issues
    /// one [`Self::delete`] per key; [`crate::CosHttpObjectStore`] issues the real
    /// `POST /?delete` request (1000 keys per request).
    async fn delete_many(&self, keys: &[String]) -> Result<()> {
        for key in keys {
            self.delete(key).await?;
        }
        Ok(())
    }

    /// Releases backend resources. Idempotent; the default is a no-op.
    fn close(&self) {}

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
        self.objects
            .write()
            .await
            .insert(key.to_string(), data.to_vec());
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
        assert_eq!(
            store.list_prefix("a/").await.unwrap(),
            vec!["a/1.txt", "a/2.txt"]
        );
        store.delete("a/1.txt").await.unwrap();
        assert!(!store.exists("a/1.txt").await.unwrap());
        assert_eq!(store.len().await, 2);
    }

    /// The default `list_page` walks the sorted keyspace and hands back an opaque
    /// token that resumes after the last returned key.
    #[tokio::test]
    async fn default_list_page_paginates_and_resumes() {
        let store = InMemoryCosObjectStore::new();
        for key in ["p/a", "p/b", "p/c", "q/d"] {
            store.put_string(key, "x").await.unwrap();
        }

        let first = store.list_page("p/", None, 2).await.unwrap();
        assert_eq!(first.keys, vec!["p/a", "p/b"]);
        assert!(first.is_truncated());
        assert_eq!(first.next_continuation_token.as_deref(), Some("p/b"));

        let second = store
            .list_page("p/", first.next_continuation_token.as_deref(), 2)
            .await
            .unwrap();
        assert_eq!(second.keys, vec!["p/c"]);
        assert!(!second.is_truncated());
        assert!(second.next_continuation_token.is_none());

        // `max_keys == 0` means "backend default" rather than "no keys".
        let all = store.list_page("p/", None, 0).await.unwrap();
        assert_eq!(all.keys.len(), 3);
        assert!(all.next_continuation_token.is_none());

        // A token past the end yields an empty terminal page.
        let past_end = store.list_page("p/", Some("p/z"), 2).await.unwrap();
        assert!(past_end.keys.is_empty());
        assert!(past_end.next_continuation_token.is_none());
    }

    /// Batch delete mirrors Java `deleteObjects`: all keys disappear, missing keys
    /// are silently ignored.
    #[tokio::test]
    async fn delete_many_ignores_missing_keys() {
        let store = InMemoryCosObjectStore::new();
        store.put_string("a", "1").await.unwrap();
        store.put_string("b", "2").await.unwrap();
        store.put_string("c", "3").await.unwrap();

        store
            .delete_many(&["a".to_string(), "ghost".to_string(), "c".to_string()])
            .await
            .unwrap();

        assert_eq!(store.list_prefix("").await.unwrap(), vec!["b".to_string()]);
        // Deleting nothing is a no-op, not an error.
        store.delete_many(&[]).await.unwrap();
    }

    /// The trait-level `close` default is a no-op and idempotent.
    #[test]
    fn close_is_a_noop_by_default() {
        let store = InMemoryCosObjectStore::new();
        store.close();
        store.close();
    }
}
