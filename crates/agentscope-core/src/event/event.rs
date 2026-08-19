//! Fine-grained agent events, mirroring the Java `AgentEvent` hierarchy.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::message::JsonMap;

use super::event_type::AgentEventType;

fn new_id() -> String {
    Uuid::new_v4().to_string().replace('-', "")
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// A fine-grained event emitted during agent execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentEvent {
    #[serde(rename = "AGENT_START")]
    AgentStart(EventData),
    #[serde(rename = "AGENT_END")]
    AgentEnd(EventData),
    #[serde(rename = "AGENT_RESULT")]
    AgentResult(EventData),
    #[serde(rename = "MODEL_CALL_START")]
    ModelCallStart(EventData),
    #[serde(rename = "MODEL_CALL_END")]
    ModelCallEnd(EventData),
    #[serde(rename = "TEXT_BLOCK_START")]
    TextBlockStart(EventData),
    #[serde(rename = "TEXT_BLOCK_DELTA")]
    TextBlockDelta(EventData),
    #[serde(rename = "TEXT_BLOCK_END")]
    TextBlockEnd(EventData),
    #[serde(rename = "THINKING_BLOCK_START")]
    ThinkingBlockStart(EventData),
    #[serde(rename = "THINKING_BLOCK_DELTA")]
    ThinkingBlockDelta(EventData),
    #[serde(rename = "THINKING_BLOCK_END")]
    ThinkingBlockEnd(EventData),
    #[serde(rename = "DATA_BLOCK_START")]
    DataBlockStart(EventData),
    #[serde(rename = "DATA_BLOCK_DELTA")]
    DataBlockDelta(EventData),
    #[serde(rename = "DATA_BLOCK_END")]
    DataBlockEnd(EventData),
    #[serde(rename = "TOOL_CALL_START")]
    ToolCallStart(EventData),
    #[serde(rename = "TOOL_CALL_DELTA")]
    ToolCallDelta(EventData),
    #[serde(rename = "TOOL_CALL_END")]
    ToolCallEnd(EventData),
    #[serde(rename = "TOOL_RESULT_START")]
    ToolResultStart(EventData),
    #[serde(rename = "TOOL_RESULT_TEXT_DELTA")]
    ToolResultTextDelta(EventData),
    #[serde(rename = "TOOL_RESULT_DATA_DELTA")]
    ToolResultDataDelta(EventData),
    #[serde(rename = "TOOL_RESULT_END")]
    ToolResultEnd(EventData),
    #[serde(rename = "EXCEED_MAX_ITERS")]
    ExceedMaxIters(EventData),
    #[serde(rename = "REQUIRE_USER_CONFIRM")]
    RequireUserConfirm(EventData),
    #[serde(rename = "REQUIRE_EXTERNAL_EXECUTION")]
    RequireExternalExecution(EventData),
    #[serde(rename = "USER_CONFIRM_RESULT")]
    UserConfirmResult(EventData),
    #[serde(rename = "EXTERNAL_EXECUTION_RESULT")]
    ExternalExecutionResult(EventData),
    #[serde(rename = "REQUEST_STOP")]
    RequestStop(EventData),
    #[serde(rename = "SUBAGENT_EXPOSED")]
    SubagentExposed(EventData),
    #[serde(rename = "HINT_BLOCK")]
    HintBlock(EventData),
    #[serde(rename = "ALL_TOOLS_DENIED")]
    AllToolsDenied(EventData),
    #[serde(rename = "CUSTOM")]
    Custom(EventData),
}

/// Common payload fields shared by all events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EventData {
    /// Unique identifier of the event.
    #[serde(default = "new_id")]
    pub id: String,
    /// Creation timestamp (RFC 3339).
    #[serde(default = "now_iso")]
    pub created_at: String,
    /// Source path identifying the originating agent (e.g. `main/researcher`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Optional metadata attached to this event.
    #[serde(default, skip_serializing_if = "JsonMap::is_empty")]
    pub metadata: JsonMap,
    /// Additional type-specific fields (block text, tool name, etc.).
    #[serde(flatten)]
    pub fields: JsonMap,
}

impl AgentEvent {
    /// Returns the event type.
    pub fn event_type(&self) -> AgentEventType {
        match self {
            AgentEvent::AgentStart(_) => AgentEventType::AgentStart,
            AgentEvent::AgentEnd(_) => AgentEventType::AgentEnd,
            AgentEvent::AgentResult(_) => AgentEventType::AgentResult,
            AgentEvent::ModelCallStart(_) => AgentEventType::ModelCallStart,
            AgentEvent::ModelCallEnd(_) => AgentEventType::ModelCallEnd,
            AgentEvent::TextBlockStart(_) => AgentEventType::TextBlockStart,
            AgentEvent::TextBlockDelta(_) => AgentEventType::TextBlockDelta,
            AgentEvent::TextBlockEnd(_) => AgentEventType::TextBlockEnd,
            AgentEvent::ThinkingBlockStart(_) => AgentEventType::ThinkingBlockStart,
            AgentEvent::ThinkingBlockDelta(_) => AgentEventType::ThinkingBlockDelta,
            AgentEvent::ThinkingBlockEnd(_) => AgentEventType::ThinkingBlockEnd,
            AgentEvent::DataBlockStart(_) => AgentEventType::DataBlockStart,
            AgentEvent::DataBlockDelta(_) => AgentEventType::DataBlockDelta,
            AgentEvent::DataBlockEnd(_) => AgentEventType::DataBlockEnd,
            AgentEvent::ToolCallStart(_) => AgentEventType::ToolCallStart,
            AgentEvent::ToolCallDelta(_) => AgentEventType::ToolCallDelta,
            AgentEvent::ToolCallEnd(_) => AgentEventType::ToolCallEnd,
            AgentEvent::ToolResultStart(_) => AgentEventType::ToolResultStart,
            AgentEvent::ToolResultTextDelta(_) => AgentEventType::ToolResultTextDelta,
            AgentEvent::ToolResultDataDelta(_) => AgentEventType::ToolResultDataDelta,
            AgentEvent::ToolResultEnd(_) => AgentEventType::ToolResultEnd,
            AgentEvent::ExceedMaxIters(_) => AgentEventType::ExceedMaxIters,
            AgentEvent::RequireUserConfirm(_) => AgentEventType::RequireUserConfirm,
            AgentEvent::RequireExternalExecution(_) => AgentEventType::RequireExternalExecution,
            AgentEvent::UserConfirmResult(_) => AgentEventType::UserConfirmResult,
            AgentEvent::ExternalExecutionResult(_) => AgentEventType::ExternalExecutionResult,
            AgentEvent::RequestStop(_) => AgentEventType::RequestStop,
            AgentEvent::SubagentExposed(_) => AgentEventType::SubagentExposed,
            AgentEvent::HintBlock(_) => AgentEventType::HintBlock,
            AgentEvent::AllToolsDenied(_) => AgentEventType::AllToolsDenied,
            AgentEvent::Custom(_) => AgentEventType::Custom,
        }
    }

    /// Returns the underlying data payload.
    pub fn data(&self) -> &EventData {
        match self {
            AgentEvent::AgentStart(d) => d,
            AgentEvent::AgentEnd(d) => d,
            AgentEvent::AgentResult(d) => d,
            AgentEvent::ModelCallStart(d) => d,
            AgentEvent::ModelCallEnd(d) => d,
            AgentEvent::TextBlockStart(d) => d,
            AgentEvent::TextBlockDelta(d) => d,
            AgentEvent::TextBlockEnd(d) => d,
            AgentEvent::ThinkingBlockStart(d) => d,
            AgentEvent::ThinkingBlockDelta(d) => d,
            AgentEvent::ThinkingBlockEnd(d) => d,
            AgentEvent::DataBlockStart(d) => d,
            AgentEvent::DataBlockDelta(d) => d,
            AgentEvent::DataBlockEnd(d) => d,
            AgentEvent::ToolCallStart(d) => d,
            AgentEvent::ToolCallDelta(d) => d,
            AgentEvent::ToolCallEnd(d) => d,
            AgentEvent::ToolResultStart(d) => d,
            AgentEvent::ToolResultTextDelta(d) => d,
            AgentEvent::ToolResultDataDelta(d) => d,
            AgentEvent::ToolResultEnd(d) => d,
            AgentEvent::ExceedMaxIters(d) => d,
            AgentEvent::RequireUserConfirm(d) => d,
            AgentEvent::RequireExternalExecution(d) => d,
            AgentEvent::UserConfirmResult(d) => d,
            AgentEvent::ExternalExecutionResult(d) => d,
            AgentEvent::RequestStop(d) => d,
            AgentEvent::SubagentExposed(d) => d,
            AgentEvent::HintBlock(d) => d,
            AgentEvent::AllToolsDenied(d) => d,
            AgentEvent::Custom(d) => d,
        }
    }

    /// Returns the event id.
    pub fn id(&self) -> &str {
        &self.data().id
    }

    /// Returns the event creation timestamp.
    pub fn created_at(&self) -> &str {
        &self.data().created_at
    }

    /// Returns the source path, if any.
    pub fn source(&self) -> Option<&str> {
        self.data().source.as_deref()
    }

    /// Returns the event metadata.
    pub fn metadata(&self) -> &JsonMap {
        &self.data().metadata
    }

    /// Creates a new event of the given type with default fields.
    pub fn new(event_type: AgentEventType) -> Self {
        Self::with_fields(event_type, JsonMap::new())
    }

    /// Creates a new event with the given flattened type-specific fields.
    pub fn with_fields(event_type: AgentEventType, fields: JsonMap) -> Self {
        let data = EventData {
            id: new_id(),
            created_at: now_iso(),
            source: None,
            metadata: JsonMap::new(),
            fields,
        };
        AgentEvent::from_parts(event_type, data)
    }

    fn from_parts(event_type: AgentEventType, data: EventData) -> Self {
        match event_type {
            AgentEventType::AgentStart => AgentEvent::AgentStart(data),
            AgentEventType::AgentEnd => AgentEvent::AgentEnd(data),
            AgentEventType::AgentResult => AgentEvent::AgentResult(data),
            AgentEventType::ModelCallStart => AgentEvent::ModelCallStart(data),
            AgentEventType::ModelCallEnd => AgentEvent::ModelCallEnd(data),
            AgentEventType::TextBlockStart => AgentEvent::TextBlockStart(data),
            AgentEventType::TextBlockDelta => AgentEvent::TextBlockDelta(data),
            AgentEventType::TextBlockEnd => AgentEvent::TextBlockEnd(data),
            AgentEventType::ThinkingBlockStart => AgentEvent::ThinkingBlockStart(data),
            AgentEventType::ThinkingBlockDelta => AgentEvent::ThinkingBlockDelta(data),
            AgentEventType::ThinkingBlockEnd => AgentEvent::ThinkingBlockEnd(data),
            AgentEventType::DataBlockStart => AgentEvent::DataBlockStart(data),
            AgentEventType::DataBlockDelta => AgentEvent::DataBlockDelta(data),
            AgentEventType::DataBlockEnd => AgentEvent::DataBlockEnd(data),
            AgentEventType::ToolCallStart => AgentEvent::ToolCallStart(data),
            AgentEventType::ToolCallDelta => AgentEvent::ToolCallDelta(data),
            AgentEventType::ToolCallEnd => AgentEvent::ToolCallEnd(data),
            AgentEventType::ToolResultStart => AgentEvent::ToolResultStart(data),
            AgentEventType::ToolResultTextDelta => AgentEvent::ToolResultTextDelta(data),
            AgentEventType::ToolResultDataDelta => AgentEvent::ToolResultDataDelta(data),
            AgentEventType::ToolResultEnd => AgentEvent::ToolResultEnd(data),
            AgentEventType::ExceedMaxIters => AgentEvent::ExceedMaxIters(data),
            AgentEventType::RequireUserConfirm => AgentEvent::RequireUserConfirm(data),
            AgentEventType::RequireExternalExecution => AgentEvent::RequireExternalExecution(data),
            AgentEventType::UserConfirmResult => AgentEvent::UserConfirmResult(data),
            AgentEventType::ExternalExecutionResult => AgentEvent::ExternalExecutionResult(data),
            AgentEventType::RequestStop => AgentEvent::RequestStop(data),
            AgentEventType::SubagentExposed => AgentEvent::SubagentExposed(data),
            AgentEventType::HintBlock => AgentEvent::HintBlock(data),
            AgentEventType::AllToolsDenied => AgentEvent::AllToolsDenied(data),
            AgentEventType::Custom => AgentEvent::Custom(data),
        }
    }

    /// Sets the source path and returns the event (subagent forwarding).
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        match &mut self {
            AgentEvent::AgentStart(d) => d.source = Some(source.into()),
            AgentEvent::AgentEnd(d) => d.source = Some(source.into()),
            AgentEvent::AgentResult(d) => d.source = Some(source.into()),
            AgentEvent::ModelCallStart(d) => d.source = Some(source.into()),
            AgentEvent::ModelCallEnd(d) => d.source = Some(source.into()),
            AgentEvent::TextBlockStart(d) => d.source = Some(source.into()),
            AgentEvent::TextBlockDelta(d) => d.source = Some(source.into()),
            AgentEvent::TextBlockEnd(d) => d.source = Some(source.into()),
            AgentEvent::ThinkingBlockStart(d) => d.source = Some(source.into()),
            AgentEvent::ThinkingBlockDelta(d) => d.source = Some(source.into()),
            AgentEvent::ThinkingBlockEnd(d) => d.source = Some(source.into()),
            AgentEvent::DataBlockStart(d) => d.source = Some(source.into()),
            AgentEvent::DataBlockDelta(d) => d.source = Some(source.into()),
            AgentEvent::DataBlockEnd(d) => d.source = Some(source.into()),
            AgentEvent::ToolCallStart(d) => d.source = Some(source.into()),
            AgentEvent::ToolCallDelta(d) => d.source = Some(source.into()),
            AgentEvent::ToolCallEnd(d) => d.source = Some(source.into()),
            AgentEvent::ToolResultStart(d) => d.source = Some(source.into()),
            AgentEvent::ToolResultTextDelta(d) => d.source = Some(source.into()),
            AgentEvent::ToolResultDataDelta(d) => d.source = Some(source.into()),
            AgentEvent::ToolResultEnd(d) => d.source = Some(source.into()),
            AgentEvent::ExceedMaxIters(d) => d.source = Some(source.into()),
            AgentEvent::RequireUserConfirm(d) => d.source = Some(source.into()),
            AgentEvent::RequireExternalExecution(d) => d.source = Some(source.into()),
            AgentEvent::UserConfirmResult(d) => d.source = Some(source.into()),
            AgentEvent::ExternalExecutionResult(d) => d.source = Some(source.into()),
            AgentEvent::RequestStop(d) => d.source = Some(source.into()),
            AgentEvent::SubagentExposed(d) => d.source = Some(source.into()),
            AgentEvent::HintBlock(d) => d.source = Some(source.into()),
            AgentEvent::AllToolsDenied(d) => d.source = Some(source.into()),
            AgentEvent::Custom(d) => d.source = Some(source.into()),
        }
        self
    }

    /// Inserts a metadata entry and returns the event.
    pub fn with_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        let d = self.data_mut();
        d.metadata.insert(key.into(), value);
        self
    }

    fn data_mut(&mut self) -> &mut EventData {
        match self {
            AgentEvent::AgentStart(d) => d,
            AgentEvent::AgentEnd(d) => d,
            AgentEvent::AgentResult(d) => d,
            AgentEvent::ModelCallStart(d) => d,
            AgentEvent::ModelCallEnd(d) => d,
            AgentEvent::TextBlockStart(d) => d,
            AgentEvent::TextBlockDelta(d) => d,
            AgentEvent::TextBlockEnd(d) => d,
            AgentEvent::ThinkingBlockStart(d) => d,
            AgentEvent::ThinkingBlockDelta(d) => d,
            AgentEvent::ThinkingBlockEnd(d) => d,
            AgentEvent::DataBlockStart(d) => d,
            AgentEvent::DataBlockDelta(d) => d,
            AgentEvent::DataBlockEnd(d) => d,
            AgentEvent::ToolCallStart(d) => d,
            AgentEvent::ToolCallDelta(d) => d,
            AgentEvent::ToolCallEnd(d) => d,
            AgentEvent::ToolResultStart(d) => d,
            AgentEvent::ToolResultTextDelta(d) => d,
            AgentEvent::ToolResultDataDelta(d) => d,
            AgentEvent::ToolResultEnd(d) => d,
            AgentEvent::ExceedMaxIters(d) => d,
            AgentEvent::RequireUserConfirm(d) => d,
            AgentEvent::RequireExternalExecution(d) => d,
            AgentEvent::UserConfirmResult(d) => d,
            AgentEvent::ExternalExecutionResult(d) => d,
            AgentEvent::RequestStop(d) => d,
            AgentEvent::SubagentExposed(d) => d,
            AgentEvent::HintBlock(d) => d,
            AgentEvent::AllToolsDenied(d) => d,
            AgentEvent::Custom(d) => d,
        }
    }
}
