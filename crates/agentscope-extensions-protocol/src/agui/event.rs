//! AG-UI event model, mirroring `io.agentscope.core.agui.event`.
//!
//! * [`AguiEventType`] - the full AG-UI event-type enumeration.
//! * [`AguiEvent`] - a serde tagged union discriminated by its `type` field.
//! * [`AguiEvents`] - helpers for copying events with new base properties.
//!
//! Deserialization is tolerant: an unknown `type` (or a known type whose payload
//! cannot be parsed) deserializes into [`AguiEvent::Unknown`] instead of failing,
//! so a server can keep streaming when a client adds new event kinds.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::model::AguiMessage;

/// Enumeration of all AG-UI protocol event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AguiEventType {
    /// An agent run has started.
    RunStarted,
    /// An agent run has finished.
    RunFinished,
    /// An agent run failed.
    RunError,
    /// A step started.
    StepStarted,
    /// A step finished.
    StepFinished,
    /// Start of a text message.
    TextMessageStart,
    /// Incremental text content of a message.
    TextMessageContent,
    /// End of a text message.
    TextMessageEnd,
    /// Auto start/close text message chunk.
    TextMessageChunk,
    /// Start of a tool call.
    ToolCallStart,
    /// Incremental tool-call arguments.
    ToolCallArgs,
    /// End of a tool call.
    ToolCallEnd,
    /// Auto start/close tool-call chunk.
    ToolCallChunk,
    /// Result of a tool call.
    ToolCallResult,
    /// Full state snapshot.
    StateSnapshot,
    /// Incremental state delta (JSON Patch).
    StateDelta,
    /// Snapshot of the conversation messages.
    MessagesSnapshot,
    /// Snapshot of an activity.
    ActivitySnapshot,
    /// Incremental activity delta.
    ActivityDelta,
    /// Raw pass-through event.
    Raw,
    /// Custom structured event.
    Custom,
    /// Start of a reasoning/thinking phase.
    ReasoningStart,
    /// Start of a reasoning message.
    ReasoningMessageStart,
    /// Incremental reasoning content.
    ReasoningMessageContent,
    /// End of a reasoning message.
    ReasoningMessageEnd,
    /// Auto start/close reasoning message chunk.
    ReasoningMessageChunk,
    /// End of a reasoning phase.
    ReasoningEnd,
    /// Encrypted reasoning value.
    ReasoningEncryptedValue,
}

impl AguiEventType {
    /// The wire name of this event type.
    pub fn as_str(&self) -> &'static str {
        match self {
            AguiEventType::RunStarted => "RUN_STARTED",
            AguiEventType::RunFinished => "RUN_FINISHED",
            AguiEventType::RunError => "RUN_ERROR",
            AguiEventType::StepStarted => "STEP_STARTED",
            AguiEventType::StepFinished => "STEP_FINISHED",
            AguiEventType::TextMessageStart => "TEXT_MESSAGE_START",
            AguiEventType::TextMessageContent => "TEXT_MESSAGE_CONTENT",
            AguiEventType::TextMessageEnd => "TEXT_MESSAGE_END",
            AguiEventType::TextMessageChunk => "TEXT_MESSAGE_CHUNK",
            AguiEventType::ToolCallStart => "TOOL_CALL_START",
            AguiEventType::ToolCallArgs => "TOOL_CALL_ARGS",
            AguiEventType::ToolCallEnd => "TOOL_CALL_END",
            AguiEventType::ToolCallChunk => "TOOL_CALL_CHUNK",
            AguiEventType::ToolCallResult => "TOOL_CALL_RESULT",
            AguiEventType::StateSnapshot => "STATE_SNAPSHOT",
            AguiEventType::StateDelta => "STATE_DELTA",
            AguiEventType::MessagesSnapshot => "MESSAGES_SNAPSHOT",
            AguiEventType::ActivitySnapshot => "ACTIVITY_SNAPSHOT",
            AguiEventType::ActivityDelta => "ACTIVITY_DELTA",
            AguiEventType::Raw => "RAW",
            AguiEventType::Custom => "CUSTOM",
            AguiEventType::ReasoningStart => "REASONING_START",
            AguiEventType::ReasoningMessageStart => "REASONING_MESSAGE_START",
            AguiEventType::ReasoningMessageContent => "REASONING_MESSAGE_CONTENT",
            AguiEventType::ReasoningMessageEnd => "REASONING_MESSAGE_END",
            AguiEventType::ReasoningMessageChunk => "REASONING_MESSAGE_CHUNK",
            AguiEventType::ReasoningEnd => "REASONING_END",
            AguiEventType::ReasoningEncryptedValue => "REASONING_ENCRYPTED_VALUE",
        }
    }

    /// Parses a wire event-type name.
    pub fn from_wire(name: &str) -> Option<Self> {
        let all = [
            AguiEventType::RunStarted,
            AguiEventType::RunFinished,
            AguiEventType::RunError,
            AguiEventType::StepStarted,
            AguiEventType::StepFinished,
            AguiEventType::TextMessageStart,
            AguiEventType::TextMessageContent,
            AguiEventType::TextMessageEnd,
            AguiEventType::TextMessageChunk,
            AguiEventType::ToolCallStart,
            AguiEventType::ToolCallArgs,
            AguiEventType::ToolCallEnd,
            AguiEventType::ToolCallChunk,
            AguiEventType::ToolCallResult,
            AguiEventType::StateSnapshot,
            AguiEventType::StateDelta,
            AguiEventType::MessagesSnapshot,
            AguiEventType::ActivitySnapshot,
            AguiEventType::ActivityDelta,
            AguiEventType::Raw,
            AguiEventType::Custom,
            AguiEventType::ReasoningStart,
            AguiEventType::ReasoningMessageStart,
            AguiEventType::ReasoningMessageContent,
            AguiEventType::ReasoningMessageEnd,
            AguiEventType::ReasoningMessageChunk,
            AguiEventType::ReasoningEnd,
            AguiEventType::ReasoningEncryptedValue,
        ];
        all.into_iter().find(|t| t.as_str() == name)
    }
}

/// A JSON Patch operation (RFC 6902) used by [`AguiEvent::StateDelta`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonPatchOperation {
    /// Operation name (`add`, `remove`, `replace`, `move`, `copy`, `test`).
    pub op: String,
    /// JSON Pointer path.
    pub path: String,
    /// Value to add/replace (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Source path for `move`/`copy` operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

impl JsonPatchOperation {
    /// Creates an `add` operation.
    pub fn add(path: impl Into<String>, value: Option<Value>) -> Self {
        Self {
            op: "add".to_string(),
            path: path.into(),
            value,
            from: None,
        }
    }

    /// Creates a `remove` operation.
    pub fn remove(path: impl Into<String>) -> Self {
        Self {
            op: "remove".to_string(),
            path: path.into(),
            value: None,
            from: None,
        }
    }

    /// Creates a `replace` operation.
    pub fn replace(path: impl Into<String>, value: Option<Value>) -> Self {
        Self {
            op: "replace".to_string(),
            path: path.into(),
            value,
            from: None,
        }
    }

    /// Creates a `move` operation.
    pub fn move_(from: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            op: "move".to_string(),
            path: path.into(),
            value: None,
            from: Some(from.into()),
        }
    }
}

/// An interrupt emitted when a run pauses, awaiting user resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interrupt {
    /// Interrupt identifier.
    pub id: String,
    /// Interrupt reason (e.g. `tool_call`, `input_required`).
    pub reason: String,
    /// Human-readable message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Tool call the interrupt applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// JSON schema describing the expected response payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Map<String, Value>>,
    /// ISO-8601 expiration timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Additional interrupt metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

impl Interrupt {
    /// Creates an interrupt with the given id and reason.
    pub fn new(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reason: reason.into(),
            message: None,
            tool_call_id: None,
            response_schema: None,
            expires_at: None,
            metadata: None,
        }
    }
}

/// Outcome of a finished run: success or an interrupt awaiting resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RunFinishedOutcome {
    /// The run completed successfully.
    Success,
    /// The run was interrupted and awaits user input.
    Interrupt {
        /// Pending interrupts.
        #[serde(default)]
        interrupts: Vec<Interrupt>,
    },
}

impl Default for RunFinishedOutcome {
    fn default() -> Self {
        RunFinishedOutcome::Success
    }
}

/// An AG-UI protocol event.
///
/// Serialized as a JSON object with a `type` discriminator and camelCase fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "SCREAMING_SNAKE_CASE",
    rename_all_fields = "camelCase"
)]
pub enum AguiEvent {
    RunStarted {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_run_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    RunFinished {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<RunFinishedOutcome>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    RunError {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    StepStarted {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        step_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    StepFinished {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        step_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    TextMessageStart {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(default)]
        role: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    TextMessageContent {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(default)]
        delta: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    TextMessageEnd {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    TextMessageChunk {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ToolCallStart {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        tool_call_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ToolCallArgs {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        tool_call_id: String,
        #[serde(default)]
        delta: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ToolCallEnd {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ToolCallChunk {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_message_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ToolCallResult {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    StateSnapshot {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        snapshot: Map<String, Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    StateDelta {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        delta: Vec<JsonPatchOperation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    MessagesSnapshot {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        messages: Vec<AguiMessage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ActivitySnapshot {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(default)]
        activity_type: String,
        #[serde(default)]
        content: Map<String, Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        replace: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ActivityDelta {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(default)]
        activity_type: String,
        #[serde(default)]
        patch: Vec<JsonPatchOperation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    Raw {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        event: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    Custom {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ReasoningStart {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ReasoningMessageStart {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(default)]
        role: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ReasoningMessageContent {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(default)]
        delta: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ReasoningMessageEnd {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ReasoningMessageChunk {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ReasoningEnd {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    ReasoningEncryptedValue {
        thread_id: String,
        run_id: String,
        #[serde(default)]
        subtype: String,
        #[serde(default)]
        entity_id: String,
        #[serde(default)]
        encrypted_value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_event: Option<Value>,
    },
    /// Fallback for unknown/unparseable event types (tolerant decoding).
    #[serde(rename = "UNKNOWN")]
    Unknown {
        /// The unrecognized wire `type` value.
        raw_type: String,
        /// The remaining payload fields.
        #[serde(flatten)]
        payload: Map<String, Value>,
    },
}

impl AguiEvent {
    /// The event type, or `None` for [`AguiEvent::Unknown`].
    pub fn event_type(&self) -> Option<AguiEventType> {
        Some(match self {
            AguiEvent::RunStarted { .. } => AguiEventType::RunStarted,
            AguiEvent::RunFinished { .. } => AguiEventType::RunFinished,
            AguiEvent::RunError { .. } => AguiEventType::RunError,
            AguiEvent::StepStarted { .. } => AguiEventType::StepStarted,
            AguiEvent::StepFinished { .. } => AguiEventType::StepFinished,
            AguiEvent::TextMessageStart { .. } => AguiEventType::TextMessageStart,
            AguiEvent::TextMessageContent { .. } => AguiEventType::TextMessageContent,
            AguiEvent::TextMessageEnd { .. } => AguiEventType::TextMessageEnd,
            AguiEvent::TextMessageChunk { .. } => AguiEventType::TextMessageChunk,
            AguiEvent::ToolCallStart { .. } => AguiEventType::ToolCallStart,
            AguiEvent::ToolCallArgs { .. } => AguiEventType::ToolCallArgs,
            AguiEvent::ToolCallEnd { .. } => AguiEventType::ToolCallEnd,
            AguiEvent::ToolCallChunk { .. } => AguiEventType::ToolCallChunk,
            AguiEvent::ToolCallResult { .. } => AguiEventType::ToolCallResult,
            AguiEvent::StateSnapshot { .. } => AguiEventType::StateSnapshot,
            AguiEvent::StateDelta { .. } => AguiEventType::StateDelta,
            AguiEvent::MessagesSnapshot { .. } => AguiEventType::MessagesSnapshot,
            AguiEvent::ActivitySnapshot { .. } => AguiEventType::ActivitySnapshot,
            AguiEvent::ActivityDelta { .. } => AguiEventType::ActivityDelta,
            AguiEvent::Raw { .. } => AguiEventType::Raw,
            AguiEvent::Custom { .. } => AguiEventType::Custom,
            AguiEvent::ReasoningStart { .. } => AguiEventType::ReasoningStart,
            AguiEvent::ReasoningMessageStart { .. } => AguiEventType::ReasoningMessageStart,
            AguiEvent::ReasoningMessageContent { .. } => AguiEventType::ReasoningMessageContent,
            AguiEvent::ReasoningMessageEnd { .. } => AguiEventType::ReasoningMessageEnd,
            AguiEvent::ReasoningMessageChunk { .. } => AguiEventType::ReasoningMessageChunk,
            AguiEvent::ReasoningEnd { .. } => AguiEventType::ReasoningEnd,
            AguiEvent::ReasoningEncryptedValue { .. } => AguiEventType::ReasoningEncryptedValue,
            AguiEvent::Unknown { .. } => return None,
        })
    }

    /// The thread id, when the event carries one.
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            AguiEvent::RunStarted { thread_id, .. }
            | AguiEvent::RunFinished { thread_id, .. }
            | AguiEvent::RunError { thread_id, .. }
            | AguiEvent::StepStarted { thread_id, .. }
            | AguiEvent::StepFinished { thread_id, .. }
            | AguiEvent::TextMessageStart { thread_id, .. }
            | AguiEvent::TextMessageContent { thread_id, .. }
            | AguiEvent::TextMessageEnd { thread_id, .. }
            | AguiEvent::TextMessageChunk { thread_id, .. }
            | AguiEvent::ToolCallStart { thread_id, .. }
            | AguiEvent::ToolCallArgs { thread_id, .. }
            | AguiEvent::ToolCallEnd { thread_id, .. }
            | AguiEvent::ToolCallChunk { thread_id, .. }
            | AguiEvent::ToolCallResult { thread_id, .. }
            | AguiEvent::StateSnapshot { thread_id, .. }
            | AguiEvent::StateDelta { thread_id, .. }
            | AguiEvent::MessagesSnapshot { thread_id, .. }
            | AguiEvent::ActivitySnapshot { thread_id, .. }
            | AguiEvent::ActivityDelta { thread_id, .. }
            | AguiEvent::Raw { thread_id, .. }
            | AguiEvent::Custom { thread_id, .. }
            | AguiEvent::ReasoningStart { thread_id, .. }
            | AguiEvent::ReasoningMessageStart { thread_id, .. }
            | AguiEvent::ReasoningMessageContent { thread_id, .. }
            | AguiEvent::ReasoningMessageEnd { thread_id, .. }
            | AguiEvent::ReasoningMessageChunk { thread_id, .. }
            | AguiEvent::ReasoningEnd { thread_id, .. }
            | AguiEvent::ReasoningEncryptedValue { thread_id, .. } => Some(thread_id),
            AguiEvent::Unknown { .. } => None,
        }
    }

    /// The run id, when the event carries one.
    pub fn run_id(&self) -> Option<&str> {
        match self {
            AguiEvent::RunStarted { run_id, .. }
            | AguiEvent::RunFinished { run_id, .. }
            | AguiEvent::RunError { run_id, .. }
            | AguiEvent::StepStarted { run_id, .. }
            | AguiEvent::StepFinished { run_id, .. }
            | AguiEvent::TextMessageStart { run_id, .. }
            | AguiEvent::TextMessageContent { run_id, .. }
            | AguiEvent::TextMessageEnd { run_id, .. }
            | AguiEvent::TextMessageChunk { run_id, .. }
            | AguiEvent::ToolCallStart { run_id, .. }
            | AguiEvent::ToolCallArgs { run_id, .. }
            | AguiEvent::ToolCallEnd { run_id, .. }
            | AguiEvent::ToolCallChunk { run_id, .. }
            | AguiEvent::ToolCallResult { run_id, .. }
            | AguiEvent::StateSnapshot { run_id, .. }
            | AguiEvent::StateDelta { run_id, .. }
            | AguiEvent::MessagesSnapshot { run_id, .. }
            | AguiEvent::ActivitySnapshot { run_id, .. }
            | AguiEvent::ActivityDelta { run_id, .. }
            | AguiEvent::Raw { run_id, .. }
            | AguiEvent::Custom { run_id, .. }
            | AguiEvent::ReasoningStart { run_id, .. }
            | AguiEvent::ReasoningMessageStart { run_id, .. }
            | AguiEvent::ReasoningMessageContent { run_id, .. }
            | AguiEvent::ReasoningMessageEnd { run_id, .. }
            | AguiEvent::ReasoningMessageChunk { run_id, .. }
            | AguiEvent::ReasoningEnd { run_id, .. }
            | AguiEvent::ReasoningEncryptedValue { run_id, .. } => Some(run_id),
            AguiEvent::Unknown { .. } => None,
        }
    }

    /// The optional event timestamp.
    pub fn timestamp(&self) -> Option<i64> {
        match self {
            AguiEvent::RunStarted { timestamp, .. }
            | AguiEvent::RunFinished { timestamp, .. }
            | AguiEvent::RunError { timestamp, .. }
            | AguiEvent::StepStarted { timestamp, .. }
            | AguiEvent::StepFinished { timestamp, .. }
            | AguiEvent::TextMessageStart { timestamp, .. }
            | AguiEvent::TextMessageContent { timestamp, .. }
            | AguiEvent::TextMessageEnd { timestamp, .. }
            | AguiEvent::TextMessageChunk { timestamp, .. }
            | AguiEvent::ToolCallStart { timestamp, .. }
            | AguiEvent::ToolCallArgs { timestamp, .. }
            | AguiEvent::ToolCallEnd { timestamp, .. }
            | AguiEvent::ToolCallChunk { timestamp, .. }
            | AguiEvent::ToolCallResult { timestamp, .. }
            | AguiEvent::StateSnapshot { timestamp, .. }
            | AguiEvent::StateDelta { timestamp, .. }
            | AguiEvent::MessagesSnapshot { timestamp, .. }
            | AguiEvent::ActivitySnapshot { timestamp, .. }
            | AguiEvent::ActivityDelta { timestamp, .. }
            | AguiEvent::Raw { timestamp, .. }
            | AguiEvent::Custom { timestamp, .. }
            | AguiEvent::ReasoningStart { timestamp, .. }
            | AguiEvent::ReasoningMessageStart { timestamp, .. }
            | AguiEvent::ReasoningMessageContent { timestamp, .. }
            | AguiEvent::ReasoningMessageEnd { timestamp, .. }
            | AguiEvent::ReasoningMessageChunk { timestamp, .. }
            | AguiEvent::ReasoningEnd { timestamp, .. }
            | AguiEvent::ReasoningEncryptedValue { timestamp, .. } => *timestamp,
            AguiEvent::Unknown { .. } => None,
        }
    }

    /// The optional raw source event.
    pub fn raw_event(&self) -> Option<&Value> {
        match self {
            AguiEvent::RunStarted { raw_event, .. }
            | AguiEvent::RunFinished { raw_event, .. }
            | AguiEvent::RunError { raw_event, .. }
            | AguiEvent::StepStarted { raw_event, .. }
            | AguiEvent::StepFinished { raw_event, .. }
            | AguiEvent::TextMessageStart { raw_event, .. }
            | AguiEvent::TextMessageContent { raw_event, .. }
            | AguiEvent::TextMessageEnd { raw_event, .. }
            | AguiEvent::TextMessageChunk { raw_event, .. }
            | AguiEvent::ToolCallStart { raw_event, .. }
            | AguiEvent::ToolCallArgs { raw_event, .. }
            | AguiEvent::ToolCallEnd { raw_event, .. }
            | AguiEvent::ToolCallChunk { raw_event, .. }
            | AguiEvent::ToolCallResult { raw_event, .. }
            | AguiEvent::StateSnapshot { raw_event, .. }
            | AguiEvent::StateDelta { raw_event, .. }
            | AguiEvent::MessagesSnapshot { raw_event, .. }
            | AguiEvent::ActivitySnapshot { raw_event, .. }
            | AguiEvent::ActivityDelta { raw_event, .. }
            | AguiEvent::Raw { raw_event, .. }
            | AguiEvent::Custom { raw_event, .. }
            | AguiEvent::ReasoningStart { raw_event, .. }
            | AguiEvent::ReasoningMessageStart { raw_event, .. }
            | AguiEvent::ReasoningMessageContent { raw_event, .. }
            | AguiEvent::ReasoningMessageEnd { raw_event, .. }
            | AguiEvent::ReasoningMessageChunk { raw_event, .. }
            | AguiEvent::ReasoningEnd { raw_event, .. }
            | AguiEvent::ReasoningEncryptedValue { raw_event, .. } => raw_event.as_ref(),
            AguiEvent::Unknown { .. } => None,
        }
    }
}

// ---- tolerant deserialization ----

fn req_str(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn opt_str(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn str_or_default(map: &Map<String, Value>, key: &str) -> String {
    opt_str(map, key).unwrap_or_default()
}

fn opt_i64(map: &Map<String, Value>, key: &str) -> Option<i64> {
    map.get(key).and_then(|v| v.as_i64())
}

fn opt_value(map: &Map<String, Value>, key: &str) -> Option<Value> {
    map.get(key).filter(|v| !v.is_null()).cloned()
}

fn opt_map(map: &Map<String, Value>, key: &str) -> Option<Map<String, Value>> {
    map.get(key).and_then(|v| v.as_object()).cloned()
}

fn opt_bool(map: &Map<String, Value>, key: &str) -> Option<bool> {
    map.get(key).and_then(|v| v.as_bool())
}

fn parse_json<T: serde::de::DeserializeOwned>(map: &Map<String, Value>, key: &str) -> Option<T> {
    map.get(key).and_then(|v| serde_json::from_value(v.clone()).ok())
}

fn parse_known(ty: &str, map: &Map<String, Value>) -> Option<AguiEvent> {
    let thread_id = req_str(map, "threadId")?;
    let run_id = req_str(map, "runId")?;
    let event = match ty {
        "RUN_STARTED" => AguiEvent::RunStarted {
            thread_id,
            run_id,
            parent_run_id: opt_str(map, "parentRunId"),
            input: opt_value(map, "input"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "RUN_FINISHED" => AguiEvent::RunFinished {
            thread_id,
            run_id,
            result: opt_value(map, "result"),
            outcome: parse_json(map, "outcome"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "RUN_ERROR" => AguiEvent::RunError {
            thread_id,
            run_id,
            message: str_or_default(map, "message"),
            code: opt_str(map, "code"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "STEP_STARTED" => AguiEvent::StepStarted {
            thread_id,
            run_id,
            step_name: str_or_default(map, "stepName"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "STEP_FINISHED" => AguiEvent::StepFinished {
            thread_id,
            run_id,
            step_name: str_or_default(map, "stepName"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TEXT_MESSAGE_START" => AguiEvent::TextMessageStart {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            role: str_or_default(map, "role"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TEXT_MESSAGE_CONTENT" => AguiEvent::TextMessageContent {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            delta: str_or_default(map, "delta"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TEXT_MESSAGE_END" => AguiEvent::TextMessageEnd {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TEXT_MESSAGE_CHUNK" => AguiEvent::TextMessageChunk {
            thread_id,
            run_id,
            message_id: opt_str(map, "messageId"),
            role: opt_str(map, "role"),
            delta: opt_str(map, "delta"),
            name: opt_str(map, "name"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TOOL_CALL_START" => AguiEvent::ToolCallStart {
            thread_id,
            run_id,
            tool_call_id: str_or_default(map, "toolCallId"),
            tool_call_name: str_or_default(map, "toolCallName"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TOOL_CALL_ARGS" => AguiEvent::ToolCallArgs {
            thread_id,
            run_id,
            tool_call_id: str_or_default(map, "toolCallId"),
            delta: str_or_default(map, "delta"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TOOL_CALL_END" => AguiEvent::ToolCallEnd {
            thread_id,
            run_id,
            tool_call_id: str_or_default(map, "toolCallId"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TOOL_CALL_CHUNK" => AguiEvent::ToolCallChunk {
            thread_id,
            run_id,
            tool_call_id: opt_str(map, "toolCallId"),
            tool_call_name: opt_str(map, "toolCallName"),
            parent_message_id: opt_str(map, "parentMessageId"),
            delta: opt_str(map, "delta"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "TOOL_CALL_RESULT" => AguiEvent::ToolCallResult {
            thread_id,
            run_id,
            tool_call_id: str_or_default(map, "toolCallId"),
            content: opt_str(map, "content"),
            role: opt_str(map, "role"),
            message_id: opt_str(map, "messageId"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "STATE_SNAPSHOT" => AguiEvent::StateSnapshot {
            thread_id,
            run_id,
            snapshot: opt_map(map, "snapshot").unwrap_or_default(),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "STATE_DELTA" => AguiEvent::StateDelta {
            thread_id,
            run_id,
            delta: parse_json(map, "delta").unwrap_or_default(),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "MESSAGES_SNAPSHOT" => AguiEvent::MessagesSnapshot {
            thread_id,
            run_id,
            messages: parse_json(map, "messages").unwrap_or_default(),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "ACTIVITY_SNAPSHOT" => AguiEvent::ActivitySnapshot {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            activity_type: str_or_default(map, "activityType"),
            content: opt_map(map, "content").unwrap_or_default(),
            replace: opt_bool(map, "replace"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "ACTIVITY_DELTA" => AguiEvent::ActivityDelta {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            activity_type: str_or_default(map, "activityType"),
            patch: parse_json(map, "patch").unwrap_or_default(),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "RAW" => AguiEvent::Raw {
            thread_id,
            run_id,
            event: opt_value(map, "event"),
            source: opt_str(map, "source"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "CUSTOM" => AguiEvent::Custom {
            thread_id,
            run_id,
            name: str_or_default(map, "name"),
            value: opt_value(map, "value"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "REASONING_START" => AguiEvent::ReasoningStart {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            encrypted_content: opt_str(map, "encryptedContent"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "REASONING_MESSAGE_START" => AguiEvent::ReasoningMessageStart {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            role: str_or_default(map, "role"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "REASONING_MESSAGE_CONTENT" => AguiEvent::ReasoningMessageContent {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            delta: str_or_default(map, "delta"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "REASONING_MESSAGE_END" => AguiEvent::ReasoningMessageEnd {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "REASONING_MESSAGE_CHUNK" => AguiEvent::ReasoningMessageChunk {
            thread_id,
            run_id,
            message_id: opt_str(map, "messageId"),
            delta: opt_str(map, "delta"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "REASONING_END" => AguiEvent::ReasoningEnd {
            thread_id,
            run_id,
            message_id: str_or_default(map, "messageId"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        "REASONING_ENCRYPTED_VALUE" => AguiEvent::ReasoningEncryptedValue {
            thread_id,
            run_id,
            subtype: str_or_default(map, "subtype"),
            entity_id: str_or_default(map, "entityId"),
            encrypted_value: str_or_default(map, "encryptedValue"),
            timestamp: opt_i64(map, "timestamp"),
            raw_event: opt_value(map, "rawEvent"),
        },
        _ => return None,
    };
    Some(event)
}

impl<'de> Deserialize<'de> for AguiEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let map = value.as_object().ok_or_else(|| {
            serde::de::Error::custom("AG-UI event must be a JSON object")
        })?;
        let ty = map
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| serde::de::Error::custom("AG-UI event missing string `type` field"))?;
        if let Some(event) = parse_known(ty, map) {
            return Ok(event);
        }
        let mut payload = map.clone();
        payload.remove("type");
        Ok(AguiEvent::Unknown {
            raw_type: ty.to_string(),
            payload,
        })
    }
}

/// Utilities for working with AG-UI events.
pub struct AguiEvents;

impl AguiEvents {
    /// Copies an event while replacing only the base properties (`timestamp` and
    /// `rawEvent`), mirroring `AguiEvents.withBaseProperties`.
    pub fn with_base_properties(event: &AguiEvent, timestamp: Option<i64>, raw_event: Option<Value>) -> AguiEvent {
        macro_rules! with_base {
            ($variant:ident { $($field:ident),* $(,)? }) => {
                AguiEvent::$variant {
                    $($field: $field.clone(),)*
                    timestamp,
                    raw_event,
                }
            };
        }
        match event {
            AguiEvent::RunStarted {
                thread_id,
                run_id,
                parent_run_id,
                input,
                ..
            } => with_base!(RunStarted { thread_id, run_id, parent_run_id, input }),
            AguiEvent::RunFinished {
                thread_id,
                run_id,
                result,
                outcome,
                ..
            } => with_base!(RunFinished { thread_id, run_id, result, outcome }),
            AguiEvent::RunError {
                thread_id,
                run_id,
                message,
                code,
                ..
            } => with_base!(RunError { thread_id, run_id, message, code }),
            AguiEvent::StepStarted {
                thread_id,
                run_id,
                step_name,
                ..
            } => with_base!(StepStarted { thread_id, run_id, step_name }),
            AguiEvent::StepFinished {
                thread_id,
                run_id,
                step_name,
                ..
            } => with_base!(StepFinished { thread_id, run_id, step_name }),
            AguiEvent::TextMessageStart {
                thread_id,
                run_id,
                message_id,
                role,
                ..
            } => with_base!(TextMessageStart { thread_id, run_id, message_id, role }),
            AguiEvent::TextMessageContent {
                thread_id,
                run_id,
                message_id,
                delta,
                ..
            } => with_base!(TextMessageContent { thread_id, run_id, message_id, delta }),
            AguiEvent::TextMessageEnd {
                thread_id,
                run_id,
                message_id,
                ..
            } => with_base!(TextMessageEnd { thread_id, run_id, message_id }),
            AguiEvent::TextMessageChunk {
                thread_id,
                run_id,
                message_id,
                role,
                delta,
                name,
                ..
            } => with_base!(TextMessageChunk { thread_id, run_id, message_id, role, delta, name }),
            AguiEvent::ToolCallStart {
                thread_id,
                run_id,
                tool_call_id,
                tool_call_name,
                ..
            } => with_base!(ToolCallStart { thread_id, run_id, tool_call_id, tool_call_name }),
            AguiEvent::ToolCallArgs {
                thread_id,
                run_id,
                tool_call_id,
                delta,
                ..
            } => with_base!(ToolCallArgs { thread_id, run_id, tool_call_id, delta }),
            AguiEvent::ToolCallEnd {
                thread_id,
                run_id,
                tool_call_id,
                ..
            } => with_base!(ToolCallEnd { thread_id, run_id, tool_call_id }),
            AguiEvent::ToolCallChunk {
                thread_id,
                run_id,
                tool_call_id,
                tool_call_name,
                parent_message_id,
                delta,
                ..
            } => with_base!(ToolCallChunk {
                thread_id,
                run_id,
                tool_call_id,
                tool_call_name,
                parent_message_id,
                delta
            }),
            AguiEvent::ToolCallResult {
                thread_id,
                run_id,
                tool_call_id,
                content,
                role,
                message_id,
                ..
            } => with_base!(ToolCallResult {
                thread_id,
                run_id,
                tool_call_id,
                content,
                role,
                message_id
            }),
            AguiEvent::StateSnapshot {
                thread_id,
                run_id,
                snapshot,
                ..
            } => with_base!(StateSnapshot { thread_id, run_id, snapshot }),
            AguiEvent::StateDelta {
                thread_id,
                run_id,
                delta,
                ..
            } => with_base!(StateDelta { thread_id, run_id, delta }),
            AguiEvent::MessagesSnapshot {
                thread_id,
                run_id,
                messages,
                ..
            } => with_base!(MessagesSnapshot { thread_id, run_id, messages }),
            AguiEvent::ActivitySnapshot {
                thread_id,
                run_id,
                message_id,
                activity_type,
                content,
                replace,
                ..
            } => with_base!(ActivitySnapshot {
                thread_id,
                run_id,
                message_id,
                activity_type,
                content,
                replace
            }),
            AguiEvent::ActivityDelta {
                thread_id,
                run_id,
                message_id,
                activity_type,
                patch,
                ..
            } => with_base!(ActivityDelta {
                thread_id,
                run_id,
                message_id,
                activity_type,
                patch
            }),
            AguiEvent::Raw {
                thread_id,
                run_id,
                event,
                source,
                ..
            } => with_base!(Raw { thread_id, run_id, event, source }),
            AguiEvent::Custom {
                thread_id,
                run_id,
                name,
                value,
                ..
            } => with_base!(Custom { thread_id, run_id, name, value }),
            AguiEvent::ReasoningStart {
                thread_id,
                run_id,
                message_id,
                encrypted_content,
                ..
            } => with_base!(ReasoningStart { thread_id, run_id, message_id, encrypted_content }),
            AguiEvent::ReasoningMessageStart {
                thread_id,
                run_id,
                message_id,
                role,
                ..
            } => with_base!(ReasoningMessageStart { thread_id, run_id, message_id, role }),
            AguiEvent::ReasoningMessageContent {
                thread_id,
                run_id,
                message_id,
                delta,
                ..
            } => with_base!(ReasoningMessageContent { thread_id, run_id, message_id, delta }),
            AguiEvent::ReasoningMessageEnd {
                thread_id,
                run_id,
                message_id,
                ..
            } => with_base!(ReasoningMessageEnd { thread_id, run_id, message_id }),
            AguiEvent::ReasoningMessageChunk {
                thread_id,
                run_id,
                message_id,
                delta,
                ..
            } => with_base!(ReasoningMessageChunk { thread_id, run_id, message_id, delta }),
            AguiEvent::ReasoningEnd {
                thread_id,
                run_id,
                message_id,
                ..
            } => with_base!(ReasoningEnd { thread_id, run_id, message_id }),
            AguiEvent::ReasoningEncryptedValue {
                thread_id,
                run_id,
                subtype,
                entity_id,
                encrypted_value,
                ..
            } => with_base!(ReasoningEncryptedValue {
                thread_id,
                run_id,
                subtype,
                entity_id,
                encrypted_value
            }),
            AguiEvent::Unknown { .. } => event.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_type_wire_names_are_screaming_snake_case() {
        assert_eq!(AguiEventType::RunStarted.as_str(), "RUN_STARTED");
        assert_eq!(AguiEventType::ToolCallResult.as_str(), "TOOL_CALL_RESULT");
        assert_eq!(
            AguiEventType::ReasoningMessageChunk.as_str(),
            "REASONING_MESSAGE_CHUNK"
        );
        assert_eq!(
            AguiEventType::from_wire("STATE_SNAPSHOT"),
            Some(AguiEventType::StateSnapshot)
        );
        assert_eq!(AguiEventType::from_wire("NOPE"), None);
    }

    #[test]
    fn run_started_serializes_with_type_and_camel_case() {
        let event = AguiEvent::RunStarted {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            parent_run_id: None,
            input: None,
            timestamp: Some(42),
            raw_event: None,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], json!("RUN_STARTED"));
        assert_eq!(value["threadId"], json!("t1"));
        assert_eq!(value["runId"], json!("r1"));
        assert_eq!(value["timestamp"], json!(42));
        assert!(value.get("parentRunId").is_none());
        assert!(value.get("input").is_none());
    }

    #[test]
    fn text_message_content_round_trips() {
        let event = AguiEvent::TextMessageContent {
            thread_id: "t".to_string(),
            run_id: "r".to_string(),
            message_id: "m".to_string(),
            delta: "hello".to_string(),
            timestamp: None,
            raw_event: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"TEXT_MESSAGE_CONTENT\",\"threadId\":\"t\",\"runId\":\"r\",\"messageId\":\"m\",\"delta\":\"hello\"}"
        );
        let back: AguiEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
        assert_eq!(back.event_type(), Some(AguiEventType::TextMessageContent));
    }

    #[test]
    fn every_known_type_has_a_distinct_discriminator() {
        let samples = [
            (AguiEvent::RunStarted { thread_id: "t".into(), run_id: "r".into(), parent_run_id: None, input: None, timestamp: None, raw_event: None }, "RUN_STARTED"),
            (AguiEvent::RunFinished { thread_id: "t".into(), run_id: "r".into(), result: None, outcome: None, timestamp: None, raw_event: None }, "RUN_FINISHED"),
            (AguiEvent::RunError { thread_id: "t".into(), run_id: "r".into(), message: "e".into(), code: None, timestamp: None, raw_event: None }, "RUN_ERROR"),
            (AguiEvent::StepStarted { thread_id: "t".into(), run_id: "r".into(), step_name: "s".into(), timestamp: None, raw_event: None }, "STEP_STARTED"),
            (AguiEvent::StepFinished { thread_id: "t".into(), run_id: "r".into(), step_name: "s".into(), timestamp: None, raw_event: None }, "STEP_FINISHED"),
            (AguiEvent::TextMessageStart { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), role: "assistant".into(), timestamp: None, raw_event: None }, "TEXT_MESSAGE_START"),
            (AguiEvent::TextMessageEnd { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), timestamp: None, raw_event: None }, "TEXT_MESSAGE_END"),
            (AguiEvent::TextMessageChunk { thread_id: "t".into(), run_id: "r".into(), message_id: None, role: None, delta: Some("d".into()), name: None, timestamp: None, raw_event: None }, "TEXT_MESSAGE_CHUNK"),
            (AguiEvent::ToolCallStart { thread_id: "t".into(), run_id: "r".into(), tool_call_id: "c".into(), tool_call_name: "f".into(), timestamp: None, raw_event: None }, "TOOL_CALL_START"),
            (AguiEvent::ToolCallArgs { thread_id: "t".into(), run_id: "r".into(), tool_call_id: "c".into(), delta: "{}".into(), timestamp: None, raw_event: None }, "TOOL_CALL_ARGS"),
            (AguiEvent::ToolCallEnd { thread_id: "t".into(), run_id: "r".into(), tool_call_id: "c".into(), timestamp: None, raw_event: None }, "TOOL_CALL_END"),
            (AguiEvent::ToolCallChunk { thread_id: "t".into(), run_id: "r".into(), tool_call_id: None, tool_call_name: None, parent_message_id: None, delta: None, timestamp: None, raw_event: None }, "TOOL_CALL_CHUNK"),
            (AguiEvent::ToolCallResult { thread_id: "t".into(), run_id: "r".into(), tool_call_id: "c".into(), content: None, role: None, message_id: None, timestamp: None, raw_event: None }, "TOOL_CALL_RESULT"),
            (AguiEvent::StateSnapshot { thread_id: "t".into(), run_id: "r".into(), snapshot: Map::new(), timestamp: None, raw_event: None }, "STATE_SNAPSHOT"),
            (AguiEvent::StateDelta { thread_id: "t".into(), run_id: "r".into(), delta: vec![], timestamp: None, raw_event: None }, "STATE_DELTA"),
            (AguiEvent::MessagesSnapshot { thread_id: "t".into(), run_id: "r".into(), messages: vec![], timestamp: None, raw_event: None }, "MESSAGES_SNAPSHOT"),
            (AguiEvent::ActivitySnapshot { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), activity_type: "a".into(), content: Map::new(), replace: None, timestamp: None, raw_event: None }, "ACTIVITY_SNAPSHOT"),
            (AguiEvent::ActivityDelta { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), activity_type: "a".into(), patch: vec![], timestamp: None, raw_event: None }, "ACTIVITY_DELTA"),
            (AguiEvent::Raw { thread_id: "t".into(), run_id: "r".into(), event: None, source: None, timestamp: None, raw_event: None }, "RAW"),
            (AguiEvent::Custom { thread_id: "t".into(), run_id: "r".into(), name: "n".into(), value: None, timestamp: None, raw_event: None }, "CUSTOM"),
            (AguiEvent::ReasoningStart { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), encrypted_content: None, timestamp: None, raw_event: None }, "REASONING_START"),
            (AguiEvent::ReasoningMessageStart { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), role: "assistant".into(), timestamp: None, raw_event: None }, "REASONING_MESSAGE_START"),
            (AguiEvent::ReasoningMessageContent { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), delta: "d".into(), timestamp: None, raw_event: None }, "REASONING_MESSAGE_CONTENT"),
            (AguiEvent::ReasoningMessageEnd { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), timestamp: None, raw_event: None }, "REASONING_MESSAGE_END"),
            (AguiEvent::ReasoningMessageChunk { thread_id: "t".into(), run_id: "r".into(), message_id: None, delta: None, timestamp: None, raw_event: None }, "REASONING_MESSAGE_CHUNK"),
            (AguiEvent::ReasoningEnd { thread_id: "t".into(), run_id: "r".into(), message_id: "m".into(), timestamp: None, raw_event: None }, "REASONING_END"),
            (AguiEvent::ReasoningEncryptedValue { thread_id: "t".into(), run_id: "r".into(), subtype: "s".into(), entity_id: "e".into(), encrypted_value: "v".into(), timestamp: None, raw_event: None }, "REASONING_ENCRYPTED_VALUE"),
        ];
        for (event, expected) in samples {
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(value["type"], json!(expected));
            let back: AguiEvent = serde_json::from_value(value).unwrap();
            assert_eq!(back, event, "round-trip failed for {expected}");
        }
    }

    #[test]
    fn unknown_type_is_tolerated() {
        let raw = r#"{"type":"FUTURE_EVENT","threadId":"t","runId":"r","extra":1}"#;
        let event: AguiEvent = serde_json::from_str(raw).unwrap();
        match event {
            AguiEvent::Unknown { ref raw_type, ref payload } => {
                assert_eq!(raw_type, "FUTURE_EVENT");
                assert_eq!(payload["extra"], json!(1));
                assert_eq!(payload["threadId"], json!("t"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert_eq!(event.event_type(), None);
    }

    #[test]
    fn missing_base_ids_fall_back_to_unknown() {
        let raw = r#"{"type":"TEXT_MESSAGE_CONTENT","delta":"x"}"#;
        let event: AguiEvent = serde_json::from_str(raw).unwrap();
        assert!(matches!(event, AguiEvent::Unknown { .. }));
    }

    #[test]
    fn run_finished_outcome_is_tagged_lowercase() {
        let success = serde_json::to_value(RunFinishedOutcome::Success).unwrap();
        assert_eq!(success, json!({"type": "success"}));
        let interrupt = RunFinishedOutcome::Interrupt {
            interrupts: vec![Interrupt::new("i1", "tool_call")],
        };
        let value = serde_json::to_value(&interrupt).unwrap();
        assert_eq!(value["type"], json!("interrupt"));
        assert_eq!(value["interrupts"][0]["id"], json!("i1"));
        assert_eq!(value["interrupts"][0]["reason"], json!("tool_call"));
    }

    #[test]
    fn interrupt_metadata_and_tool_call_id_use_camel_case() {
        let mut interrupt = Interrupt::new("i1", "tool_call");
        interrupt.tool_call_id = Some("c1".to_string());
        interrupt.metadata = Some(serde_json::from_value(json!({"k": "v"})).unwrap());
        let value = serde_json::to_value(&interrupt).unwrap();
        assert_eq!(value["toolCallId"], json!("c1"));
        assert_eq!(value["metadata"]["k"], json!("v"));
    }

    #[test]
    fn json_patch_helpers_build_rfc6902_ops() {
        assert_eq!(JsonPatchOperation::add("/a", Some(json!(1))).op, "add");
        assert_eq!(JsonPatchOperation::remove("/a").op, "remove");
        assert_eq!(JsonPatchOperation::replace("/a", Some(json!(2))).op, "replace");
        let mv = JsonPatchOperation::move_("/a", "/b");
        assert_eq!(mv.from.as_deref(), Some("/a"));
        assert_eq!(mv.path, "/b");
        let value = serde_json::to_value(&mv).unwrap();
        assert_eq!(value["op"], json!("move"));
    }

    #[test]
    fn with_base_properties_replaces_only_base_fields() {
        let event = AguiEvent::TextMessageContent {
            thread_id: "t".to_string(),
            run_id: "r".to_string(),
            message_id: "m".to_string(),
            delta: "d".to_string(),
            timestamp: Some(1),
            raw_event: Some(json!({"old": true})),
        };
        let updated =
            AguiEvents::with_base_properties(&event, Some(99), Some(json!({"new": true})));
        assert_eq!(updated.timestamp(), Some(99));
        assert_eq!(updated.raw_event(), Some(&json!({"new": true})));
        match updated {
            AguiEvent::TextMessageContent { delta, message_id, .. } => {
                assert_eq!(delta, "d");
                assert_eq!(message_id, "m");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn state_delta_round_trips_patch_ops() {
        let event = AguiEvent::StateDelta {
            thread_id: "t".to_string(),
            run_id: "r".to_string(),
            delta: vec![
                JsonPatchOperation::add("/x", Some(json!(1))),
                JsonPatchOperation::remove("/y"),
            ],
            timestamp: None,
            raw_event: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AguiEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }
}
