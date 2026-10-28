//! ReMe API data transfer objects.
//!
//! Field names mirror the Java `io.agentscope.core.memory.reme` DTOs.

use serde::{Deserialize, Serialize};

use agentscope_core::JsonMap;

/// A message in the ReMe API format.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReMeMessage {
    /// Role of the sender, `"user"` or `"assistant"`.
    pub role: String,
    /// Text content.
    pub content: String,
}

impl ReMeMessage {
    /// Creates a message.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

/// A conversation sequence sent to ReMe.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReMeTrajectory {
    /// Messages forming this trajectory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<ReMeMessage>>,
}

impl ReMeTrajectory {
    /// Creates a trajectory from messages.
    pub fn new(messages: Vec<ReMeMessage>) -> Self {
        Self {
            messages: Some(messages),
        }
    }
}

/// Request body for `POST /summary_personal_memory`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReMeAddRequest {
    /// Workspace identifier.
    #[serde(rename = "workspace_id", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Trajectories to process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trajectories: Option<Vec<ReMeTrajectory>>,
}

impl ReMeAddRequest {
    /// Creates a request with the workspace id and trajectories (Java convenience ctor).
    pub fn new(workspace_id: impl Into<String>, trajectories: Vec<ReMeTrajectory>) -> Self {
        Self {
            workspace_id: Some(workspace_id.into()),
            trajectories: Some(trajectories),
        }
    }
}

/// Request body for `POST /retrieve_personal_memory`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReMeSearchRequest {
    /// Workspace identifier.
    #[serde(rename = "workspace_id", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Search query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Maximum number of results (Java default `5`).
    #[serde(rename = "top_k", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
}

impl Default for ReMeSearchRequest {
    fn default() -> Self {
        Self {
            workspace_id: None,
            query: None,
            top_k: Some(5),
        }
    }
}

impl ReMeSearchRequest {
    /// Creates a request with Java defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a request with workspace, query and `top_k` (Java convenience ctor).
    pub fn with_parts(
        workspace_id: impl Into<String>,
        query: impl Into<String>,
        top_k: u32,
    ) -> Self {
        Self {
            workspace_id: Some(workspace_id.into()),
            query: Some(query.into()),
            top_k: Some(top_k),
        }
    }

    /// Sets the workspace identifier.
    pub fn with_workspace_id(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspace_id = Some(workspace_id.into());
        self
    }

    /// Sets the query.
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }

    /// Sets `top_k`.
    pub fn with_top_k(mut self, top_k: u32) -> Self {
        self.top_k = Some(top_k);
        self
    }
}

/// A single memory item returned by ReMe (shared by add / search responses).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReMeMemoryItem {
    /// Workspace identifier.
    #[serde(
        rename = "workspace_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<String>,
    /// Memory identifier.
    #[serde(rename = "memory_id", default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
    /// Memory type.
    #[serde(
        rename = "memory_type",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub memory_type: Option<String>,
    /// When the memory should be used.
    #[serde(
        rename = "when_to_use",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub when_to_use: Option<String>,
    /// Memory content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Relevance score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Creation timestamp (string, as returned by ReMe).
    #[serde(
        rename = "time_created",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub time_created: Option<String>,
    /// Modification timestamp (string, as returned by ReMe).
    #[serde(
        rename = "time_modified",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub time_modified: Option<String>,
    /// Author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Additional metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    /// Target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Reflection subject.
    #[serde(
        rename = "reflection_subject",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reflection_subject: Option<String>,
}

/// Update counters reported by the ReMe add API.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReMeUpdateResult {
    /// Number of deleted memories.
    #[serde(
        rename = "deleted_count",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub deleted_count: Option<i64>,
    /// Number of inserted memories.
    #[serde(
        rename = "inserted_count",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub inserted_count: Option<i64>,
}

/// Metadata block of a ReMe add response.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReMeAddMetadata {
    /// Memory list.
    #[serde(
        rename = "memory_list",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub memory_list: Option<Vec<ReMeMemoryItem>>,
    /// Deleted memory ids.
    #[serde(
        rename = "deleted_memory_ids",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub deleted_memory_ids: Option<Vec<String>>,
    /// Update result counters.
    #[serde(
        rename = "update_result",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub update_result: Option<ReMeUpdateResult>,
}

/// Response of the ReMe add API.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReMeAddResponse {
    /// Answer text (usually empty for add).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// Whether the operation succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    /// Metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ReMeAddMetadata>,
}

/// Metadata block of a ReMe search response.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReMeSearchMetadata {
    /// Memory list.
    #[serde(
        rename = "memory_list",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub memory_list: Option<Vec<ReMeMemoryItem>>,
}

/// Response of the ReMe search API.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReMeSearchResponse {
    /// Answer text containing retrieved memories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// Whether the operation succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    /// Metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ReMeSearchMetadata>,
}

impl ReMeSearchResponse {
    /// Extracts memory contents, mirroring Java `ReMeSearchResponse.getMemories`.
    pub fn memories(&self) -> Vec<String> {
        self.metadata
            .as_ref()
            .and_then(|m| m.memory_list.as_ref())
            .map(|items| items.iter().filter_map(|i| i.content.clone()).collect())
            .unwrap_or_default()
    }
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
        let message = ReMeMessage::new("user", "remember");
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value, expected(r#"{"role":"user","content":"remember"}"#));
        let back: ReMeMessage = serde_json::from_value(value).unwrap();
        assert_eq!(back, message);
        assert_eq!(ReMeMessage::default(), ReMeMessage::new("", ""));
    }

    #[test]
    fn trajectory_wraps_messages_and_skips_absent_field() {
        let trajectory = ReMeTrajectory::new(vec![ReMeMessage::new("user", "hi")]);
        let value = serde_json::to_value(&trajectory).unwrap();
        assert_eq!(value["messages"][0]["content"], "hi");
        let back: ReMeTrajectory = serde_json::from_value(value).unwrap();
        assert_eq!(back, trajectory);

        assert_eq!(
            serde_json::to_value(ReMeTrajectory::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn add_request_serializes_workspace_id_and_trajectories() {
        let request = ReMeAddRequest::new(
            "ws",
            vec![ReMeTrajectory::new(vec![ReMeMessage::new("user", "hi")])],
        );
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["workspace_id"], "ws");
        assert_eq!(value["trajectories"][0]["messages"][0]["role"], "user");
        let back: ReMeAddRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
        assert!(serde_json::to_value(ReMeAddRequest::default())
            .unwrap()
            .get("workspace_id")
            .is_none());
    }

    #[test]
    fn search_request_defaults_top_k_to_five() {
        let request = ReMeSearchRequest::new();
        assert_eq!(request.top_k, Some(5));
        assert_eq!(request, ReMeSearchRequest::default());
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value, expected(r#"{"top_k":5}"#));
        let back: ReMeSearchRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn search_request_with_parts_serializes_query_and_top_k() {
        let request = ReMeSearchRequest::with_parts("ws", "q", 3)
            .with_query("q2")
            .with_top_k(7);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["workspace_id"], "ws");
        assert_eq!(value["query"], "q2");
        assert_eq!(value["top_k"], 7);
        let back: ReMeSearchRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn memory_item_deserializes_snake_case_fields() {
        let item: ReMeMemoryItem = serde_json::from_str(
            r#"{"workspace_id":"ws","memory_id":"m1","memory_type":"profile","when_to_use":"always","content":"c","score":0.25,"time_created":"2024-01-01T00:00:00Z","time_modified":"2024-01-02T00:00:00Z","author":"a","target":"t","reflection_subject":"s","metadata":{"k":"v"}}"#,
        )
        .unwrap();
        assert_eq!(item.workspace_id.as_deref(), Some("ws"));
        assert_eq!(item.memory_id.as_deref(), Some("m1"));
        assert_eq!(item.memory_type.as_deref(), Some("profile"));
        assert_eq!(item.when_to_use.as_deref(), Some("always"));
        assert_eq!(item.content.as_deref(), Some("c"));
        assert_eq!(item.score, Some(0.25));
        assert_eq!(item.time_created.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(item.time_modified.as_deref(), Some("2024-01-02T00:00:00Z"));
        assert_eq!(item.author.as_deref(), Some("a"));
        assert_eq!(item.target.as_deref(), Some("t"));
        assert_eq!(item.reflection_subject.as_deref(), Some("s"));
        assert_eq!(item.metadata.as_ref().unwrap()["k"], "v");

        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["memory_id"], "m1");
        assert_eq!(value["reflection_subject"], "s");
        assert!(value.get("memoryId").is_none());
    }

    #[test]
    fn memory_item_serializes_nothing_when_empty() {
        assert_eq!(
            serde_json::to_value(ReMeMemoryItem::default()).unwrap(),
            json!({})
        );
        let item: ReMeMemoryItem = serde_json::from_str("{}").unwrap();
        assert_eq!(item, ReMeMemoryItem::default());
    }

    #[test]
    fn add_response_deserializes_metadata_and_update_result() {
        let response: ReMeAddResponse = serde_json::from_str(
            r#"{"answer":"","success":true,"metadata":{"memory_list":[{"content":"new"}],"deleted_memory_ids":["m0"],"update_result":{"deleted_count":1,"inserted_count":2}}}"#,
        )
        .unwrap();
        assert_eq!(response.answer.as_deref(), Some(""));
        assert_eq!(response.success, Some(true));
        let metadata = response.metadata.as_ref().unwrap();
        assert_eq!(
            metadata.memory_list.as_ref().unwrap()[0].content.as_deref(),
            Some("new")
        );
        assert_eq!(
            metadata.deleted_memory_ids.clone(),
            Some(vec!["m0".to_string()])
        );
        let update = metadata.update_result.as_ref().unwrap();
        assert_eq!(update.deleted_count, Some(1));
        assert_eq!(update.inserted_count, Some(2));

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["metadata"]["update_result"]["inserted_count"], 2);
        assert_eq!(value["metadata"]["deleted_memory_ids"][0], "m0");
    }

    #[test]
    fn search_response_memories_extracts_contents_in_order() {
        let response: ReMeSearchResponse = serde_json::from_str(
            r#"{"answer":"","success":true,"metadata":{"memory_list":[{"content":"m1"},{"memory_id":"no-content"},{"content":"m2"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            response.memories(),
            vec!["m1".to_string(), "m2".to_string()]
        );
        assert_eq!(response.success, Some(true));
        let roundtrip: ReMeSearchResponse =
            serde_json::from_value(serde_json::to_value(&response).unwrap()).unwrap();
        assert_eq!(roundtrip, response);
    }

    #[test]
    fn search_response_without_metadata_has_no_memories() {
        let response: ReMeSearchResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(response, ReMeSearchResponse::default());
        assert!(response.memories().is_empty());
        assert_eq!(
            serde_json::to_value(ReMeSearchResponse::default()).unwrap(),
            json!({})
        );

        // A metadata block without a memory list also yields no memories.
        let empty_metadata: ReMeSearchResponse =
            serde_json::from_str(r#"{"metadata":{}}"#).unwrap();
        assert!(empty_metadata.memories().is_empty());
        assert!(empty_metadata
            .metadata
            .as_ref()
            .unwrap()
            .memory_list
            .is_none());
    }
}
