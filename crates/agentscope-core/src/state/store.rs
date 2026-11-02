//! Persistent storage for agent state, mirroring `io.agentscope.core.state`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::Result;
use async_trait::async_trait;

/// Sentinel version for backends that do not support optimistic concurrency, and the
/// value returned by [`AgentStateStore::save_if_version`] when a CAS write conflicts
/// (nothing written). Mirrors the Java `AgentStateStore.UNVERSIONED`.
pub const UNVERSIONED: i64 = -1;

/// A state value paired with its optimistic-concurrency version, mirroring the Java
/// `VersionedState`.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionedState {
    /// The loaded state, or `None` when the key is absent.
    pub value: Option<serde_json::Value>,
    /// Store version (`0` for an absent key on a versioning backend, [`UNVERSIONED`]
    /// on a backend without optimistic concurrency).
    pub version: i64,
}

impl VersionedState {
    /// Whether a value was present in the store.
    pub fn is_present(&self) -> bool {
        self.value.is_some()
    }
}

/// Persistent storage interface for AgentScope agent state, keyed by a
/// `(userId, sessionId)` pair.
#[async_trait]
pub trait AgentStateStore: Send + Sync {
    /// Save a single value (full replacement).
    async fn save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    );

    /// Fallible variant of [`AgentStateStore::save`].
    ///
    /// The infallible `save*` / `get*` / `exists` / `delete*` methods are **best-effort**: a
    /// backend that cannot land a write may drop the failure silently, which is fine for
    /// "state is a cache" callers but hides a lost write from a caller that must not
    /// continue. Use the `try_*` variant when the outcome matters. Backends with a real
    /// error source (e.g. a failed atomic write or a corrupt payload) override these; the
    /// default keeps the infallible behaviour and only reports what it can diagnose from
    /// its inputs. Consistent with [`AgentStateStore::try_get_versioned`] /
    /// [`AgentStateStore::try_save_if_version`].
    async fn try_save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<()> {
        require_session_id(session_id)?;
        self.save(user_id, session_id, key, value).await;
        Ok(())
    }

    /// Save a list of values.
    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    );

    /// Fallible variant of [`AgentStateStore::save_list`] (same contract as
    /// [`AgentStateStore::try_save`]).
    async fn try_save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) -> Result<()> {
        require_session_id(session_id)?;
        self.save_list(user_id, session_id, key, values).await;
        Ok(())
    }

    /// Get a single value, or `None` if not present.
    async fn get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Option<serde_json::Value>;

    /// Fallible variant of [`AgentStateStore::get`]: also reports failures that
    /// [`AgentStateStore::get`] collapses to "nothing stored" (same contract as
    /// [`AgentStateStore::try_get_versioned`]).
    async fn try_get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>> {
        require_session_id(session_id)?;
        Ok(self.get(user_id, session_id, key).await)
    }

    /// Get a list of values (empty when not present).
    async fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Vec<serde_json::Value>;

    /// Fallible variant of [`AgentStateStore::get_list`] (same contract as
    /// [`AgentStateStore::try_get`]).
    async fn try_get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<Vec<serde_json::Value>> {
        require_session_id(session_id)?;
        Ok(self.get_list(user_id, session_id, key).await)
    }

    /// Whether a session has any persisted state.
    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool;

    /// Fallible variant of [`AgentStateStore::exists`] (same contract as
    /// [`AgentStateStore::try_get`]).
    async fn try_exists(&self, user_id: Option<&str>, session_id: &str) -> Result<bool> {
        require_session_id(session_id)?;
        Ok(self.exists(user_id, session_id).await)
    }

    /// Delete a session and all its data.
    async fn delete(&self, user_id: Option<&str>, session_id: &str);

    /// Fallible variant of [`AgentStateStore::delete`] (same contract as
    /// [`AgentStateStore::try_get`]).
    async fn try_delete(&self, user_id: Option<&str>, session_id: &str) -> Result<()> {
        require_session_id(session_id)?;
        self.delete(user_id, session_id).await;
        Ok(())
    }

    /// Delete a single state value, keeping the rest of the session intact.
    ///
    /// Mirrors the extra `delete(userId, sessionId, key)` overload in Java's
    /// `AgentStateStore`: it drops the value's payload **and** any side objects the backend
    /// keeps for it (e.g. COS removes `.json`, `.list.json` and `.list.hash`).
    ///
    /// Not every backend can honour a per-key delete, so the default implementation reports an
    /// error instead of silently succeeding — callers must see that the value is still there.
    /// [`InMemoryAgentStateStore`] and the object-storage backends override it.
    async fn delete_key(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Result<()> {
        let _ = (user_id, session_id, key);
        Err(crate::Error::Other(
            "this AgentStateStore backend does not support per-key delete".to_string(),
        ))
    }

    /// Fallible variant of [`AgentStateStore::delete_key`] (same contract as
    /// [`AgentStateStore::try_get`]). Propagates the default
    /// `delete_key` refusal, so "backend cannot delete a single key" stays visible.
    async fn try_delete_key(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<()> {
        require_session_id(session_id)?;
        self.delete_key(user_id, session_id, key).await
    }

    /// List session identifiers under the given user namespace (`None` = anonymous).
    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String>;

    /// Fallible variant of [`AgentStateStore::list_session_ids`].
    ///
    /// There is no `session_id` to validate here, so the default only preserves the
    /// infallible behaviour; backends that can fail to enumerate (e.g. an unreadable
    /// directory) override it.
    async fn try_list_session_ids(&self, user_id: Option<&str>) -> Result<Vec<String>> {
        Ok(self.list_session_ids(user_id).await)
    }

    /// Whether this backend supports optimistic concurrency through
    /// [`AgentStateStore::get_versioned`] / [`AgentStateStore::save_if_version`].
    ///
    /// Default `false`: [`AgentStateStore::save_if_version`] degrades to an
    /// unconditional [`AgentStateStore::save`].
    fn supports_versioning(&self) -> bool {
        false
    }

    /// Loads a single state value together with its store version.
    ///
    /// The default implementation wraps [`AgentStateStore::get`] and reports
    /// [`UNVERSIONED`]. On versioning backends an absent key reports version `0`.
    async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> VersionedState {
        VersionedState {
            value: self.get(user_id, session_id, key).await,
            version: UNVERSIONED,
        }
    }

    /// Fallible variant of [`AgentStateStore::get_versioned`].
    ///
    /// `get_versioned` collapses every failure to "nothing stored", which is fine for a
    /// best-effort read but hides a corrupt payload or an unreachable backend from a caller
    /// that must not continue on a wrong assumption. Backends that can detect such failures
    /// override this; the default preserves the infallible behaviour.
    async fn try_get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<VersionedState> {
        Ok(self.get_versioned(user_id, session_id, key).await)
    }

    /// Compare-and-swap write of a single state value.
    ///
    /// * `expected_version == 0` — create-if-absent (also satisfied by a row whose stored
    ///   version is still `0`, e.g. backfilled by a schema migration).
    /// * `expected_version == `[`UNVERSIONED`] — unconditional overwrite (same as `save`).
    /// * otherwise — write only when the stored version equals `expected_version`.
    ///
    /// Returns the new version on success, or [`UNVERSIONED`] on conflict (nothing written).
    /// The default implementation always calls [`AgentStateStore::save`] and returns
    /// [`UNVERSIONED`], preserving last-writer-wins behaviour for non-versioning backends.
    async fn save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> i64 {
        let _ = expected_version;
        self.save(user_id, session_id, key, value).await;
        UNVERSIONED
    }

    /// Fallible variant of [`AgentStateStore::save_if_version`].
    ///
    /// `save_if_version` reports a failed write as a CAS conflict ([`UNVERSIONED`]) or drops
    /// it entirely, so a caller cannot tell "another writer won" from "the write never
    /// landed". Backends that can report the difference override this; the default preserves
    /// the infallible behaviour and therefore never fails.
    async fn try_save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> Result<i64> {
        Ok(self
            .save_if_version(user_id, session_id, key, value, expected_version)
            .await)
    }
}

const ANON_USER: &str = "__anon__";

fn normalize_user(user_id: Option<&str>) -> String {
    match user_id {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => ANON_USER.to_string(),
    }
}

fn require_session_id(session_id: &str) -> Result<String> {
    if session_id.trim().is_empty() {
        return Err(crate::Error::Other("sessionId must not be blank".into()));
    }
    Ok(session_id.to_string())
}

#[derive(Debug, Default)]
struct SessionData {
    single: HashMap<String, serde_json::Value>,
    list: HashMap<String, Vec<serde_json::Value>>,
    /// Version assigned to each single-value key (absent = never written).
    versions: HashMap<String, i64>,
}

/// In-memory implementation of [`AgentStateStore`] (single-process, not persistent).
#[derive(Debug, Clone, Default)]
pub struct InMemoryAgentStateStore {
    inner: Arc<std::sync::RwLock<HashMap<String, HashMap<String, SessionData>>>>,
}

impl InMemoryAgentStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_session_count(&self) -> usize {
        self.inner.read().unwrap().values().map(|m| m.len()).sum()
    }

    pub fn clear_all(&self) {
        self.inner.write().unwrap().clear();
    }
}

#[async_trait]
impl AgentStateStore for InMemoryAgentStateStore {
    async fn save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) {
        let _ = require_session_id(session_id);
        let mut g = self.inner.write().unwrap();
        let session = g
            .entry(normalize_user(user_id))
            .or_default()
            .entry(session_id.to_string())
            .or_insert_with(SessionData::default);
        let next = session.versions.get(key).copied().unwrap_or(0) + 1;
        session.single.insert(key.to_string(), value);
        session.versions.insert(key.to_string(), next);
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) {
        let _ = require_session_id(session_id);
        let mut g = self.inner.write().unwrap();
        g.entry(normalize_user(user_id))
            .or_default()
            .entry(session_id.to_string())
            .or_insert_with(SessionData::default)
            .list
            .insert(key.to_string(), values.to_vec());
    }

    async fn get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Option<serde_json::Value> {
        let g = self.inner.read().unwrap();
        g.get(&normalize_user(user_id))?
            .get(session_id)?
            .single
            .get(key)
            .cloned()
    }

    async fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Vec<serde_json::Value> {
        let g = self.inner.read().unwrap();
        match g.get(&normalize_user(user_id)) {
            Some(bucket) => bucket
                .get(session_id)
                .and_then(|s| s.list.get(key))
                .cloned()
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        self.inner
            .read()
            .unwrap()
            .get(&normalize_user(user_id))
            .map(|m| m.contains_key(session_id))
            .unwrap_or(false)
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if let Some(bucket) = self
            .inner
            .write()
            .unwrap()
            .get_mut(&normalize_user(user_id))
        {
            bucket.remove(session_id);
        }
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        self.inner
            .read()
            .unwrap()
            .get(&normalize_user(user_id))
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    async fn delete_key(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Result<()> {
        require_session_id(session_id)?;
        if let Some(session) = self
            .inner
            .write()
            .unwrap()
            .get_mut(&normalize_user(user_id))
            .and_then(|bucket| bucket.get_mut(session_id))
        {
            session.single.remove(key);
            session.list.remove(key);
            session.versions.remove(key);
        }
        Ok(())
    }

    fn supports_versioning(&self) -> bool {
        true
    }

    async fn get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> VersionedState {
        let g = self.inner.read().unwrap();
        let session = g
            .get(&normalize_user(user_id))
            .and_then(|bucket| bucket.get(session_id));
        let Some(session) = session else {
            return VersionedState {
                value: None,
                version: 0,
            };
        };
        VersionedState {
            value: session.single.get(key).cloned(),
            version: session.versions.get(key).copied().unwrap_or(0),
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
        let _ = require_session_id(session_id);
        // A single write lock covers the read-check-write so the caller never observes a
        // concurrent writer's version (Java #3236 / #3220).
        let mut g = self.inner.write().unwrap();
        let session = g
            .entry(normalize_user(user_id))
            .or_default()
            .entry(session_id.to_string())
            .or_insert_with(SessionData::default);
        let current = session.versions.get(key).copied().unwrap_or(0);
        if expected_version != UNVERSIONED && current != expected_version {
            return UNVERSIONED;
        }
        let next = current + 1;
        session.single.insert(key.to_string(), value);
        session.versions.insert(key.to_string(), next);
        next
    }

    // The in-memory backend cannot fail to read or write, but it can still be handed an
    // unusable key. Reporting that instead of silently writing to the `""` session keeps the
    // fallible surface honest.
    async fn try_get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<VersionedState> {
        require_session_id(session_id)?;
        Ok(self.get_versioned(user_id, session_id, key).await)
    }

    async fn try_save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> Result<i64> {
        require_session_id(session_id)?;
        Ok(self
            .save_if_version(user_id, session_id, key, value, expected_version)
            .await)
    }
}

/// JSON file-based [`AgentStateStore`] implementation.
///
/// On-disk layout: `<root>/<user>/<session>/<key>.json|jsonl`.
#[derive(Debug, Clone)]
pub struct JsonFileAgentStateStore {
    root: PathBuf,
}

fn safe_segment(value: &str) -> String {
    let safe = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if safe {
        value.to_string()
    } else {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes())
    }
}

fn decode_segment(segment: &str) -> String {
    let safe = segment
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if safe {
        segment.to_string()
    } else {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(segment)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|_| segment.to_string())
    }
}

impl JsonFileAgentStateStore {
    /// Creates a store rooted at `~/.agentscope/state`.
    pub fn new_default() -> Self {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".to_string());
        Self::new(PathBuf::from(home).join(".agentscope").join("state"))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn session_dir(&self, user_id: Option<&str>, session_id: &str) -> PathBuf {
        self.root
            .join(safe_segment(&normalize_user(user_id)))
            .join(safe_segment(session_id))
    }

    fn state_path(&self, user_id: Option<&str>, session_id: &str, key: &str) -> PathBuf {
        self.session_dir(user_id, session_id)
            .join(format!("{}.json", key))
    }

    fn list_path(&self, user_id: Option<&str>, session_id: &str, key: &str) -> PathBuf {
        self.session_dir(user_id, session_id)
            .join(format!("{}.jsonl", key))
    }

    async fn atomic_write(&self, path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, content).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

#[async_trait]
impl AgentStateStore for JsonFileAgentStateStore {
    async fn save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) {
        let session = require_session_id(session_id).unwrap_or_default();
        let path = self.state_path(user_id, &session, key);
        if let Ok(json) = serde_json::to_string_pretty(&value) {
            let _ = self.atomic_write(&path, &json).await;
        }
    }

    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) {
        let session = require_session_id(session_id).unwrap_or_default();
        let path = self.list_path(user_id, &session, key);
        let mut content = String::new();
        for v in values {
            if let Ok(line) = serde_json::to_string(v) {
                content.push_str(&line);
                content.push('\n');
            }
        }
        let _ = self.atomic_write(&path, &content).await;
    }

    async fn get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Option<serde_json::Value> {
        let path = self.state_path(user_id, session_id, key);
        let text = tokio::fs::read_to_string(path).await.ok()?;
        serde_json::from_str(&text).ok()
    }

    async fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Vec<serde_json::Value> {
        let path = self.list_path(user_id, session_id, key);
        let text = match tokio::fs::read_to_string(path).await {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool {
        tokio::fs::metadata(self.session_dir(user_id, session_id))
            .await
            .is_ok()
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        let _ = tokio::fs::remove_dir_all(self.session_dir(user_id, session_id)).await;
    }

    // The four `try_*` overrides below cover the operations with a real error source:
    // serialization, a failed atomic write, and a payload that exists but cannot be parsed.
    // Reporting them keeps "absent" distinguishable from "broken".
    async fn try_save(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<()> {
        require_session_id(session_id)?;
        let path = self.state_path(user_id, session_id, key);
        let json = serde_json::to_string_pretty(&value)?;
        self.atomic_write(&path, &json).await
    }

    async fn try_save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    ) -> Result<()> {
        require_session_id(session_id)?;
        let path = self.list_path(user_id, session_id, key);
        let mut content = String::new();
        for value in values {
            content.push_str(&serde_json::to_string(value)?);
            content.push('\n');
        }
        self.atomic_write(&path, &content).await
    }

    async fn try_get(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>> {
        require_session_id(session_id)?;
        let path = self.state_path(user_id, session_id, key);
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|error| {
                crate::Error::Other(format!(
                    "failed to parse state value `{key}` at `{}`: {error}",
                    path.display()
                ))
            }),
            // A missing file is the ordinary "absent key" case, not a failure.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn try_get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<Vec<serde_json::Value>> {
        require_session_id(session_id)?;
        let path = self.list_path(user_id, session_id, key);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|error| {
                    crate::Error::Other(format!(
                        "failed to parse a state list entry `{key}` at `{}`: {error}",
                        path.display()
                    ))
                })
            })
            .collect()
    }

    // The JSON backend can genuinely fail: a corrupt payload is not the same as an absent
    // one, and a failed atomic write is not the same as a lost CAS race. Both used to look
    // like "nothing stored" / "conflict" to the caller.
    async fn try_get_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
    ) -> Result<VersionedState> {
        require_session_id(session_id)?;
        let path = self.state_path(user_id, session_id, key);
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => {
                let value = serde_json::from_str(&text).map_err(|error| {
                    crate::Error::Other(format!(
                        "failed to parse state value `{key}` at `{}`: {error}",
                        path.display()
                    ))
                })?;
                Ok(VersionedState {
                    value: Some(value),
                    version: UNVERSIONED,
                })
            }
            // A missing file is the ordinary "absent key" case, not a failure.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(VersionedState {
                value: None,
                version: UNVERSIONED,
            }),
            Err(error) => Err(error.into()),
        }
    }

    async fn try_save_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
        expected_version: i64,
    ) -> Result<i64> {
        // No optimistic concurrency: the write is unconditional, like `save_if_version`,
        // but a failure to land it is reported instead of dropped.
        let _ = expected_version;
        require_session_id(session_id)?;
        let path = self.state_path(user_id, session_id, key);
        let json = serde_json::to_string_pretty(&value)?;
        self.atomic_write(&path, &json).await?;
        Ok(UNVERSIONED)
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        let user_dir = self.root.join(safe_segment(&normalize_user(user_id)));
        let mut out = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&user_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    out.push(decode_segment(&entry.file_name().to_string_lossy()));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod try_variants {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("agentscope-store-{tag}-{}-{n}", std::process::id()))
    }

    /// Backend without a real error source: the default `try_*` methods stay `Ok`.
    #[tokio::test]
    async fn default_try_methods_mirror_the_infallible_ones() {
        let store = InMemoryAgentStateStore::new();
        store
            .try_save(Some("u"), "s", "k", serde_json::json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(
            store.try_get(Some("u"), "s", "k").await.unwrap(),
            Some(serde_json::json!({"a": 1}))
        );

        store
            .try_save_list(
                Some("u"),
                "s",
                "l",
                &[serde_json::json!(1), serde_json::json!(2)],
            )
            .await
            .unwrap();
        assert_eq!(
            store.try_get_list(Some("u"), "s", "l").await.unwrap(),
            vec![serde_json::json!(1), serde_json::json!(2)]
        );

        assert!(store.try_exists(Some("u"), "s").await.unwrap());
        assert_eq!(
            store.try_list_session_ids(Some("u")).await.unwrap(),
            vec!["s".to_string()]
        );

        store.try_delete_key(Some("u"), "s", "k").await.unwrap();
        assert_eq!(store.try_get(Some("u"), "s", "k").await.unwrap(), None);

        store.try_delete(Some("u"), "s").await.unwrap();
        assert!(!store.try_exists(Some("u"), "s").await.unwrap());
    }

    /// A blank `session_id` is diagnosable from the inputs alone, so the default
    /// implementation must report it instead of "succeeding".
    #[tokio::test]
    async fn default_try_methods_reject_a_blank_session_id() {
        let store = InMemoryAgentStateStore::new();
        assert!(store
            .try_save(None, "  ", "k", serde_json::json!(1))
            .await
            .is_err());
        assert!(store.try_save_list(None, "  ", "k", &[]).await.is_err());
        assert!(store.try_get(None, "  ", "k").await.is_err());
        assert!(store.try_get_list(None, "  ", "k").await.is_err());
        assert!(store.try_exists(None, "  ").await.is_err());
        assert!(store.try_delete(None, "  ").await.is_err());
        assert!(store.try_delete_key(None, "  ", "k").await.is_err());
        // Nothing was stored under the rejected session id.
        assert_eq!(store.list_session_ids(None).await, Vec::<String>::new());
    }

    struct FailingStore;

    #[async_trait]
    impl AgentStateStore for FailingStore {
        async fn save(
            &self,
            _user: Option<&str>,
            _session: &str,
            _key: &str,
            _v: serde_json::Value,
        ) {
        }
        async fn save_list(
            &self,
            _user: Option<&str>,
            _session: &str,
            _key: &str,
            _values: &[serde_json::Value],
        ) {
        }
        async fn get(
            &self,
            _user: Option<&str>,
            _session: &str,
            _key: &str,
        ) -> Option<serde_json::Value> {
            None
        }
        async fn get_list(
            &self,
            _user: Option<&str>,
            _session: &str,
            _key: &str,
        ) -> Vec<serde_json::Value> {
            Vec::new()
        }
        async fn exists(&self, _user: Option<&str>, _session: &str) -> bool {
            false
        }
        async fn delete(&self, _user: Option<&str>, _session: &str) {}
        async fn list_session_ids(&self, _user: Option<&str>) -> Vec<String> {
            Vec::new()
        }

        async fn try_save(
            &self,
            _user: Option<&str>,
            _session: &str,
            _key: &str,
            _value: serde_json::Value,
        ) -> Result<()> {
            Err(crate::Error::Other("disk full".into()))
        }
        async fn try_get(
            &self,
            _user: Option<&str>,
            _session: &str,
            _key: &str,
        ) -> Result<Option<serde_json::Value>> {
            Err(crate::Error::Other("corrupt payload".into()))
        }
    }

    /// An overriding backend's error reaches the caller instead of being flattened to
    /// "nothing stored" / "written".
    #[tokio::test]
    async fn overridden_try_methods_surface_backend_errors() {
        let store = FailingStore;
        let error = store
            .try_save(None, "s", "k", serde_json::json!(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disk full"), "{error}");
        let error = store.try_get(None, "s", "k").await.unwrap_err();
        assert!(error.to_string().contains("corrupt payload"), "{error}");
    }

    /// Root points at an existing *file*, so the session directory cannot be created:
    /// the write fails instead of being dropped.
    #[tokio::test]
    async fn json_file_store_reports_write_failures() {
        let root = temp_root("blocked");
        tokio::fs::write(&root, b"not a directory").await.unwrap();
        let store = JsonFileAgentStateStore::new(root.clone());

        assert!(store
            .try_save(None, "s", "k", serde_json::json!({"a": 1}))
            .await
            .is_err());
        assert!(store
            .try_save_list(None, "s", "k", &[serde_json::json!({ "a": 1 })])
            .await
            .is_err());

        let _ = tokio::fs::remove_file(&root).await;
    }

    #[tokio::test]
    async fn json_file_store_distinguishes_absent_from_corrupt() {
        let root = temp_root("corrupt");
        let store = JsonFileAgentStateStore::new(root.clone());

        // Absent key: mirror `get` (None) instead of reporting a failure.
        assert_eq!(store.try_get(None, "s", "k").await.unwrap(), None);
        assert_eq!(
            store.try_get_list(None, "s", "l").await.unwrap(),
            Vec::<serde_json::Value>::new()
        );

        store
            .try_save(None, "s", "k", serde_json::json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(
            store.try_get(None, "s", "k").await.unwrap(),
            Some(serde_json::json!({"a": 1}))
        );

        // A payload that exists but cannot be parsed is a real failure.
        let session_dir = root.join("__anon__").join("s");
        tokio::fs::write(session_dir.join("k.json"), b"{not json")
            .await
            .unwrap();
        assert_eq!(store.get(None, "s", "k").await, None);
        assert!(store.try_get(None, "s", "k").await.is_err());

        tokio::fs::write(session_dir.join("l.jsonl"), b"1\n{oops\n")
            .await
            .unwrap();
        assert!(store.try_get_list(None, "s", "l").await.is_err());

        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}
