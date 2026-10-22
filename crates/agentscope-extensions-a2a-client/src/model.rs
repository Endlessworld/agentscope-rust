//! A2A wire data model.
//!
//! Field names and enum wire values follow the A2A JSON specification so that payloads are
//! interoperable with the Java SDK (which uses the `a2a-java-sdk` spec classes) and with
//! external A2A agents.
//!
//! Reference: <https://a2a-protocol.org/latest/specification/>.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Well-known JSON-RPC method names used by the A2A transport.
pub mod methods {
    pub const MESSAGE_SEND: &str = "message/send";
    pub const MESSAGE_STREAM: &str = "message/stream";
    pub const TASKS_GET: &str = "tasks/get";
    pub const TASKS_CANCEL: &str = "tasks/cancel";
    pub const TASKS_RESUBSCRIBE: &str = "tasks/resubscribe";
    pub const PUSH_CONFIG_SET: &str = "tasks/pushNotificationConfig/set";
    pub const PUSH_CONFIG_GET: &str = "tasks/pushNotificationConfig/get";
    pub const PUSH_CONFIG_LIST: &str = "tasks/pushNotificationConfig/list";
    pub const PUSH_CONFIG_DELETE: &str = "tasks/pushNotificationConfig/delete";
    pub const AGENT_CARD_GET: &str = "agent/getAuthenticatedExtendedCard";
}

/// Default relative path of the well-known agent card.
pub const WELL_KNOWN_AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// A2A message roles. Note: A2A only distinguishes `user` and `agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    #[default]
    User,
    Agent,
}

/// Kind discriminator on the [`Part`] union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartKind {
    Text,
    File,
    Data,
}

/// A content part inside an A2A [`Message`] or [`Artifact`].
///
/// Serialized with a `kind` discriminator, matching `io.a2a.spec.Part`'s Jackson mapping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Part {
    Text(TextPart),
    File(FilePart),
    Data(DataPart),
}

impl Part {
    /// Creates a text part.
    pub fn text(text: impl Into<String>) -> Self {
        Part::Text(TextPart {
            text: text.into(),
            metadata: None,
        })
    }

    /// Creates a data part carrying an arbitrary JSON object.
    pub fn data(data: Value) -> Self {
        Part::Data(DataPart {
            data,
            metadata: None,
        })
    }

    /// The `kind` discriminator value.
    pub fn kind(&self) -> PartKind {
        match self {
            Part::Text(_) => PartKind::Text,
            Part::File(_) => PartKind::File,
            Part::Data(_) => PartKind::Data,
        }
    }

    /// Read-only metadata map (never `None`).
    pub fn metadata(&self) -> &serde_json::Map<String, Value> {
        static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> =
            std::sync::OnceLock::new();
        let meta = match self {
            Part::Text(p) => &p.metadata,
            Part::File(p) => &p.metadata,
            Part::Data(p) => &p.metadata,
        };
        meta.as_ref()
            .unwrap_or_else(|| EMPTY.get_or_init(serde_json::Map::new))
    }

    /// Mutable metadata map, creating it when absent.
    pub fn metadata_mut(&mut self) -> &mut serde_json::Map<String, Value> {
        let slot = match self {
            Part::Text(p) => &mut p.metadata,
            Part::File(p) => &mut p.metadata,
            Part::Data(p) => &mut p.metadata,
        };
        slot.get_or_insert_with(serde_json::Map::new)
    }

    /// Convenience: a metadata value coerced to `String`, or `None`.
    pub fn metadata_string(&self, key: &str) -> Option<String> {
        self.metadata().get(key).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }

    /// Convenience: a metadata value interpreted as a boolean.
    pub fn metadata_bool(&self, key: &str) -> bool {
        matches!(self.metadata().get(key), Some(Value::Bool(true))) || {
            self.metadata()
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        }
    }

    /// Inserts a metadata entry, returning `self` for chaining.
    pub fn with_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.metadata_mut().insert(key.into(), value);
        self
    }
}

/// Plain text part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// File part, carrying either inline bytes or a URI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilePart {
    pub file: FileContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// File content discriminated by the presence of `bytes` (inline) or `uri` (reference).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileContent {
    /// MIME type, e.g. `image/png`.
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional file name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Base64-encoded inline bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,
    /// URI reference to the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

impl FileContent {
    /// Inline base64 file.
    pub fn with_bytes(
        mime_type: impl Into<String>,
        name: impl Into<String>,
        bytes: impl Into<String>,
    ) -> Self {
        Self {
            mime_type: Some(mime_type.into()),
            name: Some(name.into()),
            bytes: Some(bytes.into()),
            uri: None,
        }
    }

    /// URI-referenced file.
    pub fn with_uri(
        mime_type: impl Into<String>,
        name: impl Into<String>,
        uri: impl Into<String>,
    ) -> Self {
        Self {
            mime_type: Some(mime_type.into()),
            name: Some(name.into()),
            bytes: None,
            uri: Some(uri.into()),
        }
    }
}

impl FilePart {
    pub fn new(file: FileContent) -> Self {
        Self {
            file,
            metadata: None,
        }
    }
}

/// Structured (arbitrary JSON) part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataPart {
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

impl DataPart {
    pub fn new(data: Value) -> Self {
        Self {
            data,
            metadata: None,
        }
    }

    /// Attaches metadata, mirroring the Java fluent `DataPart.Builder#metadata`.
    pub fn with_metadata(mut self, metadata: serde_json::Map<String, Value>) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A2A message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "messageId")]
    pub message_id: String,
    #[serde(rename = "role")]
    pub role: MessageRole,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(rename = "contextId", skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(rename = "taskId", skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub kind: String,
}

impl Message {
    /// Builds a message with `kind: "message"` (the A2A wire default).
    pub fn new(message_id: impl Into<String>, role: MessageRole, parts: Vec<Part>) -> Self {
        Self {
            message_id: message_id.into(),
            role,
            parts,
            metadata: None,
            context_id: None,
            task_id: None,
            kind: "message".to_string(),
        }
    }
}

/// A2A task lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    Submitted,
    Working,
    #[serde(rename = "input-required")]
    InputRequired,
    Completed,
    Canceled,
    Failed,
    Rejected,
    AuthRequired,
    Unknown,
}

impl TaskState {
    /// Whether this state is terminal (no further updates expected).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Canceled | TaskState::Failed | TaskState::Rejected
        )
    }
}

/// Task status: state plus optional message and timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

impl TaskStatus {
    pub fn new(state: TaskState) -> Self {
        Self {
            state,
            message: None,
            timestamp: None,
        }
    }
}

/// A2A artifact (an output produced by a task).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

impl Artifact {
    pub fn new(artifact_id: impl Into<String>, parts: Vec<Part>) -> Self {
        Self {
            artifact_id: artifact_id.into(),
            name: None,
            description: None,
            parts,
            metadata: None,
        }
    }
}

/// A2A task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    #[serde(rename = "contextId", skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<Artifact>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    pub kind: String,
}

impl Task {
    pub fn new(id: impl Into<String>, status: TaskStatus) -> Self {
        Self {
            id: id.into(),
            context_id: None,
            status,
            artifacts: None,
            history: None,
            metadata: None,
            kind: "task".to_string(),
        }
    }
}

/// Streaming status-update event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStatusUpdateEvent {
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(rename = "contextId", skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: TaskStatus,
    #[serde(rename = "final")]
    pub is_final: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    pub kind: String,
}

impl TaskStatusUpdateEvent {
    pub fn new(task_id: impl Into<String>, status: TaskStatus, is_final: bool) -> Self {
        Self {
            task_id: task_id.into(),
            context_id: None,
            status,
            is_final,
            metadata: None,
            kind: "status-update".to_string(),
        }
    }
}

/// Streaming artifact-update event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactUpdateEvent {
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(rename = "contextId", skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub artifact: Artifact,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,
    #[serde(rename = "lastChunk", skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    pub kind: String,
}

impl TaskArtifactUpdateEvent {
    pub fn new(task_id: impl Into<String>, artifact: Artifact) -> Self {
        Self {
            task_id: task_id.into(),
            context_id: None,
            artifact,
            append: None,
            last_chunk: None,
            metadata: None,
            kind: "artifact-update".to_string(),
        }
    }
}

/// Any event that can appear on an A2A streaming response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamEvent {
    Task(Task),
    Message(Message),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

// ---------------------------------------------------------------------------
// Agent card
// ---------------------------------------------------------------------------

/// Agent provider information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentProvider {
    pub organization: String,
    pub url: String,
}

/// Agent capability flags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(rename = "pushNotifications", skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,
    #[serde(
        rename = "stateTransitionHistory",
        skip_serializing_if = "Option::is_none"
    )]
    pub state_transition_history: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<Value>>,
}

/// A2A security scheme (kept as opaque JSON for forward compatibility).
pub type SecurityScheme = Value;

/// A named transport (protocol binding) exposed by an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInterface {
    pub url: String,
    #[serde(rename = "transport")]
    pub transport: String,
}

/// A skill advertised by an agent card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub examples: Option<Vec<String>>,
    #[serde(rename = "inputModes", skip_serializing_if = "Option::is_none")]
    pub input_modes: Option<Vec<String>>,
    #[serde(rename = "outputModes", skip_serializing_if = "Option::is_none")]
    pub output_modes: Option<Vec<String>>,
}

/// A2A agent card: the discovery document served at the well-known URI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "protocolVersion", skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    #[serde(rename = "preferredTransport", skip_serializing_if = "Option::is_none")]
    pub preferred_transport: Option<String>,
    #[serde(
        rename = "additionalInterfaces",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_interfaces: Option<Vec<AgentInterface>>,
    /// Base64-encoded public key (A2A spec field).
    #[serde(rename = "iconUrl", skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    pub capabilities: AgentCapabilities,
    #[serde(rename = "defaultInputModes")]
    pub default_input_modes: Vec<String>,
    #[serde(rename = "defaultOutputModes")]
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    #[serde(rename = "securitySchemes", skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<serde_json::Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<Vec<serde_json::Map<String, Value>>>,
    #[serde(
        rename = "supportsAuthenticatedExtendedCard",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_authenticated_extended_card: Option<bool>,
    #[serde(rename = "signatures", skip_serializing_if = "Option::is_none")]
    pub signatures: Option<Vec<Value>>,
}

impl AgentCard {
    /// A minimal card with sensible defaults, mirroring `ConfigurableAgentCard`.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            url: url.into(),
            version: None,
            protocol_version: None,
            provider: None,
            preferred_transport: Some("JSONRPC".to_string()),
            additional_interfaces: None,
            icon_url: None,
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: Vec::new(),
            security_schemes: None,
            security: None,
            supports_authenticated_extended_card: None,
            signatures: None,
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC envelope
// ---------------------------------------------------------------------------

/// Parameter payload for `message/send` and `message/stream`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageSendParams {
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<MessageSendConfiguration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// Optional `message/send` configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MessageSendConfiguration {
    #[serde(
        rename = "acceptedOutputModes",
        skip_serializing_if = "Option::is_none"
    )]
    pub accepted_output_modes: Option<Vec<String>>,
    #[serde(rename = "historyLength", skip_serializing_if = "Option::is_none")]
    pub history_length: Option<i64>,
    #[serde(
        rename = "pushNotificationConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub push_notification_config: Option<PushNotificationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
}

/// Push notification configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PushNotificationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(rename = "authentication", skip_serializing_if = "Option::is_none")]
    pub authentication: Option<Value>,
}

/// Task query parameters (`tasks/get`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskQueryParams {
    pub id: String,
    #[serde(rename = "historyLength", skip_serializing_if = "Option::is_none")]
    pub history_length: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// Task id parameters (`tasks/cancel`, `tasks/resubscribe`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskIdParams {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// JSON-RPC request envelope. `id` may be omitted for notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl JsonRpcRequest {
    /// Builds a `2.0` request with an optional params payload and id.
    pub fn new(method: impl Into<String>, params: Option<Value>, id: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
            id,
        }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// A2A-specific: task not found.
    pub const TASK_NOT_FOUND: i64 = -32001;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// JSON-RPC response envelope (`result` XOR `error`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl JsonRpcResponse {
    pub fn success(result: Value, id: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn failure(error: JsonRpcError, id: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(error),
            id,
        }
    }

    /// Whether the response carries an error instead of a result.
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_part_round_trip_uses_kind_tag() {
        let part = Part::text("hello");
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["kind"], "text");
        assert_eq!(json["text"], "hello");
        let back: Part = serde_json::from_value(json).unwrap();
        assert_eq!(back, part);
    }

    #[test]
    fn file_part_uses_camel_case_mime_type() {
        let part = Part::File(FilePart::new(FileContent::with_uri(
            "image/png",
            "a.png",
            "https://example.com/a.png",
        )));
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["kind"], "file");
        assert_eq!(json["file"]["mimeType"], "image/png");
        assert_eq!(json["file"]["uri"], "https://example.com/a.png");
        assert!(json["file"].get("bytes").is_none());
    }

    #[test]
    fn message_wire_field_names_match_a2a_spec() {
        let msg = Message::new("m1", MessageRole::Agent, vec![Part::text("hi")]);
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["messageId"], "m1");
        assert_eq!(json["role"], "agent");
        assert_eq!(json["kind"], "message");
        assert_eq!(json["parts"][0]["kind"], "text");
    }

    #[test]
    fn task_state_serializes_as_kebab_case() {
        assert_eq!(
            serde_json::to_value(TaskState::InputRequired).unwrap(),
            serde_json::json!("input-required")
        );
        assert_eq!(
            serde_json::to_value(TaskState::Working).unwrap(),
            serde_json::json!("working")
        );
    }

    #[test]
    fn task_state_terminality() {
        assert!(TaskState::Completed.is_terminal());
        assert!(!TaskState::Working.is_terminal());
    }

    #[test]
    fn agent_card_defaults_and_round_trip() {
        let card = AgentCard::new("test-agent", "http://localhost:8080");
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["name"], "test-agent");
        assert_eq!(json["preferredTransport"], "JSONRPC");
        assert_eq!(json["defaultInputModes"][0], "text/plain");
        let back: AgentCard = serde_json::from_value(json).unwrap();
        assert_eq!(back, card);
    }

    #[test]
    fn jsonrpc_success_and_failure_envelopes() {
        let ok = JsonRpcResponse::success(serde_json::json!({"a": 1}), Some(serde_json::json!(1)));
        let json = serde_json::to_value(&ok).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["result"]["a"], 1);
        assert!(json.get("error").is_none());
        assert!(!ok.is_error());

        let err = JsonRpcResponse::failure(
            JsonRpcError::new(JsonRpcError::TASK_NOT_FOUND, "nope"),
            None,
        );
        assert!(err.is_error());
        assert_eq!(err.error.unwrap().code, -32001);
    }

    #[test]
    fn stream_event_untagged_deserializes_status_update() {
        let ev = TaskStatusUpdateEvent::new("t1", TaskStatus::new(TaskState::Working), false);
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], "status-update");
        assert_eq!(json["taskId"], "t1");
        assert_eq!(json["final"], false);
        let back: StreamEvent = serde_json::from_value(json).unwrap();
        match back {
            StreamEvent::StatusUpdate(e) => assert_eq!(e.task_id, "t1"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn part_metadata_helpers() {
        let part = Part::text("x")
            .with_metadata("a", serde_json::json!("v"))
            .with_metadata("stream", serde_json::json!(true));
        assert_eq!(part.metadata_string("a").as_deref(), Some("v"));
        assert!(part.metadata_bool("stream"));
        assert!(!part.metadata_bool("a"));
        assert!(part.metadata_string("missing").is_none());
    }
}
