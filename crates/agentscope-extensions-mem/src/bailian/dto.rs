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
