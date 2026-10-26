//! Alibaba Cloud OSS backed [`AgentStateStore`], mirroring
//! `io.agentscope.extensions.oss.OssAgentStateStore`.
//!
//! Object key layout (all under `{keyPrefix}{userId}/{sessionId}/`):
//!
//! ```text
//! {stateKey}.json        — single State value (JSON)
//! {stateKey}.list.json   — List<State> as a JSON array
//! {stateKey}.list.hash   — sampled hash for incremental append detection
//! ```
//!
//! Per-key deletion (`delete_key`) removes all three objects for one key while
//! keeping the rest of the session intact; session-level `delete` (the core
//! trait method) removes every object under the session prefix.

use std::sync::Arc;

use agentscope_core::state::store::AgentStateStore;
use agentscope_core::state::{compute_hash, needs_full_rewrite};
use async_trait::async_trait;

use crate::client::{normalize_prefix, OssClient};
use crate::error::{OssError, OssResult};

/// Default key prefix for agent state objects.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope/state/";
const ANON_USER: &str = "__anon__";
const JSON_SUFFIX: &str = ".json";
const LIST_SUFFIX: &str = ".list.json";
const HASH_SUFFIX: &str = ".list.hash";

/// OSS-backed persistent store for agent state, keyed by `(userId, sessionId)`.
#[derive(Debug, Clone)]
pub struct OssAgentStateStore {
    client: Arc<dyn OssClient>,
    bucket_name: String,
    key_prefix: String,
}

impl OssAgentStateStore {
    /// Creates a builder.
    pub fn builder() -> OssAgentStateStoreBuilder {
        OssAgentStateStoreBuilder::default()
    }

    /// Returns the configured bucket name.
    pub fn bucket_name(&self) -> &str {
        &self.bucket_name
    }

    /// Returns the normalized key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// Shuts down the underlying OSS client (mirrors Java `close()`).
    pub fn close(&self) {
        self.client.shutdown();
    }

    /// Deletes a single state key of a session, removing the value object plus
    /// its list/hash sidecars (`.json`, `.list.json`, `.list.hash`).
    ///
    /// Mirrors Java `OssAgentStateStore.delete(userId, sessionId, key)`. The
    /// core [`AgentStateStore`] trait only defines *session-level* deletion, so
    /// this is deliberately a crate-local inherent method — the same approach as
    /// `MongodbAgentStateStore::delete_key` in `agentscope-extensions-mongodb`;
    /// core/harness traits are left untouched.
    ///
    /// Deletion is idempotent: keys that do not exist (or whose sidecars are
    /// missing) are ignored, so repeated calls succeed. The first backend error
    /// is returned instead of being swallowed (Java throws `RuntimeException`).
    pub async fn delete_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> OssResult<()> {
        let state_key = self.state_object_key(user_id, session_id, key)?;
        let list_key = self.list_object_key(user_id, session_id, key)?;
        let hash_key = self.hash_object_key(user_id, session_id, key)?;
        self.client.delete(&state_key)?;
        self.client.delete(&list_key)?;
        self.client.delete(&hash_key)?;
        Ok(())
    }

    // ---- internal helpers ----

    fn require_session_id(session_id: &str) -> OssResult<()> {
        if session_id.trim().is_empty() {
            return Err(OssError::BlankIdentifier("sessionId"));
        }
        Ok(())
    }

    fn session_prefix(&self, user_id: Option<&str>, session_id: &str) -> OssResult<String> {
        Self::require_session_id(session_id)?;
        Ok(format!(
            "{}{}/{}/",
            self.key_prefix,
            normalize_user(user_id),
            session_id
        ))
    }

    fn state_object_key(&self, user_id: Option<&str>, session_id: &str, key: &str) -> OssResult<String> {
        Ok(format!("{}{}{}", self.session_prefix(user_id, session_id)?, key, JSON_SUFFIX))
    }

    fn list_object_key(&self, user_id: Option<&str>, session_id: &str, key: &str) -> OssResult<String> {
        Ok(format!("{}{}{}", self.session_prefix(user_id, session_id)?, key, LIST_SUFFIX))
    }

    fn hash_object_key(&self, user_id: Option<&str>, session_id: &str, key: &str) -> OssResult<String> {
        Ok(format!("{}{}{}", self.session_prefix(user_id, session_id)?, key, HASH_SUFFIX))
    }

    fn put_string(&self, object_key: &str, content: &str) -> OssResult<()> {
        self.client.put(object_key, content.as_bytes())
    }

    fn get_string(&self, object_key: &str) -> OssResult<Option<String>> {
        match self.client.get(object_key)? {
            Some(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            None => Ok(None),
        }
    }
}

fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => ANON_USER.to_string(),
    }
}

#[async_trait]
impl AgentStateStore for OssAgentStateStore {
    async fn save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) {
        let object_key = match self.state_object_key(user_id, session_id, key) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "OssAgentStateStore::save rejected");
                return;
            }
        };
        let json = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
        if let Err(e) = self.put_string(&object_key, &json) {
            tracing::error!(error = %e, key = %object_key, "failed to save state");
        }
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) {
        let (list_key, hash_key) = match (
            self.list_object_key(user_id, session_id, key),
            self.hash_object_key(user_id, session_id, key),
        ) {
            (Ok(l), Ok(h)) => (l, h),
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(error = %e, "OssAgentStateStore::save_list rejected");
                return;
            }
        };

        let result: OssResult<()> = (|| {
            let current_hash = compute_hash(values);
            let stored_hash = self.get_string(&hash_key)?;

            let existing_count = if stored_hash.is_some() {
                match self.get_string(&list_key)? {
                    Some(existing_json) => serde_json::from_str::<Vec<serde_json::Value>>(&existing_json)
                        .map(|v| v.len())
                        .unwrap_or(0),
                    None => 0,
                }
            } else {
                0
            };

            let rewrite =
                needs_full_rewrite(values, stored_hash.as_deref(), existing_count)
                    || values.len() != existing_count;

            if rewrite {
                let json = serde_json::to_string(values)?;
                self.put_string(&list_key, &json)?;
            }
            self.put_string(&hash_key, &current_hash)?;
            Ok(())
        })();

        if let Err(e) = result {
            tracing::error!(error = %e, key, "failed to save list");
        }
    }

    async fn get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Option<serde_json::Value> {
        let object_key = self.state_object_key(user_id, session_id, key).ok()?;
        let json = self.get_string(&object_key).ok()??;
        serde_json::from_str(&json).ok()
    }

    async fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Vec<serde_json::Value> {
        let list_key = match self.list_object_key(user_id, session_id, key) {
            Ok(k) => k,
            Err(_) => return Vec::new(),
        };
        let Some(json) = self.get_string(&list_key).ok().flatten() else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<serde_json::Value>>(&json).unwrap_or_default()
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        let prefix = match self.session_prefix(user_id, session_id) {
            Ok(p) => p,
            Err(_) => return false,
        };
        match self.client.list_page(&prefix, 1, None) {
            Ok(page) => !page.objects.is_empty(),
            Err(e) => {
                tracing::error!(error = %e, "failed to check session existence");
                false
            }
        }
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        let prefix = match self.session_prefix(user_id, session_id) {
            Ok(p) => p,
            Err(_) => return,
        };
        match self.client.list_all(&prefix) {
            Ok(keys) => {
                for k in keys {
                    let _ = self.client.delete(&k);
                }
            }
            Err(e) => tracing::error!(error = %e, "failed to delete session"),
        }
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let user_prefix = format!("{}{}/", self.key_prefix, normalize_user(user_id));
        let mut session_ids = std::collections::BTreeSet::new();
        match self.client.list_all(&user_prefix) {
            Ok(keys) => {
                for key in keys {
                    let remainder = &key[user_prefix.len()..];
                    if let Some(slash) = remainder.find('/') {
                        if slash > 0 {
                            session_ids.insert(remainder[..slash].to_string());
                        }
                    }
                }
            }
            Err(e) => tracing::error!(error = %e, "failed to list sessions"),
        }
        session_ids.into_iter().collect()
    }
}

/// Builder for [`OssAgentStateStore`].
#[derive(Default)]
pub struct OssAgentStateStoreBuilder {
    client: Option<Arc<dyn OssClient>>,
    bucket_name: Option<String>,
    key_prefix: Option<String>,
}

impl OssAgentStateStoreBuilder {
    /// Sets the OSS client (required).
    pub fn client(mut self, client: Arc<dyn OssClient>) -> Self {
        self.client = Some(client);
        self
    }

    /// Sets the target bucket name (required, must not be blank).
    pub fn bucket_name(mut self, bucket: impl Into<String>) -> Self {
        self.bucket_name = Some(bucket.into());
        self
    }

    /// Sets the object key prefix (defaults to [`DEFAULT_KEY_PREFIX`]).
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Some(prefix.into());
        self
    }

    /// Builds the store, validating required fields.
    pub fn build(self) -> OssResult<OssAgentStateStore> {
        let client = self.client.ok_or(OssError::MissingClient)?;
        let bucket_name = self.bucket_name.ok_or(OssError::BlankBucket)?;
        if bucket_name.trim().is_empty() {
            return Err(OssError::BlankBucket);
        }
        let key_prefix = normalize_prefix(self.key_prefix.as_deref(), DEFAULT_KEY_PREFIX);
        Ok(OssAgentStateStore { client, bucket_name, key_prefix })
    }
}
