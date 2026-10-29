//! Tencent Cloud COS backed [`AgentStateStore`], mirroring
//! `io.agentscope.extensions.cos.CosAgentStateStore`.
//!
//! State objects are stored as JSON in the object store with the layout:
//!
//! ```text
//! {prefix}{userId}/{sessionId}/{stateKey}.json       — single State value
//! {prefix}{userId}/{sessionId}/{stateKey}.list.json  — List<State> as JSON array
//! {prefix}{userId}/{sessionId}/{stateKey}.list.hash  — hash for incremental append
//! ```

use std::collections::BTreeSet;
use std::sync::Arc;

use agentscope_core::state::list_hash::{compute_hash, needs_full_rewrite};
use agentscope_core::state::AgentStateStore;
use async_trait::async_trait;

use crate::layout::{session_prefix, DEFAULT_STATE_PREFIX, HASH_SUFFIX, JSON_SUFFIX, LIST_SUFFIX};
use crate::object_store::SharedObjectStore;
use crate::{CosError, Result};

/// Marker type indicating the concrete `CosAgentStateStore` implementation.
pub struct CosAgentStateMarker;

/// Cos-backed agent state store.
///
/// Use [`CosAgentStateStore::builder`] to construct:
///
/// ```no_run
/// # use std::sync::Arc;
/// # use agentscope_extensions_cos::{CosAgentStateStore, InMemoryCosObjectStore};
/// let store = CosAgentStateStore::builder()
///     .object_store(InMemoryCosObjectStore::new().shared())
///     .key_prefix("agentscope/state/")
///     .build()
///     .unwrap();
/// ```
#[derive(Clone)]
pub struct CosAgentStateStore {
    object_store: SharedObjectStore,
    key_prefix: String,
}

impl std::fmt::Debug for CosAgentStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosAgentStateStore")
            .field("key_prefix", &self.key_prefix)
            .finish_non_exhaustive()
    }
}

impl CosAgentStateStore {
    pub fn builder() -> CosAgentStateStoreBuilder {
        CosAgentStateStoreBuilder::default()
    }

    /// The normalized key prefix in effect.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// Releases the underlying object store, mirroring Java's
    /// `CosAgentStateStore.close()` (which shuts the `COSClient` down).
    ///
    /// Idempotent. The backend is typically shared with the other COS components of a
    /// [`crate::CosDistributedStore`], so closing it closes them too.
    pub fn close(&self) {
        self.object_store.close();
    }

    fn state_key(&self, user: Option<&str>, session: &str, key: &str) -> Result<String> {
        Ok(format!(
            "{}{key}{JSON_SUFFIX}",
            session_prefix(&self.key_prefix, user, session)?
        ))
    }

    fn list_key(&self, user: Option<&str>, session: &str, key: &str) -> Result<String> {
        Ok(format!(
            "{}{key}{LIST_SUFFIX}",
            session_prefix(&self.key_prefix, user, session)?
        ))
    }

    fn hash_key(&self, user: Option<&str>, session: &str, key: &str) -> Result<String> {
        Ok(format!(
            "{}{key}{HASH_SUFFIX}",
            session_prefix(&self.key_prefix, user, session)?
        ))
    }
}

/// Builder for [`CosAgentStateStore`].
#[derive(Default)]
pub struct CosAgentStateStoreBuilder {
    object_store: Option<SharedObjectStore>,
    key_prefix: Option<String>,
}

impl CosAgentStateStoreBuilder {
    pub fn object_store(mut self, store: SharedObjectStore) -> Self {
        self.object_store = Some(store);
        self
    }

    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Some(prefix.into());
        self
    }

    pub fn build(self) -> Result<CosAgentStateStore> {
        let object_store = self
            .object_store
            .ok_or_else(|| CosError::Config("object_store must not be null".to_string()))?;
        Ok(CosAgentStateStore {
            object_store,
            key_prefix: crate::layout::normalize_prefix(
                self.key_prefix.as_deref(),
                DEFAULT_STATE_PREFIX,
            ),
        })
    }
}

#[async_trait]
impl AgentStateStore for CosAgentStateStore {
    async fn save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) {
        let object_key = match self.state_key(user_id, session_id, key) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error = %e, "Failed to save state: {key}");
                return;
            }
        };
        let json = match serde_json::to_string(&value) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(error = %e, "Failed to serialize state: {key}");
                return;
            }
        };
        if let Err(e) = self.object_store.put_string(&object_key, &json).await {
            tracing::error!(error = %e, "Failed to write COS object: {object_key}");
        }
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) {
        let list_key = match self.list_key(user_id, session_id, key) {
            Ok(k) => k,
            Err(_) => return,
        };
        let hash_key = match self.hash_key(user_id, session_id, key) {
            Ok(k) => k,
            Err(_) => return,
        };

        // `existing_count` mirrors Java's `existingCount`: the length of the list
        // currently stored, independent of whether a hash object exists. Keeping
        // these decoupled is what makes `needs_full_rewrite` Case 2 (missing hash
        // but existing data) reachable.
        let existing_count = match self
            .object_store
            .get_string(&list_key)
            .await
            .unwrap_or(None)
        {
            Some(json) => serde_json::from_str::<Vec<serde_json::Value>>(&json)
                .map(|v| v.len())
                .unwrap_or(0),
            None => 0,
        };
        let stored_hash = self
            .object_store
            .get_string(&hash_key)
            .await
            .unwrap_or(None);

        let current_hash = compute_hash(values);
        let full_rewrite = needs_full_rewrite(values, stored_hash.as_deref(), existing_count);

        if full_rewrite || values.len() != existing_count {
            if let Ok(json) = serde_json::to_string(values) {
                if let Err(e) = self.object_store.put_string(&list_key, &json).await {
                    tracing::error!(error = %e, "Failed to write COS list object: {list_key}");
                    return;
                }
            }
        }
        if let Err(e) = self.object_store.put_string(&hash_key, &current_hash).await {
            tracing::error!(error = %e, "Failed to write COS hash object: {hash_key}");
        }
    }

    async fn get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Option<serde_json::Value> {
        let object_key = self.state_key(user_id, session_id, key).ok()?;
        let json = self.object_store.get_string(&object_key).await.ok()??;
        serde_json::from_str(&json).ok()
    }

    async fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Vec<serde_json::Value> {
        let list_key = match self.list_key(user_id, session_id, key) {
            Ok(k) => k,
            Err(_) => return Vec::new(),
        };
        match self.object_store.get_string(&list_key).await.ok().flatten() {
            Some(json) => serde_json::from_str(&json).unwrap_or_default(),
            None => Vec::new(),
        }
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        let prefix = match session_prefix(&self.key_prefix, user_id, session_id) {
            Ok(p) => p,
            Err(_) => return false,
        };
        match self.object_store.list_prefix(&prefix).await {
            Ok(keys) => !keys.is_empty(),
            Err(_) => false,
        }
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        let prefix = match session_prefix(&self.key_prefix, user_id, session_id) {
            Ok(p) => p,
            Err(_) => return,
        };
        if let Ok(keys) = self.object_store.list_prefix(&prefix).await {
            // Batch delete (Java `deleteKeys`: 1000 keys per `deleteObjects` call).
            if !keys.is_empty() {
                let _ = self.object_store.delete_many(&keys).await;
            }
        }
    }

    async fn delete_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> agentscope_core::error::Result<()> {
        // Java `CosAgentStateStore.delete(userId, sessionId, key)` removes the value plus its
        // list payload and incremental hash, so a later `saveList` cannot append onto a stale
        // hash. All three objects go out in one batch (`deleteKeys`).
        let to_core = |e: CosError| {
            agentscope_core::error::Error::Other(format!("COS per-key delete failed: {e}"))
        };
        let keys = [
            self.state_key(user_id, session_id, key).map_err(to_core)?,
            self.list_key(user_id, session_id, key).map_err(to_core)?,
            self.hash_key(user_id, session_id, key).map_err(to_core)?,
        ];
        self.object_store.delete_many(&keys).await.map_err(to_core)?;
        Ok(())
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let user_prefix = format!(
            "{}{}/",
            self.key_prefix,
            crate::layout::normalize_user(user_id)
        );
        let keys = match self.object_store.list_prefix(&user_prefix).await {
            Ok(k) => k,
            Err(_) => return Vec::new(),
        };
        let mut session_ids = BTreeSet::new();
        for key in keys {
            let remainder = &key[user_prefix.len()..];
            if let Some(slash) = remainder.find('/') {
                if slash > 0 {
                    session_ids.insert(remainder[..slash].to_string());
                }
            }
        }
        session_ids.into_iter().collect()
    }
}

/// Convenience constructor from an already-built shared [`CosObjectStore`].
pub fn cos_agent_state_store(
    object_store: SharedObjectStore,
    key_prefix: Option<&str>,
) -> Result<CosAgentStateStore> {
    CosAgentStateStore::builder()
        .object_store(object_store)
        .apply_prefix(key_prefix)
        .build()
}

trait BuilderPrefixExt {
    fn apply_prefix(self, prefix: Option<&str>) -> Self;
}

impl BuilderPrefixExt for CosAgentStateStoreBuilder {
    fn apply_prefix(self, prefix: Option<&str>) -> Self {
        match prefix {
            Some(p) => self.key_prefix(p),
            None => self,
        }
    }
}

/// Ergonomic alias for the shared `Arc<dyn AgentStateStore>` form.
pub fn shared_agent_state_store(store: CosAgentStateStore) -> Arc<dyn AgentStateStore> {
    Arc::new(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::InMemoryCosObjectStore;

    fn store() -> CosAgentStateStore {
        CosAgentStateStore::builder()
            .object_store(InMemoryCosObjectStore::new().shared())
            .build()
            .unwrap()
    }

    /// In-memory backend that counts writes, used to assert that an unchanged list
    /// skips the list-object rewrite (only `.list.hash` is refreshed).
    #[derive(Debug, Default)]
    struct CountingStore {
        inner: InMemoryCosObjectStore,
        puts: std::sync::atomic::AtomicUsize,
    }

    impl CountingStore {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn puts(&self) -> usize {
            self.puts.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl crate::CosObjectStore for CountingStore {
        async fn get(&self, key: &str) -> crate::Result<Option<Vec<u8>>> {
            self.inner.get(key).await
        }
        async fn put(&self, key: &str, data: &[u8]) -> crate::Result<()> {
            self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.put(key, data).await
        }
        async fn delete(&self, key: &str) -> crate::Result<()> {
            self.inner.delete(key).await
        }
        async fn exists(&self, key: &str) -> crate::Result<bool> {
            self.inner.exists(key).await
        }
        async fn len(&self) -> usize {
            self.inner.len().await
        }
        async fn list_prefix(&self, prefix: &str) -> crate::Result<Vec<String>> {
            self.inner.list_prefix(prefix).await
        }
    }

    #[tokio::test]
    async fn save_get_roundtrip() {
        let s = store();
        s.save(Some("u1"), "s1", "k", serde_json::json!({"a": 1}))
            .await;
        let got = s.get(Some("u1"), "s1", "k").await.unwrap();
        assert_eq!(got, serde_json::json!({"a": 1}));
        assert!(s.get(Some("u1"), "s1", "missing").await.is_none());
    }

    #[tokio::test]
    async fn list_save_get_roundtrip() {
        let s = store();
        let values = vec![serde_json::json!(1), serde_json::json!(2)];
        s.save_list(Some("u1"), "s1", "items", &values).await;
        assert_eq!(s.get_list(Some("u1"), "s1", "items").await, values);
        assert!(s.get_list(Some("u1"), "s1", "nope").await.is_empty());
    }

    #[tokio::test]
    async fn exists_delete_and_list_sessions() {
        let s = store();
        assert!(!s.exists(Some("u1"), "s1").await);
        s.save(Some("u1"), "s1", "a", serde_json::json!(1)).await;
        s.save(Some("u1"), "s2", "b", serde_json::json!(2)).await;
        assert!(s.exists(Some("u1"), "s1").await);
        let mut ids = s.list_session_ids(Some("u1")).await;
        ids.sort();
        assert_eq!(ids, vec!["s1".to_string(), "s2".to_string()]);
        s.delete(Some("u1"), "s1").await;
        assert!(!s.exists(Some("u1"), "s1").await);
        assert!(s.exists(Some("u1"), "s2").await);
    }

    #[tokio::test]
    async fn anonymous_user_uses_anon_segment() {
        let s = store();
        s.save(None, "s1", "a", serde_json::json!(1)).await;
        assert_eq!(s.get(None, "s1", "a").await.unwrap(), serde_json::json!(1));
        assert_eq!(s.list_session_ids(None).await, vec!["s1".to_string()]);
    }

    #[test]
    fn prefix_normalization() {
        let s = CosAgentStateStore::builder()
            .object_store(InMemoryCosObjectStore::new().shared())
            .key_prefix("/my/prefix")
            .build()
            .unwrap();
        assert_eq!(s.key_prefix(), "my/prefix/");
        let s2 = CosAgentStateStore::builder()
            .object_store(InMemoryCosObjectStore::new().shared())
            .build()
            .unwrap();
        assert_eq!(s2.key_prefix(), DEFAULT_STATE_PREFIX);
    }

    /// `[a]` then `[a, b]`: the previously stored hash covers the unchanged prefix,
    /// so the append must **not** raise the full-rewrite flag.
    ///
    /// Regression guard: the removed local implementation compared
    /// `compute_list_hash(whole list).starts_with(stored_hash)`, which is never true
    /// for the full-list hashes of different list sizes — appends were always
    /// flagged as full rewrites.
    ///
    /// Note on the COS layout: the list lives in a single JSON object, so an append
    /// still rewrites that object (the new element has to be persisted). What the
    /// fix restores is the *predicate* semantics shared with Java/core.
    #[tokio::test]
    async fn pure_append_does_not_trigger_full_rewrite() {
        let backend = InMemoryCosObjectStore::new().shared();
        let s = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();

        s.save_list(Some("u1"), "s1", "items", &[serde_json::json!("a")])
            .await;
        let hash_key = s.hash_key(Some("u1"), "s1", "items").unwrap();
        let stored_hash = backend.get_string(&hash_key).await.unwrap().unwrap();

        let appended = [serde_json::json!("a"), serde_json::json!("b")];

        // The append is recognized as appending the existing `[a]` prefix ...
        assert!(
            !needs_full_rewrite(&appended[..], Some(&stored_hash), 1),
            "appending to an unmodified prefix must not require a full rewrite"
        );

        // ... while the old implementation's `starts_with` condition is provably
        // false here, i.e. it *did* demand a full rewrite for the same input.
        let whole = compute_hash(&appended[..]);
        let old_predicate = !whole.starts_with(&stored_hash);
        assert!(
            old_predicate,
            "full-list hashes of different sizes are not prefix-related — the removed \
             `!compute_list_hash(values).starts_with(stored_hash)` always yielded `true`"
        );

        s.save_list(Some("u1"), "s1", "items", &appended).await;

        // The appended data is persisted and the hash tracks the new full list.
        assert_eq!(
            s.get_list(Some("u1"), "s1", "items").await,
            appended.to_vec()
        );
        assert_eq!(backend.get_string(&hash_key).await.unwrap().unwrap(), whole);
    }

    /// A same-length, unmodified list is not rewritten (dedup path preserved).
    #[tokio::test]
    async fn identical_list_is_not_rewritten() {
        let backend = CountingStore::new();
        let s = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();
        let values = vec![serde_json::json!(1), serde_json::json!(2)];

        s.save_list(Some("u1"), "s1", "items", &values).await;
        assert_eq!(backend.puts(), 2, "list object + hash object");

        s.save_list(Some("u1"), "s1", "items", &values).await;
        assert_eq!(
            backend.puts(),
            3,
            "an unchanged list must not rewrite the list object"
        );
    }

    /// `[a, b]` then `[a, b, c]` is likewise a pure append.
    #[test]
    fn appended_prefix_hash_stays_valid() {
        let prefix = vec![serde_json::json!("a"), serde_json::json!("b")];
        let full = vec![
            serde_json::json!("a"),
            serde_json::json!("b"),
            serde_json::json!("c"),
        ];
        let stored = compute_hash(&prefix);
        assert!(!needs_full_rewrite(&full, Some(&stored), prefix.len()));
    }

    /// Modifying an element inside the previously stored prefix requires a rewrite.
    #[test]
    fn modified_element_triggers_full_rewrite() {
        let stored = compute_hash(&[serde_json::json!("a"), serde_json::json!("b")]);
        let changed = vec![
            serde_json::json!("a"),
            serde_json::json!("CHANGED"),
            serde_json::json!("c"),
        ];
        assert!(needs_full_rewrite(&changed, Some(&stored), 2));
    }

    /// Shrinking the list requires a rewrite.
    #[test]
    fn shrink_triggers_full_rewrite() {
        let stored = compute_hash(&[serde_json::json!(1), serde_json::json!(2)]);
        assert!(needs_full_rewrite(
            &[serde_json::json!(1)],
            Some(&stored),
            2
        ));
    }

    /// With no pre-existing data a missing hash must not force a rewrite.
    #[test]
    fn empty_existing_state_does_not_force_rewrite() {
        assert!(!needs_full_rewrite(&[serde_json::json!(1)], None, 0));
        // ... but a missing hash *with* existing data cannot be verified.
        assert!(needs_full_rewrite(&[serde_json::json!(1)], None, 1));
    }

    /// First write over legacy `.list.json` data written by the old SHA-256 scheme:
    /// `existing_count` is observed but the hash object is missing/stale, so the
    /// store rewrites the list and self-heals. No data is lost.
    #[tokio::test]
    async fn legacy_state_without_hash_self_heals() {
        let backend = InMemoryCosObjectStore::new().shared();
        let s = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();

        let list_key = s.list_key(Some("u1"), "s1", "items").unwrap();
        let hash_key = s.hash_key(Some("u1"), "s1", "items").unwrap();
        let legacy = vec![serde_json::json!("old-1"), serde_json::json!("old-2")];
        backend
            .put_string(&list_key, &serde_json::to_string(&legacy).unwrap())
            .await
            .unwrap();
        // Legacy SHA-256 hash object present, but unrelated to core's scheme.
        backend.put_string(&hash_key, "deadbeef").await.unwrap();

        let updated = vec![
            serde_json::json!("old-1"),
            serde_json::json!("old-2"),
            serde_json::json!("new"),
        ];
        s.save_list(Some("u1"), "s1", "items", &updated).await;

        assert_eq!(s.get_list(Some("u1"), "s1", "items").await, updated);
        assert_eq!(
            backend.get_string(&hash_key).await.unwrap().unwrap(),
            compute_hash(&updated)
        );
    }

    /// Object keys match the Java layout literally, including the anonymous user
    /// segment and the `.json` / `.list.json` / `.list.hash` suffixes.
    #[tokio::test]
    async fn object_keys_follow_the_java_layout() {
        let backend = InMemoryCosObjectStore::new().shared();
        let s = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();

        s.save(Some("u1"), "s1", "k", serde_json::json!(1)).await;
        s.save(None, "s2", "k", serde_json::json!(2)).await;
        s.save_list(Some("u1"), "s1", "items", &[serde_json::json!("a")])
            .await;

        let mut keys = backend.list_prefix("").await.unwrap();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "agentscope/state/__anon__/s2/k.json".to_string(),
                "agentscope/state/u1/s1/items.list.hash".to_string(),
                "agentscope/state/u1/s1/items.list.json".to_string(),
                "agentscope/state/u1/s1/k.json".to_string(),
            ]
        );
    }

    /// Java `delete(userId, sessionId, key)` removes the value plus its list payload and
    /// incremental hash, leaving every other key of the session intact.
    #[tokio::test]
    async fn delete_key_removes_value_list_and_hash_only() {
        let backend = InMemoryCosObjectStore::new().shared();
        let s = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();

        s.save(Some("u1"), "s1", "k", serde_json::json!(1)).await;
        s.save_list(Some("u1"), "s1", "k", &[serde_json::json!("a")])
            .await;
        s.save(Some("u1"), "s1", "other", serde_json::json!(2))
            .await;

        s.delete_key(Some("u1"), "s1", "k").await.unwrap();

        let mut keys = backend.list_prefix("").await.unwrap();
        keys.sort();
        assert_eq!(keys, vec!["agentscope/state/u1/s1/other.json".to_string()]);
        assert_eq!(s.get(Some("u1"), "s1", "k").await, None);
        assert!(s.get_list(Some("u1"), "s1", "k").await.is_empty());
        assert_eq!(
            s.get(Some("u1"), "s1", "other").await,
            Some(serde_json::json!(2))
        );
    }

    /// Blank / `/`-only / backslash / leading-slash prefixes all normalize the Java
    /// way; a blank prefix falls back to `agentscope/state/`.
    #[test]
    fn prefix_edge_cases_normalize_like_java() {
        let build = |prefix: Option<&str>| {
            let mut builder =
                CosAgentStateStore::builder().object_store(InMemoryCosObjectStore::new().shared());
            if let Some(prefix) = prefix {
                builder = builder.key_prefix(prefix);
            }
            builder.build().unwrap()
        };

        assert_eq!(build(None).key_prefix(), DEFAULT_STATE_PREFIX);
        assert_eq!(build(Some("")).key_prefix(), DEFAULT_STATE_PREFIX);
        assert_eq!(build(Some("   ")).key_prefix(), DEFAULT_STATE_PREFIX);
        assert_eq!(build(Some("/")).key_prefix(), DEFAULT_STATE_PREFIX);
        assert_eq!(
            build(Some("/agentscope\\state")).key_prefix(),
            "agentscope/state/"
        );
        // A non-blank prefix keeps its surrounding whitespace (Java parity).
        assert_eq!(
            build(Some(" agentscope/state ")).key_prefix(),
            " agentscope/state /"
        );
        assert_eq!(
            build(Some("agentscope/state/")).key_prefix(),
            "agentscope/state/"
        );
    }

    /// The builder rejects a missing object store instead of panicking later.
    #[test]
    fn builder_rejects_missing_object_store() {
        let err = CosAgentStateStore::builder().build().unwrap_err();
        assert!(err.to_string().contains("object_store must not be null"));

        let err = CosAgentStateStore::builder()
            .key_prefix("custom/")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("object_store must not be null"));
    }

    /// Session deletion goes through one batch call (Java `deleteObjects`), not one
    /// `deleteObject` per key, and clears the whole session prefix.
    #[tokio::test]
    async fn delete_session_uses_a_single_batch_call() {
        #[derive(Debug, Default)]
        struct BatchStore {
            inner: InMemoryCosObjectStore,
            deletes: std::sync::atomic::AtomicUsize,
            batches: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl crate::CosObjectStore for BatchStore {
            async fn get(&self, key: &str) -> crate::Result<Option<Vec<u8>>> {
                self.inner.get(key).await
            }
            async fn put(&self, key: &str, data: &[u8]) -> crate::Result<()> {
                self.inner.put(key, data).await
            }
            async fn delete(&self, key: &str) -> crate::Result<()> {
                self.deletes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.delete(key).await
            }
            async fn delete_many(&self, keys: &[String]) -> crate::Result<()> {
                self.batches
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                for key in keys {
                    self.inner.delete(key).await?;
                }
                Ok(())
            }
            async fn exists(&self, key: &str) -> crate::Result<bool> {
                self.inner.exists(key).await
            }
            async fn len(&self) -> usize {
                self.inner.len().await
            }
            async fn list_prefix(&self, prefix: &str) -> crate::Result<Vec<String>> {
                self.inner.list_prefix(prefix).await
            }
        }

        let backend = Arc::new(BatchStore::default());
        let s = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();
        s.save(Some("u1"), "s1", "a", serde_json::json!(1)).await;
        s.save(Some("u1"), "s1", "b", serde_json::json!(2)).await;
        s.save(Some("u1"), "s2", "c", serde_json::json!(3)).await;

        s.delete(Some("u1"), "s1").await;

        assert_eq!(backend.batches.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(backend.deletes.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!s.exists(Some("u1"), "s1").await);
        assert!(s.exists(Some("u1"), "s2").await);

        // An empty session prefix must not issue a batch call at all.
        s.delete(Some("u1"), "s1").await;
        assert_eq!(backend.batches.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// `close()` delegates to the object store and is idempotent (Java
    /// `CosAgentStateStore.close()` shuts the `COSClient` down).
    #[tokio::test]
    async fn close_delegates_and_is_idempotent() {
        #[derive(Debug, Default)]
        struct ClosingStore {
            inner: InMemoryCosObjectStore,
            closes: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl crate::CosObjectStore for ClosingStore {
            async fn get(&self, key: &str) -> crate::Result<Option<Vec<u8>>> {
                self.inner.get(key).await
            }
            async fn put(&self, key: &str, data: &[u8]) -> crate::Result<()> {
                self.inner.put(key, data).await
            }
            async fn delete(&self, key: &str) -> crate::Result<()> {
                self.inner.delete(key).await
            }
            async fn exists(&self, key: &str) -> crate::Result<bool> {
                self.inner.exists(key).await
            }
            async fn len(&self) -> usize {
                self.inner.len().await
            }
            async fn list_prefix(&self, prefix: &str) -> crate::Result<Vec<String>> {
                self.inner.list_prefix(prefix).await
            }
            fn close(&self) {
                self.closes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let backend = Arc::new(ClosingStore::default());
        let s = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();
        s.save(Some("u1"), "s1", "k", serde_json::json!(1)).await;

        s.close();
        s.close();
        assert_eq!(backend.closes.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Closing an in-memory backend does not lose data; only a real client stops
        // accepting requests (covered by the HTTP adapter tests).
        assert_eq!(
            s.get(Some("u1"), "s1", "k").await.unwrap(),
            serde_json::json!(1)
        );
    }
}
