//! Bailian retrieve request/response DTOs.
//!
//! Field names are pinned to what the `bailian20231229` 2.8.2 SDK actually puts on the wire
//! (verified by serializing the SDK's `RetrieveRequest` and by round-tripping a response body
//! through `TeaModel.toModel`):
//!
//! * **request** keys are `camelCase` (`indexId`, `denseSimilarityTopK`, `queryHistory[].role`, ...);
//! * **response** keys are `PascalCase` (`Code`, `Data`, `Nodes`, `Text`, `Score`, `Metadata`).
//!
//! Fields follow the SDK's `NameInMap` annotations, and `rerankMinScore` / `rerankTopN` are
//! top-level request fields (the SDK sets them on the request, not inside the `Rerank` entry).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bailian::query_history::QueryRole;

/// One entry of the `Rerank` array (`RetrieveRequestRerank`, `NameInMap = "ModelName"`).
///
/// The serialized key is `modelName` (`camelCase`), which is what the SDK's gson-based
/// `Common.toJSONString` actually emits — verified by serializing the SDK request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RerankSpec {
    /// Rerank model name; omitted when blank (Java `StringUtils.isEmpty` guard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

/// One entry of the `Rewrite` array (`RetrieveRequestRewrite`, `NameInMap = "ModelName"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RewriteSpec {
    /// Rewrite model name; omitted when blank (Java `StringUtils.isEmpty` guard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

/// One entry of the `QueryHistory` array.
///
/// The SDK's `NameInMap` values are already lowercase (`role`, `content`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryHistorySpec {
    /// Who sent the message (`user` / `assistant`).
    pub role: QueryRole,
    /// The message text.
    pub content: String,
}

/// The `POST /{workspaceId}/index/retrieve` request body.
///
/// Every optional field mirrors a `if (x != null)` guard in `BailianClient.retrieve`, so an
/// unset field is omitted from the JSON rather than sent as `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RetrieveRequest {
    /// Knowledge base index id.
    pub index_id: String,
    /// Query text.
    pub query: String,
    /// Dense (vector) similarity top-K.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dense_similarity_top_k: Option<i32>,
    /// Sparse (keyword) similarity top-K.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_similarity_top_k: Option<i32>,
    /// Whether reranking is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_reranking: Option<bool>,
    /// Rerank model selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank: Option<Vec<RerankSpec>>,
    /// Minimum rerank score (top-level field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_min_score: Option<f32>,
    /// Top-N after reranking (top-level field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_top_n: Option<i32>,
    /// Whether multi-turn rewrite is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_rewrite: Option<bool>,
    /// Rewrite model selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<Vec<RewriteSpec>>,
    /// Conversation history for the rewrite model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_history: Option<Vec<QueryHistorySpec>>,
    /// Search filters, each an arbitrary string map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_filters: Option<Vec<HashMap<String, String>>>,
    /// Whether Bailian should persist the retriever history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_retriever_history: Option<bool>,
}

/// The retrieve response body (`RetrieveResponseBody`).
///
/// Response keys are `PascalCase` in the SDK (`NameInMap` = `Code`, `Data`, `Message`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RetrieveResponseBody {
    /// Business status code, as a string.
    #[serde(default, rename = "Code", skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Retrieval payload.
    #[serde(default, rename = "Data", skip_serializing_if = "Option::is_none")]
    pub data: Option<RetrieveResponseData>,
    /// Human-readable message.
    #[serde(default, rename = "Message", skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Request id assigned by Bailian.
    #[serde(default, rename = "RequestId", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// HTTP-ish status string.
    #[serde(default, rename = "Status", skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Whether the call succeeded.
    #[serde(default, rename = "Success", skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
}

impl RetrieveResponseBody {
    /// The retrieved nodes, or an empty slice when the payload is absent.
    pub fn nodes(&self) -> &[RetrieveNode] {
        self.data
            .as_ref()
            .map(|data| data.nodes.as_slice())
            .unwrap_or(&[])
    }
}

/// The `Data` object of a retrieve response (`RetrieveResponseBodyData`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RetrieveResponseData {
    /// Retrieved nodes.
    #[serde(default, rename = "Nodes")]
    pub nodes: Vec<RetrieveNode>,
}

/// A single retrieved node (`RetrieveResponseBodyDataNodes`).
///
/// `metadata` is typed as `Object` by the SDK, so it is kept as a JSON [`Value`] here and
/// narrowed to an object by the converter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RetrieveNode {
    /// Chunk text.
    #[serde(default, rename = "Text", skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Similarity score.
    #[serde(default, rename = "Score", skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Arbitrary metadata.
    #[serde(default, rename = "Metadata", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl RetrieveNode {
    /// The metadata as an object map, or an empty map when absent / not an object.
    ///
    /// Reproduces the Java `metadataObj instanceof Map` guard.
    pub fn metadata_map(&self) -> HashMap<String, Value> {
        match &self.metadata {
            Some(Value::Object(map)) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_camel_case_keys_and_omits_none() {
        let request = RetrieveRequest {
            index_id: "idx-1".to_string(),
            query: "hello".to_string(),
            dense_similarity_top_k: Some(50),
            ..Default::default()
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["indexId"], "idx-1");
        assert_eq!(json["query"], "hello");
        assert_eq!(json["denseSimilarityTopK"], 50);
        // Only the two required plus the one optional field are present.
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert!(json.get("enableReranking").is_none());
        assert!(json.get("saveRetrieverHistory").is_none());
    }

    #[test]
    fn request_matches_sdk_field_names_including_nested_arrays() {
        let request = RetrieveRequest {
            index_id: "idx-1".to_string(),
            query: "hello".to_string(),
            enable_reranking: Some(true),
            rerank: Some(vec![RerankSpec {
                model_name: Some("gte-rerank-hybrid".to_string()),
            }]),
            rerank_min_score: Some(0.2),
            rerank_top_n: Some(5),
            rewrite: Some(vec![RewriteSpec {
                model_name: Some("conv-rewrite-qwen-1.8b".to_string()),
            }]),
            query_history: Some(vec![QueryHistorySpec {
                role: QueryRole::User,
                content: "hi".to_string(),
            }]),
            save_retriever_history: Some(false),
            ..Default::default()
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["enableReranking"], true);
        assert_eq!(json["rerank"][0]["modelName"], "gte-rerank-hybrid");
        assert_eq!(json["rerankTopN"], 5);
        assert_eq!(json["rewrite"][0]["modelName"], "conv-rewrite-qwen-1.8b");
        assert_eq!(json["queryHistory"][0]["role"], "user");
        assert_eq!(json["queryHistory"][0]["content"], "hi");
        assert_eq!(json["saveRetrieverHistory"], false);

        // `rerankMinScore` is an f32: `to_value` widens it to f64 (0.20000000298023224),
        // but the serialized wire form is `0.2`, which is exactly what the Java SDK sent.
        assert!(
            serde_json::to_string(&request)
                .unwrap()
                .contains(r#""rerankMinScore":0.2"#),
            "{}",
            serde_json::to_string(&request).unwrap()
        );
    }

    #[test]
    fn blank_rerank_model_name_is_omitted() {
        let request = RetrieveRequest {
            index_id: "i".to_string(),
            query: "q".to_string(),
            rerank: Some(vec![RerankSpec { model_name: None }]),
            ..Default::default()
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["rerank"][0], serde_json::json!({}));
    }

    #[test]
    fn response_parses_pascal_case_keys() {
        let body = r#"{
            "Code": "0",
            "Success": true,
            "Status": "200",
            "RequestId": "req-1",
            "Message": "ok",
            "Data": { "Nodes": [
                { "Text": "hello", "Score": 0.9, "Metadata": { "doc_id": "d1", "_id": "c1" } }
            ] }
        }"#;
        let parsed: RetrieveResponseBody = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.code.as_deref(), Some("0"));
        assert_eq!(parsed.request_id.as_deref(), Some("req-1"));
        assert_eq!(parsed.success, Some(true));
        assert_eq!(parsed.nodes().len(), 1);
        let node = &parsed.nodes()[0];
        assert_eq!(node.text.as_deref(), Some("hello"));
        assert_eq!(node.score, Some(0.9));
        assert_eq!(node.metadata_map().get("doc_id").unwrap(), "d1");
    }

    #[test]
    fn response_ignores_lowercase_keys_like_the_sdk() {
        // The SDK only binds PascalCase keys (`TeaModel.toModel`); lowercase is ignored.
        let body = r#"{ "code": "0", "data": { "nodes": [{ "text": "x" }] } }"#;
        let parsed: RetrieveResponseBody = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.code, None);
        assert!(parsed.nodes().is_empty());
    }

    #[test]
    fn metadata_map_tolerates_non_object_metadata() {
        let node = RetrieveNode {
            metadata: Some(serde_json::json!("not-an-object")),
            ..Default::default()
        };
        assert!(node.metadata_map().is_empty());
        assert!(RetrieveNode::default().metadata_map().is_empty());
    }

    #[test]
    fn missing_data_yields_no_nodes() {
        let parsed: RetrieveResponseBody = serde_json::from_str(r#"{"Code":"0"}"#).unwrap();
        assert!(parsed.nodes().is_empty());
    }
}
