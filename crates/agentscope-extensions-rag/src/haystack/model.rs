//! HayStack response / document DTOs and the retrieve request body.
//!
//! Ports `model.{HayStackResponse, HayStackDocument, SparseEmbedding, ByteStream}`. Field names
//! mirror the Java `@JsonProperty` values exactly and unknown fields are ignored, like
//! `@JsonIgnoreProperties(ignoreUnknown = true)`.
//!
//! The request body is modelled by [`HayStackRequest`]; Java assembled it as a `HashMap`, so the
//! mixed `snake_case` keys (`top_k`, `scale_score`, ...) are preserved verbatim.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A Haystack `ByteStream` (binary payload plus its metadata).
///
/// Java exposed `data` as `List<Integer>` (one entry per byte); that shape is kept so the wire
/// format round-trips unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ByteStream {
    /// Raw bytes, each as an integer in `0..=255`.
    pub data: Option<Vec<i32>>,
    /// Arbitrary metadata.
    pub meta: Option<HashMap<String, Value>>,
    /// MIME type (`mime_type`).
    #[serde(rename = "mime_type")]
    pub mime_type: Option<String>,
}

/// A sparse embedding: parallel `indices` / `values` arrays for the non-zero entries.
///
/// Ports `SparseEmbedding`; Java built it with a builder, which is reproduced here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SparseEmbedding {
    /// Indices of the non-zero elements.
    pub indices: Option<Vec<i32>>,
    /// Values of the non-zero elements.
    pub values: Option<Vec<f32>>,
}

impl SparseEmbedding {
    /// Starts a builder.
    pub fn builder() -> SparseEmbeddingBuilder {
        SparseEmbeddingBuilder::default()
    }
}

/// Consuming builder for [`SparseEmbedding`].
#[derive(Debug, Clone, Default)]
pub struct SparseEmbeddingBuilder {
    indices: Option<Vec<i32>>,
    values: Option<Vec<f32>>,
}

impl SparseEmbeddingBuilder {
    /// Sets the non-zero indices.
    pub fn indices(mut self, indices: Vec<i32>) -> Self {
        self.indices = Some(indices);
        self
    }

    /// Sets the non-zero values.
    pub fn values(mut self, values: Vec<f32>) -> Self {
        self.values = Some(values);
        self
    }

    /// Builds the embedding.
    pub fn build(self) -> SparseEmbedding {
        SparseEmbedding {
            indices: self.indices,
            values: self.values,
        }
    }
}

/// A Haystack document as returned by (or sent to) the RAG server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HayStackDocument {
    /// Document / chunk id.
    pub id: Option<String>,
    /// Chunk text.
    pub content: Option<String>,
    /// Optional binary payload.
    pub blob: Option<ByteStream>,
    /// Arbitrary metadata.
    pub meta: Option<HashMap<String, Value>>,
    /// Similarity score.
    pub score: Option<f64>,
    /// Dense embedding vector.
    pub embedding: Option<Vec<f64>>,
    /// Sparse embedding (`sparse_embedding`).
    #[serde(rename = "sparse_embedding")]
    pub sparse_embedding: Option<SparseEmbedding>,
    /// Per-document error string, as emitted by Haystack.
    pub error: Option<String>,
}

/// The Haystack RAG server response.
///
/// Covers both the plain retriever shape (`documents`) and the `SentenceWindowRetriever` shape
/// (`context_windows` + `context_documents`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HayStackResponse {
    /// Business status code; compared against the configured `successCode`.
    pub code: Option<i32>,
    /// Retrieved documents.
    pub documents: Option<Vec<HayStackDocument>>,
    /// Context windows (`context_windows`) of the sentence-window retriever.
    #[serde(rename = "context_windows")]
    pub context_windows: Option<Vec<String>>,
    /// Context documents (`context_documents`) of the sentence-window retriever.
    #[serde(rename = "context_documents")]
    pub context_documents: Option<Vec<HayStackDocument>>,
    /// Error message, set when `code` signals a failure.
    pub error: Option<String>,
}

impl HayStackResponse {
    /// The documents to convert, preferring `documents` over `context_documents`.
    ///
    /// Reproduces the Java selection
    /// `documents != null ? documents : context_documents`; an absent / empty pair yields an
    /// empty slice.
    pub fn effective_documents(&self) -> &[HayStackDocument] {
        match (self.documents.as_deref(), self.context_documents.as_deref()) {
            (Some(documents), _) => documents,
            (None, Some(context)) => context,
            (None, None) => &[],
        }
    }

    /// Whether both document lists are absent (Java logged a warning for this case).
    pub fn is_empty_payload(&self) -> bool {
        self.documents.is_none() && self.context_documents.is_none()
    }
}

/// The retrieve request body sent to the custom Haystack RAG server.
///
/// Java built this as a `HashMap<String, Object>`, so key names are the literal wire strings
/// (`query`, `top_k`, `scale_score`, `return_embedding`, `score_threshold`, `group_by`,
/// `group_size`, `filters`, `query_embedding`, `query_sparse_embedding`, `documents`,
/// `retrieved_documents`, `window_size`). Only `query` is always present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HayStackRequest {
    /// Query text (required).
    pub query: String,
    /// Maximum number of chunks to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    /// Whether to normalise scores into `0..=1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_score: Option<bool>,
    /// Whether the response should carry embeddings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_embedding: Option<bool>,
    /// Minimal score threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_threshold: Option<f64>,
    /// Payload field to group by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    /// Maximum number of points per group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_size: Option<i32>,
    /// Document-store filters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<HashMap<String, Value>>,
    /// Dense query embedding (bypasses Haystack's embedders).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_embedding: Option<Vec<f32>>,
    /// Sparse query embedding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_sparse_embedding: Option<SparseEmbedding>,
    /// Documents supplied inline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documents: Option<Vec<HayStackDocument>>,
    /// Documents to rerank / expand around.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieved_documents: Option<Vec<HayStackDocument>>,
    /// Number of adjacent documents to include on each side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_size: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_parses_the_plain_retriever_shape() {
        let body = r#"{
            "code": 0,
            "documents": [{
                "id": "0fab3c1c6c433368",
                "content": "Test Content",
                "blob": null,
                "meta": { "file_path": "test.txt" },
                "score": 1.2303546667099,
                "embedding": [0.01008153147995472, -0.04555170238018036],
                "sparse_embedding": null,
                "error": null
            }]
        }"#;
        let response: HayStackResponse = serde_json::from_str(body).unwrap();
        assert_eq!(response.code, Some(0));
        let documents = response.effective_documents();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].id.as_deref(), Some("0fab3c1c6c433368"));
        assert_eq!(documents[0].content.as_deref(), Some("Test Content"));
        assert_eq!(documents[0].score, Some(1.2303546667099));
        assert_eq!(documents[0].embedding.as_ref().unwrap().len(), 2);
        assert_eq!(
            documents[0].meta.as_ref().unwrap()["file_path"],
            serde_json::json!("test.txt")
        );
        assert!(documents[0].blob.is_none());
        assert!(documents[0].sparse_embedding.is_none());
    }

    #[test]
    fn response_parses_the_sentence_window_shape() {
        let body = r#"{
            "code": 0,
            "context_windows": ["Test Content 0 "],
            "context_documents": [{
                "id": "9e4cd66e14377767fdc6b9",
                "content": "Test Content 1",
                "meta": { "source": "usr_90.txt", "split_id": 21 }
            }],
            "error": null
        }"#;
        let response: HayStackResponse = serde_json::from_str(body).unwrap();
        assert_eq!(response.context_windows.as_ref().unwrap().len(), 1);
        // No `documents`, so the context documents are used.
        let documents = response.effective_documents();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].meta.as_ref().unwrap()["source"], "usr_90.txt");
        assert!(documents[0].score.is_none());
    }

    #[test]
    fn documents_take_priority_over_context_documents() {
        let response = HayStackResponse {
            documents: Some(vec![HayStackDocument {
                id: Some("plain".to_string()),
                ..Default::default()
            }]),
            context_documents: Some(vec![HayStackDocument {
                id: Some("context".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(response.effective_documents()[0].id.as_deref(), Some("plain"));
        assert!(!response.is_empty_payload());
    }

    #[test]
    fn missing_document_lists_are_reported_as_empty_payload() {
        let response: HayStackResponse = serde_json::from_str(r#"{"code":0}"#).unwrap();
        assert!(response.is_empty_payload());
        assert!(response.effective_documents().is_empty());

        // An explicitly empty list is a payload, not a missing one.
        let response: HayStackResponse =
            serde_json::from_str(r#"{"code":0,"documents":[]}"#).unwrap();
        assert!(!response.is_empty_payload());
        assert!(response.effective_documents().is_empty());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let body = r#"{"code":0,"documents":[],"extra":{"nested":true}}"#;
        let response: HayStackResponse = serde_json::from_str(body).unwrap();
        assert_eq!(response.code, Some(0));
    }

    #[test]
    fn sparse_embedding_serializes_with_java_field_names() {
        let sparse = SparseEmbedding::builder()
            .indices(vec![1, 5, 9])
            .values(vec![0.5, 0.25, 0.125])
            .build();
        let json = serde_json::to_value(&sparse).unwrap();
        assert_eq!(json["indices"], serde_json::json!([1, 5, 9]));
        assert_eq!(json["values"], serde_json::json!([0.5, 0.25, 0.125]));
        assert_eq!(SparseEmbedding::default().indices, None);
    }

    #[test]
    fn byte_stream_uses_snake_case_mime_type_key() {
        let stream: ByteStream = serde_json::from_str(
            r#"{"data":[104,105],"meta":{"source":"a"},"mime_type":"text/plain"}"#,
        )
        .unwrap();
        assert_eq!(stream.data.as_ref().unwrap(), &vec![104, 105]);
        assert_eq!(stream.mime_type.as_deref(), Some("text/plain"));

        let json = serde_json::to_value(&stream).unwrap();
        assert_eq!(json["mime_type"], "text/plain");
        assert!(json.get("mimeType").is_none());
    }

    #[test]
    fn document_embedding_and_sparse_embedding_are_optional() {
        let document: HayStackDocument =
            serde_json::from_str(r#"{"id":"a","content":"x"}"#).unwrap();
        assert!(document.embedding.is_none());
        assert!(document.sparse_embedding.is_none());
        assert!(document.blob.is_none());

        let document: HayStackDocument = serde_json::from_str(
            r#"{"sparse_embedding":{"indices":[1],"values":[0.5]}}"#,
        )
        .unwrap();
        let sparse = document.sparse_embedding.unwrap();
        assert_eq!(sparse.indices.unwrap(), vec![1]);
        assert_eq!(sparse.values.unwrap(), vec![0.5]);
    }

    #[test]
    fn request_serializes_to_the_java_wire_keys() {
        let request = HayStackRequest {
            query: "test query".to_string(),
            top_k: Some(3),
            scale_score: Some(true),
            return_embedding: Some(true),
            window_size: Some(2),
            ..Default::default()
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["query"], "test query");
        assert_eq!(json["top_k"], 3);
        assert_eq!(json["scale_score"], true);
        assert_eq!(json["return_embedding"], true);
        assert_eq!(json["window_size"], 2);
    }

    #[test]
    fn minimal_request_contains_only_the_query() {
        let request = HayStackRequest {
            query: "q".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 1);
        assert_eq!(json["query"], "q");
    }
}
