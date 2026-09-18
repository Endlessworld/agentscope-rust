//! Conversion of RAGFlow chunks into AgentScope [`Document`]s.
//!
//! Ports `io.agentscope.core.rag.integration.ragflow.RAGFlowDocumentConverter`, including
//! the payload assembly of `buildPayloadFromChunk` (fields already consumed as ids/content
//! are skipped; empty strings are omitted).

use serde_json::{json, Map, Value};

use crate::model::{Document, DocumentMetadata};
use crate::ragflow::model::{RAGFlowChunk, RAGFlowResponse};

/// Stateless converter between the RAGFlow response model and [`Document`].
pub struct RAGFlowDocumentConverter;

impl RAGFlowDocumentConverter {
    /// Converts every chunk of a response, skipping invalid ones.
    ///
    /// Mirrors the `RAGFlowKnowledge.retrieve` mapping: a missing payload or a missing
    /// chunk list yields an empty vector.
    pub fn from_response(response: &RAGFlowResponse) -> Vec<Document> {
        Self::convert_to_documents(response.chunks())
    }

    /// Converts a chunk list, skipping `None` (blank content) results.
    pub fn convert_to_documents(chunks: &[RAGFlowChunk]) -> Vec<Document> {
        chunks.iter().filter_map(Self::convert_to_document).collect()
    }

    /// Converts a single chunk, returning `None` when the content is missing or blank.
    ///
    /// Mirrors Java `convertToDocument`:
    /// * content is required (blank content is skipped, with a warning logged),
    /// * `doc_id` falls back to the chunk id and then `"unknown"`,
    /// * `chunk_id` falls back to `"0"`,
    /// * the score prefers the legacy `score` field and defaults to `0.0`.
    pub fn convert_to_document(chunk: &RAGFlowChunk) -> Option<Document> {
        let text = chunk.content.as_deref().unwrap_or("");
        if text.trim().is_empty() {
            return None;
        }

        let doc_id = match non_empty(chunk.document_id.as_deref()) {
            Some(id) => id.to_string(),
            None => non_empty(chunk.id.as_deref()).unwrap_or("unknown").to_string(),
        };
        let chunk_id = non_empty(chunk.id.as_deref()).unwrap_or("0").to_string();

        let payload = Self::build_payload_from_chunk(chunk);
        let mut document = Document::new(DocumentMetadata::with_payload(
            agentscope_core::message::ContentBlock::Text(
                agentscope_core::message::TextBlock::new(text),
            ),
            doc_id,
            chunk_id.clone(),
            Value::Object(payload),
        ));

        // Java always sets a score here (`chunk.getScore()` or 0.0).
        document.set_score(chunk.effective_score().unwrap_or(0.0));

        if let Some(document_name) = document.payload_value("document_name") {
            tracing::debug!(document_name = %document_name, chunk_id = %chunk_id, "converted RAGFlow chunk");
        }
        Some(document)
    }

    /// Builds the payload map from the chunk's metadata fields.
    ///
    /// Mirrors Java `buildPayloadFromChunk`; `doc_id`/`id`/`content` are deliberately not
    /// duplicated because they are already promoted to the document metadata.
    fn build_payload_from_chunk(chunk: &RAGFlowChunk) -> Map<String, Value> {
        let mut payload = Map::new();

        if let Some(value) = &chunk.document_keyword {
            payload.insert("document_keyword".to_string(), json!(value));
        }
        if let Some(value) = &chunk.kb_id {
            payload.insert("kb_id".to_string(), json!(value));
        }
        if let Some(value) = non_empty(chunk.content_ltks.as_deref()) {
            payload.insert("content_ltks".to_string(), json!(value));
        }
        if let Some(value) = non_empty(chunk.highlight.as_deref()) {
            payload.insert("highlight".to_string(), json!(value));
        }
        if let Some(value) = chunk.vector_similarity {
            payload.insert("vector_similarity".to_string(), json!(value));
        }
        if let Some(value) = chunk.term_similarity {
            payload.insert("term_similarity".to_string(), json!(value));
        }
        if let Some(positions) = chunk.positions.as_ref().filter(|p| !p.is_empty()) {
            payload.insert("positions".to_string(), json!(positions));
        }
        if let Some(value) = non_empty(chunk.image_id.as_deref()) {
            payload.insert("image_id".to_string(), json!(value));
        }
        if let Some(keywords) = chunk
            .important_keywords
            .as_ref()
            .filter(|k| !k.is_empty())
        {
            payload.insert("important_keywords".to_string(), json!(keywords));
        }
        if let Some(metadata) = chunk.metadata.as_ref().filter(|v| is_non_empty_object(v)) {
            payload.insert("metadata".to_string(), metadata.clone());
        }
        if let Some(value) = &chunk.dataset_id {
            payload.insert("dataset_id".to_string(), json!(value));
        }
        if let Some(value) = &chunk.document_name {
            payload.insert("document_name".to_string(), json!(value));
        }

        payload
    }
}

/// Returns `Some(trimmed-free value)` only when the option holds a non-empty string.
///
/// Mirrors the Java `x != null && !x.isEmpty()` guards (Java does not trim).
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// Whether the value is a JSON object with at least one entry (Java `!map.isEmpty()`).
fn is_non_empty_object(value: &Value) -> bool {
    matches!(value, Value::Object(map) if !map.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ragflow::model::RAGFlowResponse;

    fn chunk(id: &str, content: &str, document_id: &str, score: f64) -> RAGFlowChunk {
        RAGFlowChunk {
            id: Some(id.to_string()),
            content: Some(content.to_string()),
            document_id: Some(document_id.to_string()),
            score: Some(score),
            ..Default::default()
        }
    }

    #[test]
    fn empty_input_yields_no_documents() {
        assert!(RAGFlowDocumentConverter::convert_to_documents(&[]).is_empty());
        assert!(RAGFlowDocumentConverter::from_response(&RAGFlowResponse::default()).is_empty());
    }

    #[test]
    fn converts_multiple_chunks() {
        let chunks = vec![
            chunk("chunk-1", "Content 1", "doc-1", 0.9),
            chunk("chunk-2", "Content 2", "doc-2", 0.8),
        ];
        let documents = RAGFlowDocumentConverter::convert_to_documents(&chunks);
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].metadata.doc_id, "doc-1");
        assert_eq!(documents[0].metadata.chunk_id, "chunk-1");
        assert_eq!(documents[0].metadata.content_text(), "Content 1");
        assert_eq!(documents[0].score, Some(0.9));
    }

    #[test]
    fn blank_and_missing_content_are_skipped() {
        let chunks = vec![
            RAGFlowChunk {
                id: Some("chunk-1".to_string()),
                content: Some(String::new()),
                ..Default::default()
            },
            RAGFlowChunk {
                id: Some("chunk-2".to_string()),
                content: Some("   ".to_string()),
                ..Default::default()
            },
            RAGFlowChunk {
                id: Some("chunk-3".to_string()),
                content: None,
                ..Default::default()
            },
            chunk("chunk-4", "Valid content", "doc", 0.5),
        ];
        let documents = RAGFlowDocumentConverter::convert_to_documents(&chunks);
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].metadata.chunk_id, "chunk-4");
    }

    #[test]
    fn doc_id_falls_back_to_chunk_id_then_unknown() {
        let with_chunk_id = RAGFlowChunk {
            id: Some("chunk-1".to_string()),
            content: Some("txt".to_string()),
            ..Default::default()
        };
        let document = RAGFlowDocumentConverter::convert_to_document(&with_chunk_id).unwrap();
        assert_eq!(document.metadata.doc_id, "chunk-1");

        let without_ids = RAGFlowChunk {
            content: Some("txt".to_string()),
            ..Default::default()
        };
        let document = RAGFlowDocumentConverter::convert_to_document(&without_ids).unwrap();
        assert_eq!(document.metadata.doc_id, "unknown");
        assert_eq!(document.metadata.chunk_id, "0");
    }

    #[test]
    fn score_defaults_to_zero_when_absent() {
        let no_score = RAGFlowChunk {
            id: Some("c".to_string()),
            content: Some("txt".to_string()),
            ..Default::default()
        };
        let document = RAGFlowDocumentConverter::convert_to_document(&no_score).unwrap();
        assert_eq!(document.score, Some(0.0));
    }

    #[test]
    fn similarity_is_used_when_legacy_score_absent() {
        let only_similarity = RAGFlowChunk {
            id: Some("c".to_string()),
            content: Some("txt".to_string()),
            similarity: Some(0.42),
            ..Default::default()
        };
        let document = RAGFlowDocumentConverter::convert_to_document(&only_similarity).unwrap();
        assert_eq!(document.score, Some(0.42));
    }

    #[test]
    fn payload_mirrors_java_field_rules() {
        let full = RAGFlowChunk {
            id: Some("chunk-1".to_string()),
            content: Some("content".to_string()),
            content_ltks: Some("tokenised".to_string()),
            highlight: Some("<em>hi</em>".to_string()),
            document_id: Some("doc-1".to_string()),
            document_keyword: Some("1.txt".to_string()),
            kb_id: Some("kb-1".to_string()),
            similarity: Some(0.9),
            vector_similarity: Some(0.8),
            term_similarity: Some(1.0),
            positions: Some(vec![vec![0, 100]]),
            image_id: Some("img-1".to_string()),
            important_keywords: Some(vec!["ragflow".to_string()]),
            metadata: Some(json!({"lang": "en"})),
            score: Some(0.95),
            dataset_id: Some("ds-1".to_string()),
            document_name: Some("1.txt".to_string()),
        };
        let document = RAGFlowDocumentConverter::convert_to_document(&full).unwrap();
        let payload = document.payload();

        assert_eq!(payload["document_keyword"], json!("1.txt"));
        assert_eq!(payload["kb_id"], json!("kb-1"));
        assert_eq!(payload["content_ltks"], json!("tokenised"));
        assert_eq!(payload["highlight"], json!("<em>hi</em>"));
        assert_eq!(payload["vector_similarity"], json!(0.8));
        assert_eq!(payload["term_similarity"], json!(1.0));
        assert_eq!(payload["positions"], json!([[0, 100]]));
        assert_eq!(payload["image_id"], json!("img-1"));
        assert_eq!(payload["important_keywords"], json!(["ragflow"]));
        assert_eq!(payload["metadata"], json!({"lang": "en"}));
        assert_eq!(payload["dataset_id"], json!("ds-1"));
        assert_eq!(payload["document_name"], json!("1.txt"));

        // Consumed fields are not duplicated into the payload.
        assert!(!payload.contains_key("document_id"));
        assert!(!payload.contains_key("id"));
        assert!(!payload.contains_key("content"));
        assert!(!payload.contains_key("score"));
    }

    #[test]
    fn empty_optional_fields_are_omitted_from_payload() {
        let chunk = RAGFlowChunk {
            id: Some("c".to_string()),
            content: Some("txt".to_string()),
            content_ltks: Some(String::new()),
            highlight: Some(String::new()),
            image_id: Some(String::new()),
            important_keywords: Some(Vec::new()),
            metadata: Some(json!({})),
            positions: Some(Vec::new()),
            ..Default::default()
        };
        let document = RAGFlowDocumentConverter::convert_to_document(&chunk).unwrap();
        let payload = document.payload();
        for key in [
            "content_ltks",
            "highlight",
            "image_id",
            "important_keywords",
            "metadata",
            "positions",
        ] {
            assert!(!payload.contains_key(key), "{key} should have been omitted");
        }
        assert!(payload.contains_key("document_name") == false);
    }

    #[test]
    fn from_response_uses_chunks_of_payload() {
        let response: RAGFlowResponse = serde_json::from_str(
            r#"{"code": 0, "data": {"chunks": [{"id": "c1", "document_id": "d1", "content": "hi", "score": 0.7}], "total": 1}}"#,
        )
        .unwrap();
        let documents = RAGFlowDocumentConverter::from_response(&response);
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].score, Some(0.7));
    }

    #[test]
    fn from_response_without_payload_is_empty() {
        let response: RAGFlowResponse =
            serde_json::from_str(r#"{"code": 0, "data": false}"#).unwrap();
        assert!(RAGFlowDocumentConverter::from_response(&response).is_empty());
    }
}
