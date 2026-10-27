//! Conversion of HayStack documents into AgentScope [`Document`]s.
//!
//! Ports `HayStackDocumentConverter`: `file_path` (or `source`, for the `SentenceWindowRetriever`)
//! becomes the document id, the Haystack id becomes the chunk id, every metadata key is copied
//! into the payload, and the score / embedding are carried over when present.

use agentscope_core::message::{ContentBlock, TextBlock};
use serde_json::{Map, Value};

use crate::haystack::model::HayStackDocument;
use crate::model::{Document, DocumentMetadata};

/// Stateless converter between the Haystack document model and [`Document`].
pub struct HayStackDocumentConverter;

impl HayStackDocumentConverter {
    /// Converts a document list, skipping entries that fail conversion.
    ///
    /// Java returned an empty list for a null / empty input; the slice handling makes that
    /// implicit.
    pub fn convert_to_documents(documents: &[HayStackDocument]) -> Vec<Document> {
        documents
            .iter()
            .filter_map(Self::convert_to_document)
            .collect()
    }

    /// Converts a single document, returning `None` when the content is missing or blank.
    ///
    /// Mirrors `convertToDocument`:
    /// * blank content is skipped (with the chunk id logged by the caller);
    /// * `docId` prefers `meta.file_path`, then `meta.source`, then the Haystack id, then
    ///   `"unknown"`;
    /// * `chunkId` is the Haystack id, defaulting to `"0"`;
    /// * the payload is the full metadata map;
    /// * a non-empty `embedding` is attached and the score is set when present.
    pub fn convert_to_document(document: &HayStackDocument) -> Option<Document> {
        let content = document.content.as_deref().unwrap_or("");
        if content.trim().is_empty() {
            tracing::debug!(
                id = ?document.id,
                "skipping HayStack document with null or empty content"
            );
            return None;
        }

        let meta = document.meta.clone().unwrap_or_default();

        // `file_path` first, then `source` for the sentence-window retriever.
        let doc_id = metadata_string(&meta, "file_path")
            .or_else(|| metadata_string(&meta, "source"))
            .or_else(|| non_empty(document.id.as_deref()).map(|id| id.to_string()))
            .unwrap_or_else(|| "unknown".to_string());

        let chunk_id = match non_empty(document.id.as_deref()) {
            Some(id) => id.to_string(),
            None => "0".to_string(),
        };

        // Every metadata key is preserved (unlike the Bailian/RAGFlow converters, Haystack keeps
        // `file_path` / `source` in the payload as well).
        let payload: Map<String, Value> = meta.into_iter().collect();

        let mut converted = Document::new(DocumentMetadata::with_payload(
            ContentBlock::Text(TextBlock::new(content)),
            doc_id,
            chunk_id.clone(),
            Value::Object(payload),
        ));

        if let Some(embedding) = document.embedding.as_ref().filter(|e| !e.is_empty()) {
            converted.set_embedding(embedding.clone());
        }
        if let Some(score) = document.score {
            converted.set_score(score);
        }

        tracing::debug!(id = %chunk_id, "converted HayStack document");
        Some(converted)
    }
}

/// `Some(value)` when the option holds a non-blank string.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// Reads a metadata entry as a string, mirroring Java's `Object.toString()` fallback.
fn metadata_string(meta: &std::collections::HashMap<String, Value>, key: &str) -> Option<String> {
    match meta.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        Some(Value::Null) | None => None,
        Some(Value::String(_)) => None,
        Some(other) => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn document(
        id: Option<&str>,
        content: Option<&str>,
        meta: Option<Vec<(&str, Value)>>,
    ) -> HayStackDocument {
        HayStackDocument {
            id: id.map(|id| id.to_string()),
            content: content.map(|content| content.to_string()),
            meta: meta.map(|entries| {
                entries
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value))
                    .collect::<HashMap<_, _>>()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn converts_content_and_prefers_file_path_as_doc_id() {
        let source = document(
            Some("0fab3c1c6c433368"),
            Some("Test Content"),
            Some(vec![("file_path", serde_json::json!("test.txt"))]),
        );
        let converted = HayStackDocumentConverter::convert_to_document(&source).unwrap();
        assert_eq!(converted.metadata.doc_id, "test.txt");
        assert_eq!(converted.metadata.chunk_id, "0fab3c1c6c433368");
        assert_eq!(converted.metadata.content_text(), "Test Content");
        // Haystack keeps file_path in the payload.
        assert_eq!(
            converted.payload_value("file_path").unwrap(),
            &serde_json::json!("test.txt")
        );
    }

    #[test]
    fn falls_back_to_source_then_id_then_unknown() {
        let with_source = document(
            Some("chunk-1"),
            Some("x"),
            Some(vec![("source", serde_json::json!("usr_90.txt"))]),
        );
        assert_eq!(
            HayStackDocumentConverter::convert_to_document(&with_source)
                .unwrap()
                .metadata
                .doc_id,
            "usr_90.txt"
        );

        let id_only = document(Some("chunk-1"), Some("x"), None);
        assert_eq!(
            HayStackDocumentConverter::convert_to_document(&id_only)
                .unwrap()
                .metadata
                .doc_id,
            "chunk-1"
        );

        let blank_meta = document(
            Some("  "),
            Some("x"),
            Some(vec![("other", serde_json::json!(1))]),
        );
        assert_eq!(
            HayStackDocumentConverter::convert_to_document(&blank_meta)
                .unwrap()
                .metadata
                .doc_id,
            "unknown"
        );
    }

    #[test]
    fn chunk_id_defaults_to_zero_when_the_id_is_absent() {
        let converted =
            HayStackDocumentConverter::convert_to_document(&document(None, Some("x"), None))
                .unwrap();
        assert_eq!(converted.metadata.chunk_id, "0");
    }

    #[test]
    fn blank_or_missing_content_is_skipped() {
        assert!(
            HayStackDocumentConverter::convert_to_document(&document(None, None, None)).is_none()
        );
        assert!(HayStackDocumentConverter::convert_to_document(&document(
            Some("a"),
            Some("  "),
            None
        ))
        .is_none());
        assert!(HayStackDocumentConverter::convert_to_document(&document(
            Some("a"),
            Some("valid content"),
            None
        ))
        .is_some());
    }

    #[test]
    fn embedding_is_attached_only_when_non_empty() {
        let with_embedding = HayStackDocument {
            id: Some("a".to_string()),
            content: Some("x".to_string()),
            embedding: Some(vec![0.01008153147995472, -0.04555170238018036]),
            ..Default::default()
        };
        let converted = HayStackDocumentConverter::convert_to_document(&with_embedding).unwrap();
        assert_eq!(converted.embedding.as_ref().unwrap().len(), 2);

        let empty_embedding = HayStackDocument {
            embedding: Some(vec![]),
            ..with_embedding
        };
        let converted = HayStackDocumentConverter::convert_to_document(&empty_embedding).unwrap();
        assert!(converted.embedding.is_none());
    }

    #[test]
    fn score_is_optional() {
        let scored = HayStackDocument {
            id: Some("a".to_string()),
            content: Some("x".to_string()),
            score: Some(0.91),
            ..Default::default()
        };
        assert_eq!(
            HayStackDocumentConverter::convert_to_document(&scored)
                .unwrap()
                .score,
            Some(0.91)
        );
        let unscored = HayStackDocument {
            score: None,
            ..scored
        };
        assert_eq!(
            HayStackDocumentConverter::convert_to_document(&unscored)
                .unwrap()
                .score,
            None
        );
    }

    #[test]
    fn convert_to_documents_skips_invalid_entries() {
        let documents = vec![
            document(Some("d1"), Some("content 1"), None),
            document(Some("d2"), Some(""), None),
            document(None, None, None),
            document(Some("d3"), Some("content 3"), None),
        ];
        let converted = HayStackDocumentConverter::convert_to_documents(&documents);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].metadata.chunk_id, "d1");
        assert_eq!(converted[1].metadata.chunk_id, "d3");
        assert!(HayStackDocumentConverter::convert_to_documents(&[]).is_empty());
    }

    #[test]
    fn non_string_metadata_values_are_stringified_for_doc_id() {
        let doc = document(
            Some("a"),
            Some("x"),
            Some(vec![("file_path", serde_json::json!(42))]),
        );
        assert_eq!(
            HayStackDocumentConverter::convert_to_document(&doc)
                .unwrap()
                .metadata
                .doc_id,
            "42"
        );
    }
}
