//! Persistent storage for agent state, mirroring `io.agentscope.core.state`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use crate::error::Result;

/// Persistent storage interface for AgentScope agent state, keyed by a
/// `(userId, sessionId)` pair.
#[async_trait]
pub trait AgentStateStore: Send + Sync {
    /// Save a single value (full replacement).
    async fn save(&self, user_id: Option<&str>, session_id: &str, key: &str, value: serde_json::Value);

    /// Save a list of values.
    async fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        key: &str,
        values: &[serde_json::Value],
    );

    /// Get a single value, or `None` if not present.
    async fn get(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Option<serde_json::Value>;

    /// Get a list of values (empty when not present).
    async fn get_list(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Vec<serde_json::Value>;

    /// Whether a session has any persisted state.
    async fn exists(&self, user_id: Option<&str>, session_id: &str) -> bool;

    /// Delete a session and all its data.
    async fn delete(&self, user_id: Option<&str>, session_id: &str);

    /// List session identifiers under the given user namespace (`None` = anonymous).
    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String>;

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
    async fn save(&self, user_id: Option<&str>, session_id: &str, key: &str, value: serde_json::Value) {
        let _ = require_session_id(session_id);
        let mut g = self.inner.write().unwrap();
        g.entry(normalize_user(user_id)).or_default()
            .entry(session_id.to_string()).or_insert_with(SessionData::default)
            .single.insert(key.to_string(), value);
    }

    async fn save_list(&self, user_id: Option<&str>, session_id: &str, key: &str, values: &[serde_json::Value]) {
        let _ = require_session_id(session_id);
        let mut g = self.inner.write().unwrap();
        g.entry(normalize_user(user_id)).or_default()
            .entry(session_id.to_string()).or_insert_with(SessionData::default)
            .list.insert(key.to_string(), values.to_vec());
    }

    async fn get(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Option<serde_json::Value> {
        let g = self.inner.read().unwrap();
        g.get(&normalize_user(user_id))?.get(session_id)?.single.get(key).cloned()
    }

    async fn get_list(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Vec<serde_json::Value> {
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
        self.inner.read().unwrap().get(&normalize_user(user_id)).map(|m| m.contains_key(session_id)).unwrap_or(false)
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        if let Some(bucket) = self.inner.write().unwrap().get_mut(&normalize_user(user_id)) {
            bucket.remove(session_id);
        }
    }

    async fn list_session_ids(&self, user_id: Option<&str>) -> Vec<String> {
        self.inner.read().unwrap().get(&normalize_user(user_id)).map(|m| m.keys().cloned().collect()).unwrap_or_default()
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
        self.session_dir(user_id, session_id).join(format!("{}.json", key))
    }

    fn list_path(&self, user_id: Option<&str>, session_id: &str, key: &str) -> PathBuf {
        self.session_dir(user_id, session_id).join(format!("{}.jsonl", key))
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
    async fn save(&self, user_id: Option<&str>, session_id: &str, key: &str, value: serde_json::Value) {
        let session = require_session_id(session_id).unwrap_or_default();
        let path = self.state_path(user_id, &session, key);
        if let Ok(json) = serde_json::to_string_pretty(&value) {
            let _ = self.atomic_write(&path, &json).await;
        }
    }

    async fn save_list(&self, user_id: Option<&str>, session_id: &str, key: &str, values: &[serde_json::Value]) {
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

    async fn get(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Option<serde_json::Value> {
        let path = self.state_path(user_id, session_id, key);
        let text = tokio::fs::read_to_string(path).await.ok()?;
        serde_json::from_str(&text).ok()
    }

    async fn get_list(&self, user_id: Option<&str>, session_id: &str, key: &str) -> Vec<serde_json::Value> {
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
        tokio::fs::metadata(self.session_dir(user_id, session_id)).await.is_ok()
    }

    async fn delete(&self, user_id: Option<&str>, session_id: &str) {
        let _ = tokio::fs::remove_dir_all(self.session_dir(user_id, session_id)).await;
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
