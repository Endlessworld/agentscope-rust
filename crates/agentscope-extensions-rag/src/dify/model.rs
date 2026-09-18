//! Dify retrieve-API response DTOs.
//!
//! Ports `io.agentscope.core.rag.integration.dify.model.DifyResponse` and its nested
//! `Query` / `Record` / `Segment` / `DocumentInfo` classes. Field names mirror the Java
//! `@JsonProperty` annotations exactly, and every struct ignores unknown fields like
//! Jackson's `@JsonIgnoreProperties(ignoreUnknown = true)`.

use serde::{Deserialize, Serialize};

/// Root response of `POST /datasets/{dataset_id}/retrieve`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DifyResponse {
    /// Echoed query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<Query>,
    /// Retrieved records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records: Option<Vec<Record>>,
}

impl DifyResponse {
    /// Creates an empty response (Java returns this when the body parses to `null`).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Number of records, treating a missing list as zero (Java logs this count).
    pub fn record_count(&self) -> usize {
        self.records.as_ref().map_or(0, Vec::len)
    }
}

/// The `query` object of the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Query {
    /// Query content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// A single retrieval record: a segment plus its score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Record {
    /// The matched segment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment: Option<Segment>,
    /// Similarity score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// A document chunk (segment) as returned by Dify.
///
/// Java exposes a getter/setter for every field; the Rust port keeps the same field set with
/// snake_case names plus explicit `#[serde(rename = ...)]` for the API keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Segment {
    /// Segment id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Owning document id (`document_id`).
    #[serde(default, rename = "document_id", skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Chunk text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Optional canned answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// Position within the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    /// Word count (`word_count`).
    #[serde(default, rename = "word_count", skip_serializing_if = "Option::is_none")]
    pub word_count: Option<i64>,
    /// Token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<i64>,
    /// Extracted keywords.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    /// Index node id (`index_node_id`).
    #[serde(default, rename = "index_node_id", skip_serializing_if = "Option::is_none")]
    pub index_node_id: Option<String>,
    /// Index node hash (`index_node_hash`).
    #[serde(default, rename = "index_node_hash", skip_serializing_if = "Option::is_none")]
    pub index_node_hash: Option<String>,
    /// Hit count (`hit_count`).
    #[serde(default, rename = "hit_count", skip_serializing_if = "Option::is_none")]
    pub hit_count: Option<i64>,
    /// Whether the segment is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Disable timestamp (`disabled_at`).
    #[serde(default, rename = "disabled_at", skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<i64>,
    /// Who disabled it (`disabled_by`).
    #[serde(default, rename = "disabled_by", skip_serializing_if = "Option::is_none")]
    pub disabled_by: Option<String>,
    /// Indexing status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Creator (`created_by`).
    #[serde(default, rename = "created_by", skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// Creation timestamp (`created_at`).
    #[serde(default, rename = "created_at", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    /// Indexing timestamp (`indexing_at`).
    #[serde(default, rename = "indexing_at", skip_serializing_if = "Option::is_none")]
    pub indexing_at: Option<i64>,
    /// Completion timestamp (`completed_at`).
    #[serde(default, rename = "completed_at", skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    /// Indexing error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stop timestamp (`stopped_at`).
    #[serde(default, rename = "stopped_at", skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<i64>,
    /// Nested document information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentInfo>,
}

/// Nested `document` object of a segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DocumentInfo {
    /// Document id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Data source type (`data_source_type`).
    #[serde(default, rename = "data_source_type", skip_serializing_if = "Option::is_none")]
    pub data_source_type: Option<String>,
    /// Document name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "query": { "content": "what is rust" },
      "records": [
        {
          "segment": {
            "id": "seg-1",
            "document_id": "doc-1",
            "content": "Rust is a systems language.",
            "position": 1,
            "word_count": 42,
            "tokens": 7,
            "keywords": ["rust", "systems"],
            "index_node_id": "node-1",
            "index_node_hash": "hash-1",
            "hit_count": 3,
            "enabled": true,
            "created_at": 1700000000,
            "document": { "id": "doc-1", "name": "intro.md", "data_source_type": "upload_file" },
            "unknown_field": 123
          },
          "score": 0.95
        }
      ],
      "extra_top_level": true
    }"#;

    #[test]
    fn deserializes_java_shaped_payload_and_ignores_unknowns() {
        let parsed: DifyResponse = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(parsed.record_count(), 1);
        assert_eq!(parsed.query.as_ref().unwrap().content.as_deref(), Some("what is rust"));

        let record = &parsed.records.as_ref().unwrap()[0];
        assert_eq!(record.score, Some(0.95));
        let segment = record.segment.as_ref().unwrap();
        assert_eq!(segment.id.as_deref(), Some("seg-1"));
        assert_eq!(segment.document_id.as_deref(), Some("doc-1"));
        assert_eq!(segment.content.as_deref(), Some("Rust is a systems language."));
        assert_eq!(segment.position, Some(1));
        assert_eq!(segment.word_count, Some(42));
        assert_eq!(segment.tokens, Some(7));
        assert_eq!(segment.keywords.as_ref().unwrap(), &["rust", "systems"]);
        assert_eq!(segment.index_node_id.as_deref(), Some("node-1"));
        assert_eq!(segment.index_node_hash.as_deref(), Some("hash-1"));
        assert_eq!(segment.hit_count, Some(3));
        assert_eq!(segment.enabled, Some(true));
        assert_eq!(segment.created_at, Some(1_700_000_000));

        let doc = segment.document.as_ref().unwrap();
        assert_eq!(doc.id.as_deref(), Some("doc-1"));
        assert_eq!(doc.name.as_deref(), Some("intro.md"));
        assert_eq!(doc.data_source_type.as_deref(), Some("upload_file"));
    }

    #[test]
    fn empty_object_deserializes_to_empty_response() {
        let parsed: DifyResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, DifyResponse::empty());
        assert_eq!(parsed.record_count(), 0);
    }

    #[test]
    fn round_trips_using_api_field_names() {
        let parsed: DifyResponse = serde_json::from_str(SAMPLE).unwrap();
        let value = serde_json::to_value(&parsed).unwrap();
        let segment = &value["records"][0]["segment"];
        assert!(segment.get("document_id").is_some());
        assert!(segment.get("word_count").is_some());
        assert!(segment.get("index_node_hash").is_some());
        assert!(segment.get("hit_count").is_some());
        assert_eq!(segment["document"]["data_source_type"], "upload_file");
    }

    #[test]
    fn secondary_round_trip_is_stable() {
        let parsed: DifyResponse = serde_json::from_str(SAMPLE).unwrap();
        let json = serde_json::to_string(&parsed).unwrap();
        let again: DifyResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, again);
    }
}
