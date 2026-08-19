//! Represents a chat completion response from a language model.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::message::{ChatUsage, ContentBlock, JsonMap};

/// A chat completion response from a language model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatResponse {
    /// Unique identifier for this response.
    pub id: String,
    /// Content blocks containing the response content.
    pub content: Vec<ContentBlock>,
    /// Token usage information for this response.
    pub usage: ChatUsage,
    /// Additional metadata from the model provider.
    #[serde(default)]
    pub metadata: JsonMap,
    /// Reason the model stopped generating tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

impl ChatResponse {
    /// Creates a new `ChatResponse`.
    pub fn new(
        id: impl Into<String>,
        content: Vec<ContentBlock>,
        usage: ChatUsage,
        metadata: Option<JsonMap>,
        finish_reason: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            content,
            usage,
            metadata: metadata.unwrap_or_default(),
            finish_reason,
        }
    }

    /// Returns a copy with the given id.
    pub fn with_id(&self, new_id: impl Into<String>) -> Self {
        Self {
            id: new_id.into(),
            content: self.content.clone(),
            usage: self.usage,
            metadata: self.metadata.clone(),
            finish_reason: self.finish_reason.clone(),
        }
    }

    /// Creates a new builder.
    pub fn builder() -> ChatResponseBuilder {
        ChatResponseBuilder::default()
    }
}

/// Builder for `ChatResponse`.
#[derive(Debug, Default)]
pub struct ChatResponseBuilder {
    id: Option<String>,
    content: Option<Vec<ContentBlock>>,
    usage: Option<ChatUsage>,
    metadata: Option<JsonMap>,
    finish_reason: Option<String>,
}

impl ChatResponseBuilder {
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn content(mut self, content: Vec<ContentBlock>) -> Self {
        self.content = Some(content);
        self
    }

    pub fn usage(mut self, usage: ChatUsage) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn metadata(mut self, metadata: JsonMap) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn finish_reason(mut self, finish_reason: impl Into<String>) -> Self {
        self.finish_reason = Some(finish_reason.into());
        self
    }

    /// Builds the response, auto-generating the id if absent (e.g. Ollama).
    pub fn build(self) -> ChatResponse {
        let id = self
            .id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        ChatResponse {
            id,
            content: self.content.unwrap_or_default(),
            usage: self.usage.unwrap_or_default(),
            metadata: self.metadata.unwrap_or_default(),
            finish_reason: self.finish_reason,
        }
    }
}
