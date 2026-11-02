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
//! Version semantics follow the harness [`AgentStateStore`] contract
//! (`expected == 0` = create-if-absent, `-1` = [`UNVERSIONED`] = unconditional). Here
//! [`RedisAgentStateStore::save_if_version`] and
//! [`RedisAgentStateStore::get_versioned`] surface the Java `saveIfVersion` /
//! `getVersioned` operations, which are built directly on the shared [`SAVE_SCRIPT`] CAS
//! primitive.
//!
//! The **fallible** [`AgentStateStore::try_get_versioned`] /
//! [`AgentStateStore::try_save_if_version`] overrides are the error-preserving surface
//! (Java throws a `RuntimeException`); the infallible trait methods are derived from them
//! and only log before degrading, so a backend outage can no longer masquerade as "no
//! state" without a trace (REDIS-05).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use agentscope_core::error::{Error, Result};
use agentscope_core::state::{compute_hash, needs_full_rewrite, AgentStateStore, UNVERSIONED};

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

/// Removes one member from the session key index (`SREM`).
///
/// Used by [`AgentStateStore::delete_key`]: the index member must disappear together with
/// the payload. It runs as a single-key script so it stays routable by its own slot (a
/// multi-key command could cross slots on Redis Cluster).
pub(crate) const DISCARD_TRACKED_KEY_SCRIPT: &str = "return redis.call('SREM', KEYS[1], ARGV[1])";

/// Lua script: the whole `save_list` write burst in one round-trip (REDIS-09).
///
/// `KEYS = [list, list-hash, session-keys]`, `ARGV = [trackMember, digest, rewrite]
/// ++ items`, where `rewrite == '1'` deletes the list before pushing. Without it a process
/// or connection that died between the `DEL` / `RPUSH`s / `SET :_hash` / `SADD :_keys`
/// left a half-written (or untracked) list behind.
///
/// Like [`SAVE_SCRIPT`] it names three keys, which the V1 key layout pins to one slot
/// through the `{user/session}` hash tag.
pub(crate) const SAVE_LIST_SCRIPT: &str = "local member = ARGV[1] \
     local digest = ARGV[2] \
     if ARGV[3] == '1' then redis.call('del', KEYS[1]) end \
     for i = 4, #ARGV do redis.call('rpush', KEYS[1], ARGV[i]) end \
     redis.call('set', KEYS[2], digest) \
     redis.call('sadd', KEYS[3], member) \
     return #ARGV - 3";

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

    /// Clears all sessions stored in Redis (testing / ops cleanup). Returns the number of
    /// deleted keys.
    ///
    /// Rust-side extension: the unified Java store has no such method (only the two
    /// `@Deprecated` client-specific stores do), and **nothing in this crate calls it** — it
    /// stays for operator cleanup and tests instead of being deleted (REDIS-10).
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

    /// Fallible write: a rejected `SAVE_SCRIPT` (connection lost, AUTH failure, wrong type)
    /// is returned instead of being logged away, so a lost write is never mistaken for a
    /// successful one (REDIS-07).
    async fn try_save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
    ) -> Result<()> {
        self.save_versioned(user_id, session_id, key, &value, UNCONDITIONAL_EXPECTED)
            .await
            .map(|_| ())
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

    /// Fallible list write (same contract as the infallible `save_list`).
    ///
    /// The write burst itself runs as one Lua script (REDIS-09), so a caller never sees a
    /// half-written list; the change-detection read before it can still be stale under a
    /// competing writer, which the stored hash heals on the next write.
    async fn try_save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[Value],
    ) -> Result<()> {
        self.save_list_inner(user_id, session_id, key, values).await
    }

    async fn get(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Option<Value> {
        match self.get_inner(user_id, session_id, key).await {
            Ok(value) => value,
            Err(e) => {
                tracing::error!("RedisAgentStateStore::get failed for key {key}: {e}");
                None
            }
        }
    }

    /// Fallible read: a transport failure or an undecodable payload is an `Err` instead of
    /// the "nothing stored" the infallible getter reports (REDIS-07).
    async fn try_get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<Option<Value>> {
        self.get_inner(user_id, session_id, key).await
    }

    async fn get_list(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Vec<Value> {
        match self.get_list_items(user_id, session_id, key).await {
            Ok(jsons) => jsons
                .into_iter()
                .filter_map(|json| match serde_json::from_str(&json) {
                    Ok(value) => Some(value),
                    Err(e) => {
                        tracing::warn!(
                            "RedisAgentStateStore::get_list skipping an undecodable item of key {key}: {e}"
                        );
                        None
                    }
                })
                .collect(),
            Err(e) => {
                tracing::error!("RedisAgentStateStore::get_list failed for key {key}: {e}");
                Vec::new()
            }
        }
    }

    /// Fallible list read: a transport failure **or** an undecodable item is an `Err`, so a
    /// corrupt entry can no longer shrink the list silently (REDIS-07).
    async fn try_get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<Vec<Value>> {
        let jsons = self.get_list_items(user_id, session_id, key).await?;
        jsons
            .into_iter()
            .map(|json| serde_json::from_str(&json).map_err(Error::from))
            .collect()
    }

    /// Whether the session has at least one tracked key.
    ///
    /// A single `SCARD` on the session index answers it, exactly like Java
    /// `RedisAgentStateStore#exists` (`getSetSize(keysKey) > 0`); the previous shape issued
    /// an `EXISTS` first and discarded its answer (REDIS-13). A backend failure is still
    /// reported as `false` (the trait has no `Result` channel) but is now logged.
    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        match self.exists_inner(user_id, session_id).await {
            Ok(exists) => exists,
            Err(e) => {
                tracing::error!(
                    "RedisAgentStateStore::exists failed for session {}/{}: {e}",
                    key_layout::normalize_user(user_id),
                    session_id
                );
                false
            }
        }
    }

    /// Fallible probe: a backend failure is an `Err` instead of `false`, so "could not ask"
    /// cannot be mistaken for "the session is gone" (REDIS-07).
    async fn try_exists(&self, user_id: Option<&str>, session_id: &str) -> Result<bool> {
        self.exists_inner(user_id, session_id).await
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if let Err(e) = self.delete_inner(user_id, session_id).await {
            // Java logs `Failed to delete session: user/session`; the failing slot must be
            // identifiable from the log alone (REDIS-05).
            tracing::error!(
                "RedisAgentStateStore::delete failed for session {}/{}: {e}",
                key_layout::normalize_user(user_id),
                session_id
            );
        }
    }

    /// Fallible delete: a failed `DEL` / session-index read is returned instead of logged
    /// away, so a half-deleted session is visible to the caller (REDIS-07).
    async fn try_delete(&self, user_id: Option<&str>, session_id: &str) -> Result<()> {
        self.delete_inner(user_id, session_id).await
    }

    /// Deletes one state value, leaving the rest of the session intact.
    ///
    /// Redis can honour this, so the default "unsupported" error of the core trait would be
    /// a silent capability gap (COS / MongoDB / OSS all implement it). The `:_keys` index
    /// tells the payload shape apart: a `{key}:list` member means the list plus its
    /// `:_hash` marker, any other member means the JSON string plus its `:ver` counter.
    /// Commands are issued one key per call, like [`AgentStateStore::delete`], because a V0
    /// layout can put the two keys in different cluster slots.
    async fn delete_key(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Result<()> {
        self.delete_key_inner(user_id, session_id, key).await
    }

    /// Fallible per-key delete: identical to [`AgentStateStore::delete_key`], which already
    /// returns `Result`; the explicit override keeps the `try_*` family complete (REDIS-07).
    async fn try_delete_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<()> {
        self.delete_key_inner(user_id, session_id, key).await
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        match self.list_session_ids_inner(user_id).await {
            Ok(sessions) => sessions,
            Err(e) => {
                tracing::error!("RedisAgentStateStore::list_session_ids failed: {e}");
                Vec::new()
            }
        }
    }

    /// Fallible enumeration: a failed `SCAN` / invalid V1 user id is an `Err` instead of an
    /// empty list, so "the user has no sessions" is distinguishable from "the scan failed"
    /// (REDIS-07).
    async fn try_list_session_ids(&self, user_id: Option<&str>) -> Result<Vec<String>> {
        self.list_session_ids_inner(user_id).await
    }

    fn supports_versioning(&self) -> bool {
        true
    }

    /// Infallible view of [`RedisAgentStateStore::get_versioned`].
    ///
    /// Derived from [`AgentStateStore::try_get_versioned`], so both surfaces share one
    /// implementation. A backend failure collapses to `version = `[`UNVERSIONED`], the same
    /// value a non-versioning backend reports; callers that must not continue on a wrong
    /// assumption call `try_get_versioned` instead (REDIS-05 / REDIS-11).
    async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> agentscope_core::state::VersionedState {
        match AgentStateStore::try_get_versioned(self, user_id, session_id, key).await {
            Ok(versioned) => versioned,
            Err(e) => {
                tracing::error!(
                    "RedisAgentStateStore::get_versioned failed for session {}/{} key {key}: {e}",
                    key_layout::normalize_user(user_id),
                    session_id
                );
                agentscope_core::state::VersionedState {
                    value: None,
                    version: UNVERSIONED,
                }
            }
        }
    }

    /// Fallible read: reports a transport / decoding failure instead of the
    /// "nothing stored" that [`AgentStateStore::get_versioned`] would return (REDIS-05).
    async fn try_get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<agentscope_core::state::VersionedState> {
        let versioned = RedisAgentStateStore::get_versioned(self, user_id, session_id, key).await?;
        let version = i64::try_from(versioned.version).map_err(|_| {
            Error::Other(format!(
                "state version {} for key {key} exceeds i64::MAX",
                versioned.version
            ))
        })?;
        Ok(agentscope_core::state::VersionedState {
            value: versioned.value,
            version,
        })
    }

    /// Infallible CAS write, derived from [`AgentStateStore::try_save_if_version`].
    ///
    /// `UNVERSIONED` means "another writer won **or** the write never landed"; Java throws
    /// for the latter. Callers that must not lose an update call `try_save_if_version`
    /// (REDIS-05 / REDIS-11).
    async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
        expected_version: i64,
    ) -> i64 {
        match AgentStateStore::try_save_if_version(
            self,
            user_id,
            session_id,
            key,
            value,
            expected_version,
        )
        .await
        {
            Ok(version) => version,
            Err(e) => {
                tracing::error!(
                    "RedisAgentStateStore::save_if_version failed for session {}/{} key {key} (expected {expected_version}): {e}",
                    key_layout::normalize_user(user_id),
                    session_id
                );
                UNVERSIONED
            }
        }
    }

    /// Fallible CAS write: `Ok(UNVERSIONED)` is a real conflict, `Err` means the write
    /// never landed (REDIS-05).
    async fn try_save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: Value,
        expected_version: i64,
    ) -> Result<i64> {
        // Core contract: `UNVERSIONED` (-1) is the unconditional sentinel and `0` is a real
        // CAS version meaning "create if absent". The u64-backed implementation uses
        // `UNCONDITIONAL_EXPECTED` (u64::MAX) for the former, so the sentinels never collide.
        // Successful writes always bump to >= 1, so an inherent `Ok(0)` is unambiguously a
        // conflict, which core spells `UNVERSIONED`.
        let expected = if expected_version == UNVERSIONED {
            UNCONDITIONAL_EXPECTED
        } else if expected_version < 0 {
            return Err(Error::Other(format!(
                "invalid expected_version {expected_version} for key {key}: must be >= 0 or UNVERSIONED ({UNVERSIONED})"
            )));
        } else {
            expected_version as u64
        };
        let written =
            RedisAgentStateStore::save_if_version(self, user_id, session_id, key, &value, expected)
                .await?;
        if written == 0 {
            return Ok(UNVERSIONED);
        }
        i64::try_from(written).map_err(|_| {
            Error::Other(format!(
                "state version {written} for key {key} exceeds i64::MAX"
            ))
        })
    }
}

impl RedisAgentStateStore {
    /// Strict single-value read shared by [`AgentStateStore::get`] and
    /// [`AgentStateStore::try_get`]: `Ok(None)` when the key is absent, `Err` on a transport
    /// failure or an undecodable payload.
    async fn get_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<Option<Value>> {
        let layout = self.resolve_key_layout(user_id, session_id)?;
        let redis_key = layout.state_key(key);
        match self.client.get(&redis_key).await? {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Raw list items (JSON strings) before decoding, shared by the lenient
    /// [`AgentStateStore::get_list`] and the strict [`AgentStateStore::try_get_list`].
    async fn get_list_items(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<Vec<String>> {
        let layout = self.resolve_key_layout(user_id, session_id)?;
        self.client.range_list(&layout.list_key(key), 0, -1).await
    }

    /// `SCARD` on the session key index (`> 0` = the session has state).
    async fn exists_inner(&self, user_id: Option<&str>, session_id: &str) -> Result<bool> {
        let layout = self.resolve_key_layout(user_id, session_id)?;
        Ok(self.client.get_set_size(&layout.keys_key()).await? > 0)
    }

    /// `SCAN` + key-name parsing shared by [`AgentStateStore::list_session_ids`] and
    /// [`AgentStateStore::try_list_session_ids`].
    async fn list_session_ids_inner(&self, user_id: Option<&str>) -> Result<Vec<String>> {
        let user_segment = key_layout::normalize_user(user_id);
        let pattern = match self.key_layout_version {
            KeyLayoutVersion::V0 => v0_keys_pattern(&self.key_prefix, &user_segment),
            KeyLayoutVersion::V1 => {
                validate_v1_user_id(&user_segment)?;
                v1_keys_pattern(&self.key_prefix, &user_segment)
            }
        };
        let keys = self.client.find_keys_by_pattern(&pattern).await?;
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
        Ok(out)
    }

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

        // Which items the script has to push, and whether the stored list must go first.
        let (rewrite, items) = if needs_full_rewrite(values, stored_hash.as_deref(), existing_count)
        {
            (true, values)
        } else {
            (false, &values[values.len().min(existing_count)..])
        };

        let mut args: Vec<String> = Vec::with_capacity(3 + items.len());
        args.push(layout.list_track_key(key));
        args.push(current_hash);
        args.push(if rewrite { "1" } else { "0" }.to_string());
        for item in items {
            args.push(serde_json::to_string(item)?);
        }

        // One script writes the payload, the change marker and the session index together
        // (REDIS-09); all three keys share the V1 hash-tag slot.
        self.client
            .eval_script(SAVE_LIST_SCRIPT, &[list_key, hash_key, keys_key], &args)
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

    /// Per-key delete shared by the [`AgentStateStore::delete_key`] override.
    async fn delete_key_inner(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<()> {
        if key.trim().is_empty() {
            return Err(Error::Other("key must not be blank".into()));
        }
        let layout = self.resolve_key_layout(user_id, session_id)?;
        let keys_key = layout.keys_key();
        let tracked = self.client.get_set_members(&keys_key).await?;
        // `:_keys` stores the *list* member as `{key}:list`; a single value is stored as the
        // bare key name. Both shapes can exist under one key name (a `save` followed by a
        // `save_list`), so each tracked shape is removed on its own.
        let list_track_key = layout.list_track_key(key);
        let is_list = tracked.iter().any(|member| member == &list_track_key);
        let is_single = tracked.iter().any(|member| member == key);

        let mut payload_keys = Vec::new();
        if is_single || !is_list {
            // Untracked keys have no index member to classify them by, so the single-value
            // shape is assumed (the two shapes never share a Redis key).
            let state_key = layout.state_key(key);
            let version_key = version_support::version_key(&state_key);
            payload_keys.push(state_key);
            payload_keys.push(version_key);
        }
        if is_list {
            let list_key = layout.list_key(key);
            payload_keys.push(list_key.clone());
            payload_keys.push(format!("{list_key}{HASH_SUFFIX}"));
        }
        // One key per command, for the same CROSSSLOT reason as `delete_inner`.
        for payload_key in payload_keys {
            self.client.delete_keys(&[payload_key]).await?;
        }

        let mut discarded_members = Vec::new();
        if is_single {
            discarded_members.push(key.to_string());
        }
        if is_list {
            discarded_members.push(list_track_key);
        }
        for member in discarded_members {
            let _ = self
                .client
                .eval_script(DISCARD_TRACKED_KEY_SCRIPT, &[keys_key.clone()], &[member])
                .await?;
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

    /// REDIS-13: Java answers `exists` with a single `getSetSize(keysKey)`; the Rust path
    /// used to issue `EXISTS` first and throw that answer away.
    #[tokio::test]
    async fn exists_costs_one_round_trip() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;

        assert!(store.exists(Some("u1"), "s1").await);
        assert_eq!(adapter.set_size_calls(), 1, "one SCARD answers it");
        assert_eq!(adapter.key_exists_calls(), 0, "no EXISTS round trip");

        assert!(!store.exists(Some("u1"), "empty").await);
        assert_eq!(adapter.set_size_calls(), 2);
        assert_eq!(adapter.key_exists_calls(), 0);
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

    // ---- per-key delete (core `AgentStateStore::delete_key`) --------------

    #[tokio::test]
    async fn delete_key_removes_one_value_and_keeps_the_rest_of_the_session() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "a", json!(1)).await;
        store.save(Some("u1"), "s1", "b", json!(2)).await;
        store.save_list(Some("u1"), "s1", "l", &[json!(3)]).await;

        AgentStateStore::delete_key(&store, Some("u1"), "s1", "a")
            .await
            .expect("Redis supports per-key delete");

        assert_eq!(store.get(Some("u1"), "s1", "a").await, None);
        assert!(adapter.string_of("t:session:u1/s1:a").is_none());
        assert!(
            adapter.string_of("t:session:u1/s1:a:ver").is_none(),
            "the version counter goes with the payload"
        );
        assert_eq!(store.get(Some("u1"), "s1", "b").await, Some(json!(2)));
        assert_eq!(store.get_list(Some("u1"), "s1", "l").await, vec![json!(3)]);
        assert_eq!(
            adapter.set_members("t:session:u1/s1:_keys"),
            vec!["b".to_string(), "l:list".to_string()],
            "only the deleted key's index member is dropped"
        );
        assert!(
            store.exists(Some("u1"), "s1").await,
            "the session still has data"
        );
    }

    #[tokio::test]
    async fn delete_key_removes_a_list_with_its_hash_marker() {
        let (store, adapter) = store();
        store
            .save_list(Some("u1"), "s1", "l", &[json!(1), json!(2)])
            .await;
        store.save(Some("u1"), "s1", "kept", json!("v")).await;

        AgentStateStore::delete_key(&store, Some("u1"), "s1", "l")
            .await
            .expect("per-key delete accepts a list");

        assert!(adapter.list("t:session:u1/s1:l:list").is_empty());
        assert!(adapter.string_of("t:session:u1/s1:l:list:_hash").is_none());
        assert!(store.get_list(Some("u1"), "s1", "l").await.is_empty());
        assert_eq!(
            adapter.set_members("t:session:u1/s1:_keys"),
            vec!["kept".to_string()]
        );
        assert_eq!(store.get(Some("u1"), "s1", "kept").await, Some(json!("v")));
    }

    #[tokio::test]
    async fn delete_key_removes_both_shapes_when_one_key_name_holds_both() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!("single")).await;
        store
            .save_list(Some("u1"), "s1", "k", &[json!(1), json!(2)])
            .await;

        AgentStateStore::delete_key(&store, Some("u1"), "s1", "k")
            .await
            .expect("per-key delete drops every shape stored under the key name");

        assert!(adapter.string_of("t:session:u1/s1:k").is_none());
        assert!(
            adapter.string_of("t:session:u1/s1:k:ver").is_none(),
            "the single value's version counter goes too"
        );
        assert!(adapter.list("t:session:u1/s1:k:list").is_empty());
        assert!(adapter.string_of("t:session:u1/s1:k:list:_hash").is_none());
        assert!(
            adapter.set_members("t:session:u1/s1:_keys").is_empty(),
            "both the bare key and the `k:list` index members are dropped"
        );
        assert!(!store.exists(Some("u1"), "s1").await);
    }

    #[tokio::test]
    async fn delete_key_is_idempotent_and_leaves_untracked_keys_alone() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "tracked", json!(1)).await;

        // A key that was never written: no index rewrite, no collateral deletion.
        AgentStateStore::delete_key(&store, Some("u1"), "s1", "never-written")
            .await
            .expect("deleting an absent key is not an error");
        assert_eq!(store.get(Some("u1"), "s1", "tracked").await, Some(json!(1)));
        assert_eq!(
            adapter.set_members("t:session:u1/s1:_keys"),
            vec!["tracked".to_string()]
        );

        // Deleting the tracked key twice is a no-op the second time.
        AgentStateStore::delete_key(&store, Some("u1"), "s1", "tracked")
            .await
            .expect("first delete succeeds");
        AgentStateStore::delete_key(&store, Some("u1"), "s1", "tracked")
            .await
            .expect("second delete is a no-op");
        assert_eq!(store.get(Some("u1"), "s1", "tracked").await, None);
        assert!(adapter.set_members("t:session:u1/s1:_keys").is_empty());
        assert!(
            !store.exists(Some("u1"), "s1").await,
            "the emptied session is gone, exactly as Redis drops an emptied set"
        );
    }

    #[tokio::test]
    async fn delete_key_rejects_a_blank_session_or_key_before_io() {
        let (store, adapter) = store();
        assert!(AgentStateStore::delete_key(&store, Some("u1"), "  ", "k")
            .await
            .is_err());
        assert!(AgentStateStore::delete_key(&store, Some("u1"), "s1", "  ")
            .await
            .is_err());
        assert_eq!(adapter.delete_calls(), 0, "nothing is deleted");
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

    // ---- core `try_*` surface (REDIS-05 / REDIS-11) -----------------------

    #[tokio::test]
    async fn try_get_versioned_returns_payload_and_version() {
        let (store, _adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        store.save(Some("u1"), "s1", "k", json!(2)).await;

        let versioned = AgentStateStore::try_get_versioned(&store, Some("u1"), "s1", "k")
            .await
            .expect("reachable backend");
        assert_eq!(versioned.value, Some(json!(2)));
        assert_eq!(versioned.version, 2);

        let absent = AgentStateStore::try_get_versioned(&store, Some("u1"), "s1", "missing")
            .await
            .expect("a missing key is not an error");
        assert_eq!(absent.value, None);
        assert_eq!(absent.version, 0, "create-if-absent version");
    }

    /// REDIS-05: a backend failure must reach the caller instead of reading as "no state".
    /// The infallible surface keeps its old `Option`-shaped contract but now logs and
    /// reports [`UNVERSIONED`] (REDIS-11).
    #[tokio::test]
    async fn try_get_versioned_surfaces_a_backend_failure() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        adapter.configure(|d| d.fail_get = true);

        let error = AgentStateStore::try_get_versioned(&store, Some("u1"), "s1", "k")
            .await
            .expect_err("a transport failure must not look like a missing key");
        assert!(
            format!("{error}").contains("injected Redis GET failure"),
            "{error}"
        );

        let degraded = AgentStateStore::get_versioned(&store, Some("u1"), "s1", "k").await;
        assert_eq!(degraded.value, None);
        assert_eq!(degraded.version, UNVERSIONED);
    }

    #[tokio::test]
    async fn try_save_if_version_writes_and_bumps_the_version() {
        let (store, _adapter) = store();
        assert_eq!(
            AgentStateStore::try_save_if_version(&store, Some("u1"), "s1", "k", json!(1), 0)
                .await
                .unwrap(),
            1,
            "0 == create-if-absent"
        );
        assert_eq!(
            AgentStateStore::try_save_if_version(&store, Some("u1"), "s1", "k", json!(2), 1)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            AgentStateStore::try_save_if_version(
                &store,
                Some("u1"),
                "s1",
                "k",
                json!(3),
                UNVERSIONED
            )
            .await
            .unwrap(),
            3,
            "UNVERSIONED == unconditional"
        );
        assert_eq!(store.get(Some("u1"), "s1", "k").await, Some(json!(3)));
    }

    /// REDIS-05: "another writer won" (`Ok(UNVERSIONED)`) must be distinguishable from
    /// "the write never landed" (`Err`). The infallible surface flattens both to
    /// [`UNVERSIONED`].
    #[tokio::test]
    async fn try_save_if_version_separates_conflict_from_transport_failure() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await; // version 1

        let conflict =
            AgentStateStore::try_save_if_version(&store, Some("u1"), "s1", "k", json!(2), 7)
                .await
                .expect("a conflict is not a transport error");
        assert_eq!(conflict, UNVERSIONED);

        adapter.configure(|d| d.fail_eval = true);
        let error =
            AgentStateStore::try_save_if_version(&store, Some("u1"), "s1", "k", json!(3), 1)
                .await
                .expect_err("a failed write must not look like a conflict");
        assert!(
            format!("{error}").contains("injected Redis EVAL failure"),
            "{error}"
        );
        assert_eq!(
            AgentStateStore::save_if_version(&store, Some("u1"), "s1", "k", json!(3), 1).await,
            UNVERSIONED,
            "the infallible surface still degrades"
        );

        adapter.configure(|d| d.fail_eval = false);
        assert_eq!(
            store.get(Some("u1"), "s1", "k").await,
            Some(json!(1)),
            "the failed write left the stored state alone"
        );
    }

    #[tokio::test]
    async fn try_save_if_version_rejects_a_negative_expected_version() {
        let (store, adapter) = store();
        let error =
            AgentStateStore::try_save_if_version(&store, Some("u1"), "s1", "k", json!(1), -5)
                .await
                .expect_err("only UNVERSIONED may be negative");
        assert!(
            format!("{error}").contains("invalid expected_version -5"),
            "{error}"
        );
        assert_eq!(adapter.eval_calls(), 0, "rejected before any Redis write");
        assert_eq!(
            AgentStateStore::save_if_version(&store, Some("u1"), "s1", "k", json!(1), -5).await,
            UNVERSIONED
        );
        assert_eq!(store.get(Some("u1"), "s1", "k").await, None);
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

    // ---- fallible `try_*` surface (REDIS-07) ----------------------------

    #[tokio::test]
    async fn try_save_and_try_get_round_trip_and_surface_a_failure() {
        let (store, adapter) = store();
        assert!(
            AgentStateStore::try_save(&store, Some("u1"), "  ", "k", json!(1))
                .await
                .is_err(),
            "a blank session is rejected before any IO"
        );

        AgentStateStore::try_save(&store, Some("u1"), "s1", "k", json!({"n": 1}))
            .await
            .expect("a healthy write lands");
        assert_eq!(
            AgentStateStore::try_get(&store, Some("u1"), "s1", "k")
                .await
                .expect("a healthy read"),
            Some(json!({"n": 1}))
        );
        assert_eq!(
            AgentStateStore::try_get(&store, Some("u1"), "s1", "missing")
                .await
                .expect("an absent key is not an error"),
            None
        );

        adapter.configure(|d| d.fail_eval = true);
        assert!(
            AgentStateStore::try_save(&store, Some("u1"), "s1", "k", json!(2))
                .await
                .is_err(),
            "a rejected SAVE_SCRIPT must not look like a successful write"
        );

        adapter.configure(|d| {
            d.fail_eval = false;
            d.fail_get = true;
        });
        assert!(
            AgentStateStore::try_get(&store, Some("u1"), "s1", "k")
                .await
                .is_err(),
            "a lost connection must not read as 'nothing stored'"
        );
        assert_eq!(
            store.get(Some("u1"), "s1", "k").await,
            None,
            "the infallible surface keeps degrading for existing callers"
        );
    }

    #[tokio::test]
    async fn try_save_list_and_try_get_list_report_a_corrupt_item() {
        let (store, adapter) = store();
        AgentStateStore::try_save_list(&store, Some("u1"), "s1", "l", &[json!(1), json!(2)])
            .await
            .expect("a healthy list write");
        assert_eq!(
            AgentStateStore::try_get_list(&store, Some("u1"), "s1", "l")
                .await
                .expect("a healthy list read"),
            vec![json!(1), json!(2)]
        );

        // A corrupt item is skipped by the infallible read but reported by the fallible one.
        adapter.seed_list("t:session:u1/s1:l:list", &["1", "not-json"]);
        assert_eq!(
            store.get_list(Some("u1"), "s1", "l").await,
            vec![json!(1)],
            "the best-effort read still drops the bad item"
        );
        assert!(
            AgentStateStore::try_get_list(&store, Some("u1"), "s1", "l")
                .await
                .is_err(),
            "the fallible read surfaces the corrupt item"
        );

        adapter.configure(|d| {
            d.fail_get = true;
            d.fail_range = true;
        });
        assert!(
            AgentStateStore::try_save_list(&store, Some("u1"), "s1", "l", &[json!(3)])
                .await
                .is_err()
        );
        assert!(AgentStateStore::try_get_list(&store, Some("u1"), "s1", "l")
            .await
            .is_err());
        assert!(
            store.get_list(Some("u1"), "s1", "l").await.is_empty(),
            "the infallible read still degrades to an empty list"
        );
    }

    #[tokio::test]
    async fn try_exists_separates_an_empty_session_from_a_backend_failure() {
        let (store, adapter) = store();
        assert!(!AgentStateStore::try_exists(&store, Some("u1"), "s1")
            .await
            .expect("an empty session is a clean `false`"));
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        assert!(AgentStateStore::try_exists(&store, Some("u1"), "s1")
            .await
            .expect("the session has state"));

        adapter.configure(|d| d.fail_set_size = true);
        assert!(
            AgentStateStore::try_exists(&store, Some("u1"), "s1")
                .await
                .is_err(),
            "a failed SCARD must not read as 'the session is gone'"
        );
        assert!(!store.exists(Some("u1"), "s1").await);
    }

    #[tokio::test]
    async fn try_delete_and_try_delete_key_surface_a_failure() {
        let (store, adapter) = store();
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        AgentStateStore::try_delete_key(&store, Some("u1"), "s1", "k")
            .await
            .expect("per-key delete");
        assert!(!store.exists(Some("u1"), "s1").await);
        assert!(
            AgentStateStore::try_delete_key(&store, Some("u1"), "s1", " ")
                .await
                .is_err()
        );

        AgentStateStore::try_delete(&store, Some("u1"), "s1")
            .await
            .expect("deleting an empty session is a no-op");
        store.save(Some("u1"), "s1", "k2", json!(2)).await;

        adapter.configure(|d| d.fail_set_members = true);
        assert!(AgentStateStore::try_delete(&store, Some("u1"), "s1")
            .await
            .is_err());
        assert!(
            AgentStateStore::try_delete_key(&store, Some("u1"), "s1", "k2")
                .await
                .is_err()
        );
        assert!(
            AgentStateStore::try_delete(&store, Some("u1"), "  ")
                .await
                .is_err(),
            "a blank session is rejected"
        );

        adapter.configure(|d| d.fail_set_members = false);
        assert_eq!(
            store.get(Some("u1"), "s1", "k2").await,
            Some(json!(2)),
            "the failed deletes left the value in place"
        );
        AgentStateStore::try_delete(&store, Some("u1"), "s1")
            .await
            .expect("delete succeeds once the store recovers");
        assert!(!store.exists(Some("u1"), "s1").await);
    }

    #[tokio::test]
    async fn try_list_session_ids_surfaces_a_scan_failure() {
        let (store, adapter) = store();
        assert!(AgentStateStore::try_list_session_ids(&store, Some("u1"))
            .await
            .expect("an empty enumeration is a clean empty list")
            .is_empty());
        store.save(Some("u1"), "s2", "k", json!(1)).await;
        store.save(Some("u1"), "s1", "k", json!(1)).await;
        assert_eq!(
            AgentStateStore::try_list_session_ids(&store, Some("u1"))
                .await
                .expect("enumeration"),
            vec!["s1".to_string(), "s2".to_string()]
        );

        adapter.configure(|d| d.fail_scan = true);
        assert!(
            AgentStateStore::try_list_session_ids(&store, Some("u1"))
                .await
                .is_err(),
            "a failed SCAN must not read as 'no sessions'"
        );
        assert!(store.list_session_ids(Some("u1")).await.is_empty());
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
