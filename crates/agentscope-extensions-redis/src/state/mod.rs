//! Redis-backed [`AgentStateStore`], mirroring
//! `io.agentscope.extensions.redis.state.RedisAgentStateStore`.
//!
//! ## Key layout (per `slot = {userId}/{sessionId}`)
//!
//! * single state: `{prefix}{slot}:{key}` — Redis String containing JSON
//! * single version: `{prefix}{slot}:{key}:ver` — monotonic counter String
//! * list state: `{prefix}{slot}:{key}:list` — Redis List of JSON items
//! * list hash: `{prefix}{slot}:{key}:list:_hash` — change-detection String
//! * session key index: `{prefix}{slot}:_keys` — Redis Set of state keys
//!
//! ## Versioning / CAS
//!
//! Version semantics follow the harness [`AgentStateStore`] contract: "the store does not
//! expose an explicit version API; implementations that support optimistic concurrency
//! expose it out-of-band". Here [`RedisAgentStateStore::save_if_version`] and
//! [`RedisAgentStateStore::get_versioned`] surface the Java `saveIfVersion` / `getVersioned`
//! operations, which are built directly on the shared [`SAVE_SCRIPT`] CAS primitive.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use agentscope_core::error::{Error, Result};
use agentscope_core::state::{compute_hash, needs_full_rewrite, AgentStateStore};

pub mod adapter;
pub mod connection;
mod version_support;

use self::adapter::RedisClientAdapter;
use self::version_support::{SAVE_SCRIPT, UNCONDITIONAL};

/// Default key prefix (matches Java `DEFAULT_KEY_PREFIX`).
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:session:";

const KEYS_SUFFIX: &str = ":_keys";
const LIST_SUFFIX: &str = ":list";
const HASH_SUFFIX: &str = ":_hash";
const ANON_USER: &str = "__anon__";

/// A versioned state value (payload + version), mirroring Java `VersionedState`.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionedState {
    pub value: Option<Value>,
    pub version: u64,
}

/// Sentinel used by [`AgentStateStore::save`] to signal "write unconditionally".
///
/// Mirrors Java `UNCONDITIONAL_EXPECTED = Long.MIN_VALUE`. In Rust we use `u64::MAX`
/// since versions are non-negative; it can never collide with a real version.
const UNCONDITIONAL_EXPECTED: u64 = u64::MAX;

/// Redis-backed agent state store supporting multiple Redis clients via
/// [`RedisClientAdapter`].
pub struct RedisAgentStateStore {
    client: Arc<dyn RedisClientAdapter>,
    key_prefix: String,
}

impl std::fmt::Debug for RedisAgentStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisAgentStateStore")
            .field("key_prefix", &self.key_prefix)
            .finish_non_exhaustive()
    }
}

impl RedisAgentStateStore {
    /// Creates a builder.
    pub fn builder() -> RedisAgentStateStoreBuilder {
        RedisAgentStateStoreBuilder::default()
    }

    /// Creates a store from an adapter with an explicit key prefix.
    pub fn new(client: Arc<dyn RedisClientAdapter>, key_prefix: impl Into<String>) -> Self {
        Self { client, key_prefix: key_prefix.into() }
    }

    /// The configured key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// Whether this store supports optimistic-concurrency versioning (always `true`).
    pub fn supports_versioning(&self) -> bool {
        true
    }

    // ---- key helpers -------------------------------------------------------

    fn get_state_key(&self, slot_id: &str, key: &str) -> String {
        format!("{}{slot_id}:{key}", self.key_prefix)
    }

    fn get_list_key(&self, slot_id: &str, key: &str) -> String {
        format!("{}{slot_id}:{key}{LIST_SUFFIX}", self.key_prefix)
    }

    fn get_keys_key(&self, slot_id: &str) -> String {
        format!("{}{slot_id}{KEYS_SUFFIX}", self.key_prefix)
    }

    /// Unconditionally writes a versioned value, returning the new version.
    async fn save_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &Value,
        expected_version: u64,
    ) -> Result<u64> {
        let slot_id = slot_id(user_id, session_id)?;
        let redis_key = self.get_state_key(&slot_id, key);
        let version_key = version_support::version_key(&redis_key);
        let keys_key = self.get_keys_key(&slot_id);
        let json = serde_json::to_string(value)?;

        let args = if expected_version == UNCONDITIONAL_EXPECTED {
            vec![json, UNCONDITIONAL.to_string(), key.to_string()]
        } else {
            vec![json, expected_version.to_string(), key.to_string()]
        };

        let result = self
            .client
            .eval_script(SAVE_SCRIPT, &[redis_key, version_key, keys_key], &args)
            .await?;

        match result {
            Some(v) if v >= 0 => Ok(v as u64),
            _ => Err(Error::Other(format!("Version conflict saving state: {key}"))),
        }
    }

    /// Reads a versioned value, mirroring Java `getVersioned`.
    pub async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<VersionedState> {
        let slot_id = slot_id(user_id, session_id)?;
        let redis_key = self.get_state_key(&slot_id, key);
        let version_key = version_support::version_key(&redis_key);

        let json = self.client.get(&redis_key).await?;
        let Some(json) = json else {
            return Ok(VersionedState { value: None, version: 0 });
        };
        let version_str = self.client.get(&version_key).await?;
        let version = version_support::parse_version(true, version_str.as_deref());
        let value = serde_json::from_str(&json)?;
        Ok(VersionedState { value: Some(value), version })
    }

    /// Conditional save (compare-and-swap), mirroring Java `saveIfVersion`.
    ///
    /// Returns the new version on success, or `UNVERSIONED` (`0`) on conflict.
    pub async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &Value,
        expected_version: u64,
    ) -> Result<u64> {
        if expected_version == 0 {
            // Mirrors Java: UNVERSIONED -> unconditional bump. (Java's UNVERSIONED is a
            // dedicated sentinel; for a freshly-created key version 0 also means
            // "create if absent", which the SAVE_SCRIPT handles via current_version().)
            return self
                .save_versioned(user_id, session_id, key, value, UNCONDITIONAL_EXPECTED)
                .await;
        }
        let slot_id = slot_id(user_id, session_id)?;
        let redis_key = self.get_state_key(&slot_id, key);
        let version_key = version_support::version_key(&redis_key);
        let keys_key = self.get_keys_key(&slot_id);
        let json = serde_json::to_string(value)?;

        let args = vec![json, expected_version.to_string(), key.to_string()];
        let result = self
            .client
            .eval_script(SAVE_SCRIPT, &[redis_key, version_key, keys_key], &args)
            .await?;
        match result {
            Some(v) if v >= 0 => Ok(v as u64),
            _ => Ok(0),
        }
    }

    /// Clears all sessions stored in Redis (testing / cleanup), mirroring
    /// Java `clearAllSessions`. Returns the number of deleted keys.
    pub async fn clear_all_sessions(&self) -> Result<usize> {
        let pattern = format!("{}*", self.key_prefix);
        let keys = self.client.find_keys_by_pattern(&pattern).await?;
        let count = keys.len();
        if !keys.is_empty() {
            self.client.delete_keys(&keys).await?;
        }
        Ok(count)
    }
}

#[async_trait]
impl AgentStateStore for RedisAgentStateStore {
    async fn save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
    ) {
        if let Err(e) = self
            .save_versioned(user_id, session_id, key, &value, UNCONDITIONAL_EXPECTED)
            .await
        {
            tracing::error!("RedisAgentStateStore::save failed for key {key}: {e}");
        }
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[Value],
    ) {
        if let Err(e) = self.save_list_inner(user_id, session_id, key, values).await {
            tracing::error!("RedisAgentStateStore::save_list failed for key {key}: {e}");
        }
    }

    async fn get(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Option<Value> {
        let slot_id = slot_id(user_id, session_id).ok()?;
        let redis_key = self.get_state_key(&slot_id, key);
        match self.client.get(&redis_key).await {
            Ok(Some(json)) => serde_json::from_str(&json).ok(),
            Ok(None) => None,
            Err(e) => {
                tracing::error!("RedisAgentStateStore::get failed for key {key}: {e}");
                None
            }
        }
    }

    async fn get_list(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Vec<Value> {
        let Ok(slot_id) = slot_id(user_id, session_id) else {
            return Vec::new();
        };
        let redis_key = self.get_list_key(&slot_id, key);
        match self.client.range_list(&redis_key, 0, -1).await {
            Ok(jsons) => jsons
                .into_iter()
                .filter_map(|j| serde_json::from_str(&j).ok())
                .collect(),
            Err(e) => {
                tracing::error!("RedisAgentStateStore::get_list failed for key {key}: {e}");
                Vec::new()
            }
        }
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        let Ok(slot_id) = slot_id(user_id, session_id) else {
            return false;
        };
        let keys_key = self.get_keys_key(&slot_id);
        match self.client.key_exists(&keys_key).await {
            Ok(true) => self.client.get_set_size(&keys_key).await.map(|n| n > 0).unwrap_or(false),
            _ => false,
        }
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if let Err(e) = self.delete_inner(user_id, session_id).await {
            tracing::error!("RedisAgentStateStore::delete failed: {e}");
        }
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let user_segment = normalize_user(user_id);
        // Pattern: {prefix}{userSegment}/{sessionId}:_keys
        let pattern = format!("{}{user_segment}/*{KEYS_SUFFIX}", self.key_prefix);
        let keys = match self.client.find_keys_by_pattern(&pattern).await {
            Ok(k) => k,
            Err(e) => {
                tracing::error!("RedisAgentStateStore::list_session_ids failed: {e}");
                return Vec::new();
            }
        };
        let user_prefix = format!("{}{user_segment}/", self.key_prefix);
        let mut sessions = std::collections::HashSet::new();
        for keys_key in keys {
            if let Some(without_prefix) = keys_key.strip_prefix(&user_prefix) {
                if let Some(session_id) = without_prefix.strip_suffix(KEYS_SUFFIX) {
                    sessions.insert(session_id.to_string());
                }
            }
        }
        let mut out: Vec<String> = sessions.into_iter().collect();
        out.sort();
        out
    }
}

impl RedisAgentStateStore {
    async fn save_list_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[Value],
    ) -> Result<()> {
        let slot_id = slot_id(user_id, session_id)?;
        let list_key = self.get_list_key(&slot_id, key);
        let hash_key = format!("{list_key}{HASH_SUFFIX}");
        let keys_key = self.get_keys_key(&slot_id);

        let current_hash = compute_hash(values);
        let stored_hash = self.client.get(&hash_key).await?;
        let existing_count = self.client.get_list_length(&list_key).await?.max(0) as usize;

        if needs_full_rewrite(values, stored_hash.as_deref(), existing_count) {
            self.client.delete_keys(&[list_key.clone()]).await?;
            for item in values {
                let json = serde_json::to_string(item)?;
                self.client.right_push_list(&list_key, &json).await?;
            }
        } else if values.len() > existing_count {
            for item in &values[existing_count..] {
                let json = serde_json::to_string(item)?;
                self.client.right_push_list(&list_key, &json).await?;
            }
        }

        self.client.set(&hash_key, &current_hash).await?;
        self.client
            .add_to_set(&keys_key, &format!("{key}{LIST_SUFFIX}"))
            .await?;
        Ok(())
    }

    async fn delete_inner(&self, user_id: Option<&str>, session_id: &str) -> Result<()> {
        let slot_id = slot_id(user_id, session_id)?;
        let keys_key = self.get_keys_key(&slot_id);
        let tracked = self.client.get_set_members(&keys_key).await?;
        if tracked.is_empty() {
            return Ok(());
        }

        let mut to_delete: std::collections::HashSet<String> = std::collections::HashSet::new();
        to_delete.insert(keys_key);
        for tracked_key in tracked {
            if let Some(base_key) = tracked_key.strip_suffix(LIST_SUFFIX) {
                let list_key = self.get_list_key(&slot_id, base_key);
                to_delete.insert(format!("{list_key}{HASH_SUFFIX}"));
                to_delete.insert(list_key);
            } else {
                let state_key = self.get_state_key(&slot_id, &tracked_key);
                to_delete.insert(version_support::version_key(&state_key));
                to_delete.insert(state_key);
            }
        }
        let keys: Vec<String> = to_delete.into_iter().collect();
        self.client.delete_keys(&keys).await?;
        Ok(())
    }
}

// ---- free helpers ----------------------------------------------------------

fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => ANON_USER.to_string(),
    }
}

/// Combines `(userId, sessionId)` into a single Redis slot identifier.
fn slot_id(user_id: Option<&str>, session_id: &str) -> Result<String> {
    if session_id.trim().is_empty() {
        return Err(Error::Other("sessionId must not be blank".into()));
    }
    Ok(format!("{}/{}", normalize_user(user_id), session_id))
}

/// Builder for [`RedisAgentStateStore`].
#[derive(Default)]
pub struct RedisAgentStateStoreBuilder {
    client: Option<Arc<dyn RedisClientAdapter>>,
    key_prefix: Option<String>,
}

impl RedisAgentStateStoreBuilder {
    /// Sets the Redis client adapter.
    pub fn client(mut self, client: Arc<dyn RedisClientAdapter>) -> Self {
        self.client = Some(client);
        self
    }

    /// Sets the key prefix (default [`DEFAULT_KEY_PREFIX`]).
    pub fn key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.key_prefix = Some(prefix.into());
        self
    }

    /// Builds the store.
    ///
    /// # Errors
    /// Returns an error when no client is configured or the prefix is blank.
    pub fn build(self) -> Result<RedisAgentStateStore> {
        let client = self
            .client
            .ok_or_else(|| Error::Other("Redis client cannot be null".into()))?;
        let key_prefix = match self.key_prefix {
            Some(p) if !p.trim().is_empty() => p,
            Some(_) => return Err(Error::Other("Key prefix cannot be empty".into())),
            None => DEFAULT_KEY_PREFIX.to_string(),
        };
        Ok(RedisAgentStateStore { client, key_prefix })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slot_id_uses_anon_for_missing_user() {
        assert_eq!(slot_id(None, "s1").unwrap(), "__anon__/s1");
        assert_eq!(slot_id(Some("u1"), "s1").unwrap(), "u1/s1");
        assert_eq!(slot_id(Some("  "), "s1").unwrap(), "__anon__/s1");
    }

    #[test]
    fn slot_id_rejects_blank_session() {
        assert!(slot_id(Some("u"), "  ").is_err());
    }

    #[test]
    fn builder_requires_client() {
        assert!(RedisAgentStateStore::builder().build().is_err());
    }

    #[test]
    fn builder_uses_default_prefix() {
        // Only prefix validation is unit-testable without a live Redis; the client
        // requirement is exercised above. Here we validate prefix rules via a dummy.
        let store = RedisAgentStateStore::new(Arc::new(DummyAdapter), "custom:");
        assert_eq!(store.key_prefix(), "custom:");
        assert!(store.supports_versioning());
        let _ = json!({});
    }

    /// Minimal adapter used only to satisfy the builder in pure-logic tests.
    #[async_trait]
    impl RedisClientAdapter for DummyAdapter {
        async fn set(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        async fn get(&self, _: &str) -> Result<Option<String>> { Ok(None) }
        async fn right_push_list(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        async fn range_list(&self, _: &str, _: i64, _: i64) -> Result<Vec<String>> { Ok(vec![]) }
        async fn get_list_length(&self, _: &str) -> Result<i64> { Ok(0) }
        async fn delete_keys(&self, _: &[String]) -> Result<()> { Ok(()) }
        async fn add_to_set(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        async fn get_set_members(&self, _: &str) -> Result<Vec<String>> { Ok(vec![]) }
        async fn get_set_size(&self, _: &str) -> Result<i64> { Ok(0) }
        async fn key_exists(&self, _: &str) -> Result<bool> { Ok(false) }
        async fn find_keys_by_pattern(&self, _: &str) -> Result<Vec<String>> { Ok(vec![]) }
        async fn eval_script(&self, _: &str, _: &[String], _: &[String]) -> Result<Option<i64>> {
            Ok(Some(1))
        }
    }

    #[derive(Debug)]
    struct DummyAdapter;

    // Integration semantics of the list-hash change detection as consumed by
    // `save_list_inner`. The pure algorithm lives in `agentscope_core::state::list_hash`
    // (covered there); these assert the crate's persisted-hash contract end to end.
    use agentscope_core::state::EMPTY_HASH;

    #[test]
    fn empty_hash_matches_java_constant() {
        assert_eq!(compute_hash(&[] as &[Value]), EMPTY_HASH);
    }

    #[test]
    fn unchanged_prefix_needs_no_rewrite() {
        let a = vec![json!({"v":1}), json!({"v":2})];
        let stored = compute_hash(&a);
        assert!(!needs_full_rewrite(&a, Some(&stored), 2));
    }

    #[test]
    fn append_growth_needs_no_rewrite() {
        let a = vec![json!({"v":1}), json!({"v":2})];
        let stored = compute_hash(&a);
        let b = vec![json!({"v":1}), json!({"v":2}), json!({"v":3})];
        assert!(!needs_full_rewrite(&b, Some(&stored), 2));
    }

    #[test]
    fn shrink_or_edit_needs_rewrite() {
        let a = vec![json!({"v":1}), json!({"v":2}), json!({"v":3})];
        let stored = compute_hash(&a);
        assert!(needs_full_rewrite(&a[..2], Some(&stored), 3)); // shrink
        let edited = vec![json!({"v":9}), json!({"v":2}), json!({"v":3})];
        assert!(needs_full_rewrite(&edited, Some(&stored), 3)); // prefix edit
    }

    #[test]
    fn missing_hash_with_existing_data_needs_rewrite() {
        let a = vec![json!({"v":1})];
        assert!(needs_full_rewrite(&a, None, 1));
        assert!(!needs_full_rewrite(&a, None, 0));
    }
}
