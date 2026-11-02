//! MongoDB-backed [`AgentStateStore`], mirroring
//! `io.agentscope.extensions.mongodb.state.MongoAgentStateStore`.
//!
//! Each session is stored as a single document with a compound `_id` of
//! `{user, session}` to prevent cross-tenant key collisions. State keys are stored
//! inside a `states` sub-document (via dot notation, e.g. `states.foo`), keeping them
//! isolated from reserved top-level fields. Per-key versions live in a `versions`
//! sub-document and list-content hashes in a `hashes` sub-document.
//!
//! The harness [`AgentStateStore`] trait does not surface versions or CAS, so this
//! implementation additionally exposes [`MongodbAgentStateStore::get_version`] and
//! [`MongodbAgentStateStore::save_if_version`] with the same semantics as the Java
//! store; the trait methods delegate to them and discard the version.

use std::collections::BTreeSet;
use std::sync::Arc;

use agentscope_core::state::{compute_hash, needs_full_rewrite, AgentStateStore};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::client::{
    FindFilter, FindOneAndUpdateOptions, InMemoryMongoClient, MongoClient, MongoError, MongoResult,
    Update, UpdateOptions,
};
use crate::constants::{DEFAULT_DATABASE, SESSIONS_COLLECTION};
use crate::index_utils::{create_index_with_migration, IndexSpec};
use crate::key_escaper;

const ANON_USER: &str = "__anon__";
const FIELD_USER_ID: &str = "user_id";
const FIELD_SESSION_ID: &str = "session_id";
const FIELD_UPDATED_AT: &str = "_updated_at";
const FIELD_STATES: &str = "states";
const FIELD_VERSIONS: &str = "versions";
const FIELD_HASHES: &str = "hashes";

/// Sentinel returned by [`MongodbAgentStateStore::save_if_version`] when the
/// conditional write lost the CAS race.
///
/// Re-exported from `agentscope_core::state` so the versioning protocol has a single
/// source of truth shared with the other storage backends (value `-1`, which never
/// collides with a real stored version: those start at 1).
pub use agentscope_core::state::UNVERSIONED;

/// Async callback invoked before a session slot is deleted, receiving the owned
/// `(user_id, session_id)`.
///
/// It returns a boxed future so the callback can await the backend directly; a
/// synchronous callback would have to bridge with `futures::executor::block_on`, which
/// stalls (and can deadlock) the runtime.
pub type SessionDeleteCallback =
    Arc<dyn Fn(Option<String>, String) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Validates a state key for use as a field name inside the `states` sub-document.
///
/// The key must match `^[a-zA-Z_][a-zA-Z0-9_]*$` — no dots, dollar signs, or Unicode
/// — to prevent dot-traversal and operator injection.
pub fn validate_state_key(key: &str) -> Result<(), MongoError> {
    if key.trim().is_empty() {
        return Err(MongoError::InvalidArgument("key must not be blank".into()));
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(invalid_key(key));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(invalid_key(key));
    }
    Ok(())
}

fn invalid_key(key: &str) -> MongoError {
    // Single line, like the Java message (MONGO-10).
    MongoError::InvalidArgument(format!(
        "key must match pattern ^[a-zA-Z_][a-zA-Z0-9_]*$ but was: {key} \
         (MongoDB field names cannot contain '.' or '$')"
    ))
}

fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => ANON_USER.to_string(),
    }
}

/// Builds the compound `_id` object `{user, session}` for a session slot.
fn slot_id(user_id: Option<&str>, session_id: &str) -> Value {
    json!({ "user": normalize_user(user_id), "session": session_id })
}

pub(crate) fn slot_filter(user_id: Option<&str>, session_id: &str) -> FindFilter {
    FindFilter::eq("_id", slot_id(user_id, session_id))
}

fn require_session_id(session_id: &str) -> Result<(), MongoError> {
    if session_id.trim().is_empty() {
        return Err(MongoError::InvalidArgument(
            "sessionId must not be blank".into(),
        ));
    }
    Ok(())
}

/// MongoDB-backed [`AgentStateStore`].
#[derive(Clone)]
pub struct MongodbAgentStateStore {
    client: Arc<dyn MongoClient>,
    collection: String,
    /// Session TTL in days as configured through the builder; `None` disables the TTL
    /// index. The builder rejects non-positive values (MONGO-07).
    ttl_days: Option<i64>,
    on_delete: Option<SessionDeleteCallback>,
}

impl std::fmt::Debug for MongodbAgentStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongodbAgentStateStore")
            .field("collection", &self.collection)
            .field("ttl_days", &self.ttl_days)
            .field("has_on_delete", &self.on_delete.is_some())
            .finish()
    }
}

impl MongodbAgentStateStore {
    /// Creates a store over the given client and collection with default options.
    pub fn new(client: Arc<dyn MongoClient>, collection: impl Into<String>) -> Self {
        Self {
            client,
            collection: collection.into(),
            ttl_days: None,
            on_delete: None,
        }
    }

    /// The session TTL in days configured on the builder, if any.
    pub fn ttl_days(&self) -> Option<i64> {
        self.ttl_days
    }

    /// The collection this store reads and writes (database-scoped when the store was
    /// built through [`crate::MongodbDistributedStore`]).
    pub fn collection(&self) -> &str {
        &self.collection
    }

    /// Creates a store with the default collection name
    /// ([`SESSIONS_COLLECTION`]).
    pub fn with_defaults(client: Arc<dyn MongoClient>) -> Self {
        Self::new(client, SESSIONS_COLLECTION)
    }

    /// Returns a builder for fine-grained configuration.
    pub fn builder() -> MongodbAgentStateStoreBuilder {
        MongodbAgentStateStoreBuilder::default()
    }

    /// Ensures the required indexes exist. Must be called before use when the store was
    /// built with [`MongodbAgentStateStoreBuilder::build`] (Java's constructor does it
    /// eagerly; [`MongodbAgentStateStoreBuilder::build_with_indexes`] and
    /// [`crate::MongodbDistributedStore::try_new`] are the eager equivalents).
    ///
    /// The session TTL index is derived from the `ttl_days` configured on the
    /// builder — mirroring Java `MongoAgentStateStore` calling
    /// `ensureIndexes(builder.ttlDays)` — so callers never have to pass the TTL
    /// again after building the store.
    pub async fn ensure_indexes(&self) -> MongoResult<()> {
        create_index_with_migration(
            &self.client,
            &self.collection,
            &IndexSpec::compound_ascending(&[FIELD_USER_ID, FIELD_SESSION_ID]),
        )
        .await?;
        if let Some(days) = self.ttl_days {
            create_index_with_migration(
                &self.client,
                &self.collection,
                &IndexSpec::ascending(&[FIELD_UPDATED_AT])
                    .sparse()
                    .expire_after_seconds(days * 24 * 3600),
            )
            .await?;
        }
        Ok(())
    }

    /// Reads the stored version for a single key (0 when absent), mirroring Java
    /// `getVersioned`.
    pub async fn get_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<(Option<Value>, i64), MongoError> {
        validate_state_key(key)?;
        let filter = slot_filter(user_id, session_id);
        // Projected read (MONGO-05): only the requested value and version are transferred,
        // like Java `Projections.include(stateField, versionField)`.
        let state_field = format!("{FIELD_STATES}.{key}");
        let version_field = format!("{FIELD_VERSIONS}.{key}");
        let doc = self
            .client
            .find_one_projected(
                &self.collection,
                &filter,
                &[state_field.as_str(), version_field.as_str()],
            )
            .await?;
        let Some(doc) = doc else {
            return Ok((None, 0));
        };
        let Some(states) = doc.get(FIELD_STATES).and_then(|v| v.as_object()) else {
            return Ok((None, 0));
        };
        if !states.contains_key(key) {
            return Ok((None, 0));
        }
        let version = doc
            .get(FIELD_VERSIONS)
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let value = states.get(key).map(key_escaper::unescape);
        Ok((value, version))
    }

    /// Compare-and-swap write of a single value.
    ///
    /// Returns the new version on success, [`UNVERSIONED`] when the CAS failed, and
    /// (for `expected_version == UNVERSIONED`) the new version of an unconditional
    /// last-writer-wins write.
    pub async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &Value,
        expected_version: i64,
    ) -> Result<i64, MongoError> {
        validate_state_key(key)?;
        require_session_id(session_id)?;
        let filter = slot_filter(user_id, session_id);
        let state_field = format!("{FIELD_STATES}.{key}");
        let version_field = format!("{FIELD_VERSIONS}.{key}");
        let value_doc = key_escaper::escape(value);
        let now = chrono::Utc::now().timestamp_millis();

        if expected_version == UNVERSIONED {
            // Unconditional overwrite (last-writer-wins), retrying once on the
            // first-insert duplicate-key race.
            let set_fields = Update::new()
                .set(state_field, value_doc)
                .inc(version_field, 1)
                .set(FIELD_UPDATED_AT, json!(now));
            let set_on_insert = Update::new()
                .set_on_insert(FIELD_USER_ID, json!(normalize_user(user_id)))
                .set_on_insert(FIELD_SESSION_ID, json!(session_id));
            let mut attempt = 0;
            loop {
                match self
                    .client
                    .find_one_and_update(
                        &self.collection,
                        &filter,
                        &set_fields.clone().combine(set_on_insert.clone()),
                        &FindOneAndUpdateOptions {
                            upsert: true,
                            return_after: true,
                        },
                    )
                    .await
                {
                    Ok(Some(doc)) => {
                        let v = doc
                            .get(FIELD_VERSIONS)
                            .and_then(|m| m.as_object())
                            .and_then(|m| m.get(key))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        return Ok(v);
                    }
                    Ok(None) => return Ok(UNVERSIONED),
                    Err(MongoError::DuplicateKey) if attempt == 0 => {
                        attempt += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        if expected_version == 0 {
            // Create only if the version field does not yet exist.
            let filter = filter.not_exists(version_field.clone());
            let update = Update::new()
                .set(state_field, value_doc)
                .set(version_field, json!(1))
                .set(FIELD_UPDATED_AT, json!(now))
                .set_on_insert(FIELD_USER_ID, json!(normalize_user(user_id)))
                .set_on_insert(FIELD_SESSION_ID, json!(session_id));
            match self
                .client
                .find_one_and_update(
                    &self.collection,
                    &filter,
                    &update,
                    &FindOneAndUpdateOptions {
                        upsert: true,
                        return_after: true,
                    },
                )
                .await
            {
                Ok(Some(doc)) => {
                    let v = doc
                        .get(FIELD_VERSIONS)
                        .and_then(|m| m.as_object())
                        .and_then(|m| m.get(key))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    Ok(if v == 1 { 1 } else { UNVERSIONED })
                }
                Ok(None) => Ok(UNVERSIONED),
                Err(MongoError::DuplicateKey) => Ok(UNVERSIONED),
                Err(e) => Err(e),
            }
        } else {
            let filter = filter.and(FindFilter::eq(
                version_field.clone(),
                json!(expected_version),
            ));
            let update = Update::new()
                .set(state_field, value_doc)
                .inc(version_field, 1)
                .set(FIELD_UPDATED_AT, json!(now));
            match self
                .client
                .find_one_and_update(
                    &self.collection,
                    &filter,
                    &update,
                    &FindOneAndUpdateOptions {
                        upsert: false,
                        return_after: true,
                    },
                )
                .await
            {
                Ok(Some(doc)) => {
                    let v = doc
                        .get(FIELD_VERSIONS)
                        .and_then(|m| m.as_object())
                        .and_then(|m| m.get(key))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(UNVERSIONED);
                    Ok(v)
                }
                Ok(None) => Ok(UNVERSIONED),
                Err(e) => Err(e),
            }
        }
    }

    async fn save_value_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), MongoError> {
        validate_state_key(key)?;
        require_session_id(session_id)?;
        let filter = slot_filter(user_id, session_id);
        let state_field = format!("{FIELD_STATES}.{key}");
        let version_field = format!("{FIELD_VERSIONS}.{key}");
        let now = chrono::Utc::now().timestamp_millis();
        let set_fields = Update::new()
            .set(state_field, key_escaper::escape(value))
            .inc(version_field, 1)
            .set(FIELD_UPDATED_AT, json!(now));
        let set_on_insert = Update::new()
            .set_on_insert(FIELD_USER_ID, json!(normalize_user(user_id)))
            .set_on_insert(FIELD_SESSION_ID, json!(session_id));
        self.client
            .update_one(
                &self.collection,
                &filter,
                &set_fields.combine(set_on_insert),
                &UpdateOptions { upsert: true },
            )
            .await
    }

    async fn save_list_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[Value],
    ) -> Result<(), MongoError> {
        validate_state_key(key)?;
        require_session_id(session_id)?;
        let filter = slot_filter(user_id, session_id);
        let state_field = format!("{FIELD_STATES}.{key}");
        let hash_field = format!("{FIELD_HASHES}.{key}");
        let now = chrono::Utc::now().timestamp_millis();

        // Obtain the existing count and stored hash server-side, in one round trip
        // (MONGO-05): mirrors Java's `$cond/$isArray/$size` + `storedHash` aggregation, so
        // a long history never transfers the array just to count it.
        let (existing_count, stored_hash) = self
            .client
            .first_field_size_and_string(&self.collection, &filter, &state_field, &hash_field)
            .await?;
        let existing_count = existing_count.unwrap_or(0);
        let current_hash = compute_hash(values);

        let encoded: Vec<Value> = values.iter().map(key_escaper::escape).collect();
        let set_on_insert = Update::new()
            .set_on_insert(FIELD_USER_ID, json!(normalize_user(user_id)))
            .set_on_insert(FIELD_SESSION_ID, json!(session_id));

        if needs_full_rewrite(values, stored_hash.as_deref(), existing_count) {
            let set_fields = Update::new()
                .set(state_field, Value::Array(encoded))
                .set(hash_field, json!(current_hash))
                .set(FIELD_UPDATED_AT, json!(now));
            self.client
                .update_one(
                    &self.collection,
                    &filter,
                    &set_fields.combine(set_on_insert),
                    &UpdateOptions { upsert: true },
                )
                .await
        } else if values.len() > existing_count {
            let new_items: Vec<Value> = encoded[existing_count..].to_vec();
            let update = Update::new()
                .push_each(state_field, new_items)
                .set(hash_field, json!(current_hash))
                .set(FIELD_UPDATED_AT, json!(now))
                .combine(set_on_insert);
            self.client
                .update_one(
                    &self.collection,
                    &filter,
                    &update,
                    &UpdateOptions { upsert: true },
                )
                .await
        } else {
            // Hash unchanged but size decreased — force full rewrite.
            let set_fields = Update::new()
                .set(state_field, Value::Array(encoded))
                .set(hash_field, json!(current_hash))
                .set(FIELD_UPDATED_AT, json!(now));
            self.client
                .update_one(
                    &self.collection,
                    &filter,
                    &set_fields.combine(set_on_insert),
                    &UpdateOptions { upsert: true },
                )
                .await
        }
    }

    async fn get_field(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        list: bool,
    ) -> Option<Value> {
        let filter = slot_filter(user_id, session_id);
        // Projected read (MONGO-05): transferring the whole session document to read one
        // key is exactly the read amplification Java avoids with
        // `Projections.include(stateField)`.
        let state_field = format!("{FIELD_STATES}.{key}");
        let doc = self
            .client
            .find_one_projected(&self.collection, &filter, &[state_field.as_str()])
            .await
            .ok()
            .flatten()?;
        let states = doc.get(FIELD_STATES).and_then(|v| v.as_object())?;
        let raw = states.get(key)?;
        if list {
            Some(Value::Array(
                raw.as_array()?.iter().map(key_escaper::unescape).collect(),
            ))
        } else {
            Some(key_escaper::unescape(raw))
        }
    }

    /// Deletes a single key from a session slot, removing its value, version and
    /// list hash (`$unset`), mirroring Java `MongoAgentStateStore.delete(userId,
    /// sessionId, key)`.
    ///
    /// Returns an error when `key` is not a valid state key (see
    /// [`validate_state_key`]); on a missing slot the update is a no-op.
    pub async fn delete_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<(), MongoError> {
        self.delete_key_inner(user_id, session_id, key).await
    }

    async fn delete_key_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<(), MongoError> {
        validate_state_key(key)?;
        require_session_id(session_id)?;
        let filter = slot_filter(user_id, session_id);
        let update = Update::new()
            .unset(format!("{FIELD_STATES}.{key}"))
            .unset(format!("{FIELD_VERSIONS}.{key}"))
            .unset(format!("{FIELD_HASHES}.{key}"))
            .set(
                FIELD_UPDATED_AT,
                json!(chrono::Utc::now().timestamp_millis()),
            );
        self.client
            .update_one(
                &self.collection,
                &filter,
                &update,
                &UpdateOptions::default(),
            )
            .await
    }
}

#[async_trait]
impl AgentStateStore for MongodbAgentStateStore {
    async fn save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) {
        if let Err(e) = self
            .save_value_inner(user_id, session_id, key, &value)
            .await
        {
            tracing::warn!(key, error = %e, "[mongo-state] save failed");
        }
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) {
        if let Err(e) = self.save_list_inner(user_id, session_id, key, values).await {
            tracing::warn!(key, error = %e, "[mongo-state] save_list failed");
        }
    }

    async fn get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Option<serde_json::Value> {
        if validate_state_key(key).is_err() {
            return None;
        }
        self.get_field(user_id, session_id, key, false).await
    }

    async fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Vec<serde_json::Value> {
        if validate_state_key(key).is_err() {
            return Vec::new();
        }
        match self.get_field(user_id, session_id, key, true).await {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        }
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        let filter = slot_filter(user_id, session_id);
        // A count alone answers "does the slot exist?" without transferring the session
        // document (MONGO-05); `count` would otherwise be an unused seam method (MONGO-10).
        matches!(
            self.client.count(&self.collection, &filter).await,
            Ok(n) if n > 0
        )
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if let Some(cb) = &self.on_delete {
            // Awaited, with no block_on bridge. The callback owns its error policy: the
            // cascade in MongodbDistributedStore logs failures and never blocks deletion.
            cb(user_id.map(str::to_string), session_id.to_string()).await;
        }
        let filter = slot_filter(user_id, session_id);
        let _ = self.client.delete_one(&self.collection, &filter).await;
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let filter = FindFilter::eq(FIELD_USER_ID, json!(normalize_user(user_id)));
        match self
            .client
            .distinct(&self.collection, FIELD_SESSION_ID, &filter)
            .await
        {
            Ok(values) => {
                let set: BTreeSet<String> = values
                    .into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                set.into_iter().collect()
            }
            Err(_) => Vec::new(),
        }
    }

    fn supports_versioning(&self) -> bool {
        true
    }

    async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> agentscope_core::state::VersionedState {
        match MongodbAgentStateStore::get_version(self, user_id, session_id, key).await {
            Ok((value, version)) => agentscope_core::state::VersionedState { value, version },
            // Error path: report UNVERSIONED (no usable version) instead of a stale 0, so a
            // CAS writer can never mistake a failed read for create-if-absent.
            Err(_) => agentscope_core::state::VersionedState {
                value: None,
                version: agentscope_core::state::UNVERSIONED,
            },
        }
    }

    async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> i64 {
        match MongodbAgentStateStore::save_if_version(
            self,
            user_id,
            session_id,
            key,
            &value,
            expected_version,
        )
        .await
        {
            Ok(v) => v,
            Err(_) => agentscope_core::state::UNVERSIONED,
        }
    }
}

/// Builder for [`MongodbAgentStateStore`].
#[derive(Default)]
pub struct MongodbAgentStateStoreBuilder {
    client: Option<Arc<dyn MongoClient>>,
    collection: Option<String>,
    ttl_days: Option<i64>,
    on_delete: Option<SessionDeleteCallback>,
}

impl MongodbAgentStateStoreBuilder {
    pub fn client(mut self, client: Arc<dyn MongoClient>) -> Self {
        self.client = Some(client);
        self
    }

    pub fn collection(mut self, collection: impl Into<String>) -> Self {
        self.collection = Some(collection.into());
        self
    }

    /// Optional session TTL in days. When positive, a sparse TTL index is created on
    /// `_updated_at` by [`MongodbAgentStateStore::ensure_indexes`], which reads the
    /// value retained by [`MongodbAgentStateStoreBuilder::build`]. Defaults to `None`
    /// (no expiry); a non-positive value is rejected by `build()`, mirroring Java
    /// `Builder.ttlDays` throwing `IllegalArgumentException` (MONGO-07).
    pub fn ttl_days(mut self, days: i64) -> Self {
        self.ttl_days = Some(days);
        self
    }

    /// Optional async callback invoked before a session document is deleted, receiving
    /// the owned `(user_id, session_id)`. Used by [`crate::MongodbDistributedStore`] to
    /// cascade-delete associated snapshots.
    pub fn on_delete(mut self, cb: SessionDeleteCallback) -> Self {
        self.on_delete = Some(cb);
        self
    }

    /// Builds the store. This is synchronous; call
    /// [`MongodbAgentStateStore::ensure_indexes`] to create indexes — it picks up
    /// the `ttl_days` configured here and creates the session TTL index.
    ///
    /// Fails when `ttl_days` was set to a non-positive value: silently disabling expiry
    /// would leave sessions alive forever with no feedback (MONGO-07; Java's
    /// `Builder.ttlDays` throws the same way).
    pub fn build(self) -> MongoResult<MongodbAgentStateStore> {
        let client = self
            .client
            .ok_or_else(|| MongoError::InvalidArgument("client must be provided".into()))?;
        if let Some(days) = self.ttl_days {
            if days <= 0 {
                return Err(MongoError::InvalidArgument(
                    "ttlDays must be positive or null".into(),
                ));
            }
        }
        let store = MongodbAgentStateStore {
            client,
            collection: self
                .collection
                .unwrap_or_else(|| SESSIONS_COLLECTION.to_string()),
            ttl_days: self.ttl_days,
            on_delete: self.on_delete,
        };
        Ok(store)
    }

    /// Builds the store **and** creates its indexes (composite `(user_id, session_id)`
    /// plus the `ttl_days`-driven `_updated_at` TTL index), so the store can never be
    /// used in the "TTL configured but index missing → sessions never expire" state
    /// (MONGO-03).
    ///
    /// This is the Rust equivalent of Java `MongoAgentStateStore`'s constructor, which
    /// calls `ensureIndexes(builder.ttlDays)` eagerly. [`Self::build`] stays available for
    /// callers that manage the index lifecycle themselves (e.g. migrations).
    pub async fn build_with_indexes(self) -> MongoResult<MongodbAgentStateStore> {
        let store = self.build()?;
        store.ensure_indexes().await?;
        Ok(store)
    }
}

/// Convenience: a store backed by the in-process [`InMemoryMongoClient`], used for
/// tests and embedded deployments.
pub fn in_memory_store() -> MongodbAgentStateStore {
    MongodbAgentStateStore::with_defaults(Arc::new(InMemoryMongoClient::new()))
}

/// The default database name (re-exported for callers that mirror the Java API).
pub const fn default_database() -> &'static str {
    DEFAULT_DATABASE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MongodbAgentStateStore {
        in_memory_store()
    }

    #[tokio::test]
    async fn save_and_get_single_value() {
        let s = store();
        s.save(Some("u1"), "sess", "foo", json!({"a": 1})).await;
        assert_eq!(
            s.get(Some("u1"), "sess", "foo").await,
            Some(json!({"a": 1}))
        );
        assert_eq!(s.get(Some("u1"), "sess", "missing").await, None);
    }

    /// MONGO-05: single-key reads project server-side instead of transferring the whole
    /// session document (Java `Projections.include(stateField)`).
    #[tokio::test]
    async fn single_key_reads_project_the_requested_fields() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let s = MongodbAgentStateStore::new(raw.clone(), SESSIONS_COLLECTION);
        s.save(Some("u"), "sess", "history", json!([1, 2, 3])).await;
        s.save(
            Some("u"),
            "sess",
            "other",
            json!({ "blob": "x".repeat(1024) }),
        )
        .await;

        assert_eq!(
            s.get(Some("u"), "sess", "history").await,
            Some(json!([1, 2, 3]))
        );
        assert_eq!(
            raw.last_projection(),
            Some(vec!["states.history".to_string()]),
            "get must transfer only the requested state field"
        );

        let (value, version) = s.get_version(Some("u"), "sess", "history").await.unwrap();
        assert_eq!(value, Some(json!([1, 2, 3])));
        assert_eq!(version, 1);
        assert_eq!(
            raw.last_projection(),
            Some(vec![
                "states.history".to_string(),
                "versions.history".to_string()
            ]),
            "get_version must project the value and its version only"
        );

        // `exists` transfers a count instead of a document, so it does not widen the read.
        assert!(s.exists(Some("u"), "sess").await);
        assert!(!s.exists(Some("u"), "missing").await);
    }

    #[tokio::test]
    async fn user_namespaces_are_isolated() {
        let s = store();
        s.save(Some("u1"), "sess", "foo", json!(1)).await;
        s.save(Some("u2"), "sess", "foo", json!(2)).await;
        assert_eq!(s.get(Some("u1"), "sess", "foo").await, Some(json!(1)));
        assert_eq!(s.get(Some("u2"), "sess", "foo").await, Some(json!(2)));
    }

    #[tokio::test]
    async fn anonymous_user_normalisation() {
        let s = store();
        s.save(None, "sess", "foo", json!(1)).await;
        assert_eq!(s.get(None, "sess", "foo").await, Some(json!(1)));
        assert_eq!(s.get(Some("  "), "sess", "foo").await, Some(json!(1)));
    }

    #[tokio::test]
    async fn dotted_and_dollar_keys_are_escaped() {
        let s = store();
        s.save(Some("u"), "sess", "foo", json!({"a.b": {"$c": 1}}))
            .await;
        assert_eq!(
            s.get(Some("u"), "sess", "foo").await,
            Some(json!({"a.b": {"$c": 1}}))
        );
    }

    #[tokio::test]
    async fn invalid_state_key_is_rejected() {
        let s = store();
        assert!(validate_state_key("a.b").is_err());
        assert!(validate_state_key("$x").is_err());
        assert!(validate_state_key("1abc").is_err());
        assert!(validate_state_key("").is_err());
        // The trait swallows the error, returning None rather than panicking.
        assert_eq!(s.get(Some("u"), "sess", "bad.key").await, None);
    }

    #[tokio::test]
    async fn list_append_and_full_rewrite() {
        let s = store();
        s.save_list(Some("u"), "sess", "history", &[json!(1), json!(2)])
            .await;
        assert_eq!(
            s.get_list(Some("u"), "sess", "history").await,
            vec![json!(1), json!(2)]
        );
        // Append.
        s.save_list(
            Some("u"),
            "sess",
            "history",
            &[json!(1), json!(2), json!(3)],
        )
        .await;
        assert_eq!(
            s.get_list(Some("u"), "sess", "history").await,
            vec![json!(1), json!(2), json!(3)]
        );
        // Rewrite (removal).
        s.save_list(Some("u"), "sess", "history", &[json!(9)]).await;
        assert_eq!(
            s.get_list(Some("u"), "sess", "history").await,
            vec![json!(9)]
        );
        // Shrink to empty.
        s.save_list(Some("u"), "sess", "history", &[]).await;
        assert_eq!(
            s.get_list(Some("u"), "sess", "history").await,
            Vec::<Value>::new()
        );
    }

    /// Java `MongoAgentStateStoreTest#saveListAppendOnNewSessionIncludesSetOnInsert`: the
    /// first write of a slot records the session identity columns (`user_id` /
    /// `session_id`) so `listSessionIds` can find it by `distinct`.
    #[tokio::test]
    async fn set_on_insert_records_session_identity() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let s = MongodbAgentStateStore::with_defaults(raw.clone());
        s.save(Some("u1"), "sess", "k", json!(1)).await;
        s.save_list(Some("u1"), "sess2", "list", &[json!(1)]).await;

        for session in ["sess", "sess2"] {
            let filter = FindFilter::eq("_id", json!({ "user": "u1", "session": session }));
            let doc = raw
                .find_one(crate::constants::SESSIONS_COLLECTION, &filter)
                .await
                .unwrap()
                .expect("session document exists");
            assert_eq!(doc.get("user_id").and_then(|v| v.as_str()), Some("u1"));
            assert_eq!(
                doc.get("session_id").and_then(|v| v.as_str()),
                Some(session)
            );
        }
    }

    /// Java `MongoAgentStateStoreContractTest#concurrentWriters_onlyOneSucceeds`: the
    /// create-if-absent branch is atomic on the server, so two racing writers cannot both
    /// create the same key.
    #[tokio::test]
    async fn concurrent_create_if_absent_only_one_writer_wins() {
        let s = store();
        let a = json!("a");
        let b = json!("b");
        let first = s.save_if_version(Some("u"), "sess", "k", &a, 0);
        let second = s.save_if_version(Some("u"), "sess", "k", &b, 0);
        let (first, second) = tokio::join!(first, second);
        let results = [first.unwrap(), second.unwrap()];
        assert_eq!(
            results.iter().filter(|v| **v == 1).count(),
            1,
            "exactly one create-if-absent may win: {results:?}"
        );
        assert_eq!(
            results.iter().filter(|v| **v == UNVERSIONED).count(),
            1,
            "the loser reports UNVERSIONED: {results:?}"
        );
        let (_, version) = s.get_version(Some("u"), "sess", "k").await.unwrap();
        assert_eq!(version, 1);
    }

    /// Java validates `sessionId` in `slotId(...)` and throws `IllegalArgumentException`
    /// for a blank id; the inherent APIs report it as `InvalidArgument`.
    #[tokio::test]
    async fn blank_session_id_is_rejected() {
        let s = store();
        assert!(s
            .save_if_version(Some("u"), "  ", "k", &json!(1), UNVERSIONED)
            .await
            .is_err());
        assert!(s.delete_key(Some("u"), "", "k").await.is_err());
        assert!(s.delete_key(Some("u"), "sess", "bad.key").await.is_err());
    }

    #[tokio::test]
    async fn versions_increment_and_cas_works() {
        let s = store();
        // Create with expected version 0.
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(1), 0)
                .await
                .unwrap(),
            1
        );
        // Second create-if-absent fails (version exists).
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(2), 0)
                .await
                .unwrap(),
            UNVERSIONED
        );
        // CAS on the right version succeeds and bumps to 2.
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(3), 1)
                .await
                .unwrap(),
            2
        );
        // Stale CAS fails.
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(4), 1)
                .await
                .unwrap(),
            UNVERSIONED
        );
        let (value, version) = s.get_version(Some("u"), "sess", "k").await.unwrap();
        assert_eq!(value, Some(json!(3)));
        assert_eq!(version, 2);
    }

    /// The core `AgentStateStore` versioning surface (not just the inherent
    /// `get_version` / `save_if_version` pair): `supports_versioning()` is true, an absent
    /// key reports version `0` (not `UNVERSIONED`), and a lost CAS reports `UNVERSIONED`
    /// without overwriting the stored value.
    #[tokio::test]
    async fn trait_versioning_surface_matches_core_contract() {
        let s = store();
        assert!(AgentStateStore::supports_versioning(&s));

        let absent = s.get_versioned(Some("u"), "sess", "k").await;
        assert_eq!(absent.value, None);
        assert_eq!(
            absent.version, 0,
            "an absent key reports version 0 on a versioning backend"
        );

        assert_eq!(
            AgentStateStore::save_if_version(&s, Some("u"), "sess", "k", json!(1), 0).await,
            1
        );
        let current = s.get_versioned(Some("u"), "sess", "k").await;
        assert_eq!(current.value, Some(json!(1)));
        assert_eq!(current.version, 1);

        assert_eq!(
            AgentStateStore::save_if_version(&s, Some("u"), "sess", "k", json!(2), 0).await,
            agentscope_core::state::UNVERSIONED
        );
        assert_eq!(s.get(Some("u"), "sess", "k").await, Some(json!(1)));
    }

    #[tokio::test]
    async fn unconditional_save_if_version_uses_negative_one() {
        let s = store();
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(1), UNVERSIONED)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(2), UNVERSIONED)
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn exists_delete_and_list_session_ids() {
        let s = store();
        s.save(Some("u"), "s1", "k", json!(1)).await;
        s.save(Some("u"), "s2", "k", json!(1)).await;
        assert!(s.exists(Some("u"), "s1").await);
        let mut ids = s.list_session_ids(Some("u")).await;
        ids.sort();
        assert_eq!(ids, vec!["s1".to_string(), "s2".to_string()]);
        s.delete(Some("u"), "s1").await;
        assert!(!s.exists(Some("u"), "s1").await);
        assert_eq!(s.list_session_ids(Some("u")).await, vec!["s2".to_string()]);
    }

    #[tokio::test]
    async fn delete_single_key_unset() {
        let s = store();
        s.save(Some("u"), "sess", "k", json!(1)).await;
        s.delete_key_inner(Some("u"), "sess", "k").await.unwrap();
        assert_eq!(s.get(Some("u"), "sess", "k").await, None);
    }

    #[tokio::test]
    async fn public_delete_key_removes_value_version_and_hash() {
        let s = store();
        // Single value plus a list key (which also stores a version + hash).
        s.save(Some("u"), "sess", "value_key", json!(1)).await;
        s.save_list(Some("u"), "sess", "list_key", &[json!(1), json!(2)])
            .await;
        assert_eq!(
            s.get_version(Some("u"), "sess", "value_key")
                .await
                .unwrap()
                .1,
            1
        );

        s.delete_key(Some("u"), "sess", "value_key").await.unwrap();
        assert_eq!(s.get(Some("u"), "sess", "value_key").await, None);
        // The whole `(states, versions, hashes)` entry is gone; re-saving starts fresh.
        assert_eq!(
            s.get_version(Some("u"), "sess", "value_key").await.unwrap(),
            (None, 0)
        );
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "value_key", &json!(9), 0)
                .await
                .unwrap(),
            1
        );
        // The unrelated list key is untouched.
        assert_eq!(
            s.get_list(Some("u"), "sess", "list_key").await,
            vec![json!(1), json!(2)]
        );
        // Deleting a missing key is a no-op, not an error.
        s.delete_key(Some("u"), "sess", "never_existed")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_key_validates_state_key() {
        let s = store();
        assert!(s.delete_key(Some("u"), "sess", "bad.key").await.is_err());
        assert!(s.delete_key(Some("u"), "sess", "").await.is_err());
    }

    #[tokio::test]
    async fn key_escaper_round_trips_dots_dollars_nul_and_backslash() {
        let s = store();
        // A value whose nested object keys exercise all four escape classes.
        let tricky = json!({
            "a.b": { "$c": { "d\\e": "v1", "nul\u{0}key": "v2" } },
            "plain": 3
        });
        s.save(Some("u"), "sess", "k", tricky.clone()).await;
        assert_eq!(s.get(Some("u"), "sess", "k").await, Some(tricky.clone()));

        // The same value through the list path.
        s.save_list(Some("u"), "sess", "l", std::slice::from_ref(&tricky))
            .await;
        assert_eq!(s.get_list(Some("u"), "sess", "l").await, vec![tricky]);
    }

    #[tokio::test]
    async fn ttl_index_is_created_only_when_positive() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let client: Arc<dyn MongoClient> = raw.clone();
        let s = MongodbAgentStateStore::builder()
            .client(client.clone())
            .ttl_days(7)
            .build()
            .unwrap();

        s.ensure_indexes().await.unwrap();
        let indexes = client
            .list_indexes(crate::constants::SESSIONS_COLLECTION)
            .await
            .unwrap();
        let ttl = indexes
            .iter()
            .find(|i| i.expire_after_seconds.is_some())
            .expect("ttl index");
        assert_eq!(ttl.expire_after_seconds, Some(7 * 24 * 3600));
        assert!(ttl.sparse);

        // MONGO-07: a non-positive TTL is a configuration error, not a silent "TTL off"
        // (Java `Builder.ttlDays` throws IllegalArgumentException).
        for days in [0, -1] {
            let err = MongodbAgentStateStore::builder()
                .client(Arc::new(InMemoryMongoClient::new()))
                .ttl_days(days)
                .build()
                .expect_err("a non-positive ttl_days must be rejected");
            assert!(
                err.to_string().contains("ttlDays must be positive"),
                "unexpected error for ttl_days({days}): {err}"
            );
        }

        // Without `ttl_days` no TTL index is created at all.
        let raw2 = Arc::new(InMemoryMongoClient::new());
        let client2: Arc<dyn MongoClient> = raw2.clone();
        let s2 = MongodbAgentStateStore::builder()
            .client(client2.clone())
            .build()
            .unwrap();
        s2.ensure_indexes().await.unwrap();
        assert!(client2
            .list_indexes(crate::constants::SESSIONS_COLLECTION)
            .await
            .unwrap()
            .iter()
            .all(|i| i.expire_after_seconds.is_none()));
    }

    /// Regression test for the builder `ttl_days` being dropped on `build()`:
    /// the value must be retained by the store and drive the TTL index created by
    /// the no-argument `ensure_indexes()`, mirroring Java
    /// `MongoAgentStateStore` calling `ensureIndexes(builder.ttlDays)`.
    #[tokio::test]
    async fn builder_ttl_days_is_retained_and_creates_ttl_index() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let client: Arc<dyn MongoClient> = raw.clone();
        let s = MongodbAgentStateStore::builder()
            .client(client.clone())
            .ttl_days(3)
            .build()
            .unwrap();

        assert_eq!(s.ttl_days(), Some(3));

        s.ensure_indexes().await.unwrap();
        let indexes = client
            .list_indexes(crate::constants::SESSIONS_COLLECTION)
            .await
            .unwrap();
        let ttl = indexes
            .iter()
            .find(|i| i.expire_after_seconds.is_some())
            .expect("ttl index created from the builder's ttl_days");
        assert_eq!(ttl.expire_after_seconds, Some(3 * 86_400));
        assert_eq!(ttl.keys, vec![("_updated_at".to_string(), 1)]);
        assert!(ttl.sparse);

        // A store built without `ttl_days` keeps `None`.
        let s2 = MongodbAgentStateStore::with_defaults(Arc::new(InMemoryMongoClient::new()));
        assert_eq!(s2.ttl_days(), None);
        let s3 = MongodbAgentStateStore::builder()
            .client(Arc::new(InMemoryMongoClient::new()))
            .build()
            .unwrap();
        assert_eq!(s3.ttl_days(), None);
    }

    /// MONGO-03: `build_with_indexes` creates the composite **and** the `ttl_days`-driven
    /// TTL index eagerly (Java's constructor equivalent), so a store can never be used in
    /// the "TTL configured but index missing → sessions never expire" state.
    #[tokio::test]
    async fn build_with_indexes_creates_the_composite_and_ttl_indexes() {
        let raw = Arc::new(InMemoryMongoClient::new());
        let client: Arc<dyn MongoClient> = raw.clone();
        let s = MongodbAgentStateStore::builder()
            .client(client.clone())
            .ttl_days(2)
            .build_with_indexes()
            .await
            .expect("indexes are created while building");

        let indexes = client
            .list_indexes(crate::constants::SESSIONS_COLLECTION)
            .await
            .unwrap();
        assert!(
            indexes.iter().any(|i| i.keys
                == vec![("user_id".to_string(), 1), ("session_id".to_string(), 1)]
                && !i.sparse),
            "composite (user_id, session_id) index must be created eagerly: {indexes:?}"
        );
        let ttl = indexes
            .iter()
            .find(|i| i.expire_after_seconds.is_some())
            .expect("ttl index created eagerly from the builder's ttl_days");
        assert_eq!(ttl.expire_after_seconds, Some(2 * 86_400));
        assert_eq!(ttl.keys, vec![("_updated_at".to_string(), 1)]);
        assert_eq!(s.ttl_days(), Some(2));
    }

    #[tokio::test]
    async fn on_delete_callback_fires() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = fired.clone();
        let observed = Arc::new(std::sync::Mutex::new(None::<(String, String)>));
        let observed_slot = observed.clone();
        let store = MongodbAgentStateStore::builder()
            .client(Arc::new(InMemoryMongoClient::new()))
            .on_delete(Arc::new(move |user, session| {
                let flag = flag.clone();
                let observed = observed_slot.clone();
                Box::pin(async move {
                    *observed.lock().unwrap() = Some((user.unwrap_or_default(), session));
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                })
            }))
            .build()
            .unwrap();
        store.save(Some("u"), "sess", "k", json!(1)).await;
        store.delete(Some("u"), "sess").await;
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        // The callback receives the owned identity of the deleted slot and runs before
        // the document is removed, so a cascade can still read the session's slots.
        assert_eq!(
            observed.lock().unwrap().clone(),
            Some(("u".to_string(), "sess".to_string()))
        );
        assert!(!store.exists(Some("u"), "sess").await);
    }
}
