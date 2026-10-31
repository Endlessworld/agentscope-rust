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
use agentscope_core::state::{AgentStateStore, VersionedState, UNVERSIONED};
use async_trait::async_trait;

use crate::layout::{
    session_prefix, DEFAULT_STATE_PREFIX, HASH_SUFFIX, JSON_SUFFIX, LIST_SUFFIX, VERSION_SUFFIX,
};
use crate::object_store::{ConditionalWrite, SharedObjectStore};
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

    /// Key of the version counter used by [`AgentStateStore::save_if_version`].
    ///
    /// Same `{key}.version` side object as the base store; COS has no per-object
    /// version field, and keeping the counter out of the state JSON preserves the
    /// Java-compatible payload layout.
    fn version_key(&self, user: Option<&str>, session: &str, key: &str) -> Result<String> {
        Ok(format!(
            "{}{key}{VERSION_SUFFIX}",
            session_prefix(&self.key_prefix, user, session)?
        ))
    }

    /// Parses a stored version counter.
    ///
    /// A malformed or negative counter is a real error: reporting it as `0` would
    /// turn a corrupt object into a spurious "create-if-absent" CAS.
    fn parse_version(version_key: &str, content: &str) -> Result<i64> {
        let text = content.trim();
        let version = text.parse::<i64>().map_err(|e| {
            CosError::Serialization(format!(
                "version object {version_key} is not an integer: {e}"
            ))
        })?;
        if version < 0 {
            return Err(CosError::Serialization(format!(
                "version object {version_key} is negative ({version})"
            )));
        }
        Ok(version)
    }

    /// Reads the version counter of `version_key`; `None` means "never written".
    async fn read_version(&self, version_key: &str) -> Result<Option<i64>> {
        let Some(content) = self.object_store.get_string(version_key).await? else {
            return Ok(None);
        };
        Self::parse_version(version_key, &content).map(Some)
    }

    /// Best-effort unconditional counter increment used by [`AgentStateStore::save`].
    ///
    /// `save` keeps last-writer-wins semantics, but it must still advance the
    /// counter: otherwise a plain write followed by a CAS at the stale version would
    /// silently overwrite it.
    async fn bump_version(&self, version_key: &str) -> Result<i64> {
        let current = self.read_version(version_key).await?.unwrap_or(0);
        let next = current + 1;
        self.object_store
            .put_string(version_key, &next.to_string())
            .await?;
        Ok(next)
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
            return;
        }
        // Keep the CAS counter in step with the value even on the unconditional path.
        match self.version_key(user_id, session_id, key) {
            Ok(version_key) => {
                if let Err(e) = self.bump_version(&version_key).await {
                    tracing::error!(error = %e, "Failed to bump COS state version: {version_key}");
                }
            }
            Err(e) => tracing::error!(error = %e, "Failed to resolve version key: {key}"),
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
        // One object is all that is needed, so ask for a single key instead of
        // listing the whole session prefix (Java `CosAgentStateStore.exists` sets
        // `maxKeys=1` for the same reason).
        match self.object_store.list_page(&prefix, None, 1).await {
            Ok(page) => !page.keys.is_empty(),
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
        // hash. The Rust port also drops the CAS counter, otherwise a re-created key would
        // keep the deleted key's version and a later `save_if_version(0)` could never
        // succeed. All four objects go out in one batch (`deleteKeys`).
        let to_core = |e: CosError| {
            agentscope_core::error::Error::Other(format!("COS per-key delete failed: {e}"))
        };
        let keys = [
            self.state_key(user_id, session_id, key).map_err(to_core)?,
            self.list_key(user_id, session_id, key).map_err(to_core)?,
            self.hash_key(user_id, session_id, key).map_err(to_core)?,
            self.version_key(user_id, session_id, key)
                .map_err(to_core)?,
        ];
        self.object_store
            .delete_many(&keys)
            .await
            .map_err(to_core)?;
        Ok(())
    }

    fn supports_versioning(&self) -> bool {
        // Only advertise CAS when the backend can actually enforce a precondition;
        // otherwise callers degrade to last-writer-wins `save` instead of seeing every
        // CAS fail.
        self.object_store.supports_conditional_writes()
    }

    async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> VersionedState {
        match self.try_get_versioned(user_id, session_id, key).await {
            Ok(state) => state,
            Err(e) => {
                // Infallible surface: log the failure, but never report
                // `UNVERSIONED` here — that would let the next CAS overwrite blindly.
                tracing::error!(error = %e, "Failed to read versioned state: {key}");
                VersionedState {
                    value: None,
                    version: 0,
                }
            }
        }
    }

    async fn try_get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> agentscope_core::error::Result<VersionedState> {
        let to_core = |e: CosError| {
            agentscope_core::error::Error::Other(format!("COS versioned read failed: {e}"))
        };
        let object_key = self.state_key(user_id, session_id, key).map_err(to_core)?;
        let version_key = self
            .version_key(user_id, session_id, key)
            .map_err(to_core)?;

        let value = match self
            .object_store
            .get_string(&object_key)
            .await
            .map_err(to_core)?
        {
            Some(json) => Some(
                serde_json::from_str::<serde_json::Value>(&json).map_err(|e| {
                    agentscope_core::error::Error::Other(format!(
                        "COS state object {object_key} is not valid JSON: {e}"
                    ))
                })?,
            ),
            None => None,
        };
        let version = self
            .read_version(&version_key)
            .await
            .map_err(to_core)?
            .unwrap_or(0);
        Ok(VersionedState { value, version })
    }

    async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> i64 {
        match self
            .try_save_if_version(user_id, session_id, key, value, expected_version)
            .await
        {
            Ok(version) => version,
            Err(e) => {
                // Infallible contract: report the failure as "not written" (the same
                // signal as a lost CAS) so the caller never assumes a successful write.
                // `UNVERSIONED` here means "nothing was written", not "overwrite".
                tracing::error!(error = %e, "Failed to write versioned state: {key}");
                UNVERSIONED
            }
        }
    }

    /// Compare-and-swap on the `{key}.version` side object.
    ///
    /// The counter is claimed with a **conditional** PUT (`If-Match` against the
    /// version object's ETag, `If-None-Match: *` when it does not exist yet), so two
    /// processes racing on the same version cannot both win: COS answers `412` and
    /// the loser gets [`UNVERSIONED`]. The value is written only after the counter is
    /// claimed.
    async fn try_save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> agentscope_core::error::Result<i64> {
        let to_core = |e: CosError| {
            agentscope_core::error::Error::Other(format!("COS versioned write failed: {e}"))
        };
        let object_key = self.state_key(user_id, session_id, key).map_err(to_core)?;
        let version_key = self
            .version_key(user_id, session_id, key)
            .map_err(to_core)?;
        let json = serde_json::to_string(&value).map_err(|e| {
            to_core(CosError::Serialization(format!(
                "failed to serialize state {key}: {e}"
            )))
        })?;

        if expected_version == UNVERSIONED {
            // Unconditional overwrite (`save` semantics) that still advances the
            // counter so later CAS reads do not observe a stale version.
            self.object_store
                .put_string(&object_key, &json)
                .await
                .map_err(to_core)?;
            return self.bump_version(&version_key).await.map_err(to_core);
        }

        let (current, etag) = match self
            .object_store
            .get_etagged(&version_key)
            .await
            .map_err(to_core)?
        {
            Some(object) => {
                let text = String::from_utf8(object.data).map_err(|e| {
                    to_core(CosError::Serialization(format!(
                        "version object {version_key} is not UTF-8: {e}"
                    )))
                })?;
                (
                    Self::parse_version(&version_key, &text).map_err(to_core)?,
                    Some(object.etag),
                )
            }
            None => (0, None),
        };
        if current != expected_version {
            return Ok(UNVERSIONED);
        }

        let next = current + 1;
        let counter = next.to_string();
        let claimed = match etag.as_deref() {
            Some(etag) => {
                self.object_store
                    .put_if_match(&version_key, counter.as_bytes(), etag)
                    .await
            }
            None => {
                self.object_store
                    .put_if_absent(&version_key, counter.as_bytes())
                    .await
            }
        }
        .map_err(to_core)?;
        if claimed == ConditionalWrite::Conflict {
            // Another writer claimed this version between our read and our write.
            return Ok(UNVERSIONED);
        }

        self.object_store
            .put_string(&object_key, &json)
            .await
            .map_err(to_core)?;
        Ok(next)
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
                "agentscope/state/__anon__/s2/k.version".to_string(),
                "agentscope/state/u1/s1/items.list.hash".to_string(),
                "agentscope/state/u1/s1/items.list.json".to_string(),
                "agentscope/state/u1/s1/k.json".to_string(),
                "agentscope/state/u1/s1/k.version".to_string(),
            ]
        );
    }

    /// Java `delete(userId, sessionId, key)` removes the value plus its list payload and
    /// incremental hash, leaving every other key of the session intact. The Rust port also
    /// drops the CAS counter so a re-created key starts over at version 0.
    #[tokio::test]
    async fn delete_key_removes_value_list_hash_and_version() {
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
        assert_eq!(
            keys,
            vec![
                "agentscope/state/u1/s1/other.json".to_string(),
                "agentscope/state/u1/s1/other.version".to_string(),
            ]
        );
        assert_eq!(s.get(Some("u1"), "s1", "k").await, None);
        assert!(s.get_list(Some("u1"), "s1", "k").await.is_empty());
        assert_eq!(
            s.get(Some("u1"), "s1", "other").await,
            Some(serde_json::json!(2))
        );
    }

    /// `save` stays last-writer-wins but must still advance the counter, otherwise a
    /// CAS at the stale version could overwrite it silently.
    #[tokio::test]
    async fn save_advances_the_version_counter() {
        let s = store();
        let fresh = s.get_versioned(Some("u1"), "s1", "k").await;
        assert_eq!(fresh.version, 0);
        assert!(fresh.value.is_none());

        s.save(Some("u1"), "s1", "k", serde_json::json!({"n": 1}))
            .await;
        let first = s.get_versioned(Some("u1"), "s1", "k").await;
        assert_eq!(first.version, 1);
        assert_eq!(first.value, Some(serde_json::json!({"n": 1})));

        s.save(Some("u1"), "s1", "k", serde_json::json!({"n": 2}))
            .await;
        assert_eq!(s.get_versioned(Some("u1"), "s1", "k").await.version, 2);
    }

    /// Expected version `0` creates; a second CAS at `0` must report the conflict and
    /// leave the stored value untouched.
    #[tokio::test]
    async fn save_if_version_creates_then_reports_the_lost_race() {
        let s = store();
        let created = s
            .save_if_version(Some("u1"), "s1", "k", serde_json::json!({"v": 1}), 0)
            .await;
        assert_eq!(created, 1);
        assert_eq!(
            s.get(Some("u1"), "s1", "k").await,
            Some(serde_json::json!({"v": 1}))
        );

        let conflicted = s
            .save_if_version(Some("u1"), "s1", "k", serde_json::json!({"v": 2}), 0)
            .await;
        assert_eq!(conflicted, UNVERSIONED);
        assert_eq!(
            s.get(Some("u1"), "s1", "k").await,
            Some(serde_json::json!({"v": 1})),
            "a lost CAS must not write"
        );

        let advanced = s
            .save_if_version(Some("u1"), "s1", "k", serde_json::json!({"v": 2}), 1)
            .await;
        assert_eq!(advanced, 2);
        assert_eq!(
            s.get(Some("u1"), "s1", "k").await,
            Some(serde_json::json!({"v": 2}))
        );
    }

    /// Two writers that both read version 1: the first CAS wins, the second one's
    /// `If-Match` no longer matches the version object, so it must conflict instead of
    /// overwriting the winner's value.
    #[tokio::test]
    async fn stale_etag_cas_conflicts_instead_of_overwriting() {
        let backend = InMemoryCosObjectStore::new().shared();
        let a = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();
        let b = CosAgentStateStore::builder()
            .object_store(backend)
            .build()
            .unwrap();

        a.save(Some("u1"), "s1", "k", serde_json::json!({"writer": "seed"}))
            .await;
        let seen_by_a = a.try_get_versioned(Some("u1"), "s1", "k").await.unwrap();
        let seen_by_b = b.try_get_versioned(Some("u1"), "s1", "k").await.unwrap();
        assert_eq!(seen_by_a.version, 1);
        assert_eq!(seen_by_b.version, 1);

        let won = b
            .try_save_if_version(
                Some("u1"),
                "s1",
                "k",
                serde_json::json!({"writer": "b"}),
                seen_by_b.version,
            )
            .await
            .unwrap();
        assert_eq!(won, 2);

        let lost = a
            .try_save_if_version(
                Some("u1"),
                "s1",
                "k",
                serde_json::json!({"writer": "a"}),
                seen_by_a.version,
            )
            .await
            .unwrap();
        assert_eq!(lost, UNVERSIONED);
        assert_eq!(
            a.get(Some("u1"), "s1", "k").await,
            Some(serde_json::json!({"writer": "b"}))
        );
        assert_eq!(a.get_versioned(Some("u1"), "s1", "k").await.version, 2);
    }

    /// Backend wrapper that serves a **stale** view of one version object: the body and
    /// entity tag captured *before* a concurrent commit, i.e. exactly what a writer that
    /// read early would hold. All other calls are forwarded, including the conditional
    /// writes, so the real backend still enforces the precondition.
    #[derive(Debug)]
    struct StaleViewStore {
        inner: SharedObjectStore,
        stale_key: String,
        stale: crate::object_store::EtaggedObject,
    }

    #[async_trait]
    impl crate::CosObjectStore for StaleViewStore {
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
        async fn get_etagged(
            &self,
            key: &str,
        ) -> crate::Result<Option<crate::object_store::EtaggedObject>> {
            if key == self.stale_key {
                return Ok(Some(self.stale.clone()));
            }
            self.inner.get_etagged(key).await
        }
        async fn put_if_match(
            &self,
            key: &str,
            data: &[u8],
            etag: &str,
        ) -> crate::Result<crate::object_store::ConditionalWrite> {
            self.inner.put_if_match(key, data, etag).await
        }
        async fn put_if_absent(
            &self,
            key: &str,
            data: &[u8],
        ) -> crate::Result<crate::object_store::ConditionalWrite> {
            self.inner.put_if_absent(key, data).await
        }
        fn supports_conditional_writes(&self) -> bool {
            true
        }
    }

    /// A writer whose *read* predates a concurrent commit still sees the old version
    /// number, so the version comparison alone would let it write. Only the `If-Match`
    /// precondition (old entity tag) can stop it — this is what makes the CAS atomic
    /// across processes rather than merely "check then write".
    #[tokio::test]
    async fn stale_etag_conflicts_even_when_the_version_numbers_agree() {
        let backend = InMemoryCosObjectStore::new().shared();
        let writer_b = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();

        writer_b
            .save(Some("u1"), "s1", "k", serde_json::json!({"writer": "seed"}))
            .await;

        // Writer A only ever sees the version-1 view it read before B committed.
        let stale_key = writer_b.version_key(Some("u1"), "s1", "k").unwrap();
        let stale = backend.get_etagged(&stale_key).await.unwrap().unwrap();
        assert_eq!(stale.data, b"1");
        let writer_a = CosAgentStateStore::builder()
            .object_store(Arc::new(StaleViewStore {
                inner: backend.clone(),
                stale_key,
                stale,
            }))
            .build()
            .unwrap();

        // B commits version 2.
        assert_eq!(
            writer_b
                .try_save_if_version(Some("u1"), "s1", "k", serde_json::json!({"writer": "b"}), 1)
                .await
                .unwrap(),
            2
        );

        // A also believes version 1, so the version check passes — the stale `If-Match`
        // is the only thing left, and it must reject the write.
        assert_eq!(
            writer_a
                .try_save_if_version(Some("u1"), "s1", "k", serde_json::json!({"writer": "a"}), 1)
                .await
                .unwrap(),
            UNVERSIONED
        );
        assert_eq!(
            writer_b.get(Some("u1"), "s1", "k").await,
            Some(serde_json::json!({"writer": "b"}))
        );
        assert_eq!(
            writer_b.get_versioned(Some("u1"), "s1", "k").await.version,
            2
        );
    }

    /// `UNVERSIONED` means "overwrite unconditionally", and the counter must still move
    /// forward so a later CAS cannot resurrect the old version.
    #[tokio::test]
    async fn unconditional_cas_overwrites_and_bumps_the_version() {
        let s = store();
        s.save(Some("u1"), "s1", "k", serde_json::json!({"v": 1}))
            .await;
        let overwritten = s
            .try_save_if_version(
                Some("u1"),
                "s1",
                "k",
                serde_json::json!({"v": 2}),
                UNVERSIONED,
            )
            .await
            .unwrap();
        assert_eq!(overwritten, 2);
        assert_eq!(
            s.get(Some("u1"), "s1", "k").await,
            Some(serde_json::json!({"v": 2}))
        );
    }

    /// Deleting a key drops its counter too, so a re-created key starts at version 0.
    #[tokio::test]
    async fn delete_key_resets_the_version_counter() {
        let s = store();
        s.save(Some("u1"), "s1", "k", serde_json::json!(1)).await;
        assert_eq!(s.get_versioned(Some("u1"), "s1", "k").await.version, 1);

        s.delete_key(Some("u1"), "s1", "k").await.unwrap();

        let after = s.try_get_versioned(Some("u1"), "s1", "k").await.unwrap();
        assert!(after.value.is_none());
        assert_eq!(after.version, 0);
        assert_eq!(
            s.save_if_version(Some("u1"), "s1", "k", serde_json::json!(2), 0)
                .await,
            1
        );
    }

    /// A corrupt payload or counter is reported as an error by the fallible surface, and
    /// the infallible `get_versioned` must report version `0` — never `UNVERSIONED`,
    /// which would let the next CAS overwrite blindly.
    #[tokio::test]
    async fn corrupt_objects_surface_as_errors() {
        let backend = InMemoryCosObjectStore::new().shared();
        let s = CosAgentStateStore::builder()
            .object_store(backend.clone())
            .build()
            .unwrap();

        let state_key = s.state_key(Some("u1"), "s1", "k").unwrap();
        backend.put_string(&state_key, "{not json").await.unwrap();
        let err = s
            .try_get_versioned(Some("u1"), "s1", "k")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "got: {err}");
        let fallback = s.get_versioned(Some("u1"), "s1", "k").await;
        assert!(fallback.value.is_none());
        assert_eq!(fallback.version, 0, "the fallback must not be UNVERSIONED");

        let version_key = s.version_key(Some("u1"), "s1", "k").unwrap();
        // Restore a valid payload so the counter is what fails next.
        backend.put_string(&state_key, "1").await.unwrap();
        backend
            .put_string(&version_key, "not-a-number")
            .await
            .unwrap();
        let err = s
            .try_get_versioned(Some("u1"), "s1", "k")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not an integer"), "got: {err}");
        let err = s
            .try_save_if_version(Some("u1"), "s1", "k", serde_json::json!(1), 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not an integer"), "got: {err}");
    }

    /// Backend failures reach the caller through the fallible surface instead of being
    /// folded into "nothing stored".
    #[tokio::test]
    async fn backend_failures_are_not_swallowed() {
        #[derive(Debug)]
        struct FailingStore;

        #[async_trait]
        impl crate::CosObjectStore for FailingStore {
            async fn get(&self, _key: &str) -> crate::Result<Option<Vec<u8>>> {
                Err(CosError::Backend("boom".to_string()))
            }
            async fn put(&self, _key: &str, _data: &[u8]) -> crate::Result<()> {
                Err(CosError::Backend("boom".to_string()))
            }
            async fn delete(&self, _key: &str) -> crate::Result<()> {
                Err(CosError::Backend("boom".to_string()))
            }
            async fn exists(&self, _key: &str) -> crate::Result<bool> {
                Err(CosError::Backend("boom".to_string()))
            }
            async fn list_prefix(&self, _prefix: &str) -> crate::Result<Vec<String>> {
                Err(CosError::Backend("boom".to_string()))
            }
        }

        let s = CosAgentStateStore::builder()
            .object_store(Arc::new(FailingStore))
            .build()
            .unwrap();
        // A backend without conditional-write support must not advertise versioning.
        assert!(!s.supports_versioning());

        let err = s
            .try_get_versioned(Some("u1"), "s1", "k")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "got: {err}");
        assert_eq!(s.get(Some("u1"), "s1", "k").await, None);
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
