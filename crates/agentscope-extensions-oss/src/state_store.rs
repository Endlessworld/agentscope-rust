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
//!
//! # Error semantics
//!
//! Java `OssAgentStateStore` throws `RuntimeException` whenever OSS fails. The core
//! [`AgentStateStore`] trait has no error channel (its `save`/`get`/`delete` return plain
//! values), so the trait implementation can only log. Every operation therefore also has a
//! **strict** inherent twin (`save_strict`, `save_list_strict`, `get_strict`,
//! `get_list_strict`, `exists_strict`, `delete_session_strict`, `list_session_ids_strict`)
//! that returns [`OssResult`], which is how callers distinguish a backend failure from
//! "no data" — the behaviour Java callers get by catching `RuntimeException`.

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

    // ---- strict (error-propagating) variants ----
    //
    // Java `OssAgentStateStore` throws `RuntimeException` on every backend failure, but the
    // core `AgentStateStore` trait has no error channel. These inherent methods are where
    // the failure stays observable; the trait methods delegate to them and merely log.
    // Callers that must tell "backend down" from "no data" (the Java semantics) use these.

    /// Saves a single state value, propagating validation/encoding/backend failures.
    pub async fn save_strict(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> OssResult<()> {
        let object_key = self.state_object_key(user_id, session_id, key)?;
        let json = serde_json::to_string(&value)?;
        self.put_string(&object_key, &json)
    }

    /// Saves a list, propagating listing/encoding/backend failures.
    ///
    /// The incremental-append decision is delegated to
    /// [`agentscope_core::state::list_hash`] exactly as before; only the error handling is
    /// strict here.
    pub async fn save_list_strict(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) -> OssResult<()> {
        let list_key = self.list_object_key(user_id, session_id, key)?;
        let hash_key = self.hash_object_key(user_id, session_id, key)?;

        let current_hash = compute_hash(values);
        let stored_hash = self.get_string(&hash_key)?;

        // Mirrors Java `existingCount`: the length of the list actually stored, **independent**
        // of whether the hash object exists. Decoupling them is what makes
        // `needs_full_rewrite` Case 2 (missing hash + existing data) reachable, and it also
        // catches "emptied list without hash" (otherwise a stale list object would survive).
        let existing_count = match self.get_string(&list_key)? {
            Some(existing_json) => {
                serde_json::from_str::<Vec<serde_json::Value>>(&existing_json)?.len()
            }
            None => 0,
        };

        let rewrite = needs_full_rewrite(values, stored_hash.as_deref(), existing_count)
            || values.len() != existing_count;

        if rewrite {
            let json = serde_json::to_string(values)?;
            self.put_string(&list_key, &json)?;
        }
        self.put_string(&hash_key, &current_hash)
    }

    /// Loads a single state value, distinguishing "absent" (`Ok(None)`) from failure.
    pub async fn get_strict(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> OssResult<Option<serde_json::Value>> {
        let object_key = self.state_object_key(user_id, session_id, key)?;
        let Some(json) = self.get_string(&object_key)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_str(&json)?))
    }

    /// Loads a list value: absent → `Ok(vec![])`, backend/decode failure → `Err`.
    pub async fn get_list_strict(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> OssResult<Vec<serde_json::Value>> {
        let list_key = self.list_object_key(user_id, session_id, key)?;
        let Some(json) = self.get_string(&list_key)? else {
            return Ok(Vec::new());
        };
        Ok(serde_json::from_str(&json)?)
    }

    /// Whether the session holds any object, propagating backend failures.
    pub async fn exists_strict(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> OssResult<bool> {
        let prefix = self.session_prefix(user_id, session_id)?;
        Ok(!self.client.list_page(&prefix, 1, None)?.objects.is_empty())
    }

    /// Deletes every object of a session, propagating the first failure.
    pub async fn delete_session_strict(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> OssResult<()> {
        let prefix = self.session_prefix(user_id, session_id)?;
        for key in self.client.list_all(&prefix)? {
            self.client.delete(&key)?;
        }
        Ok(())
    }

    /// Lists the session ids of a user, propagating backend failures.
    pub async fn list_session_ids_strict(&self, user_id: Option<&str>) -> OssResult<Vec<String>> {
        let user_prefix = format!("{}{}/", self.key_prefix, normalize_user(user_id));
        let mut session_ids = std::collections::BTreeSet::new();
        for key in self.client.list_all(&user_prefix)? {
            let remainder = key.strip_prefix(&user_prefix).unwrap_or("");
            if let Some(slash) = remainder.find('/') {
                if slash > 0 {
                    session_ids.insert(remainder[..slash].to_string());
                }
            }
        }
        Ok(session_ids.into_iter().collect())
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

    fn state_object_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> OssResult<String> {
        Ok(format!(
            "{}{}{}",
            self.session_prefix(user_id, session_id)?,
            key,
            JSON_SUFFIX
        ))
    }

    fn list_object_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> OssResult<String> {
        Ok(format!(
            "{}{}{}",
            self.session_prefix(user_id, session_id)?,
            key,
            LIST_SUFFIX
        ))
    }

    fn hash_object_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> OssResult<String> {
        Ok(format!(
            "{}{}{}",
            self.session_prefix(user_id, session_id)?,
            key,
            HASH_SUFFIX
        ))
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

/// Logs a failure from a non-error-returning trait method.
///
/// Blank identifiers are the caller's mistake (Java also rejects them up front), so they
/// stay at `warn`; everything else is a backend/decode failure and logs at `error`.
fn log_state_failure(op: &str, key: &str, error: &OssError) {
    if matches!(error, OssError::BlankIdentifier(_)) {
        tracing::warn!(error = %error, key, op, "OssAgentStateStore rejected");
    } else {
        tracing::error!(error = %error, key, op, "OssAgentStateStore failed");
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
        if let Err(e) = self.save_strict(user_id, session_id, key, value).await {
            log_state_failure("save", key, &e);
        }
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) {
        if let Err(e) = self
            .save_list_strict(user_id, session_id, key, values)
            .await
        {
            log_state_failure("save_list", key, &e);
        }
    }

    async fn get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Option<serde_json::Value> {
        match self.get_strict(user_id, session_id, key).await {
            Ok(value) => value,
            Err(e) => {
                log_state_failure("get", key, &e);
                None
            }
        }
    }

    async fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Vec<serde_json::Value> {
        match self.get_list_strict(user_id, session_id, key).await {
            Ok(values) => values,
            Err(e) => {
                log_state_failure("get_list", key, &e);
                Vec::new()
            }
        }
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        match self.exists_strict(user_id, session_id).await {
            Ok(exists) => exists,
            Err(e) => {
                tracing::error!(error = %e, "failed to check session existence");
                false
            }
        }
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if let Err(e) = self.delete_session_strict(user_id, session_id).await {
            tracing::error!(error = %e, "failed to delete session");
        }
    }

    async fn delete_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> agentscope_core::error::Result<()> {
        // Java `OssAgentStateStore.delete(userId, sessionId, key)` removes the value plus its
        // list payload and incremental hash. The trait has an error channel here (unlike the
        // other methods), so the failure must stay observable instead of being logged away.
        OssAgentStateStore::delete_key(self, user_id, session_id, key)
            .await
            .map_err(|e| agentscope_core::error::Error::Other(format!("OSS per-key delete failed: {e}")))
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        match self.list_session_ids_strict(user_id).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(error = %e, "failed to list sessions");
                Vec::new()
            }
        }
    }

    // Versioning: `OssClient` has no conditional-put seam (`If-Match` / `If-None-Match`), so
    // a read-check-write here would be a *fake* CAS, and Java `OssAgentStateStore` does not
    // override `supportsVersioning` / `getVersioned` / `saveIfVersion` either. The core
    // defaults (`supports_versioning() == false`, `save_if_version` degrading to `save` and
    // returning `UNVERSIONED`) are therefore the intended semantics — see tracker §8.4.
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
        Ok(OssAgentStateStore {
            client,
            bucket_name,
            key_prefix,
        })
    }
}
