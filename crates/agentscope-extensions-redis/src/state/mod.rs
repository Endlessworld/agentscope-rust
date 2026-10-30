//! Redis-backed [`AgentStateStore`], mirroring
//! `io.agentscope.extensions.redis.state.RedisAgentStateStore`.
//!
//! ## Key layout (per `slot` = `{userId}/{sessionId}` for V0, `{{userId}/{sessionId}}` for V1)
//!
//! * single state: `{prefix}{slot}:{key}` — Redis String containing JSON
//! * single version: `{prefix}{slot}:{key}:ver` — monotonic counter String
//! * list state: `{prefix}{slot}:{key}:list` — Redis List of JSON items
//! * list hash: `{prefix}{slot}:{key}:list:_hash` — change-detection String
//! * session key index: `{prefix}{slot}:_keys` — Redis Set of state keys
//!
//! See [`KeyLayoutVersion`] / [`key_layout`] for the V0/V1 difference: V1 wraps the session
//! segment in a Redis hash tag so that all keys of one session land in a single Redis Cluster
//! slot (the CAS Lua script touches three keys at once).
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
pub mod key_layout;
pub(crate) mod redirect;
pub(crate) mod version_support;

use self::adapter::RedisClientAdapter;
use self::key_layout::{
    parse_v0_session_id_from_keys_key, parse_v1_session_id_from_keys_key, v0_keys_pattern,
    v1_keys_pattern, validate_v1_key_prefix, validate_v1_user_id, RedisAgentStateKeyLayout,
};
use self::version_support::{SAVE_SCRIPT, UNCONDITIONAL};

pub use self::key_layout::KeyLayoutVersion;

/// Default key prefix (matches Java `DEFAULT_KEY_PREFIX`).
pub const DEFAULT_KEY_PREFIX: &str = "agentscope:session:";

const HASH_SUFFIX: &str = ":_hash";

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
    key_layout_version: KeyLayoutVersion,
}

impl std::fmt::Debug for RedisAgentStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisAgentStateStore")
            .field("key_prefix", &self.key_prefix)
            .field("key_layout_version", &self.key_layout_version)
            .finish_non_exhaustive()
    }
}

impl RedisAgentStateStore {
    /// Creates a builder.
    pub fn builder() -> RedisAgentStateStoreBuilder {
        RedisAgentStateStoreBuilder::default()
    }

    /// Creates a store from an adapter with an explicit key prefix.
    ///
    /// Uses the default [`KeyLayoutVersion::V0`] layout; use
    /// [`RedisAgentStateStoreBuilder::key_layout_version`] to select V1 for Redis Cluster.
    pub fn new(client: Arc<dyn RedisClientAdapter>, key_prefix: impl Into<String>) -> Self {
        Self {
            client,
            key_prefix: key_prefix.into(),
            key_layout_version: KeyLayoutVersion::default(),
        }
    }

    /// The configured key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// The configured key layout version.
    pub fn key_layout_version(&self) -> KeyLayoutVersion {
        self.key_layout_version
    }

    /// Resolves the key layout for one session, validating V1 identifiers as Java does.
    fn resolve_key_layout(
        &self,
        user_id: Option<&str>,
        session_id: &str,
    ) -> Result<RedisAgentStateKeyLayout> {
        match self.key_layout_version {
            KeyLayoutVersion::V0 => {
                RedisAgentStateKeyLayout::v0(&self.key_prefix, user_id, session_id)
            }
            KeyLayoutVersion::V1 => {
                RedisAgentStateKeyLayout::v1(&self.key_prefix, user_id, session_id)
            }
        }
    }

    /// Whether this store supports optimistic-concurrency versioning (always `true`).
    pub fn supports_versioning(&self) -> bool {
        true
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
        let layout = self.resolve_key_layout(user_id, session_id)?;
        let redis_key = layout.state_key(key);
        let version_key = version_support::version_key(&redis_key);
        let keys_key = layout.keys_key();
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
            _ => Err(Error::Other(format!(
                "Version conflict saving state: {key}"
            ))),
        }
    }

    /// Reads a versioned value, mirroring Java `getVersioned`.
    pub async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<VersionedState> {
        let layout = self.resolve_key_layout(user_id, session_id)?;
        let redis_key = layout.state_key(key);
        let version_key = version_support::version_key(&redis_key);

        let json = self.client.get(&redis_key).await?;
        let Some(json) = json else {
            return Ok(VersionedState {
                value: None,
                version: 0,
            });
        };
        let version_str = self.client.get(&version_key).await?;
        let version = version_support::parse_version(true, version_str.as_deref());
        let value = serde_json::from_str(&json)?;
        Ok(VersionedState {
            value: Some(value),
            version,
        })
    }

    /// Conditional save (compare-and-swap), mirroring Java `saveIfVersion`.
    ///
    /// `expected_version` is a real CAS version; `0` means "create if absent" (the stored
    /// version must still be `0`). Pass [`UNCONDITIONAL_EXPECTED`] to overwrite without a
    /// version check.
    ///
    /// Returns the new version (always `>= 1`) on success, or `0` on conflict. `0` is never
    /// a valid success value because every write bumps the version.
    pub async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: &Value,
        expected_version: u64,
    ) -> Result<u64> {
        if expected_version == UNCONDITIONAL_EXPECTED {
            return self
                .save_versioned(user_id, session_id, key, value, UNCONDITIONAL_EXPECTED)
                .await;
        }
        let layout = self.resolve_key_layout(user_id, session_id)?;
        let redis_key = layout.state_key(key);
        let version_key = version_support::version_key(&redis_key);
        let keys_key = layout.keys_key();
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
    async fn save(&self, user_id: Option<&str>, session_id: &str, key: &str, value: Value) {
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
        let layout = self.resolve_key_layout(user_id, session_id).ok()?;
        let redis_key = layout.state_key(key);
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
        let Ok(layout) = self.resolve_key_layout(user_id, session_id) else {
            return Vec::new();
        };
        let redis_key = layout.list_key(key);
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
        let Ok(layout) = self.resolve_key_layout(user_id, session_id) else {
            return false;
        };
        let keys_key = layout.keys_key();
        match self.client.key_exists(&keys_key).await {
            Ok(true) => self
                .client
                .get_set_size(&keys_key)
                .await
                .map(|n| n > 0)
                .unwrap_or(false),
            _ => false,
        }
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if let Err(e) = self.delete_inner(user_id, session_id).await {
            tracing::error!("RedisAgentStateStore::delete failed: {e}");
        }
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let user_segment = key_layout::normalize_user(user_id);
        let pattern = match self.key_layout_version {
            KeyLayoutVersion::V0 => v0_keys_pattern(&self.key_prefix, &user_segment),
            KeyLayoutVersion::V1 => {
                if let Err(e) = validate_v1_user_id(&user_segment) {
                    tracing::error!("RedisAgentStateStore::list_session_ids failed: {e}");
                    return Vec::new();
                }
                v1_keys_pattern(&self.key_prefix, &user_segment)
            }
        };
        let keys = match self.client.find_keys_by_pattern(&pattern).await {
            Ok(k) => k,
            Err(e) => {
                tracing::error!("RedisAgentStateStore::list_session_ids failed: {e}");
                return Vec::new();
            }
        };
        let mut sessions = std::collections::HashSet::new();
        for keys_key in keys {
            let parsed = match self.key_layout_version {
                KeyLayoutVersion::V0 => {
                    parse_v0_session_id_from_keys_key(&keys_key, &self.key_prefix, &user_segment)
                }
                KeyLayoutVersion::V1 => {
                    parse_v1_session_id_from_keys_key(&keys_key, &self.key_prefix, &user_segment)
                }
            };
            if let Some(session_id) = parsed {
                sessions.insert(session_id);
            }
        }
        let mut out: Vec<String> = sessions.into_iter().collect();
        out.sort();
        out
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
        match RedisAgentStateStore::get_versioned(self, user_id, session_id, key).await {
            Ok(v) => agentscope_core::state::VersionedState {
                value: v.value,
                version: i64::try_from(v.version).unwrap_or(i64::MAX),
            },
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
        value: Value,
        expected_version: i64,
    ) -> i64 {
        // Core contract: `UNVERSIONED` (-1) is the unconditional sentinel and `0` is a real
        // CAS version meaning "create if absent". The u64-backed implementation uses
        // `UNCONDITIONAL_EXPECTED` (u64::MAX) for the former, so the sentinels never collide.
        // Successful writes always bump to >= 1, so `Ok(0)` unambiguously means "conflict".
        let expected = if expected_version == agentscope_core::state::UNVERSIONED {
            UNCONDITIONAL_EXPECTED
        } else if expected_version < 0 {
            return agentscope_core::state::UNVERSIONED;
        } else {
            expected_version as u64
        };
        match RedisAgentStateStore::save_if_version(
            self, user_id, session_id, key, &value, expected,
        )
        .await
        {
            Ok(0) => agentscope_core::state::UNVERSIONED,
            Ok(v) => i64::try_from(v).unwrap_or(i64::MAX),
            Err(_) => agentscope_core::state::UNVERSIONED,
        }
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
        let layout = self.resolve_key_layout(user_id, session_id)?;
        let list_key = layout.list_key(key);
        let hash_key = format!("{list_key}{HASH_SUFFIX}");
        let keys_key = layout.keys_key();

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
            .add_to_set(&keys_key, &layout.list_track_key(key))
            .await?;
        Ok(())
    }

    async fn delete_inner(&self, user_id: Option<&str>, session_id: &str) -> Result<()> {
        let layout = self.resolve_key_layout(user_id, session_id)?;
        let keys_key = layout.keys_key();
        let tracked = self.client.get_set_members(&keys_key).await?;
        if tracked.is_empty() {
            return Ok(());
        }

        let mut to_delete: std::collections::HashSet<String> = std::collections::HashSet::new();
        to_delete.insert(keys_key);
        for tracked_key in tracked {
            if RedisAgentStateKeyLayout::is_list_track_key(&tracked_key) {
                let base_key = RedisAgentStateKeyLayout::base_key_from_list_track_key(&tracked_key);
                let list_key = layout.list_key(base_key);
                to_delete.insert(format!("{list_key}{HASH_SUFFIX}"));
                to_delete.insert(list_key);
            } else {
                let state_key = layout.state_key(&tracked_key);
                to_delete.insert(version_support::version_key(&state_key));
                to_delete.insert(state_key);
            }
        }
        // Delete one key per command: a tracked member may reference an unknown slot, and a
        // single multi-key `DEL` would be rejected with CROSSSLOT on Redis Cluster.
        for key in to_delete {
            self.client.delete_keys(&[key]).await?;
        }
        Ok(())
    }
}

// ---- free helpers ----------------------------------------------------------

/// Builder for [`RedisAgentStateStore`].
#[derive(Default)]
pub struct RedisAgentStateStoreBuilder {
    client: Option<Arc<dyn RedisClientAdapter>>,
    key_prefix: Option<String>,
    key_layout_version: KeyLayoutVersion,
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

    /// Selects the agent-state key layout.
    ///
    /// V0 is the default for compatibility. V1 must be selected explicitly for Redis Cluster
    /// and used consistently by every store instance sharing a key prefix.
    pub fn key_layout_version(mut self, version: KeyLayoutVersion) -> Self {
        self.key_layout_version = version;
        self
    }

    /// Builds the store.
    ///
    /// # Errors
    /// Returns an error when no client is configured, the prefix is blank, or a V1 prefix
    /// contains a Redis hash-tag brace.
    pub fn build(self) -> Result<RedisAgentStateStore> {
        let client = self
            .client
            .ok_or_else(|| Error::Other("Redis client cannot be null".into()))?;
        let key_prefix = match self.key_prefix {
            Some(p) if !p.trim().is_empty() => p,
            Some(_) => return Err(Error::Other("Key prefix cannot be empty".into())),
            None => DEFAULT_KEY_PREFIX.to_string(),
        };
        if self.key_layout_version == KeyLayoutVersion::V1 {
            validate_v1_key_prefix(&key_prefix)?;
        }
        Ok(RedisAgentStateStore {
            client,
            key_prefix,
            key_layout_version: self.key_layout_version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::key_layout::ANON_USER;
    use crate::test_support::MemoryRedisAdapter;
    use agentscope_core::state::EMPTY_HASH;
    use serde_json::json;

    const PREFIX: &str = "t:session:";

    fn store() -> (RedisAgentStateStore, Arc<MemoryRedisAdapter>) {
        let adapter = Arc::new(MemoryRedisAdapter::new());
        let store = RedisAgentStateStore::builder()
            .client(adapter.clone())
            .key_prefix(PREFIX)
            .build()
            .expect("builder accepts the adapter");
        (store, adapter)
    }

    fn v1_store() -> (RedisAgentStateStore, Arc<MemoryRedisAdapter>) {
        let adapter = Arc::new(MemoryRedisAdapter::new());
        let store = RedisAgentStateStore::builder()
            .client(adapter.clone())
            .key_prefix(PREFIX)
            .key_layout_version(KeyLayoutVersion::V1)
            .build()
            .expect("builder accepts the adapter");
        (store, adapter)
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn slot_id_uses_anon_for_missing_user() {
        let anon = RedisAgentStateKeyLayout::v0(PREFIX, None, "s1").unwrap();
        assert_eq!(anon.slot_id(), "__anon__/s1");
        let named = RedisAgentStateKeyLayout::v0(PREFIX, Some("u1"), "s1").unwrap();
        assert_eq!(named.slot_id(), "u1/s1");
        let blank = RedisAgentStateKeyLayout::v0(PREFIX, Some("  "), "s1").unwrap();
        assert_eq!(blank.slot_id(), "__anon__/s1");
    }

    #[test]
    fn slot_id_rejects_blank_session() {
        assert!(RedisAgentStateKeyLayout::v0(PREFIX, Some("u"), "  ").is_err());
    }

    #[test]
    fn builder_requires_client_and_rejects_blank_prefix() {
        assert!(RedisAgentStateStore::builder().build().is_err());
        let adapter = Arc::new(MemoryRedisAdapter::new());
        assert!(RedisAgentStateStore::builder()
            .client(adapter)
            .key_prefix("   ")
            .build()
            .is_err());
    }

    #[test]
    fn builder_uses_default_prefix_and_reports_versioning() {
        let adapter = Arc::new(MemoryRedisAdapter::new());
        let store = RedisAgentStateStore::builder()
            .client(adapter)
            .build()
            .unwrap();
        assert_eq!(store.key_prefix(), DEFAULT_KEY_PREFIX);
        assert!(store.supports_versioning());
        assert_eq!(DEFAULT_KEY_PREFIX, "agentscope:session:");
    }

    #[test]
    fn key_layout_matches_java() {
        let v0 = RedisAgentStateKeyLayout::v0(PREFIX, Some("u"), "s").unwrap();
        assert_eq!(v0.state_key("k"), "t:session:u/s:k");
        assert_eq!(v0.list_key("k"), "t:session:u/s:k:list");
        assert_eq!(v0.keys_key(), "t:session:u/s:_keys");
        assert_eq!(ANON_USER, "__anon__");
        assert_eq!(crate::state::key_layout::LIST_SUFFIX, ":list");
        assert_eq!(HASH_SUFFIX, ":_hash");
        assert_eq!(crate::state::key_layout::KEYS_SUFFIX, ":_keys");
    }

    // ---- single-value save / get / exists / delete -----------------------

    #[tokio::test]
    async fn save_then_get_round_trips_json() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!({"n": 1})).await;

        assert_eq!(
            store.get(Some("u1"), "s1", "k").await,
            Some(json!({"n": 1}))
        );
        assert_eq!(
            adapter.string_of("t:session:u1/s1:k"),
            Some(r#"{"n":1}"#.to_string())
        );
        assert_eq!(
            adapter.string_of("t:session:u1/s1:k:ver"),
            Some("1".to_string())
        );
        assert_eq!(
            adapter.set_members("t:session:u1/s1:_keys"),
            vec!["k".to_string()]
        );
    }

    #[tokio::test]
    async fn save_is_unconditional_and_bumps_version() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        store.save(Some("u1"), "s1", "k", json!(2)).await;
        assert_eq!(store.get(Some("u1"), "s1", "k").await, Some(json!(2)));
        assert_eq!(
            adapter.string_of("t:session:u1/s1:k:ver"),
            Some("2".to_string())
        );
        assert_eq!(adapter.eval_calls(), 2);
    }

    #[tokio::test]
    async fn get_missing_key_is_none() {
        let (store, adapter) = store();
        assert_eq!(store.get(Some("u1"), "s1", "nope").await, None);
        assert_eq!(adapter.eval_calls(), 0, "a read issues no script");
    }

    #[tokio::test]
    async fn get_with_blank_session_is_none() {
        let (store, _adapter) = store();
        assert_eq!(store.get(Some("u1"), "  ", "k").await, None);
    }

    #[tokio::test]
    async fn anonymous_user_uses_anon_slot() {
        let (store, adapter) = store();
        store.save(None, "s1", "k", json!(1)).await;
        assert!(adapter.string_of("t:session:__anon__/s1:k").is_some());
    }

    // ---- V1 cluster-safe layout -----------------------------------------

    #[test]
    fn builder_records_the_configured_layout_version() {
        let (v0, _) = store();
        assert_eq!(
            v0.key_layout_version(),
            KeyLayoutVersion::V0,
            "V0 is the default"
        );
        let (v1, _) = v1_store();
        assert_eq!(v1.key_layout_version(), KeyLayoutVersion::V1);
    }

    #[test]
    fn builder_rejects_a_v1_prefix_containing_a_hash_tag_brace() {
        for prefix in ["scope{bad:", "scope}bad:"] {
            let adapter = Arc::new(MemoryRedisAdapter::new());
            let err = RedisAgentStateStore::builder()
                .client(adapter)
                .key_prefix(prefix)
                .key_layout_version(KeyLayoutVersion::V1)
                .build()
                .unwrap_err();
            assert!(err.to_string().contains("not supported by V1"), "{err}");
        }
    }

    #[tokio::test]
    async fn v1_save_writes_every_key_under_the_same_hash_tag() {
        let (store, adapter) = v1_store();
        store
            .save(Some("u1"), "s1", "agent_state", json!({"n": 1}))
            .await;
        store
            .save_list(Some("u1"), "s1", "messages", &[json!("m")])
            .await;

        assert_eq!(
            store.get(Some("u1"), "s1", "agent_state").await,
            Some(json!({"n": 1}))
        );
        assert_eq!(
            adapter.string_of("t:session:{u1/s1}:agent_state"),
            Some(r#"{"n":1}"#.to_string())
        );
        assert_eq!(
            adapter.string_of("t:session:{u1/s1}:agent_state:ver"),
            Some("1".to_string())
        );
        assert_eq!(
            adapter.string_of("t:session:{u1/s1}:messages:list:_hash"),
            Some(compute_hash(&[json!("m")]))
        );
        assert_eq!(
            adapter.list("t:session:{u1/s1}:messages:list"),
            vec![r#""m""#.to_string()]
        );
        assert_eq!(
            adapter.set_members("t:session:{u1/s1}:_keys"),
            vec!["agent_state".to_string(), "messages:list".to_string()]
        );
        assert!(
            adapter
                .all_keys()
                .iter()
                .all(|k| k.starts_with("t:session:{u1/s1}")),
            "every key of one session shares the hash tag: {:?}",
            adapter.all_keys()
        );
    }

    #[tokio::test]
    async fn v1_cas_keeps_three_keys_in_one_hash_tag() {
        let (store, adapter) = v1_store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        let version = store.get_versioned(Some("u1"), "s1", "k").await.unwrap();
        assert_eq!(version.version, 1);
        assert_eq!(
            store
                .save_if_version(Some("u1"), "s1", "k", &json!(2), version.version)
                .await
                .unwrap(),
            2
        );
        assert_eq!(store.get(Some("u1"), "s1", "k").await, Some(json!(2)));

        // The three keys passed to the CAS Lua script must agree on the hash tag, otherwise a
        // Redis Cluster rejects the EVAL with CROSSSLOT.
        let tag = |key: &str| {
            let start = key.find('{').map(|i| i + 1).unwrap();
            let end = start + key[start..].find('}').unwrap();
            key[start..end].to_string()
        };
        let keys = [
            "t:session:{u1/s1}:k",
            "t:session:{u1/s1}:k:ver",
            "t:session:{u1/s1}:_keys",
        ];
        assert!(keys
            .iter()
            .all(|k| adapter.string_of(k).is_some() || adapter.set_members(k).len() == 1));
        assert!(keys.iter().all(|k| tag(k) == "u1/s1"), "{keys:?}");
    }

    #[tokio::test]
    async fn v1_delete_tracks_and_removes_only_hash_tagged_keys() {
        let (store, adapter) = v1_store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        store.save_list(Some("u1"), "s1", "l", &[json!(1)]).await;

        store.delete(Some("u1"), "s1").await;

        assert!(adapter.all_keys().is_empty(), "{:?}", adapter.all_keys());
        assert!(!store.exists(Some("u1"), "s1").await);
    }

    #[tokio::test]
    async fn v1_list_session_ids_strips_the_hash_tag() {
        let (store, _adapter) = v1_store();
        store.save(Some("u1"), "beta", "k", json!(1)).await;
        store.save(Some("u1"), "alpha", "k", json!(1)).await;
        store.save(Some("u2"), "gamma", "k", json!(1)).await;

        assert_eq!(
            store.list_session_ids(Some("u1")).await,
            vec!["alpha", "beta"]
        );
        assert_eq!(store.list_session_ids(Some("u2")).await, vec!["gamma"]);
        assert!(store.list_session_ids(Some("u3")).await.is_empty());
    }

    #[tokio::test]
    async fn v1_and_v0_are_disjoint_namespaces() {
        // A V1 store never sees V0 data, and vice versa.
        let (v0, adapter) = store();
        let v1 = RedisAgentStateStore::builder()
            .client(adapter.clone())
            .key_prefix(PREFIX)
            .key_layout_version(KeyLayoutVersion::V1)
            .build()
            .unwrap();

        v0.save(Some("u1"), "s1", "k", json!("v0")).await;
        assert_eq!(v1.get(Some("u1"), "s1", "k").await, None);
        v1.save(Some("u1"), "s1", "k", json!("v1")).await;
        assert_eq!(v0.get(Some("u1"), "s1", "k").await, Some(json!("v0")));
        assert_eq!(v1.get(Some("u1"), "s1", "k").await, Some(json!("v1")));
    }

    #[tokio::test]
    async fn v1_rejects_unsafe_identifiers_before_redis_io() {
        let (store, adapter) = v1_store();

        assert!(store
            .get_versioned(Some("bad/user"), "s1", "k")
            .await
            .is_err());
        store.get(Some("bad{user"), "s1", "k").await;
        store.save(Some("bad}user"), "s1", "k", json!(1)).await;
        store
            .save_list(Some("u1"), "bad/session", "k", &[json!(1)])
            .await;
        store
            .save_list(Some("u1"), "bad{session", "k", &[json!(1)])
            .await;
        store.delete(Some("bad/user"), "s1").await;
        assert!(store.list_session_ids(Some("bad/user")).await.is_empty());

        assert_eq!(adapter.eval_calls(), 0, "no CAS script reached Redis");
        assert_eq!(adapter.delete_calls(), 0, "no DEL reached Redis");
        assert!(adapter.all_keys().is_empty());
    }

    #[tokio::test]
    async fn v1_delete_issues_one_key_per_command() {
        // A tracked member may reference an unknown slot, and a multi-key DEL is rejected with
        // CROSSSLOT on Redis Cluster, so every deletion must be a single-key command.
        let (store, adapter) = v1_store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        store.save_list(Some("u1"), "s1", "l", &[json!(1)]).await;

        store.delete(Some("u1"), "s1").await;

        assert!(
            adapter.delete_calls() >= 4,
            "each tracked key is deleted on its own"
        );
        assert_eq!(adapter.max_delete_batch(), 1, "never a multi-key DEL");
        assert!(adapter.all_keys().is_empty());
    }

    #[tokio::test]
    async fn v0_delete_also_issues_one_key_per_command() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        store.save_list(Some("u1"), "s1", "l", &[json!(1)]).await;

        store.delete(Some("u1"), "s1").await;

        assert_eq!(adapter.max_delete_batch(), 1, "never a multi-key DEL");
        assert!(adapter.all_keys().is_empty());
    }

    #[tokio::test]
    async fn corrupt_json_is_ignored_by_get() {
        let (store, adapter) = store();
        adapter.seed_string("t:session:u1/s1:k", "{not json");
        assert_eq!(store.get(Some("u1"), "s1", "k").await, None);
    }

    #[tokio::test]
    async fn exists_reflects_tracked_keys() {
        let (store, _adapter) = store();
        assert!(!store.exists(Some("u1"), "s1").await);
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        assert!(store.exists(Some("u1"), "s1").await);
        assert!(
            !store.exists(Some("u1"), "other").await,
            "per-slot tracking"
        );
        assert!(!store.exists(Some("u1"), "  ").await, "blank session");
    }

    #[tokio::test]
    async fn delete_removes_payload_version_and_tracking() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2)])
            .await;

        store.delete(Some("u1"), "s1").await;

        assert_eq!(store.get(Some("u1"), "s1", "k").await, None);
        assert!(adapter.string_of("t:session:u1/s1:k").is_none());
        assert!(adapter.string_of("t:session:u1/s1:k:ver").is_none());
        assert!(adapter.string_of("t:session:u1/s1:l:list:_hash").is_none());
        assert!(adapter.list("t:session:u1/s1:l:list").is_empty());
        assert!(adapter.string_of("t:session:u1/s1:_keys").is_none());
        assert!(!store.exists(Some("u1"), "s1").await);
    }

    #[tokio::test]
    async fn delete_on_untracked_slot_is_a_noop() {
        let (store, adapter) = store();
        store.delete(Some("u1"), "s1").await;
        assert_eq!(adapter.delete_calls(), 0, "no DEL issued for an empty slot");
        store.delete(Some("u1"), "  ").await;
        assert_eq!(adapter.delete_calls(), 0);
    }

    // ---- list save / get -------------------------------------------------

    #[tokio::test]
    async fn save_list_writes_items_hash_and_tracking() {
        let (store, adapter) = store();
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2)])
            .await;

        assert_eq!(
            store.get_list(Some("u1"), "s1", "l").await,
            vec![json!(1), json!(2)]
        );
        assert_eq!(
            adapter.list("t:session:u1/s1:l:list"),
            vec!["1".to_string(), "2".to_string()]
        );
        let expected_hash = compute_hash(&[json!(1), json!(2)]);
        assert_eq!(
            adapter.string_of("t:session:u1/s1:l:list:_hash"),
            Some(expected_hash)
        );
        assert_eq!(
            adapter.set_members("t:session:u1/s1:_keys"),
            vec!["l:list".to_string()]
        );
    }

    #[tokio::test]
    async fn save_list_first_write_does_not_delete_for_empty_prefix() {
        // core Case 3: an absent hash with no existing data means "no rewrite", so the
        // very first write is a pure append (no DEL + full re-push).
        let (store, adapter) = store();
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2)])
            .await;
        assert_eq!(adapter.delete_calls(), 0, "no full rewrite on a fresh slot");
        assert_eq!(adapter.push_calls(), 2, "exactly one RPUSH per item");
    }

    #[tokio::test]
    async fn save_list_unchanged_prefix_is_a_noop() {
        let (store, adapter) = store();
        let values = vec![json!(1), json!(2)];
        store.save_list(Some("u1"), "s1", "l", &values).await;
        assert_eq!(adapter.push_calls(), 2);

        // Identical list again: hash matches the stored prefix, length unchanged.
        store.save_list(Some("u1"), "s1", "l", &values).await;
        assert_eq!(adapter.push_calls(), 2, "no additional RPUSH");
        assert_eq!(adapter.delete_calls(), 0, "no full rewrite");
        assert_eq!(store.get_list(Some("u1"), "s1", "l").await, values);
    }

    #[tokio::test]
    async fn save_list_append_only_pushes_the_suffix() {
        let (store, adapter) = store();
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2)])
            .await;
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2), json!(3)])
            .await;

        assert_eq!(adapter.push_calls(), 3, "only the appended item is pushed");
        assert_eq!(adapter.delete_calls(), 0);
        assert_eq!(
            store.get_list(Some("u1"), "s1", "l").await,
            vec![json!(1), json!(2), json!(3)]
        );
    }

    #[tokio::test]
    async fn save_list_prefix_rewrite_triggers_full_rewrite() {
        let (store, adapter) = store();
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2)])
            .await;
        let pushes_before = adapter.push_calls();

        // Same length, different first element: the prefix hash changes -> full rewrite.
        store
            .save_list(Some("u1"), "s1", "l", &[json!(9), json!(2)])
            .await;

        assert_eq!(
            adapter.delete_calls(),
            1,
            "list key deleted before rewriting"
        );
        assert_eq!(
            adapter.push_calls(),
            pushes_before + 2,
            "whole list re-pushed"
        );
        assert_eq!(
            store.get_list(Some("u1"), "s1", "l").await,
            vec![json!(9), json!(2)]
        );
    }

    #[tokio::test]
    async fn save_list_shrink_triggers_full_rewrite() {
        let (store, adapter) = store();
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2), json!(3)])
            .await;
        let pushes_before = adapter.push_calls();

        store.save_list(Some("u1"), "s1", "l", &[json!(1)]).await;

        assert_eq!(adapter.delete_calls(), 1);
        assert_eq!(
            adapter.push_calls(),
            pushes_before + 1,
            "only the surviving item"
        );
        assert_eq!(store.get_list(Some("u1"), "s1", "l").await, vec![json!(1)]);
    }

    #[tokio::test]
    async fn save_list_with_legacy_data_and_no_hash_rewrites() {
        // Pre-existing list data with no hash marker -> needs_full_rewrite (case 2).
        let (store, adapter) = store();
        adapter.seed_list("t:session:u1/s1:l:list", &["1", "2"]);

        store.save_list(Some("u1"), "s1", "l", &[json!(7)]).await;

        assert_eq!(
            adapter.delete_calls(),
            1,
            "legacy data is replaced wholesale"
        );
        assert_eq!(store.get_list(Some("u1"), "s1", "l").await, vec![json!(7)]);
    }

    #[tokio::test]
    async fn save_list_empty_values_writes_empty_hash_constant() {
        let (store, adapter) = store();
        store.save_list(Some("u1"), "s1", "l", &[]).await;
        assert_eq!(
            adapter.string_of("t:session:u1/s1:l:list:_hash"),
            Some(EMPTY_HASH.to_string())
        );
        assert!(store.get_list(Some("u1"), "s1", "l").await.is_empty());
    }

    #[tokio::test]
    async fn get_list_skips_corrupt_items() {
        let (store, adapter) = store();
        adapter.seed_list("t:session:u1/s1:l:list", &["1", "{bad", "2"]);
        assert_eq!(
            store.get_list(Some("u1"), "s1", "l").await,
            vec![json!(1), json!(2)]
        );
        assert!(store.get_list(Some("u1"), "  ", "l").await.is_empty());
    }

    #[tokio::test]
    async fn save_list_rejects_blank_session_without_writing() {
        // `save_list` swallows errors (trait contract): nothing must be written.
        let (store, adapter) = store();
        store.save_list(Some("u1"), "  ", "l", &[json!(1)]).await;
        assert_eq!(adapter.push_calls(), 0);
        assert_eq!(adapter.eval_calls(), 0);
    }

    // ---- versioning / CAS ------------------------------------------------

    #[tokio::test]
    async fn supports_versioning_is_true() {
        let (store, _adapter) = store();
        assert!(store.supports_versioning());
    }

    #[tokio::test]
    async fn get_versioned_missing_key_is_zero() {
        let (store, _adapter) = store();
        let versioned = store.get_versioned(Some("u1"), "s1", "k").await.unwrap();
        assert_eq!(
            versioned,
            VersionedState {
                value: None,
                version: 0
            }
        );
    }

    #[tokio::test]
    async fn get_versioned_returns_payload_and_version() {
        let (store, _adapter) = store();
        store.save(Some("u1"), "s1", "k", json!({"a": 1})).await;
        store.save(Some("u1"), "s1", "k", json!({"a": 2})).await;

        let versioned = store.get_versioned(Some("u1"), "s1", "k").await.unwrap();
        assert_eq!(versioned.value, Some(json!({"a": 2})));
        assert_eq!(versioned.version, 2);
    }

    #[tokio::test]
    async fn save_if_version_zero_creates_only_when_absent() {
        let (store, _adapter) = store();
        // Core contract: 0 == create-if-absent (mirrors Java `saveIfVersion(..., 0)`).
        assert_eq!(
            store
                .save_if_version(Some("u1"), "s1", "k", &json!(1), 0)
                .await
                .unwrap(),
            1
        );
        // The key now exists at version 1, so another 0-expectation is a conflict.
        assert_eq!(
            store
                .save_if_version(Some("u1"), "s1", "k", &json!(2), 0)
                .await
                .unwrap(),
            0
        );
        assert_eq!(store.get(Some("u1"), "s1", "k").await, Some(json!(1)));
    }

    #[tokio::test]
    async fn save_if_version_unconditional_sentinel_always_writes() {
        let (store, _adapter) = store();
        // u64::MAX is the in-crate unconditional sentinel, never a real version.
        assert_eq!(
            store
                .save_if_version(Some("u1"), "s1", "k", &json!(1), UNCONDITIONAL_EXPECTED)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .save_if_version(Some("u1"), "s1", "k", &json!(2), UNCONDITIONAL_EXPECTED)
                .await
                .unwrap(),
            2
        );
        assert_eq!(store.get(Some("u1"), "s1", "k").await, Some(json!(2)));
    }

    #[tokio::test]
    async fn save_if_version_matching_version_succeeds() {
        let (store, _adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await; // version 1
        let new_version = store
            .save_if_version(Some("u1"), "s1", "k", &json!(2), 1)
            .await
            .unwrap();
        assert_eq!(new_version, 2);
        assert_eq!(store.get(Some("u1"), "s1", "k").await, Some(json!(2)));
    }

    #[tokio::test]
    async fn save_if_version_conflict_returns_zero_and_leaves_state() {
        let (store, _adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await; // version 1
        let result = store
            .save_if_version(Some("u1"), "s1", "k", &json!(99), 7)
            .await
            .unwrap();
        assert_eq!(result, 0, "conflict is signalled as version 0");
        assert_eq!(store.get(Some("u1"), "s1", "k").await, Some(json!(1)));
    }

    // ---- session enumeration ---------------------------------------------

    #[tokio::test]
    async fn list_session_ids_returns_sorted_unique_sessions() {
        let (store, _adapter) = store();
        store.save(Some("u1"), "beta", "k", json!(1)).await;
        store.save(Some("u1"), "alpha", "k", json!(1)).await;
        store.save(Some("u2"), "gamma", "k", json!(1)).await;

        assert_eq!(
            store.list_session_ids(Some("u1")).await,
            vec!["alpha", "beta"]
        );
        assert_eq!(store.list_session_ids(Some("u2")).await, vec!["gamma"]);
        assert!(store.list_session_ids(Some("u3")).await.is_empty());
    }

    #[tokio::test]
    async fn list_session_ids_handles_anonymous_slots() {
        let (store, _adapter) = store();
        store.save(None, "anon-session", "k", json!(1)).await;
        assert_eq!(store.list_session_ids(None).await, vec!["anon-session"]);
        assert_eq!(
            store.list_session_ids(Some("  ")).await,
            vec!["anon-session"]
        );
    }

    #[tokio::test]
    async fn list_session_ids_is_not_confused_by_prefix_collisions() {
        // `u1` must not pick up `u10`'s slot.
        let (store, _adapter) = store();
        store.save(Some("u1"), "mine", "k", json!(1)).await;
        store.save(Some("u10"), "theirs", "k", json!(1)).await;
        assert_eq!(store.list_session_ids(Some("u1")).await, vec!["mine"]);
    }

    // ---- maintenance -----------------------------------------------------

    #[tokio::test]
    async fn clear_all_sessions_counts_and_removes_everything() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        store.save_list(Some("u1"), "s1", "l", &[json!(1)]).await;
        store.save(Some("u2"), "s2", "k", json!(1)).await;
        assert!(!adapter.all_keys().is_empty());

        let removed = store.clear_all_sessions().await.unwrap();
        assert!(removed > 0, "reported a nonzero key count, got {removed}");
        assert!(adapter.all_keys().is_empty(), "every prefixed key deleted");
        assert!(store.list_session_ids(Some("u1")).await.is_empty());
    }

    #[tokio::test]
    async fn clear_all_sessions_is_a_noop_on_an_empty_store() {
        let (store, adapter) = store();
        assert_eq!(store.clear_all_sessions().await.unwrap(), 0);
        assert_eq!(adapter.delete_calls(), 0);
    }

    #[tokio::test]
    async fn list_session_ids_aggregates_every_scan_page() {
        // A page size of 2 over 5 slots (each contributing payload+version+keys entries)
        // forces a genuine multi-page cursor walk: stopping after the first page would
        // silently drop most sessions.
        let adapter = Arc::new(MemoryRedisAdapter::with_scan_page_size(2));
        let store = RedisAgentStateStore::builder()
            .client(adapter.clone())
            .key_prefix(PREFIX)
            .build()
            .expect("builder accepts the paged adapter");
        for session in ["s1", "s2", "s3", "s4", "s5"] {
            store.save(Some("u1"), session, "k", json!(1)).await;
        }

        let ids = store.list_session_ids(Some("u1")).await;

        assert_eq!(ids, vec!["s1", "s2", "s3", "s4", "s5"]);
        let cursors = adapter.scan_cursor_history();
        assert!(
            cursors.len() > 1,
            "expected a multi-page walk, got cursors {cursors:?}"
        );
        assert_eq!(cursors[0], 0, "iteration starts at cursor 0");
        assert!(
            cursors.iter().skip(1).all(|cursor| *cursor > 0),
            "the walk only continues on non-zero cursors: {cursors:?}"
        );
    }

    #[tokio::test]
    async fn clear_all_sessions_deletes_keys_from_every_scan_page() {
        let adapter = Arc::new(MemoryRedisAdapter::with_scan_page_size(3));
        let store = RedisAgentStateStore::builder()
            .client(adapter.clone())
            .key_prefix(PREFIX)
            .build()
            .expect("builder accepts the paged adapter");
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2)])
            .await;
        store.save(Some("u2"), "s2", "k", json!(2)).await;

        let total = adapter.all_keys().len();
        assert!(total > 3, "more keys than a single page, got {total}");

        let removed = store.clear_all_sessions().await.unwrap();

        assert_eq!(removed, total, "every page contributed keys");
        assert!(adapter.all_keys().is_empty(), "all keys deleted");
        assert!(
            adapter.scan_cursor_history().len() > 1,
            "deletion enumerated through several SCAN pages"
        );
    }

    // ---- core list-hash semantics (persisted-hash contract) --------------

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
