//! RAGFlow response DTOs.
//!
//! Ports `RAGFlowResponse`, `RAGFlowResponse.ResponseData`, `RAGFlowResponse.DocAgg` and
//! `RAGFlowChunk`. Field names mirror the Java `@JsonProperty` values exactly and unknown
//! fields are ignored, like `@JsonIgnoreProperties(ignoreUnknown = true)`.
//!
//! The interesting part is `data`: the Java `ResponseDataDeserializer` accepts `false`,
//! `null` and "not an object" and maps all of them to `null` instead of throwing. In serde
//! this is modelled by a custom [`Deserialize`] implementation on [`ResponseData`].

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Root response of `POST /api/v1/retrieval`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RAGFlowResponse {
    /// API code; `0` means success (Java compares against `0`).
    pub code: Option<i64>,
    /// Human-readable message.
    pub message: Option<String>,
    /// Retrieval payload; `None` when the API returned `false`/`null`/a non-object.
    #[serde(deserialize_with = "deserialize_response_data")]
    pub data: Option<ResponseData>,
}

impl RAGFlowResponse {
    /// Whether the response carries a non-zero API code (a Java `RAGFlowApiException`).
    pub fn is_api_error(&self) -> bool {
        self.code.is_some_and(|code| code != 0)
    }

    /// Number of chunks, treating a missing payload as zero.
    pub fn chunk_count(&self) -> usize {
        self.data
            .as_ref()
            .and_then(|d| d.chunks.as_ref())
            .map_or(0, Vec::len)
    }

    /// The chunks, or an empty slice when the payload is absent / has no chunks.
    pub fn chunks(&self) -> &[RAGFlowChunk] {
        self.data
            .as_ref()
            .and_then(|d| d.chunks.as_deref())
            .unwrap_or(&[])
    }
}

/// Deserializes `data: false | null | object` into `Option<ResponseData>`.
///
/// Reproduces `ResponseDataDeserializer.deserialize`: anything that is not a JSON object
/// yields `None` rather than an error.
fn deserialize_response_data<'de, D>(deserializer: D) -> Result<Option<ResponseData>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Object(_) => ResponseData::deserialize(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// The `data` object of a successful retrieval response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ResponseData {
    /// Retrieved chunks.
    pub chunks: Option<Vec<RAGFlowChunk>>,
    /// Total number of matching chunks.
    pub total: Option<i64>,
    /// Per-document aggregation (`doc_aggs`).
    #[serde(rename = "doc_aggs")]
    pub doc_aggs: Option<Vec<DocAgg>>,
}

/// Document-level aggregation: how many chunks came back for one document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DocAgg {
    /// Document id (`doc_id`).
    #[serde(rename = "doc_id")]
    pub doc_id: Option<String>,
    /// Document name (`doc_name`).
    #[serde(rename = "doc_name")]
    pub doc_name: Option<String>,
    /// Chunk count.
    pub count: Option<i64>,
}

/// A RAGFlow chunk (document segment).
///
/// The field set matches `RAGFlowChunk` including the legacy `score` field, which the
/// converter prefers over `similarity` when present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RAGFlowChunk {
    /// Chunk id.
    pub id: Option<String>,
    /// Chunk text.
    pub content: Option<String>,
    /// Tokenised content (`content_ltks`).
    #[serde(rename = "content_ltks")]
    pub content_ltks: Option<String>,
    /// Highlighted content.
    pub highlight: Option<String>,
    /// Owning document id (`document_id`).
    #[serde(rename = "document_id")]
    pub document_id: Option<String>,
    /// Owning document name/keyword (`document_keyword`).
    #[serde(rename = "document_keyword")]
    pub document_keyword: Option<String>,
    /// Knowledge base id (`kb_id`).
    #[serde(rename = "kb_id")]
    pub kb_id: Option<String>,
    /// Combined similarity score.
    pub similarity: Option<f64>,
    /// Vector similarity (`vector_similarity`).
    #[serde(rename = "vector_similarity")]
    pub vector_similarity: Option<f64>,
    /// Term similarity (`term_similarity`).
    #[serde(rename = "term_similarity")]
    pub term_similarity: Option<f64>,
    /// Character positions (`[start, end]` pairs).
    pub positions: Option<Vec<Vec<i64>>>,
    /// Related image id (`image_id`).
    #[serde(rename = "image_id")]
    pub image_id: Option<String>,
    /// Important keywords (`important_keywords`).
    #[serde(rename = "important_keywords")]
    pub important_keywords: Option<Vec<String>>,
    /// Arbitrary custom metadata.
    pub metadata: Option<Value>,
    /// Legacy score field kept for compatibility.
    pub score: Option<f64>,
    /// Dataset id (`dataset_id`).
    #[serde(rename = "dataset_id")]
    pub dataset_id: Option<String>,
    /// Document name (`document_name`).
    #[serde(rename = "document_name")]
    pub document_name: Option<String>,
}

impl RAGFlowChunk {
    /// The score the converter uses: the legacy `score` field when present, else
    /// `similarity`. Returns `None` when neither is set.
    pub fn effective_score(&self) -> Option<f64> {
        self.score.or(self.similarity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SUCCESS: &str = r#"{
        "code": 0,
        "data": {
            "chunks": [
                {
                    "id": "d78435d142bd5cf6704da62c778795c5",
                    "content": "ragflow content",
                    "content_ltks": "ragflow content",
                    "document_id": "5c5999ec7be811ef9cab0242ac120005",
                    "document_keyword": "1.txt",
                    "highlight": "<em>ragflow</em> content",
                    "kb_id": "c7ee74067a2c11efb21c0242ac120006",
                    "similarity": 0.9669436601210759,
                    "vector_similarity": 0.8898122004035864,
                    "term_similarity": 1.0,
                    "positions": [[0, 100], [200, 300]],
                    "image_id": "",
                    "important_keywords": ["ragflow", "content"]
                }
            ],
            "total": 1,
            "doc_aggs": [{"doc_id": "d1", "doc_name": "1.txt", "count": 1}]
        }
    }"#;

    #[test]
    fn parses_success_response() {
        let response: RAGFlowResponse = serde_json::from_str(SUCCESS).unwrap();
        assert_eq!(response.code, Some(0));
        assert!(!response.is_api_error());
        assert_eq!(response.chunk_count(), 1);

        let chunk = &response.chunks()[0];
        assert_eq!(chunk.document_keyword.as_deref(), Some("1.txt"));
        assert_eq!(chunk.positions, Some(vec![vec![0, 100], vec![200, 300]]));
        assert_eq!(chunk.similarity, Some(0.9669436601210759));
        assert_eq!(chunk.effective_score(), Some(0.9669436601210759));

        let data = response.data.as_ref().unwrap();
        assert_eq!(data.total, Some(1));
        assert_eq!(
            data.doc_aggs.as_ref().unwrap()[0].doc_name.as_deref(),
            Some("1.txt")
        );
    }

    #[test]
    fn data_false_maps_to_none() {
        let response: RAGFlowResponse =
            serde_json::from_str(r#"{"code": 404, "message": "Dataset not found", "data": false}"#)
                .unwrap();
        assert!(response.data.is_none());
        assert!(response.is_api_error());
        assert_eq!(response.message.as_deref(), Some("Dataset not found"));
        assert_eq!(response.chunk_count(), 0);
        assert!(response.chunks().is_empty());
    }

    #[test]
    fn data_null_and_missing_map_to_none() {
        let null: RAGFlowResponse =
            serde_json::from_str(r#"{"code": 0, "message": "No results", "data": null}"#).unwrap();
        assert!(null.data.is_none());
        assert!(!null.is_api_error());

        let missing: RAGFlowResponse = serde_json::from_str(r#"{"code": 0}"#).unwrap();
        assert!(missing.data.is_none());
    }

    #[test]
    fn data_string_maps_to_none_like_non_object() {
        let response: RAGFlowResponse =
            serde_json::from_str(r#"{"code": 0, "data": "unexpected"}"#).unwrap();
        assert!(response.data.is_none());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let response: RAGFlowResponse = serde_json::from_str(
            r#"{"code": 0, "future": 1, "data": {"chunks": [{"id": "c", "content": "x", "brand_new": true}]}}"#,
        )
        .unwrap();
        assert_eq!(response.chunks()[0].id.as_deref(), Some("c"));
    }

    #[test]
    fn legacy_score_wins_over_similarity() {
        let chunk = RAGFlowChunk {
            score: Some(0.4),
            similarity: Some(0.9),
            ..Default::default()
        };
        assert_eq!(chunk.effective_score(), Some(0.4));
    }

    #[test]
    fn response_round_trips() {
        let response: RAGFlowResponse = serde_json::from_str(SUCCESS).unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["data"]["doc_aggs"][0]["doc_id"], json!("d1"));
        let back: RAGFlowResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, response);
    }
}
