//! Bailian Memory API data transfer objects.
//!
//! Field names mirror the Java `io.agentscope.core.memory.bailian` DTOs, including
//! the `meta_data` (add) vs `metadata` naming difference and the `snake_case`
//! JSON property overrides.

use serde::{Deserialize, Serialize};

use agentscope_core::JsonMap;

/// A message in the Bailian Memory API format.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BailianMessage {
    /// Role of the sender, `"user"` or `"assistant"`.
    pub role: String,
    /// Text content.
    pub content: String,
}

impl BailianMessage {
    /// Creates a message.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

/// Request body for `POST /api/v2/apps/memory/add`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BailianAddRequest {
    /// User identifier (required).
    #[serde(rename = "user_id", skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Conversation messages to process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<BailianMessage>>,
    /// Content stored directly as memory (alternative to `messages`).
    #[serde(rename = "custom_content", skip_serializing_if = "Option::is_none")]
    pub custom_content: Option<String>,
    /// Memory library identifier.
    #[serde(rename = "memory_library_id", skip_serializing_if = "Option::is_none")]
    pub memory_library_id: Option<String>,
    /// Project identifier for memory rules.
    #[serde(rename = "project_id", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Profile schema identifier.
    #[serde(rename = "profile_schema", skip_serializing_if = "Option::is_none")]
    pub profile_schema: Option<String>,
    /// Additional metadata (Java `meta_data`).
    #[serde(rename = "meta_data", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
}

impl BailianAddRequest {
    /// Creates an empty request.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the user identifier.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Sets the messages.
    pub fn with_messages(mut self, messages: Vec<BailianMessage>) -> Self {
        self.messages = Some(messages);
        self
    }

    /// Sets the memory library identifier.
    pub fn with_memory_library_id(mut self, id: impl Into<String>) -> Self {
        self.memory_library_id = Some(id.into());
        self
    }

    /// Sets the project identifier.
    pub fn with_project_id(mut self, id: impl Into<String>) -> Self {
        self.project_id = Some(id.into());
        self
    }

    /// Sets the profile schema identifier.
    pub fn with_profile_schema(mut self, schema: impl Into<String>) -> Self {
        self.profile_schema = Some(schema.into());
        self
    }

    /// Sets the metadata map.
    pub fn with_metadata(mut self, metadata: JsonMap) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Request body for `POST /api/v2/apps/memory/memory_nodes/search`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BailianSearchRequest {
    /// User identifier (required).
    #[serde(rename = "user_id", skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Query messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<BailianMessage>>,
    /// Memory library identifier.
    #[serde(rename = "memory_library_id", skip_serializing_if = "Option::is_none")]
    pub memory_library_id: Option<String>,
    /// Project identifiers for memory rules.
    #[serde(rename = "project_ids", skip_serializing_if = "Option::is_none")]
    pub project_ids: Option<Vec<String>>,
    /// Maximum number of results (Java default `10`).
    #[serde(rename = "top_k", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Minimum similarity score (Java default `0.3`).
    #[serde(rename = "min_score", skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f64>,
    /// Whether to rerank results.
    #[serde(rename = "enable_rerank", skip_serializing_if = "Option::is_none")]
    pub enable_rerank: Option<bool>,
    /// Whether to enable the intent discrimination callback.
    #[serde(rename = "enable_judge", skip_serializing_if = "Option::is_none")]
    pub enable_judge: Option<bool>,
    /// Whether to enable query rewrite.
    #[serde(rename = "enable_rewrite", skip_serializing_if = "Option::is_none")]
    pub enable_rewrite: Option<bool>,
}

impl BailianSearchRequest {
    /// Creates an empty request.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the user identifier.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Sets the query messages.
    pub fn with_messages(mut self, messages: Vec<BailianMessage>) -> Self {
        self.messages = Some(messages);
        self
    }

    /// Sets `top_k`.
    pub fn with_top_k(mut self, top_k: u32) -> Self {
        self.top_k = Some(top_k);
        self
    }

    /// Sets `min_score`.
    pub fn with_min_score(mut self, min_score: f64) -> Self {
        self.min_score = Some(min_score);
        self
    }

    /// Sets `enable_rerank`.
    pub fn with_enable_rerank(mut self, enable: bool) -> Self {
        self.enable_rerank = Some(enable);
        self
    }

    /// Sets `enable_judge`.
    pub fn with_enable_judge(mut self, enable: bool) -> Self {
        self.enable_judge = Some(enable);
        self
    }

    /// Sets `enable_rewrite`.
    pub fn with_enable_rewrite(mut self, enable: bool) -> Self {
        self.enable_rewrite = Some(enable);
        self
    }

    /// Sets the memory library identifier.
    pub fn with_memory_library_id(mut self, id: impl Into<String>) -> Self {
        self.memory_library_id = Some(id.into());
        self
    }

    /// Sets the project identifiers.
    pub fn with_project_ids(mut self, ids: Vec<String>) -> Self {
        self.project_ids = Some(ids);
        self
    }
}

/// A memory node returned by the Bailian API.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BailianMemoryNode {
    /// Unique node identifier.
    #[serde(
        rename = "memory_node_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub memory_node_id: Option<String>,
    /// Memory content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Event type (`ADD`, `UPDATE`, `DELETE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// Previous content (only for `UPDATE` events).
    #[serde(
        rename = "old_content",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub old_content: Option<String>,
    /// Creation timestamp (Unix seconds).
    #[serde(
        rename = "created_at",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub created_at: Option<i64>,
    /// Update timestamp (Unix seconds).
    #[serde(
        rename = "updated_at",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_at: Option<i64>,
}

/// Response of the Bailian add API.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BailianAddResponse {
    /// Request ID for tracing.
    #[serde(
        rename = "request_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_id: Option<String>,
    /// Nodes created or updated.
    #[serde(
        rename = "memory_nodes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub memory_nodes: Option<Vec<BailianMemoryNode>>,
}

/// Response of the Bailian search API.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BailianSearchResponse {
    /// Request ID for tracing.
    #[serde(
        rename = "request_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_id: Option<String>,
    /// Nodes ordered by relevance.
    #[serde(
        rename = "memory_nodes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub memory_nodes: Option<Vec<BailianMemoryNode>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn expected(raw: &str) -> Value {
        serde_json::from_str(raw).expect("valid json literal")
    }

    #[test]
    fn message_roundtrips_role_and_content() {
        let message = BailianMessage::new("assistant", "hi");
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value, expected(r#"{"role":"assistant","content":"hi"}"#));
        let back: BailianMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back, message);
        assert_eq!(BailianMessage::default(), BailianMessage::new("", ""));
    }

    #[test]
    fn add_request_uses_meta_data_key_not_metadata() {
        let mut metadata = JsonMap::new();
        metadata.insert("k".to_string(), json!("v"));
        let request = BailianAddRequest::new()
            .with_user_id("u")
            .with_metadata(metadata);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["meta_data"]["k"], "v");
        assert!(value.get("metadata").is_none());
        let back: BailianAddRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn add_request_skips_absent_fields() {
        assert_eq!(
            serde_json::to_value(BailianAddRequest::new()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn add_request_serializes_snake_case_fields() {
        let mut request = BailianAddRequest::new()
            .with_user_id("u")
            .with_messages(vec![BailianMessage::new("user", "hi")])
            .with_memory_library_id("lib")
            .with_project_id("proj")
            .with_profile_schema("schema");
        request.custom_content = Some("direct memory".to_string());

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["user_id"], "u");
        assert_eq!(value["custom_content"], "direct memory");
        assert_eq!(value["memory_library_id"], "lib");
        assert_eq!(value["project_id"], "proj");
        assert_eq!(value["profile_schema"], "schema");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"], "hi");
        let back: BailianAddRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn search_request_serializes_snake_case_and_flags() {
        let request = BailianSearchRequest::new()
            .with_user_id("u")
            .with_messages(vec![BailianMessage::new("user", "q")])
            .with_memory_library_id("lib")
            .with_project_ids(vec!["p1".to_string(), "p2".to_string()])
            .with_top_k(3)
            .with_min_score(0.5)
            .with_enable_rerank(true)
            .with_enable_judge(false)
            .with_enable_rewrite(true);

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["project_ids"], json!(["p1", "p2"]));
        assert_eq!(value["top_k"], 3);
        assert_eq!(value["min_score"], 0.5);
        assert_eq!(value["enable_rerank"], true);
        assert_eq!(value["enable_judge"], false);
        assert_eq!(value["enable_rewrite"], true);
        assert_eq!(value["messages"][0]["content"], "q");
        let back: BailianSearchRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn search_request_has_no_dto_level_defaults() {
        // Java BailianSearchRequest has no construct defaults; top_k = 10 and
        // min_score = 0.3 come from BailianLongTermMemory.Builder.
        let request = BailianSearchRequest::new();
        assert!(request.top_k.is_none());
        assert!(request.min_score.is_none());
        assert!(request.enable_rerank.is_none());
        assert!(request.enable_judge.is_none());
        assert!(request.enable_rewrite.is_none());
        assert_eq!(serde_json::to_value(&request).unwrap(), json!({}));
    }

    #[test]
    fn memory_node_deserializes_snake_case_fields() {
        let node: BailianMemoryNode = serde_json::from_str(
            r#"{"memory_node_id":"n1","content":"c","event":"UPDATE","old_content":"o","created_at":1700000000,"updated_at":1700000001}"#,
        )
        .unwrap();
        assert_eq!(node.memory_node_id.as_deref(), Some("n1"));
        assert_eq!(node.content.as_deref(), Some("c"));
        assert_eq!(node.event.as_deref(), Some("UPDATE"));
        assert_eq!(node.old_content.as_deref(), Some("o"));
        assert_eq!(node.created_at, Some(1_700_000_000));
        assert_eq!(node.updated_at, Some(1_700_000_001));

        let value = serde_json::to_value(&node).unwrap();
        assert_eq!(value["memory_node_id"], "n1");
        assert_eq!(value["old_content"], "o");
        assert!(value.get("memoryNodeId").is_none());
    }

    #[test]
    fn memory_node_serializes_nothing_when_empty() {
        assert_eq!(
            serde_json::to_value(BailianMemoryNode::default()).unwrap(),
            json!({})
        );
        let node: BailianMemoryNode = serde_json::from_str("{}").unwrap();
        assert_eq!(node, BailianMemoryNode::default());
    }

    #[test]
    fn add_response_deserializes_request_id_and_nodes() {
        let response: BailianAddResponse = serde_json::from_str(
            r#"{"request_id":"r1","memory_nodes":[{"memory_node_id":"n1","content":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(response.request_id.as_deref(), Some("r1"));
        let nodes = response.memory_nodes.as_ref().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].memory_node_id.as_deref(), Some("n1"));

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["request_id"], "r1");
        assert_eq!(value["memory_nodes"][0]["content"], "c");
    }

    #[test]
    fn search_response_preserves_node_order() {
        let response: BailianSearchResponse = serde_json::from_str(
            r#"{"request_id":"r2","memory_nodes":[{"content":"a"},{"content":"b"}]}"#,
        )
        .unwrap();
        let nodes = response.memory_nodes.as_ref().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].content.as_deref(), Some("a"));
        assert_eq!(nodes[1].content.as_deref(), Some("b"));
        assert_eq!(response.request_id.as_deref(), Some("r2"));
    }

    #[test]
    fn responses_tolerate_missing_and_unknown_fields() {
        // Java sets FAIL_ON_UNKNOWN_PROPERTIES=false, so an envelope with
        // unexpected keys must still deserialize.
        let add: BailianAddResponse =
            serde_json::from_str(r#"{"code":"Success","data":{}}"#).unwrap();
        assert!(add.request_id.is_none());
        assert!(add.memory_nodes.is_none());

        let search: BailianSearchResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(search, BailianSearchResponse::default());
        assert_eq!(
            serde_json::to_value(BailianSearchResponse::default()).unwrap(),
            json!({})
        );
    }
}
