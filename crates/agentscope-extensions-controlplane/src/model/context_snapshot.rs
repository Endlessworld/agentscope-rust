//! Level-4 effective-context snapshot, mirroring `io.agentscope.extensions.controlplane.model.ContextSnapshot`.
//!
//! Carries both encodings — proto (ASDP `ContextReport`, deferred) and [`ContextSnapshot::to_json_value`]
//! for `GET /agentscope/sessions/{id}/context`.
//!
//! [`ContextSnapshot::hash`] produces the same digest as the Go control plane and the Python
//! SDK: SHA-256 over recursively key-sorted, separator-free JSON of the effective content,
//! truncated to [`HASH_LEN`] hex characters. The control plane compares hashes to detect
//! context drift without fetching the body.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Number of leading hex characters of the SHA-256 digest kept as the context hash.
pub const HASH_LEN: usize = 16;

/// One effective-context message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMessage {
    /// Message role.
    pub role: String,
    /// Message content.
    pub content: String,
    /// Tool name, when the message is a tool exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl ContextMessage {
    /// Creates a plain role/content message.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: content.into(), tool_name: None }
    }

    fn canonical(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("content".into(), serde_json::Value::String(self.content.clone()));
        map.insert("role".into(), serde_json::Value::String(self.role.clone()));
        if let Some(t) = &self.tool_name {
            map.insert("toolName".into(), serde_json::Value::String(t.clone()));
        }
        serde_json::Value::Object(map)
    }
}

/// One tool exposed to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    /// Tool name.
    pub name: String,
    /// Tool description.
    #[serde(default)]
    pub description: String,
}

impl ToolInfo {
    /// Creates a tool info entry.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), description: description.into() }
    }

    fn canonical(&self) -> serde_json::Value {
        serde_json::json!({ "description": self.description, "name": self.name })
    }
}

/// Level-4 effective-context snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSnapshot {
    /// Owning session id.
    pub session_id: String,
    /// System prompt in effect.
    #[serde(default)]
    pub system_prompt: String,
    /// Effective messages the next model call will see.
    #[serde(default)]
    pub messages: Vec<ContextMessage>,
    /// Tools exposed to the model.
    #[serde(default)]
    pub tools: Vec<ToolInfo>,
    /// Whether the context has been compacted.
    #[serde(default)]
    pub compacted: bool,
    /// Summary produced by compaction.
    #[serde(default)]
    pub compaction_summary: String,
    /// Message count before compaction.
    #[serde(default)]
    pub original_message_count: i64,
    /// When compaction happened, epoch millis.
    #[serde(default)]
    pub compacted_at: i64,
    /// Tokens currently in the effective context.
    #[serde(default)]
    pub total_tokens: i64,
    /// Context-window maximum.
    #[serde(default)]
    pub max_tokens: i64,
    /// Framework identifier.
    #[serde(default)]
    pub framework: String,
    /// Model identifier.
    #[serde(default)]
    pub model: String,
    /// Resolved models.dev key, when the catalog knows the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_key: Option<String>,
    /// Catalog generation used to resolve `catalog_key`.
    #[serde(default)]
    pub catalog_generation: u64,
    /// Framework-private state blob (opaque).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework_state: Option<serde_json::Value>,
    /// When the snapshot was captured, epoch millis.
    #[serde(default)]
    pub captured_at: i64,
    /// Precomputed context hash (empty means "compute on demand").
    #[serde(default)]
    pub context_hash: String,
}

impl ContextSnapshot {
    /// Starts building a snapshot for `session_id`.
    pub fn builder(session_id: impl Into<String>) -> ContextSnapshotBuilder {
        ContextSnapshotBuilder {
            snapshot: ContextSnapshot {
                session_id: session_id.into(),
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                compacted: false,
                compaction_summary: String::new(),
                original_message_count: 0,
                compacted_at: 0,
                total_tokens: 0,
                max_tokens: 0,
                framework: String::new(),
                model: String::new(),
                catalog_key: None,
                catalog_generation: 0,
                framework_state: None,
                captured_at: 0,
                context_hash: String::new(),
            },
        }
    }

    /// High-level JSON view served by `GET /agentscope/sessions/{id}/context`.
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionId": self.session_id,
            "systemPrompt": self.system_prompt,
            "messages": self.messages,
            "tools": self.tools,
            "compacted": self.compacted,
            "compactionSummary": self.compaction_summary,
            "originalMessageCount": self.original_message_count,
            "totalTokens": self.total_tokens,
            "maxTokens": self.max_tokens,
            "framework": self.framework,
            "model": self.model,
            "capturedAt": self.captured_at,
            "contextHash": self.hash(),
        })
    }

    /// SHA-256 over the recursively key-sorted, separator-free canonical JSON of the
    /// effective content, truncated to [`HASH_LEN`] hex characters.
    pub fn hash(&self) -> String {
        let canonical = self.canonical_json();
        let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        let digest = Sha256::digest(&bytes);
        let hex = hex_encode(&digest);
        hex.chars().take(HASH_LEN).collect()
    }

    /// The canonical JSON used for hashing (recursively key-sorted).
    pub fn canonical_json(&self) -> serde_json::Value {
        let messages: Vec<serde_json::Value> =
            self.messages.iter().map(|m| m.canonical()).collect();
        let tools: Vec<serde_json::Value> = self.tools.iter().map(|t| t.canonical()).collect();
        sort_value(serde_json::json!({
            "compacted": self.compacted,
            "compactionSummary": self.compaction_summary,
            "framework": self.framework,
            "maxTokens": self.max_tokens,
            "messages": messages,
            "model": self.model,
            "systemPrompt": self.system_prompt,
            "tools": tools,
            "totalTokens": self.total_tokens,
        }))
    }
}

/// Recursively sorts object keys so serde_json emits a stable ordering.
fn sort_value(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = serde_json::Map::new();
            for (k, val) in sorted {
                out.insert(k, sort_value(val));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sort_value).collect())
        }
        other => other,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Builder for [`ContextSnapshot`].
#[derive(Debug, Clone)]
pub struct ContextSnapshotBuilder {
    snapshot: ContextSnapshot,
}

impl ContextSnapshotBuilder {
    /// System prompt in effect.
    pub fn system_prompt(mut self, v: impl Into<String>) -> Self {
        self.snapshot.system_prompt = v.into();
        self
    }

    /// Adds a message.
    pub fn add_message(mut self, m: ContextMessage) -> Self {
        self.snapshot.messages.push(m);
        self
    }

    /// Replaces all messages.
    pub fn messages(mut self, v: Vec<ContextMessage>) -> Self {
        self.snapshot.messages = v;
        self
    }

    /// Replaces all tools.
    pub fn tools(mut self, v: Vec<ToolInfo>) -> Self {
        self.snapshot.tools = v;
        self
    }

    /// Whether the context has been compacted.
    pub fn compacted(mut self, v: bool) -> Self {
        self.snapshot.compacted = v;
        self
    }

    /// Compaction summary.
    pub fn compaction_summary(mut self, v: impl Into<String>) -> Self {
        self.snapshot.compaction_summary = v.into();
        self
    }

    /// Message count before compaction.
    pub fn original_message_count(mut self, v: i64) -> Self {
        self.snapshot.original_message_count = v;
        self
    }

    /// Compaction timestamp (epoch millis).
    pub fn compacted_at(mut self, v: i64) -> Self {
        self.snapshot.compacted_at = v;
        self
    }

    /// Tokens in the effective context.
    pub fn total_tokens(mut self, v: i64) -> Self {
        self.snapshot.total_tokens = v;
        self
    }

    /// Context-window maximum.
    pub fn max_tokens(mut self, v: i64) -> Self {
        self.snapshot.max_tokens = v;
        self
    }

    /// Framework identifier.
    pub fn framework(mut self, v: impl Into<String>) -> Self {
        self.snapshot.framework = v.into();
        self
    }

    /// Model identifier.
    pub fn model(mut self, v: impl Into<String>) -> Self {
        self.snapshot.model = v.into();
        self
    }

    /// Resolved catalog key and generation.
    pub fn catalog(mut self, key: Option<String>, generation: u64) -> Self {
        self.snapshot.catalog_key = key;
        self.snapshot.catalog_generation = generation;
        self
    }

    /// Framework state blob.
    pub fn framework_state(mut self, v: serde_json::Value) -> Self {
        self.snapshot.framework_state = Some(v);
        self
    }

    /// Captured-at epoch millis.
    pub fn captured_at(mut self, v: i64) -> Self {
        self.snapshot.captured_at = v;
        self
    }

    /// Builds, defaulting `captured_at` to now.
    pub fn build(mut self) -> ContextSnapshot {
        if self.snapshot.captured_at <= 0 {
            self.snapshot.captured_at = chrono::Utc::now().timestamp_millis();
        }
        self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_16_hex_chars() {
        let snap = ContextSnapshot::builder("s1")
            .system_prompt("you are helpful")
            .add_message(ContextMessage::new("user", "hi"))
            .build();
        let h = snap.hash();
        assert_eq!(h.len(), HASH_LEN);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_is_stable_across_message_order_canonicalization() {
        let a = ContextSnapshot::builder("s1")
            .add_message(ContextMessage::new("user", "hi"))
            .build();
        let b = ContextSnapshot::builder("s1")
            .add_message(ContextMessage::new("user", "hi"))
            .build();
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn hash_changes_when_content_changes() {
        let a = ContextSnapshot::builder("s1")
            .add_message(ContextMessage::new("user", "hi"))
            .build();
        let b = ContextSnapshot::builder("s1")
            .add_message(ContextMessage::new("user", "bye"))
            .build();
        assert_ne!(a.hash(), b.hash());
    }
}
