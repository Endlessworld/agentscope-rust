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
        Self { role: role.into(), content: content.into() }
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
        Self { messages: Some(messages) }
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
        Self { workspace_id: Some(workspace_id.into()), trajectories: Some(trajectories) }
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
        Self { workspace_id: None, query: None, top_k: Some(5) }
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
    #[serde(rename = "workspace_id", default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Memory identifier.
    #[serde(rename = "memory_id", default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
    /// Memory type.
    #[serde(rename = "memory_type", default, skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    /// When the memory should be used.
    #[serde(rename = "when_to_use", default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Memory content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Relevance score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Creation timestamp (string, as returned by ReMe).
    #[serde(rename = "time_created", default, skip_serializing_if = "Option::is_none")]
    pub time_created: Option<String>,
    /// Modification timestamp (string, as returned by ReMe).
    #[serde(rename = "time_modified", default, skip_serializing_if = "Option::is_none")]
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
    #[serde(rename = "reflection_subject", default, skip_serializing_if = "Option::is_none")]
    pub reflection_subject: Option<String>,
}

/// Update counters reported by the ReMe add API.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReMeUpdateResult {
    /// Number of deleted memories.
    #[serde(rename = "deleted_count", default, skip_serializing_if = "Option::is_none")]
    pub deleted_count: Option<i64>,
    /// Number of inserted memories.
    #[serde(rename = "inserted_count", default, skip_serializing_if = "Option::is_none")]
    pub inserted_count: Option<i64>,
}

/// Metadata block of a ReMe add response.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReMeAddMetadata {
    /// Memory list.
    #[serde(rename = "memory_list", default, skip_serializing_if = "Option::is_none")]
    pub memory_list: Option<Vec<ReMeMemoryItem>>,
    /// Deleted memory ids.
    #[serde(rename = "deleted_memory_ids", default, skip_serializing_if = "Option::is_none")]
    pub deleted_memory_ids: Option<Vec<String>>,
    /// Update result counters.
    #[serde(rename = "update_result", default, skip_serializing_if = "Option::is_none")]
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
    #[serde(rename = "memory_list", default, skip_serializing_if = "Option::is_none")]
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
