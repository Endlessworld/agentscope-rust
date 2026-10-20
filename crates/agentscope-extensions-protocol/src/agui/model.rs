//! AG-UI protocol data model, mirroring `io.agentscope.core.agui.model`.
//!
//! * [`AguiMessage`] - a conversation message (`content` is a [`MessageContent`] union).
//! * [`MessageContent`] - `Text(String)` or `Blocks(Vec<InputContent>)`, untagged on the wire.
//! * [`InputContent`] - a typed part discriminated by its `type` field.
//! * [`InputContentSource`] - `data` (inline base64) or `url`, discriminated by `type`.
//! * [`AguiToolCall`] / [`AguiFunctionCall`] - tool-call payloads.
//! * [`RunAgentInput`] - the complete request payload (messages/tools/context/state/resume).
//! * [`AguiContext`] - one contextual hint (description + value).
//! * [`AguiResume`] - one resume response for a previously emitted interrupt.
//! * [`AguiTool`] - a frontend-provided tool definition.
//! * [`ToolMergeMode`] - how frontend tools interact with the agent toolkit.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A message in the AG-UI protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AguiMessage {
    /// Unique message id.
    pub id: String,
    /// Role: `user`, `assistant`, `system`, `tool` or `reasoning`.
    pub role: String,
    /// Plain text or structured content parts; may be absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    /// Tool calls carried by assistant messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AguiToolCall>,
    /// Tool call id carried by tool messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Error a frontend tool reported, per the AG-UI protocol's `error` field on tool
    /// messages; absent when the tool reported no failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AguiMessage {
    /// Creates a message from already-built content.
    pub fn new(
        id: impl Into<String>,
        role: impl Into<String>,
        content: Option<MessageContent>,
        tool_calls: Vec<AguiToolCall>,
        tool_call_id: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            role: role.into(),
            content,
            tool_calls,
            tool_call_id,
            error: None,
        }
    }

    /// Attaches the error a frontend tool reported (the AG-UI `error` field).
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    /// Creates a reasoning message carrying plain text reasoning content.
    pub fn reasoning_message(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(
            id,
            "reasoning",
            Some(MessageContent::Text(content.into())),
            Vec::new(),
            None,
        )
    }

    /// Creates a user message with plain text content.
    pub fn user_message(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(
            id,
            "user",
            Some(MessageContent::Text(content.into())),
            Vec::new(),
            None,
        )
    }

    /// Creates a user message with structured content blocks.
    pub fn user_message_blocks(id: impl Into<String>, blocks: Vec<InputContent>) -> Self {
        Self::new(
            id,
            "user",
            Some(MessageContent::Blocks(blocks)),
            Vec::new(),
            None,
        )
    }

    /// Creates a message with structured content blocks and explicit role/tool metadata.
    pub fn blocks_message(
        id: impl Into<String>,
        role: impl Into<String>,
        blocks: Vec<InputContent>,
        tool_calls: Vec<AguiToolCall>,
        tool_call_id: Option<String>,
    ) -> Self {
        Self::new(
            id,
            role,
            Some(MessageContent::Blocks(blocks)),
            tool_calls,
            tool_call_id,
        )
    }

    /// The plain text content, or `None` when the content is not [`MessageContent::Text`].
    pub fn text_content(&self) -> Option<&str> {
        match &self.content {
            Some(MessageContent::Text(text)) => Some(text),
            _ => None,
        }
    }

    /// Whether the content is structured blocks.
    pub fn has_blocks(&self) -> bool {
        matches!(self.content, Some(MessageContent::Blocks(_)))
    }

    /// Whether this message carries tool calls.
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Whether the role is `user` (case-insensitive).
    pub fn is_user_message(&self) -> bool {
        self.role.eq_ignore_ascii_case("user")
    }

    /// Whether the role is `assistant` (case-insensitive).
    pub fn is_assistant_message(&self) -> bool {
        self.role.eq_ignore_ascii_case("assistant")
    }

    /// Whether the role is `system` (case-insensitive).
    pub fn is_system_message(&self) -> bool {
        self.role.eq_ignore_ascii_case("system")
    }

    /// Whether the role is `tool` (case-insensitive).
    pub fn is_tool_message(&self) -> bool {
        self.role.eq_ignore_ascii_case("tool")
    }

    /// Whether the role is `reasoning` (case-insensitive).
    pub fn is_reasoning_message(&self) -> bool {
        self.role.eq_ignore_ascii_case("reasoning")
    }
}

/// Type-safe union for an AG-UI message's `content` field.
///
/// On the wire a plain string maps to [`MessageContent::Text`] and an array to
/// [`MessageContent::Blocks`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text, serialized as a JSON string.
    Text(String),
    /// Structured parts, serialized as a JSON array.
    Blocks(Vec<InputContent>),
}

/// A typed content part, discriminated by its `type` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputContent {
    /// `{"type":"text","text":"..."}`
    Text {
        /// The text content.
        text: String,
    },
    /// `{"type":"image","source":{...},"metadata":{...}}`
    Image {
        /// The image source.
        source: InputContentSource,
        /// Optional metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<Map<String, Value>>,
    },
    /// `{"type":"audio","source":{...},"metadata":{...}}`
    Audio {
        /// The audio source.
        source: InputContentSource,
        /// Optional metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<Map<String, Value>>,
    },
    /// `{"type":"video","source":{...},"metadata":{...}}`
    Video {
        /// The video source.
        source: InputContentSource,
        /// Optional metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<Map<String, Value>>,
    },
    /// `{"type":"document","source":{...},"metadata":{...}}`
    Document {
        /// The document source.
        source: InputContentSource,
        /// Optional metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<Map<String, Value>>,
    },
}

/// A media source, discriminated by its `type` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputContentSource {
    /// `{"type":"data","value":"<base64>","mimeType":"..."}`
    Data {
        /// Base64-encoded data.
        value: String,
        /// MIME type.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// `{"type":"url","value":"...","mimeType":"..."}`
    Url {
        /// The URL.
        value: String,
        /// Optional MIME type hint.
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
}

/// A tool call inside an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AguiToolCall {
    /// Unique tool-call id.
    pub id: String,
    /// Tool-call type; `function` unless overridden.
    #[serde(default = "default_tool_call_type")]
    pub r#type: String,
    /// The function invocation details.
    pub function: AguiFunctionCall,
}

fn default_tool_call_type() -> String {
    "function".to_string()
}

impl AguiToolCall {
    /// Creates a tool call of type `function`.
    pub fn new(id: impl Into<String>, function: AguiFunctionCall) -> Self {
        Self {
            id: id.into(),
            r#type: default_tool_call_type(),
            function,
        }
    }
}

/// A function invocation within a [`AguiToolCall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AguiFunctionCall {
    /// The function name.
    pub name: String,
    /// The arguments as a JSON string; `{}` when absent.
    #[serde(default = "default_arguments")]
    pub arguments: String,
}

fn default_arguments() -> String {
    "{}".to_string()
}

impl AguiFunctionCall {
    /// Creates a function call, defaulting the arguments to `{}`.
    pub fn new(name: impl Into<String>, arguments: Option<String>) -> Self {
        Self {
            name: name.into(),
            arguments: arguments.unwrap_or_else(default_arguments),
        }
    }
}

/// Input model for running an agent via the AG-UI protocol.
///
/// Mirrors Java `RunAgentInput`: `threadId`/`runId` are required (Java enforces them with
/// `Objects.requireNonNull`), every collection defaults to empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAgentInput {
    /// The thread id for this conversation.
    pub thread_id: String,
    /// The unique run id.
    pub run_id: String,
    /// The conversation messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<AguiMessage>,
    /// Frontend-provided tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AguiTool>,
    /// Additional context information.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<AguiContext>,
    /// Initial state to load.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub state: Map<String, Value>,
    /// Additional properties to forward.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub forwarded_props: Map<String, Value>,
    /// Resume responses for previously emitted interrupts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resume: Vec<AguiResume>,
}

impl Default for RunAgentInput {
    fn default() -> Self {
        Self {
            thread_id: String::new(),
            run_id: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            context: Vec::new(),
            state: Map::new(),
            forwarded_props: Map::new(),
            resume: Vec::new(),
        }
    }
}

impl RunAgentInput {
    /// Creates a run input from its full set of fields.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        messages: Vec<AguiMessage>,
        tools: Vec<AguiTool>,
        context: Vec<AguiContext>,
        state: Map<String, Value>,
        forwarded_props: Map<String, Value>,
        resume: Vec<AguiResume>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            messages,
            tools,
            context,
            state,
            forwarded_props,
            resume,
        }
    }

    /// Creates a minimal run input carrying only the thread/run identifiers.
    pub fn of(thread_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self::new(
            thread_id,
            run_id,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Map::new(),
            Map::new(),
            Vec::new(),
        )
    }

    /// Creates a builder-style copy with different messages.
    pub fn with_messages(mut self, messages: Vec<AguiMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Creates a builder-style copy with different resume entries.
    pub fn with_resume(mut self, resume: Vec<AguiResume>) -> Self {
        self.resume = resume;
        self
    }

    /// A specific forwarded property value, or `None` when absent.
    pub fn forwarded_prop(&self, key: &str) -> Option<&Value> {
        self.forwarded_props.get(key)
    }

    /// A specific forwarded property value, or `default` when absent.
    pub fn forwarded_prop_or<'a>(&'a self, key: &str, default: &'a Value) -> &'a Value {
        self.forwarded_props.get(key).unwrap_or(default)
    }

    /// Whether any messages are present.
    pub fn has_messages(&self) -> bool {
        !self.messages.is_empty()
    }

    /// Whether any frontend tools are present.
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// Whether any context items are present.
    pub fn has_context(&self) -> bool {
        !self.context.is_empty()
    }

    /// Whether any initial state is present.
    pub fn has_state(&self) -> bool {
        !self.state.is_empty()
    }

    /// Whether any interrupt resume responses are present.
    pub fn has_resume(&self) -> bool {
        !self.resume.is_empty()
    }
}

/// A piece of contextual information in the AG-UI protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AguiContext {
    /// Description of what this context represents.
    pub description: String,
    /// The actual context value.
    pub value: String,
}

impl AguiContext {
    /// Creates a context item.
    pub fn new(description: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            value: value.into(),
        }
    }
}

/// One official AG-UI resume response for a previous interrupt.
///
/// Resume entries are sent on the next [`RunAgentInput`] for the same thread and are
/// correlated with the interrupted run by `interruptId`.
///
/// A user response that explicitly denies the request is still `resolved`, **not**
/// `cancelled`: the cancelled status is reserved for cases where the user provides no
/// meaningful input and the payload is omitted.
///
/// `reason` is an optional human-readable explanation, mirroring
/// `io.agentscope.core.agui.converter.AguiMessageConverter#RESUME_PAYLOAD_REASON`.
/// Old clients that never send the key deserialize with `reason = None`, and `None` is
/// omitted from the wire form so the field stays invisible until someone sets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AguiResume {
    /// The interrupt id from `RunFinished.outcome.interrupts[].id`.
    pub interrupt_id: String,
    /// The resume status, usually `resolved` or `cancelled`.
    pub status: String,
    /// Optional response payload for resolved interrupts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// Optional explanation supplied when the interrupt is denied or cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AguiResume {
    /// Resume status: the interrupt was resolved.
    pub const STATUS_RESOLVED: &'static str = "resolved";
    /// Resume status: the interrupt was cancelled.
    pub const STATUS_CANCELLED: &'static str = "cancelled";

    /// Creates a resume entry without a reason.
    pub fn new(interrupt_id: impl Into<String>, status: impl Into<String>, payload: Option<Value>) -> Self {
        Self {
            interrupt_id: interrupt_id.into(),
            status: status.into(),
            payload,
            reason: None,
        }
    }

    /// Attaches a human-readable reason (typically a denial or cancellation explanation).
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Whether this resume entry resolved the interrupt.
    pub fn is_resolved(&self) -> bool {
        self.status == Self::STATUS_RESOLVED
    }

    /// Whether this resume entry cancelled the interrupt.
    pub fn is_cancelled(&self) -> bool {
        self.status == Self::STATUS_CANCELLED
    }

    /// The `approved` boolean of the payload, when present.
    pub fn payload_approved(&self) -> Option<bool> {
        self.payload
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(crate::agui::constants::RESUME_PAYLOAD_APPROVED))
            .and_then(Value::as_bool)
    }

    /// The `editedArgs` object of the payload, when present.
    pub fn payload_edited_args(&self) -> Option<&Map<String, Value>> {
        self.payload
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(crate::agui::constants::RESUME_PAYLOAD_EDITED_ARGS))
            .and_then(Value::as_object)
    }

    /// The optional reason of the payload (`payload.reason`), when present and non-blank.
    pub fn payload_reason(&self) -> Option<&str> {
        self.payload
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|m| m.get(crate::agui::constants::RESUME_PAYLOAD_REASON))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
    }

    /// The effective reason, preferring the top-level `reason` key over `payload.reason`.
    ///
    /// Accepting both shapes keeps Java callers (which only send `payload.reason`) and
    /// clients that promote the reason onto the resume entry itself working.
    pub fn reason(&self) -> Option<&str> {
        self.reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .or_else(|| self.payload_reason())
    }
}

/// A tool definition in the AG-UI protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AguiTool {
    /// The tool name.
    pub name: String,
    /// The tool description.
    pub description: String,
    /// The parameters JSON Schema.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub parameters: Map<String, Value>,
}

impl AguiTool {
    /// Creates a tool definition.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Map<String, Value>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// How tools from the frontend should be merged with the agent's toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ToolMergeMode {
    /// Use only tools provided from the frontend; the agent toolkit is ignored.
    FrontendOnly,
    /// Use only the agent's toolkit; frontend tools are ignored.
    AgentOnly,
    /// Merge both; frontend tools override agent tools on name conflicts.
    #[default]
    MergeFrontendPriority,
}

impl ToolMergeMode {
    /// The Java enum constant name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolMergeMode::FrontendOnly => "FRONTEND_ONLY",
            ToolMergeMode::AgentOnly => "AGENT_ONLY",
            ToolMergeMode::MergeFrontendPriority => "MERGE_FRONTEND_PRIORITY",
        }
    }

    /// Parses a Java enum constant name (case-insensitive).
    pub fn from_wire(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "FRONTEND_ONLY" => Some(ToolMergeMode::FrontendOnly),
            "AGENT_ONLY" => Some(ToolMergeMode::AgentOnly),
            "MERGE_FRONTEND_PRIORITY" => Some(ToolMergeMode::MergeFrontendPriority),
            _ => None,
        }
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_agent_input_round_trips_with_camel_case() {
        let mut forwarded = Map::new();
        forwarded.insert("agentId".to_string(), json!("assistant"));
        let input = RunAgentInput::new(
            "t1",
            "r1",
            vec![AguiMessage::user_message("m1", "hi")],
            vec![AguiTool::new("search", "search the web", Map::new())],
            vec![AguiContext::new("tenant", "acme")],
            serde_json::from_value(json!({"count": 1})).unwrap(),
            forwarded,
            vec![AguiResume::new("i1", AguiResume::STATUS_RESOLVED, None)],
        );
        let value = serde_json::to_value(&input).unwrap();
        assert_eq!(value["threadId"], json!("t1"));
        assert_eq!(value["runId"], json!("r1"));
        assert_eq!(value["forwardedProps"]["agentId"], json!("assistant"));
        assert_eq!(value["tools"][0]["name"], json!("search"));
        assert_eq!(value["context"][0]["description"], json!("tenant"));
        assert_eq!(value["resume"][0]["interruptId"], json!("i1"));
        assert_eq!(serde_json::from_value::<RunAgentInput>(value).unwrap(), input);
        assert!(input.has_messages() && input.has_tools() && input.has_context());
        assert!(input.has_state() && input.has_resume());
        assert_eq!(input.forwarded_prop("agentId"), Some(&json!("assistant")));
        assert_eq!(input.forwarded_prop("missing"), None);
    }

    #[test]
    fn run_agent_input_defaults_optional_collections() {
        let input: RunAgentInput = serde_json::from_str(r#"{"threadId":"t","runId":"r"}"#).unwrap();
        assert_eq!(input, RunAgentInput::of("t", "r"));
        assert!(!input.has_messages() && !input.has_tools() && !input.has_resume());
        // Empty collections are omitted from the wire form, mirroring Java's absent-field shape.
        let value = serde_json::to_value(&input).unwrap();
        assert_eq!(value, json!({"threadId": "t", "runId": "r"}));
    }

    #[test]
    fn resume_helpers_distinguish_resolved_from_cancelled() {
        let denied = AguiResume::new(
            "i1",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"approved": false})),
        );
        assert!(denied.is_resolved());
        assert!(!denied.is_cancelled());
        assert_eq!(denied.payload_approved(), Some(false));
        assert!(denied.payload_edited_args().is_none());

        let cancelled = AguiResume::new("i2", AguiResume::STATUS_CANCELLED, None);
        assert!(cancelled.is_cancelled());
        assert!(!cancelled.is_resolved());
        assert_eq!(cancelled.payload_approved(), None);
    }

    #[test]
    fn resume_serializes_reason_when_present() {
        let resume = AguiResume::new("i1", AguiResume::STATUS_CANCELLED, None)
            .with_reason("user closed the approval dialog");
        let value = serde_json::to_value(&resume).unwrap();
        assert_eq!(value["interruptId"], json!("i1"));
        assert_eq!(value["status"], json!("cancelled"));
        assert_eq!(value["reason"], json!("user closed the approval dialog"));
        // The reason round-trips through the wire form unchanged.
        assert_eq!(serde_json::from_value::<AguiResume>(value).unwrap(), resume);
        assert_eq!(resume.reason(), Some("user closed the approval dialog"));
    }

    #[test]
    fn resume_omits_reason_when_absent() {
        let resume = AguiResume::new("i1", AguiResume::STATUS_RESOLVED, Some(json!({"approved": true})));
        let value = serde_json::to_value(&resume).unwrap();
        assert!(
            value.get("reason").is_none(),
            "skip_serializing_if must keep the reason key off the wire: {value}"
        );
    }

    #[test]
    fn resume_deserializes_without_reason_for_backward_compatibility() {
        // Shape emitted by pre-`reason` clients: no `reason` key at all.
        let resume: AguiResume =
            serde_json::from_str(r#"{"interruptId":"i1","status":"cancelled"}"#).unwrap();
        assert_eq!(resume.interrupt_id, "i1");
        assert_eq!(resume.status, AguiResume::STATUS_CANCELLED);
        assert_eq!(resume.reason, None);
        assert_eq!(resume.reason(), None);
        // An explicit `null` is accepted as well.
        let explicit_null: AguiResume =
            serde_json::from_str(r#"{"interruptId":"i2","status":"resolved","reason":null}"#)
                .unwrap();
        assert_eq!(explicit_null.reason, None);
    }

    #[test]
    fn resume_reason_accepts_the_payload_key_and_prefers_the_top_level_one() {
        // Java only ever reads `payload.reason`; the Rust side accepts both shapes.
        let payload_only = AguiResume::new(
            "i1",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"approved": false, "reason": "too destructive"})),
        );
        assert_eq!(payload_only.reason, None);
        assert_eq!(payload_only.payload_reason(), Some("too destructive"));
        assert_eq!(payload_only.reason(), Some("too destructive"));

        // A blank payload reason is treated as absent, matching Java's `isBlank()` check.
        let blank = AguiResume::new(
            "i2",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"approved": false, "reason": "   "})),
        );
        assert_eq!(blank.payload_reason(), None);
        assert_eq!(blank.reason(), None);

        // A top-level reason wins when both are present.
        let both = AguiResume::new(
            "i3",
            AguiResume::STATUS_CANCELLED,
            Some(json!({"reason": "payload wins?"})),
        )
        .with_reason("top level wins");
        assert_eq!(both.reason(), Some("top level wins"));
    }

    #[test]
    fn resume_edited_args_requires_a_json_object() {
        let resume = AguiResume::new(
            "i1",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"editedArgs": {"a": 1}})),
        );
        let args = resume.payload_edited_args().unwrap();
        assert_eq!(args.get("a"), Some(&json!(1)));
        let scalar = AguiResume::new(
            "i2",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"editedArgs": "nope"})),
        );
        assert!(scalar.payload_edited_args().is_none());
    }

    #[test]
    fn tool_merge_mode_round_trips_java_names() {
        assert_eq!(ToolMergeMode::default(), ToolMergeMode::MergeFrontendPriority);
        for mode in [
            ToolMergeMode::FrontendOnly,
            ToolMergeMode::AgentOnly,
            ToolMergeMode::MergeFrontendPriority,
        ] {
            assert_eq!(ToolMergeMode::from_wire(mode.as_str()), Some(mode));
        }
        assert_eq!(ToolMergeMode::from_wire("merge_frontend_priority"), Some(ToolMergeMode::MergeFrontendPriority));
        assert_eq!(ToolMergeMode::from_wire("NOPE"), None);
        assert_eq!(
            serde_json::to_value(ToolMergeMode::AgentOnly).unwrap(),
            json!("AGENT_ONLY")
        );
    }

    #[test]
    fn context_and_tool_omit_empty_parameters() {
        let tool = AguiTool::new("t", "d", Map::new());
        assert_eq!(serde_json::to_value(&tool).unwrap(), json!({"name": "t", "description": "d"}));
        assert_eq!(
            serde_json::to_value(AguiContext::new("k", "v")).unwrap(),
            json!({"description": "k", "value": "v"})
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_content_is_serialized_as_a_bare_string() {
        let message = AguiMessage::user_message("m1", "hello");
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["id"], json!("m1"));
        assert_eq!(value["role"], json!("user"));
        assert_eq!(value["content"], json!("hello"));
        assert!(value.get("toolCalls").is_none());
        assert!(value.get("toolCallId").is_none());
        assert_eq!(message.text_content(), Some("hello"));
        assert!(!message.has_blocks());
        assert_eq!(serde_json::from_value::<AguiMessage>(value).unwrap(), message);
    }

    #[test]
    fn blocks_round_trip_with_type_discriminators() {
        let blocks = vec![
            InputContent::Text {
                text: "hi".to_string(),
            },
            InputContent::Image {
                source: InputContentSource::Url {
                    value: "https://example.com/a.png".to_string(),
                    mime_type: Some("image/png".to_string()),
                },
                metadata: None,
            },
        ];
        let message = AguiMessage::user_message_blocks("m2", blocks);
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["content"][0], json!({"type": "text", "text": "hi"}));
        assert_eq!(value["content"][1]["type"], json!("image"));
        assert_eq!(value["content"][1]["source"]["type"], json!("url"));
        assert_eq!(value["content"][1]["source"]["mimeType"], json!("image/png"));
        assert!(message.has_blocks());
        assert_eq!(message.text_content(), None);
        assert_eq!(serde_json::from_value::<AguiMessage>(value).unwrap(), message);
    }

    #[test]
    fn data_source_requires_mime_type_but_url_does_not() {
        let data = serde_json::from_value::<InputContentSource>(
            json!({"type": "data", "value": "AAA", "mimeType": "image/png"}),
        )
        .unwrap();
        assert_eq!(
            data,
            InputContentSource::Data {
                value: "AAA".to_string(),
                mime_type: "image/png".to_string(),
            }
        );
        let url = serde_json::from_value::<InputContentSource>(json!({"type": "url", "value": "u"}))
            .unwrap();
        assert_eq!(
            url,
            InputContentSource::Url {
                value: "u".to_string(),
                mime_type: None,
            }
        );
    }

    #[test]
    fn tool_call_defaults_match_the_java_constructors() {
        let call = AguiToolCall::new("c1", AguiFunctionCall::new("f", None));
        assert_eq!(call.r#type, "function");
        assert_eq!(call.function.arguments, "{}");
        let value = serde_json::to_value(&call).unwrap();
        assert_eq!(value["type"], json!("function"));
        assert_eq!(value["function"]["arguments"], json!("{}"));
    }

    #[test]
    fn role_helpers_are_case_insensitive() {
        let message = AguiMessage::new("m", "Assistant", None, Vec::new(), None);
        assert!(message.is_assistant_message());
        assert!(!message.is_user_message());
        assert!(!message.has_tool_calls());
        assert_eq!(message.content, None);
        let reasoning = AguiMessage::new("m", "REASONING", None, Vec::new(), None);
        assert!(reasoning.is_reasoning_message());
        assert!(!reasoning.is_assistant_message());
    }

    #[test]
    fn reasoning_message_factory_sets_the_reasoning_role() {
        let message = AguiMessage::reasoning_message("r1", "Think step by step");
        assert_eq!(message.id, "r1");
        assert_eq!(message.role, "reasoning");
        assert!(message.is_reasoning_message());
        assert_eq!(message.text_content(), Some("Think step by step"));
        assert_eq!(message.error, None);
    }

    #[test]
    fn error_field_is_omitted_when_absent_and_carried_when_reported() {
        // The field is newer than the wire format it joins: a message without an error keeps
        // the representation it had before the field existed.
        let without_error = AguiMessage::new("m1", "tool", None, Vec::new(), Some("tc-1".into()));
        let value = serde_json::to_value(&without_error).unwrap();
        assert!(value.get("error").is_none());

        let reported = without_error.clone().with_error("sandbox unavailable");
        let value = serde_json::to_value(&reported).unwrap();
        assert_eq!(value["error"], json!("sandbox unavailable"));

        // A binding for the protocol's `error` field exists, so it survives a round trip.
        let wire = json!({
            "id": "m1",
            "role": "tool",
            "toolCallId": "tc-1",
            "error": "sandbox unavailable"
        });
        let decoded: AguiMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.error.as_deref(), Some("sandbox unavailable"));
        assert_eq!(decoded.tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(decoded, reported);
    }
}
