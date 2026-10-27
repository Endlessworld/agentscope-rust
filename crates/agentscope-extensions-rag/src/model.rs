//! Core RAG data model: [`Document`], [`DocumentMetadata`] and [`RetrieveConfig`].
//!
//! Ported from `io.agentscope.core.rag.model` (Java core). That package is marked
//! `@Deprecated(forRemoval = true)` upstream and has **no** Rust counterpart in
//! `agentscope-core`, so the model lives here (see tracker §4.2). It is intentionally
//! framework-agnostic: the only `agentscope-core` dependency is the message model
//! (`ContentBlock`), which keeps the model usable from the core `Msg` mapping layer.

use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::uuid_v3_from_name;

/// Metadata attached to a [`Document`] chunk.
///
/// Mirrors `io.agentscope.core.rag.model.DocumentMetadata`: a content block plus the
/// owning document id, the chunk id inside that document, and an arbitrary payload map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentMetadata {
    /// The chunk content (text, image, ...).
    pub content: ContentBlock,
    /// Identifier of the owning document (camelCase JSON key `docId`, like Java's
    /// Jackson bean naming).
    #[serde(rename = "docId")]
    pub doc_id: String,
    /// Identifier of this chunk within the document (`chunkId`).
    #[serde(rename = "chunkId")]
    pub chunk_id: String,
    /// Business-specific metadata (filename, department, tags, ...). Never `null`.
    #[serde(default)]
    pub payload: Value,
}

/// Renders a content block as text.
///
/// Text blocks yield their text; every other block type is rendered as its JSON form, which
/// stands in for Java's `ContentBlock.toString()` (the core Rust `ContentBlock` does not
/// implement `Display`).
pub fn content_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(t) => t.text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

impl DocumentMetadata {
    /// Creates metadata without a payload.
    pub fn new(
        content: ContentBlock,
        doc_id: impl Into<String>,
        chunk_id: impl Into<String>,
    ) -> Self {
        Self {
            content,
            doc_id: doc_id.into(),
            chunk_id: chunk_id.into(),
            payload: Value::Object(serde_json::Map::new()),
        }
    }

    /// Creates metadata with a JSON payload object.
    pub fn with_payload(
        content: ContentBlock,
        doc_id: impl Into<String>,
        chunk_id: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            content,
            doc_id: doc_id.into(),
            chunk_id: chunk_id.into(),
            payload,
        }
    }

    /// Creates metadata from a plain text chunk.
    pub fn text(
        text: impl Into<String>,
        doc_id: impl Into<String>,
        chunk_id: impl Into<String>,
    ) -> Self {
        Self::new(ContentBlock::Text(TextBlock::new(text)), doc_id, chunk_id)
    }

    /// Payload as an object map, or an empty map when absent / not an object.
    pub fn payload_map(&self) -> &serde_json::Map<String, Value> {
        static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> =
            std::sync::OnceLock::new();
        match &self.payload {
            Value::Object(m) => m,
            _ => EMPTY.get_or_init(serde_json::Map::new),
        }
    }

    /// Returns a payload value by key.
    pub fn payload_value(&self, key: &str) -> Option<&Value> {
        self.payload_map().get(key)
    }

    /// Whether a payload key exists.
    pub fn has_payload_key(&self, key: &str) -> bool {
        self.payload_map().contains_key(key)
    }

    /// Text content of the chunk; for non-text blocks this returns their JSON form,
    /// matching Java `getContentText()` closely enough for logging/prompt use.
    pub fn content_text(&self) -> String {
        content_block_text(&self.content)
    }
}

/// A RAG document chunk: metadata plus an optional embedding and similarity score.
///
/// The `id` is a deterministic version-3 UUID over `{doc_id, chunk_id, content}`,
/// exactly like Java's `UUID.nameUUIDFromBytes` (see [`crate::hash::uuid_v3_from_name`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Deterministic id.
    pub id: String,
    /// Chunk metadata.
    pub metadata: DocumentMetadata,
    /// Embedding vector, when computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f64>>,
    /// Similarity score, when returned by a search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Logical vector-space name, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_name: Option<String>,
}

// docs: keep the field document name stable for consumers.
#[allow(clippy::wrong_self_convention)]
impl Document {
    /// Creates a document from metadata, deriving its deterministic id.
    pub fn new(metadata: DocumentMetadata) -> Self {
        let id = generate_document_id(&metadata);
        Self {
            id,
            metadata,
            embedding: None,
            score: None,
            vector_name: None,
        }
    }

    /// Creates a document with an explicit id (used when a backend supplies its own id).
    pub fn with_id(id: impl Into<String>, metadata: DocumentMetadata) -> Self {
        Self {
            id: id.into(),
            metadata,
            embedding: None,
            score: None,
            vector_name: None,
        }
    }

    /// Builder-style setter for the embedding.
    pub fn set_embedding(&mut self, embedding: Vec<f64>) {
        self.embedding = Some(embedding);
    }

    /// Builder-style setter for the score.
    pub fn set_score(&mut self, score: f64) {
        self.score = Some(score);
    }

    /// Convenience view over the payload map.
    pub fn payload(&self) -> &serde_json::Map<String, Value> {
        self.metadata.payload_map()
    }

    /// Returns a payload value by key (mirrors Java `Document.getPayloadValue`).
    pub fn payload_value(&self, key: &str) -> Option<&Value> {
        self.metadata.payload_value(key)
    }

    /// Whether a payload key exists (mirrors Java `Document.hasPayloadKey`).
    pub fn has_payload_key(&self, key: &str) -> bool {
        self.metadata.has_payload_key(key)
    }
}

/// Derives the deterministic document id (version-3 UUID) from metadata.
///
/// Reproduces Java `Document.generateDocumentId`: serialize an ordered map with
/// `doc_id` / `chunk_id` / `content` and hash it with MD5 into a UUIDv3.
fn generate_document_id(metadata: &DocumentMetadata) -> String {
    #[derive(Serialize)]
    struct KeyMap<'a> {
        doc_id: &'a str,
        chunk_id: &'a str,
        content: &'a ContentBlock,
    }

    let key = KeyMap {
        doc_id: &metadata.doc_id,
        chunk_id: &metadata.chunk_id,
        content: &metadata.content,
    };

    let json = match serde_json::to_string(&key) {
        Ok(json) => json,
        // `ContentBlock` serialization cannot realistically fail; fall back to a
        // debug-free representation rather than panicking.
        Err(_) => format!("\"{}\"\"{}\"", metadata.doc_id, metadata.chunk_id),
    };

    uuid_v3_from_name(json.as_bytes())
}

/// Configuration for retrieval operations.
///
/// Mirrors `io.agentscope.core.rag.model.RetrieveConfig` (builder defaults: `limit = 5`,
/// `score_threshold = 0.5`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrieveConfig {
    /// Maximum number of documents to return.
    pub limit: usize,
    /// Minimum similarity score (inclusive) required for a document to be returned.
    pub score_threshold: f64,
    /// Optional logical vector-space name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_name: Option<String>,
    /// Optional conversation history used by context-aware backends (e.g. Bailian).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_history: Option<Vec<Msg>>,
}

impl Default for RetrieveConfig {
    fn default() -> Self {
        Self {
            limit: 5,
            score_threshold: 0.5,
            vector_name: None,
            conversation_history: None,
        }
    }
}

impl RetrieveConfig {
    /// Creates a config with explicit `limit` and `score_threshold`.
    pub fn new(limit: usize, score_threshold: f64) -> Self {
        Self {
            limit,
            score_threshold,
            ..Default::default()
        }
    }

    /// Sets the logical vector name.
    pub fn with_vector_name(mut self, name: impl Into<String>) -> Self {
        self.vector_name = Some(name.into());
        self
    }

    /// Sets the conversation history.
    pub fn with_conversation_history(mut self, history: Vec<Msg>) -> Self {
        self.conversation_history = Some(history);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_java() {
        let cfg = RetrieveConfig::default();
        assert_eq!(cfg.limit, 5);
        assert!((cfg.score_threshold - 0.5).abs() < f64::EPSILON);
        assert!(cfg.vector_name.is_none());
    }

    #[test]
    fn document_id_is_deterministic_and_ignores_score() {
        let meta = DocumentMetadata::text("hello world", "doc-1", "0");
        let mut a = Document::new(meta.clone());
        let mut b = Document::new(meta);
        assert_eq!(a.id, b.id);
        assert_eq!(a.id.len(), 36);

        a.set_score(0.9);
        b.set_score(0.1);
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn document_id_changes_with_content() {
        let a = Document::new(DocumentMetadata::text("a", "d", "0"));
        let b = Document::new(DocumentMetadata::text("b", "d", "0"));
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn metadata_serializes_with_java_field_names() {
        let meta = DocumentMetadata::text("body", "d-1", "c-0");
        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["docId"], "d-1");
        assert_eq!(json["chunkId"], "c-0");
        assert_eq!(json["content"]["type"], "text");
        assert_eq!(json["content"]["text"], "body");
    }

    #[test]
    fn metadata_round_trips() {
        let payload = serde_json::json!({"filename": "report.pdf", "tags": ["a", "b"]});
        let meta = DocumentMetadata::with_payload(
            ContentBlock::Text(TextBlock::new("x")),
            "d",
            "0",
            payload.clone(),
        );
        let json = serde_json::to_string(&meta).unwrap();
        let back: DocumentMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.payload, payload);
        assert_eq!(back, meta);
    }

    #[test]
    fn document_round_trips_with_embedding() {
        let mut doc = Document::new(DocumentMetadata::text("x", "d", "0"));
        doc.set_embedding(vec![1.0, 2.0, 3.0]);
        doc.set_score(0.75);
        let json = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(back, doc);
    }
}
