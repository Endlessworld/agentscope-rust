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
//! # Optimistic concurrency
//!
//! [`AgentStateStore::supports_versioning`] is `true` when the client can do ETag-guarded
//! conditional writes ([`OssClient::put_conditional`]). A state key's version is then the
//! token derived from the object's ETag ([`etag_version_token`]), and
//! [`AgentStateStore::save_if_version`] is a single backend-evaluated `If-Match` /
//! `If-None-Match` — two writers racing on the same expected version cannot both commit.
//! The list layout (`.list.json` / `.list.hash`) is outside the versioned API.
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

use agentscope_core::state::store::{AgentStateStore, VersionedState, UNVERSIONED};
use agentscope_core::state::{compute_hash, needs_full_rewrite};
use async_trait::async_trait;

use crate::client::{etag_version_token, normalize_prefix, OssClient, PutCondition, PutOutcome};
use crate::error::{OssError, OssResult};

/// Default key prefix for agent state objects.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope/state/";
const ANON_USER: &str = "__anon__";
const JSON_SUFFIX: &str = ".json";
const LIST_SUFFIX: &str = ".list.json";
const HASH_SUFFIX: &str = ".list.hash";

/// Keys per bulk delete request (S3/OSS `DeleteObjects` caps a request at 1000 keys).
const DELETE_BATCH_SIZE: usize = 1000;

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
    pub async fn exists_strict(&self, user_id: Option<&str>, session_id: &str) -> OssResult<bool> {
        let prefix = self.session_prefix(user_id, session_id)?;
        Ok(!self.client.list_page(&prefix, 1, None)?.objects.is_empty())
    }

    /// Deletes every object of a session, propagating the first failure.
    ///
    /// Keys are removed in [`DELETE_BATCH_SIZE`] batches through
    /// [`OssClient::delete_many`], so a session with `N` objects costs `ceil(N / 1000)`
    /// requests instead of `N` (mirrors Java `deleteKeys`).
    pub async fn delete_session_strict(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> OssResult<()> {
        let prefix = self.session_prefix(user_id, session_id)?;
        let keys = self.client.list_all(&prefix)?;
        for batch in keys.chunks(DELETE_BATCH_SIZE) {
            self.client.delete_many(batch)?;
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
            .map_err(|e| {
                agentscope_core::error::Error::Other(format!("OSS per-key delete failed: {e}"))
            })
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

    // Versioning: the state object itself carries the ETag that anchors the CAS, so a
    // client with conditional writes gives real optimistic concurrency. Clients without
    // them keep the core defaults (no versioning, `save_if_version` degrading to `save`)
    // instead of pretending a read-check-write is atomic.

    fn supports_versioning(&self) -> bool {
        self.client.supports_conditional_writes()
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
                tracing::error!(error = %e, key, "failed to read versioned state");
                VersionedState {
                    value: None,
                    version: UNVERSIONED,
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
        let object_key = self
            .state_object_key(user_id, session_id, key)
            .map_err(|e| core_error("read versioned state", e))?;
        if !self.client.supports_conditional_writes() {
            let value = self
                .get_strict(user_id, session_id, key)
                .await
                .map_err(|e| core_error("read versioned state", e))?;
            return Ok(VersionedState {
                value,
                version: UNVERSIONED,
            });
        }
        let Some(object) = self
            .client
            .get_with_etag(&object_key)
            .map_err(|e| core_error("read versioned state", e))?
        else {
            // Absent key on a versioning backend reports version 0 (core contract).
            return Ok(VersionedState {
                value: None,
                version: 0,
            });
        };
        let text = String::from_utf8_lossy(&object.content);
        let value = serde_json::from_str::<serde_json::Value>(&text).map_err(|e| {
            agentscope_core::error::Error::Other(format!(
                "OSS state object '{object_key}' is not valid JSON: {e}"
            ))
        })?;
        Ok(VersionedState {
            value: Some(value),
            version: etag_version_token(&object.etag),
        })
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
                // Reporting `UNVERSIONED` after a failed write keeps the contract honest:
                // callers treat it as "nothing committed", never as a new version.
                tracing::error!(error = %e, key, "failed to save versioned state");
                UNVERSIONED
            }
        }
    }

    async fn try_save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> agentscope_core::error::Result<i64> {
        if !self.client.supports_conditional_writes() {
            self.save_strict(user_id, session_id, key, value)
                .await
                .map_err(|e| core_error("save versioned state", e))?;
            return Ok(UNVERSIONED);
        }
        let object_key = self
            .state_object_key(user_id, session_id, key)
            .map_err(|e| core_error("save versioned state", e))?;
        let json = serde_json::to_string(&value)
            .map_err(|e| core_error("encode versioned state", OssError::Json(e)))?;

        if expected_version == UNVERSIONED {
            return match self
                .client
                .put_conditional(&object_key, json.as_bytes(), &PutCondition::Any)
                .map_err(|e| core_error("save versioned state", e))?
            {
                PutOutcome::Written { etag } => Ok(etag_version_token(&etag)),
                PutOutcome::PreconditionFailed => Err(agentscope_core::error::Error::Other(
                    "unconditional OSS state write reported a precondition failure".to_string(),
                )),
            };
        }

        let condition = if expected_version == 0 {
            PutCondition::IfNoneMatchAny
        } else {
            let Some(current) = self
                .client
                .get_with_etag(&object_key)
                .map_err(|e| core_error("save versioned state", e))?
            else {
                // Nothing stored ⇒ version 0 ≠ expected_version.
                return Ok(UNVERSIONED);
            };
            if etag_version_token(&current.etag) != expected_version {
                return Ok(UNVERSIONED);
            }
            PutCondition::IfMatch(current.etag)
        };

        match self
            .client
            .put_conditional(&object_key, json.as_bytes(), &condition)
            .map_err(|e| core_error("save versioned state", e))?
        {
            PutOutcome::Written { etag } => Ok(etag_version_token(&etag)),
            PutOutcome::PreconditionFailed => Ok(UNVERSIONED),
        }
    }
}

/// Wraps an OSS failure for the core trait's error channel.
fn core_error(op: &str, error: OssError) -> agentscope_core::error::Error {
    agentscope_core::error::Error::Other(format!("OSS {op} failed: {error}"))
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
