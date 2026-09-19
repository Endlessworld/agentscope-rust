//! Conversion of Bailian retrieve nodes into AgentScope [`Document`]s.
//!
//! Ports `BailianDocumentConverter`: it promotes `doc_id` / `_id` out of the node metadata into
//! the [`DocumentMetadata`] ids and copies every remaining metadata key into the payload.

use std::collections::HashMap;

use agentscope_core::message::{ContentBlock, TextBlock};
use serde_json::{Map, Value};

use crate::bailian::model::{RetrieveNode, RetrieveResponseBody};
use crate::model::{Document, DocumentMetadata};

/// Stateless converter between the Bailian response model and [`Document`].
///
/// Java exposed only static methods and a private constructor; an uninhabited unit struct keeps
/// the same call shape (`BailianDocumentConverter::from_response(...)`).
pub struct BailianDocumentConverter;

impl BailianDocumentConverter {
    /// Converts every node of a response body, skipping nodes that fail conversion.
    ///
    /// Mirrors `fromBailianResponse`: a null body or a null/empty `data.nodes` yields an empty
    /// vector.
    pub fn from_response(response: &RetrieveResponseBody) -> Vec<Document> {
        Self::convert_to_documents(response.nodes())
    }

    /// Converts a node list, skipping `None` results.
    pub fn convert_to_documents(nodes: &[RetrieveNode]) -> Vec<Document> {
        nodes.iter().filter_map(Self::from_bailian_node).collect()
    }

    /// Converts a single node, returning `None` when the text is missing or blank.
    ///
    /// Mirrors `fromBailianNode`:
    /// * `text` is required (blank text is skipped);
    /// * `doc_id` falls back to `"unknown"` when absent or empty;
    /// * `chunk_id` comes from the `_id` metadata key and may be empty (Java passed it through
    ///   without a fallback, and its `DocumentMetadata` only rejects `null`);
    /// * the payload keeps every metadata key except the two reserved ones;
    /// * the score is only set when the node carries one.
    ///
    /// Unlike Java, this port does not need a `try/catch` escape hatch: the only failing
    /// operations (JSON serialization) cannot fail for the values involved here.
    pub fn from_bailian_node(node: &RetrieveNode) -> Option<Document> {
        let text = node.text.as_deref().unwrap_or("");
        if text.trim().is_empty() {
            return None;
        }

        let metadata = node.metadata_map();
        let doc_id = match metadata_string(&metadata, "doc_id") {
            Some(value) if !value.is_empty() => value,
            _ => "unknown".to_string(),
        };
        let chunk_id = metadata_string(&metadata, "_id").unwrap_or_default();

        let mut payload = Map::new();
        for (key, value) in &metadata {
            if key != "doc_id" && key != "_id" {
                payload.insert(key.clone(), value.clone());
            }
        }

        let mut document = Document::new(DocumentMetadata::with_payload(
            ContentBlock::Text(TextBlock::new(text)),
            doc_id,
            chunk_id.clone(),
            Value::Object(payload),
        ));

        if let Some(score) = node.score {
            document.set_score(score);
        }

        tracing::debug!(chunk_id = %chunk_id, "converted Bailian node");
        Some(document)
    }
}

/// Reads a metadata entry as a string.
///
/// Reproduces Java `extractStringFromMetadata`: strings are returned as-is, other scalars use
/// their `toString()` form, and a missing key yields `None`.
fn metadata_string(metadata: &HashMap<String, Value>, key: &str) -> Option<String> {
    match metadata.get(key) {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(match other {
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            other => other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bailian::model::RetrieveResponseData;

    fn node(text: &str, score: Option<f64>, metadata: Value) -> RetrieveNode {
        RetrieveNode {
            text: Some(text.to_string()),
            score,
            metadata: Some(metadata),
        }
    }

    #[test]
    fn converts_node_and_promotes_reserved_metadata() {
        let node = node(
            "Test document content",
            Some(0.95),
            serde_json::json!({
                "doc_id": "doc123",
                "_id": "chunk456",
                "doc_name": "test.pdf",
                "title": "Test Document"
            }),
        );
        let document = BailianDocumentConverter::from_bailian_node(&node).unwrap();
        assert_eq!(document.score, Some(0.95));
        assert_eq!(document.metadata.doc_id, "doc123");
        assert_eq!(document.metadata.chunk_id, "chunk456");
        assert_eq!(document.metadata.content_text(), "Test document content");

        // Reserved keys are promoted, not duplicated into the payload.
        assert!(!document.has_payload_key("doc_id"));
        assert!(!document.has_payload_key("_id"));
        assert_eq!(
            document.payload_value("doc_name").unwrap(),
            &serde_json::json!("test.pdf")
        );
        assert_eq!(
            document.payload_value("title").unwrap(),
            &serde_json::json!("Test Document")
        );
    }

    #[test]
    fn blank_or_missing_text_is_skipped() {
        assert!(BailianDocumentConverter::from_bailian_node(&node("", None, Value::Null)).is_none());
        assert!(
            BailianDocumentConverter::from_bailian_node(&node("   ", None, Value::Null)).is_none()
        );
        let no_text = RetrieveNode {
            text: None,
            ..Default::default()
        };
        assert!(BailianDocumentConverter::from_bailian_node(&no_text).is_none());
    }

    #[test]
    fn doc_id_falls_back_to_unknown_when_missing_or_empty() {
        let missing = node("x", None, serde_json::json!({}));
        assert_eq!(
            BailianDocumentConverter::from_bailian_node(&missing)
                .unwrap()
                .metadata
                .doc_id,
            "unknown"
        );
        let empty = node("x", None, serde_json::json!({"doc_id": ""}));
        assert_eq!(
            BailianDocumentConverter::from_bailian_node(&empty)
                .unwrap()
                .metadata
                .doc_id,
            "unknown"
        );
    }

    #[test]
    fn chunk_id_defaults_to_empty_and_non_string_metadata_is_stringified() {
        let node = node(
            "x",
            None,
            serde_json::json!({"doc_id": 42, "_id": 7, "page": 3}),
        );
        let document = BailianDocumentConverter::from_bailian_node(&node).unwrap();
        assert_eq!(document.metadata.doc_id, "42");
        assert_eq!(document.metadata.chunk_id, "7");
        assert_eq!(document.payload_value("page").unwrap(), &serde_json::json!(3));
    }

    #[test]
    fn non_object_metadata_still_produces_a_document() {
        let node = RetrieveNode {
            text: Some("x".to_string()),
            score: None,
            metadata: Some(serde_json::json!([1, 2, 3])),
        };
        let document = BailianDocumentConverter::from_bailian_node(&node).unwrap();
        assert_eq!(document.metadata.doc_id, "unknown");
        assert!(document.payload().is_empty());
        assert_eq!(document.score, None);
    }

    #[test]
    fn from_response_handles_null_and_empty_data() {
        assert!(BailianDocumentConverter::from_response(&RetrieveResponseBody::default()).is_empty());

        let with_empty_nodes = RetrieveResponseBody {
            code: Some("0".to_string()),
            data: Some(RetrieveResponseData { nodes: vec![] }),
            ..Default::default()
        };
        assert!(BailianDocumentConverter::from_response(&with_empty_nodes).is_empty());
    }

    #[test]
    fn from_response_converts_every_valid_node_and_skips_invalid() {
        let body = RetrieveResponseBody {
            code: Some("0".to_string()),
            data: Some(RetrieveResponseData {
                nodes: vec![
                    node("first", Some(0.9), serde_json::json!({"doc_id": "d1"})),
                    node("", Some(0.5), serde_json::json!({})),
                    node("third", None, serde_json::json!({"doc_id": "d3"})),
                ],
            }),
            ..Default::default()
        };
        let documents = BailianDocumentConverter::from_response(&body);
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].score, Some(0.9));
        assert_eq!(documents[1].metadata.doc_id, "d3");
        assert_eq!(documents[1].score, None);
    }

    #[test]
    fn response_parsed_from_pascal_case_json_converts() {
        let body: RetrieveResponseBody = serde_json::from_str(
            r#"{"Code":"0","Data":{"Nodes":[{"Text":"chunk","Score":0.95,"Metadata":{"doc_id":"doc123","_id":"chunk456"}}]}}"#,
        )
        .unwrap();
        let documents = BailianDocumentConverter::from_response(&body);
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].metadata.doc_id, "doc123");
        assert_eq!(documents[0].metadata.chunk_id, "chunk456");
        assert_eq!(documents[0].score, Some(0.95));
    }
}
