//! Fine-grained event types emitted during agent execution.
use serde::{Deserialize, Serialize};

/// Canonical fine-grained event type names, mirroring `AgentEventType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEventType {
    AgentStart,
    AgentEnd,
    AgentResult,
    ModelCallStart,
    ModelCallEnd,
    TextBlockStart,
    TextBlockDelta,
    TextBlockEnd,
    ThinkingBlockStart,
    ThinkingBlockDelta,
    ThinkingBlockEnd,
    DataBlockStart,
    DataBlockDelta,
    DataBlockEnd,
    ToolCallStart,
    ToolCallDelta,
    ToolCallEnd,
    ToolResultStart,
    /// Intermediate progress published by a running tool through its emitter.
    ///
    /// Mirrors the Java `AgentEventType.TOOL_PROGRESS`. Progress is display-only: it
    /// is never fed back to the model, only the tool's return value is.
    ToolProgress,
    ToolResultTextDelta,
    ToolResultDataDelta,
    ToolResultEnd,
    ExceedMaxIters,
    RequireUserConfirm,
    RequireExternalExecution,
    UserConfirmResult,
    ExternalExecutionResult,
    RequestStop,
    SubagentExposed,
    HintBlock,
    AllToolsDenied,
    Custom,
}

impl AgentEventType {
    /// Returns the canonical wire name (e.g. `AGENT_START`).
    pub fn wire_name(&self) -> &'static str {
        match self {
            AgentEventType::AgentStart => "AGENT_START",
            AgentEventType::AgentEnd => "AGENT_END",
            AgentEventType::AgentResult => "AGENT_RESULT",
            AgentEventType::ModelCallStart => "MODEL_CALL_START",
            AgentEventType::ModelCallEnd => "MODEL_CALL_END",
            AgentEventType::TextBlockStart => "TEXT_BLOCK_START",
            AgentEventType::TextBlockDelta => "TEXT_BLOCK_DELTA",
            AgentEventType::TextBlockEnd => "TEXT_BLOCK_END",
            AgentEventType::ThinkingBlockStart => "THINKING_BLOCK_START",
            AgentEventType::ThinkingBlockDelta => "THINKING_BLOCK_DELTA",
            AgentEventType::ThinkingBlockEnd => "THINKING_BLOCK_END",
            AgentEventType::DataBlockStart => "DATA_BLOCK_START",
            AgentEventType::DataBlockDelta => "DATA_BLOCK_DELTA",
            AgentEventType::DataBlockEnd => "DATA_BLOCK_END",
            AgentEventType::ToolCallStart => "TOOL_CALL_START",
            AgentEventType::ToolCallDelta => "TOOL_CALL_DELTA",
            AgentEventType::ToolCallEnd => "TOOL_CALL_END",
            AgentEventType::ToolResultStart => "TOOL_RESULT_START",
            AgentEventType::ToolProgress => "TOOL_PROGRESS",
            AgentEventType::ToolResultTextDelta => "TOOL_RESULT_TEXT_DELTA",
            AgentEventType::ToolResultDataDelta => "TOOL_RESULT_DATA_DELTA",
            AgentEventType::ToolResultEnd => "TOOL_RESULT_END",
            AgentEventType::ExceedMaxIters => "EXCEED_MAX_ITERS",
            AgentEventType::RequireUserConfirm => "REQUIRE_USER_CONFIRM",
            AgentEventType::RequireExternalExecution => "REQUIRE_EXTERNAL_EXECUTION",
            AgentEventType::UserConfirmResult => "USER_CONFIRM_RESULT",
            AgentEventType::ExternalExecutionResult => "EXTERNAL_EXECUTION_RESULT",
            AgentEventType::RequestStop => "REQUEST_STOP",
            AgentEventType::SubagentExposed => "SUBAGENT_EXPOSED",
            AgentEventType::HintBlock => "HINT_BLOCK",
            AgentEventType::AllToolsDenied => "ALL_TOOLS_DENIED",
            AgentEventType::Custom => "CUSTOM",
        }
    }

    /// Resolves from a wire name or a legacy alias.
    pub fn from_wire_name(raw: &str) -> Option<AgentEventType> {
        let canonical = match raw {
            "RUN_STARTED" | "REPLY_START" => AgentEventType::AgentStart,
            "RUN_FINISHED" | "REPLY_END" => AgentEventType::AgentEnd,
            "MODEL_CALL_STARTED" => AgentEventType::ModelCallStart,
            "MODEL_CALL_ENDED" => AgentEventType::ModelCallEnd,
            "BINARY_BLOCK_START" => AgentEventType::DataBlockStart,
            "BINARY_BLOCK_DELTA" => AgentEventType::DataBlockDelta,
            "BINARY_BLOCK_END" => AgentEventType::DataBlockEnd,
            "TOOL_RESULT_BINARY_DELTA" => AgentEventType::ToolResultDataDelta,
            "THREAD_EXPOSED" => AgentEventType::SubagentExposed,
            _ => AgentEventType::from_wire_name_canonical(raw)?,
        };
        Some(canonical)
    }

    fn from_wire_name_canonical(raw: &str) -> Option<AgentEventType> {
        Some(match raw {
            "AGENT_START" => AgentEventType::AgentStart,
            "AGENT_END" => AgentEventType::AgentEnd,
            "AGENT_RESULT" => AgentEventType::AgentResult,
            "MODEL_CALL_START" => AgentEventType::ModelCallStart,
            "MODEL_CALL_END" => AgentEventType::ModelCallEnd,
            "TEXT_BLOCK_START" => AgentEventType::TextBlockStart,
            "TEXT_BLOCK_DELTA" => AgentEventType::TextBlockDelta,
            "TEXT_BLOCK_END" => AgentEventType::TextBlockEnd,
            "THINKING_BLOCK_START" => AgentEventType::ThinkingBlockStart,
            "THINKING_BLOCK_DELTA" => AgentEventType::ThinkingBlockDelta,
            "THINKING_BLOCK_END" => AgentEventType::ThinkingBlockEnd,
            "DATA_BLOCK_START" => AgentEventType::DataBlockStart,
            "DATA_BLOCK_DELTA" => AgentEventType::DataBlockDelta,
            "DATA_BLOCK_END" => AgentEventType::DataBlockEnd,
            "TOOL_CALL_START" => AgentEventType::ToolCallStart,
            "TOOL_CALL_DELTA" => AgentEventType::ToolCallDelta,
            "TOOL_CALL_END" => AgentEventType::ToolCallEnd,
            "TOOL_RESULT_START" => AgentEventType::ToolResultStart,
            "TOOL_PROGRESS" => AgentEventType::ToolProgress,
            "TOOL_RESULT_TEXT_DELTA" => AgentEventType::ToolResultTextDelta,
            "TOOL_RESULT_DATA_DELTA" => AgentEventType::ToolResultDataDelta,
            "TOOL_RESULT_END" => AgentEventType::ToolResultEnd,
            "EXCEED_MAX_ITERS" => AgentEventType::ExceedMaxIters,
            "REQUIRE_USER_CONFIRM" => AgentEventType::RequireUserConfirm,
            "REQUIRE_EXTERNAL_EXECUTION" => AgentEventType::RequireExternalExecution,
            "USER_CONFIRM_RESULT" => AgentEventType::UserConfirmResult,
            "EXTERNAL_EXECUTION_RESULT" => AgentEventType::ExternalExecutionResult,
            "REQUEST_STOP" => AgentEventType::RequestStop,
            "SUBAGENT_EXPOSED" => AgentEventType::SubagentExposed,
            "HINT_BLOCK" => AgentEventType::HintBlock,
            "ALL_TOOLS_DENIED" => AgentEventType::AllToolsDenied,
            "CUSTOM" => AgentEventType::Custom,
            _ => return None,
        })
    }
}
