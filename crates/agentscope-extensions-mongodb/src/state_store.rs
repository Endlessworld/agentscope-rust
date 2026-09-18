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

use agentscope_core::state::AgentStateStore;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::client::{
    FindFilter, FindOneAndUpdateOptions, InMemoryMongoClient, MongoClient, MongoError, MongoResult,
    Update, UpdateOptions,
};
use crate::constants::{DEFAULT_DATABASE, SESSIONS_COLLECTION};
use crate::index_utils::{create_index_with_migration, IndexSpec};
use crate::key_escaper;
use crate::list_hash;

const ANON_USER: &str = "__anon__";
const FIELD_USER_ID: &str = "user_id";
const FIELD_SESSION_ID: &str = "session_id";
const FIELD_UPDATED_AT: &str = "_updated_at";
const FIELD_STATES: &str = "states";
const FIELD_VERSIONS: &str = "versions";
const FIELD_HASHES: &str = "hashes";

/// Sentinel returned by [`MongodbAgentStateStore::save_if_version`] when the
/// conditional write lost the CAS race.
pub const UNVERSIONED: i64 = -1;

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
    MongoError::InvalidArgument(format!(
        "key must match pattern ^[a-zA-Z_][a-zA-Z0-9_]*$ but was: {key} 
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

fn slot_filter(user_id: Option<&str>, session_id: &str) -> FindFilter {
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
    on_delete: Option<Arc<dyn Fn(Option<&str>, &str) + Send + Sync>>,
}

impl std::fmt::Debug for MongodbAgentStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MongodbAgentStateStore")
            .field("collection", &self.collection)
            .field("has_on_delete", &self.on_delete.is_some())
            .finish()
    }
}

impl MongodbAgentStateStore {
    /// Creates a store over the given client and collection with default options.
    pub fn new(client: Arc<dyn MongoClient>, collection: impl Into<String>) -> Self {
        Self { client, collection: collection.into(), on_delete: None }
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

    /// Ensures the required indexes exist. Must be called before use (the builder
    /// constructor does it automatically).
    pub async fn ensure_indexes(&self, ttl_days: Option<i64>) -> MongoResult<()> {
        create_index_with_migration(
            &self.client,
            &self.collection,
            &IndexSpec::compound_ascending(&[FIELD_USER_ID, FIELD_SESSION_ID]),
        )
        .await?;
        if let Some(days) = ttl_days {
            if days > 0 {
                create_index_with_migration(
                    &self.client,
                    &self.collection,
                    &IndexSpec::ascending(&[FIELD_UPDATED_AT])
                        .sparse()
                        .expire_after_seconds(days * 24 * 3600),
                )
                .await?;
            }
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
        let state_field = format!("{FIELD_STATES}.{key}");
        let version_field = format!("{FIELD_VERSIONS}.{key}");
        let doc = self.client.find_one(&self.collection, &filter).await?;
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
        let _ = (state_field, version_field);
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
                        &FindOneAndUpdateOptions { upsert: true, return_after: true },
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
                    &FindOneAndUpdateOptions { upsert: true, return_after: true },
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
            let filter = filter.and(FindFilter::eq(version_field.clone(), json!(expected_version)));
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
                    &FindOneAndUpdateOptions { upsert: false, return_after: true },
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

        // Obtain the existing count and stored hash server-side.
        let existing_count = self
            .client
            .first_field_size(&self.collection, &filter, &state_field)
            .await?
            .unwrap_or(0);
        let stored_hash = self
            .client
            .first_field_string(&self.collection, &filter, &hash_field)
            .await?;
        let current_hash = list_hash::compute_hash(values);

        let encoded: Vec<Value> = values.iter().map(key_escaper::escape).collect();
        let set_on_insert = Update::new()
            .set_on_insert(FIELD_USER_ID, json!(normalize_user(user_id)))
            .set_on_insert(FIELD_SESSION_ID, json!(session_id));

        if list_hash::needs_full_rewrite(values, stored_hash.as_deref(), existing_count) {
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

    async fn get_field(&self, user_id: Option<&str>, session_id: &str, key: &str, list: bool) -> Option<Value> {
        let filter = slot_filter(user_id, session_id);
        let doc = self.client.find_one(&self.collection, &filter).await.ok().flatten()?;
        let states = doc.get(FIELD_STATES).and_then(|v| v.as_object())?;
        let raw = states.get(key)?;
        if list {
            Some(Value::Array(
                raw.as_array()?
                    .iter()
                    .map(key_escaper::unescape)
                    .collect(),
            ))
        } else {
            Some(key_escaper::unescape(raw))
        }
    }

    async fn delete_key_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<(), MongoError> {
        validate_state_key(key)?;
        let filter = slot_filter(user_id, session_id);
        let update = Update::new()
            .unset(format!("{FIELD_STATES}.{key}"))
            .unset(format!("{FIELD_VERSIONS}.{key}"))
            .unset(format!("{FIELD_HASHES}.{key}"))
            .set(FIELD_UPDATED_AT, json!(chrono::Utc::now().timestamp_millis()));
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
        if let Err(e) = self.save_value_inner(user_id, session_id, key, &value).await {
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
        matches!(
            self.client.find_one(&self.collection, &filter).await,
            Ok(Some(_))
        )
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if let Some(cb) = &self.on_delete {
            cb(user_id, session_id);
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
}

/// Builder for [`MongodbAgentStateStore`].
#[derive(Default)]
pub struct MongodbAgentStateStoreBuilder {
    client: Option<Arc<dyn MongoClient>>,
    collection: Option<String>,
    ttl_days: Option<i64>,
    on_delete: Option<Arc<dyn Fn(Option<&str>, &str) + Send + Sync>>,
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
    /// `_updated_at`. Defaults to `None` (no expiry).
    pub fn ttl_days(mut self, days: i64) -> Self {
        self.ttl_days = Some(days);
        self
    }

    /// Optional callback invoked before a session document is deleted, receiving
    /// `(user_id, session_id)`. Used by [`crate::MongodbDistributedStore`] to
    /// cascade-delete associated snapshots.
    pub fn on_delete(
        mut self,
        cb: Arc<dyn Fn(Option<&str>, &str) + Send + Sync>,
    ) -> Self {
        self.on_delete = Some(cb);
        self
    }

    /// Builds the store. This is synchronous; call
    /// [`MongodbAgentStateStore::ensure_indexes`] to create indexes.
    pub fn build(self) -> MongoResult<MongodbAgentStateStore> {
        let client = self
            .client
            .ok_or_else(|| MongoError::InvalidArgument("client must be provided".into()))?;
        let store = MongodbAgentStateStore {
            client,
            collection: self.collection.unwrap_or_else(|| SESSIONS_COLLECTION.to_string()),
            on_delete: self.on_delete,
        };
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
        assert_eq!(s.get(Some("u1"), "sess", "foo").await, Some(json!({"a": 1})));
        assert_eq!(s.get(Some("u1"), "sess", "missing").await, None);
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
        s.save(Some("u"), "sess", "foo", json!({"a.b": {"$c": 1}})).await;
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
        s.save_list(Some("u"), "sess", "history", &[json!(1), json!(2)]).await;
        assert_eq!(s.get_list(Some("u"), "sess", "history").await, vec![json!(1), json!(2)]);
        // Append.
        s.save_list(Some("u"), "sess", "history", &[json!(1), json!(2), json!(3)]).await;
        assert_eq!(
            s.get_list(Some("u"), "sess", "history").await,
            vec![json!(1), json!(2), json!(3)]
        );
        // Rewrite (removal).
        s.save_list(Some("u"), "sess", "history", &[json!(9)]).await;
        assert_eq!(s.get_list(Some("u"), "sess", "history").await, vec![json!(9)]);
        // Shrink to empty.
        s.save_list(Some("u"), "sess", "history", &[]).await;
        assert_eq!(s.get_list(Some("u"), "sess", "history").await, Vec::<Value>::new());
    }

    #[tokio::test]
    async fn versions_increment_and_cas_works() {
        let s = store();
        // Create with expected version 0.
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(1), 0).await.unwrap(),
            1
        );
        // Second create-if-absent fails (version exists).
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(2), 0).await.unwrap(),
            UNVERSIONED
        );
        // CAS on the right version succeeds and bumps to 2.
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(3), 1).await.unwrap(),
            2
        );
        // Stale CAS fails.
        assert_eq!(
            s.save_if_version(Some("u"), "sess", "k", &json!(4), 1).await.unwrap(),
            UNVERSIONED
        );
        let (value, version) = s.get_version(Some("u"), "sess", "k").await.unwrap();
        assert_eq!(value, Some(json!(3)));
        assert_eq!(version, 2);
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
    async fn on_delete_callback_fires() {
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = fired.clone();
        let store = MongodbAgentStateStore::builder()
            .client(Arc::new(InMemoryMongoClient::new()))
            .on_delete(Arc::new(move |_u, _s| {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }))
            .build()
            .unwrap();
        store.save(Some("u"), "sess", "k", json!(1)).await;
        store.delete(Some("u"), "sess").await;
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
    }
}
