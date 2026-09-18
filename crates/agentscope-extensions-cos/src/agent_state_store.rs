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

use agentscope_core::state::AgentStateStore;
use async_trait::async_trait;

use crate::layout::{
    session_prefix, DEFAULT_STATE_PREFIX, HASH_SUFFIX, JSON_SUFFIX, LIST_SUFFIX,
};
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

    fn state_key(&self, user: Option<&str>, session: &str, key: &str) -> Result<String> {
        Ok(format!("{}{key}{JSON_SUFFIX}", session_prefix(&self.key_prefix, user, session)?))
    }

    fn list_key(&self, user: Option<&str>, session: &str, key: &str) -> Result<String> {
        Ok(format!("{}{key}{LIST_SUFFIX}", session_prefix(&self.key_prefix, user, session)?))
    }

    fn hash_key(&self, user: Option<&str>, session: &str, key: &str) -> Result<String> {
        Ok(format!("{}{key}{HASH_SUFFIX}", session_prefix(&self.key_prefix, user, session)?))
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

/// Computes the incremental-append hash mirroring `ListHashUtil.computeHash`.
///
/// Java's `ListHashUtil` is core-internal; this port keeps an equivalent stable
/// hash (JSON-rendered length + content hash) so `.list.hash` values stay
/// consistent across calls. The precise algorithm is not cross-language loadable.
fn compute_list_hash(values: &[serde_json::Value]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for v in values {
        let rendered = serde_json::to_string(v).unwrap_or_default();
        hasher.update((rendered.len() as u64).to_le_bytes());
        hasher.update(rendered.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Whether the new list requires a full rewrite given the stored hash/count.
///
/// Mirrors `ListHashUtil.needsFullRewrite`: when there is no stored hash, or the
/// stored hash is a prefix of the new hash (pure append), no rewrite is implied.
fn needs_full_rewrite(values: &[serde_json::Value], stored_hash: Option<&str>, existing_count: usize) -> bool {
    match stored_hash {
        None => true,
        Some(h) => values.len() < existing_count || !compute_list_hash(values).starts_with(h),
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

        let stored_hash = self.object_store.get_string(&hash_key).await.unwrap_or(None);
        let existing_count = if stored_hash.is_some() {
            match self.object_store.get_string(&list_key).await.unwrap_or(None) {
                Some(json) => serde_json::from_str::<Vec<serde_json::Value>>(&json)
                    .map(|v| v.len())
                    .unwrap_or(0),
                None => 0,
            }
        } else {
            0
        };

        let current_hash = compute_list_hash(values);
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
            for key in keys {
                let _ = self.object_store.delete(&key).await;
            }
        }
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

    #[tokio::test]
    async fn save_get_roundtrip() {
        let s = store();
        s.save(Some("u1"), "s1", "k", serde_json::json!({"a": 1})).await;
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
}
