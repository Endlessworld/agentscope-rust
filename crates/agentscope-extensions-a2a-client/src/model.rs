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
///
/// A2A 1.0 renamed every JSON-RPC method to PascalCase (Java `org.a2aproject.sdk.spec.A2AMethods`
/// in `a2a-java-sdk` 1.4); the 0.3.0 slash-separated names are kept as `LEGACY_*` aliases that the
/// server still dispatches for two release cycles (the client only ever sends the 1.0 names).
pub mod methods {
    // ---- A2A 1.0 (the only names this crate sends) ----
    pub const MESSAGE_SEND: &str = "SendMessage";
    pub const MESSAGE_STREAM: &str = "SendStreamingMessage";
    pub const TASKS_GET: &str = "GetTask";
    pub const TASKS_CANCEL: &str = "CancelTask";
    pub const TASKS_RESUBSCRIBE: &str = "SubscribeToTask";
    /// A2A 1.0 `ListTasks`. Declared by the spec, rejected by the server (see
    /// `agentscope_extensions_a2a_server::LIST_TASKS_NOT_SUPPORTED_MESSAGE`).
    pub const TASKS_LIST: &str = "ListTasks";
    pub const PUSH_CONFIG_SET: &str = "CreateTaskPushNotificationConfig";
    pub const PUSH_CONFIG_GET: &str = "GetTaskPushNotificationConfig";
    pub const PUSH_CONFIG_LIST: &str = "ListTaskPushNotificationConfigs";
    pub const PUSH_CONFIG_DELETE: &str = "DeleteTaskPushNotificationConfig";
    pub const AGENT_CARD_GET: &str = "GetExtendedAgentCard";

    // ---- A2A 0.3 aliases (dispatch-only compatibility, remove after two cycles) ----
    pub const LEGACY_MESSAGE_SEND: &str = "message/send";
    pub const LEGACY_MESSAGE_STREAM: &str = "message/stream";
    pub const LEGACY_TASKS_GET: &str = "tasks/get";
    pub const LEGACY_TASKS_CANCEL: &str = "tasks/cancel";
    pub const LEGACY_TASKS_RESUBSCRIBE: &str = "tasks/resubscribe";
    pub const LEGACY_TASKS_LIST: &str = "tasks/list";
    pub const LEGACY_PUSH_CONFIG_SET: &str = "tasks/pushNotificationConfig/set";
    pub const LEGACY_PUSH_CONFIG_GET: &str = "tasks/pushNotificationConfig/get";
    pub const LEGACY_PUSH_CONFIG_LIST: &str = "tasks/pushNotificationConfig/list";
    pub const LEGACY_PUSH_CONFIG_DELETE: &str = "tasks/pushNotificationConfig/delete";
    pub const LEGACY_AGENT_CARD_GET: &str = "agent/getAuthenticatedExtendedCard";
}

/// A2A wire protocol version implemented by this crate (`AgentInterface.protocolVersion`).
///
/// A2A 1.0 spells the version `1.0` (not `1.0.0`); it is set on every `supportedInterfaces` entry.
pub const PROTOCOL_VERSION: &str = "1.0";

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

    /// The wire name of this state (`"completed"`, `"input-required"`, ...).
    ///
    /// The names match the A2A protocol values this enum serializes to, so diagnostics built
    /// from them stay stable regardless of the Rust variant spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Submitted => "submitted",
            TaskState::Working => "working",
            TaskState::InputRequired => "input-required",
            TaskState::Completed => "completed",
            TaskState::Canceled => "canceled",
            TaskState::Failed => "failed",
            TaskState::Rejected => "rejected",
            TaskState::AuthRequired => "auth-required",
            TaskState::Unknown => "unknown",
        }
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

/// A named transport (protocol binding) exposed by an agent (A2A 1.0 `AgentInterface`).
///
/// Wire shape (1.0): `{"url": "...", "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}`.
/// `tenant` is not modelled (this crate never pins an interface to a tenant).
///
/// The Rust field is still spelled `transport` so that sibling crates constructing the struct keep
/// compiling; the wire name is the 1.0 `protocolBinding` and the 0.3 `transport` name is accepted on
/// deserialization.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInterface {
    pub url: String,
    /// The protocol binding, e.g. `JSONRPC` (wire key `protocolBinding`).
    pub transport: String,
}

impl AgentInterface {
    /// Builds an interface for `url` over the `JSONRPC` binding.
    pub fn json_rpc(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            transport: "JSONRPC".to_string(),
        }
    }
}

impl Serialize for AgentInterface {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("url", &self.url)?;
        map.serialize_entry("protocolBinding", &self.transport)?;
        map.serialize_entry("protocolVersion", PROTOCOL_VERSION)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for AgentInterface {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct InterfaceVisitor;

        impl<'de> serde::de::Visitor<'de> for InterfaceVisitor {
            type Value = AgentInterface;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an A2A agent interface object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<AgentInterface, A::Error> {
                let mut url = String::new();
                let mut binding = String::new();
                while let Some(key) = access.next_key::<String>()? {
                    match key.as_str() {
                        "url" => url = access.next_value::<Option<String>>()?.unwrap_or_default(),
                        "protocolBinding" | "transport" => {
                            binding = access.next_value::<Option<String>>()?.unwrap_or_default();
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = access.next_value()?;
                        }
                    }
                }
                Ok(AgentInterface {
                    url,
                    transport: binding,
                })
            }
        }

        deserializer.deserialize_map(InterfaceVisitor)
    }
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
///
/// Serialization follows A2A 1.0: the discoverable endpoints live in `supportedInterfaces` (the
/// preferred one first), the security requirements are `securityRequirements`, and the 0.3
/// top-level `url` / `preferredTransport` / `additionalInterfaces` / `protocolVersion` / `security`
/// keys are **not** emitted any more. Deserialization accepts both shapes, so a 0.3 card still
/// parses: `supportedInterfaces[0]` maps onto [`url`](Self::url) +
/// [`preferred_transport`](Self::preferred_transport) and the rest onto
/// [`additional_interfaces`](Self::additional_interfaces).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCard {
    pub name: String,
    pub description: Option<String>,
    /// A URL to the documentation for this agent (A2A spec `documentationUrl`).
    pub documentation_url: Option<String>,
    /// URL of the preferred interface (1.0: `supportedInterfaces[0].url`).
    pub url: String,
    pub version: Option<String>,
    /// 0.3 top-level `protocolVersion`, kept for parsing old cards only (never serialized).
    pub protocol_version: Option<String>,
    pub provider: Option<AgentProvider>,
    /// Protocol binding of the preferred interface (1.0: `supportedInterfaces[0].protocolBinding`).
    pub preferred_transport: Option<String>,
    /// Non-preferred interfaces (1.0: `supportedInterfaces[1..]`).
    pub additional_interfaces: Option<Vec<AgentInterface>>,
    /// Icon URL (A2A spec field `iconUrl`).
    pub icon_url: Option<String>,
    pub capabilities: AgentCapabilities,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    pub security_schemes: Option<serde_json::Map<String, Value>>,
    /// Security requirements, wire key `securityRequirements` in 1.0 (`security` in 0.3).
    pub security: Option<Vec<serde_json::Map<String, Value>>>,
    pub supports_authenticated_extended_card: Option<bool>,
    pub signatures: Option<Vec<Value>>,
}

impl AgentCard {
    /// A minimal card with sensible defaults, mirroring `ConfigurableAgentCard`.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            documentation_url: None,
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

    /// The 1.0 `supportedInterfaces` list: the preferred interface first, then the others.
    ///
    /// Mirrors Java `AgentScopeAgentCardConverter#supportedInterfaces`.
    pub fn supported_interfaces(&self) -> Vec<AgentInterface> {
        let mut interfaces: Vec<AgentInterface> = Vec::new();
        if !self.url.is_empty() {
            interfaces.push(AgentInterface {
                url: self.url.clone(),
                transport: self
                    .preferred_transport
                    .clone()
                    .filter(|binding| !binding.is_empty())
                    .unwrap_or_else(|| "JSONRPC".to_string()),
            });
        }
        if let Some(additional) = &self.additional_interfaces {
            for interface in additional {
                if !interfaces.contains(interface) {
                    interfaces.push(interface.clone());
                }
            }
        }
        interfaces
    }

    /// Rebuilds a card from a JSON object, accepting the 1.0 and the 0.3 shapes.
    fn from_wire(value: Value) -> Result<Self, String> {
        let Value::Object(map) = value else {
            return Err("agent card must be a JSON object".to_string());
        };

        let name = string_field(&map, "name")
            .ok_or_else(|| "agent card is missing `name`".to_string())?;

        let interfaces =
            optional_field::<Vec<AgentInterface>>(&map, "supportedInterfaces")?.unwrap_or_default();

        let (url, preferred_transport, additional_interfaces) = if interfaces.is_empty() {
            (
                string_field(&map, "url").unwrap_or_default(),
                string_field(&map, "preferredTransport"),
                optional_field::<Vec<AgentInterface>>(&map, "additionalInterfaces")?,
            )
        } else {
            let mut interfaces = interfaces.into_iter();
            let preferred = interfaces.next().unwrap_or_default();
            let rest: Vec<AgentInterface> = interfaces.collect();
            (
                preferred.url,
                Some(preferred.transport),
                if rest.is_empty() { None } else { Some(rest) },
            )
        };

        let security = match optional_field::<Vec<serde_json::Map<String, Value>>>(
            &map,
            "securityRequirements",
        )? {
            Some(requirements) => Some(requirements),
            None => optional_field(&map, "security")?,
        };

        Ok(Self {
            name,
            description: string_field(&map, "description"),
            documentation_url: string_field(&map, "documentationUrl"),
            url,
            version: string_field(&map, "version"),
            protocol_version: string_field(&map, "protocolVersion"),
            provider: optional_field(&map, "provider")?,
            preferred_transport,
            additional_interfaces,
            icon_url: string_field(&map, "iconUrl"),
            capabilities: optional_field(&map, "capabilities")?.unwrap_or_default(),
            default_input_modes: optional_field(&map, "defaultInputModes")?.unwrap_or_default(),
            default_output_modes: optional_field(&map, "defaultOutputModes")?.unwrap_or_default(),
            skills: optional_field(&map, "skills")?.unwrap_or_default(),
            security_schemes: optional_field(&map, "securitySchemes")?,
            security,
            supports_authenticated_extended_card: optional_field(
                &map,
                "supportsAuthenticatedExtendedCard",
            )?,
            signatures: optional_field(&map, "signatures")?,
        })
    }
}

/// Reads an optional string field from a JSON object.
fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Reads and decodes an optional field, reporting the key when the shape is wrong.
fn optional_field<T: serde::de::DeserializeOwned>(
    map: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<T>, String> {
    match map.get(key) {
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| format!("invalid `{key}`: {error}")),
        None => Ok(None),
    }
}

impl Default for AgentInterface {
    fn default() -> Self {
        Self {
            url: String::new(),
            transport: String::new(),
        }
    }
}

impl Serialize for AgentCard {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", &self.name)?;
        if let Some(value) = &self.description {
            map.serialize_entry("description", value)?;
        }
        if let Some(value) = &self.documentation_url {
            map.serialize_entry("documentationUrl", value)?;
        }
        if let Some(value) = &self.version {
            map.serialize_entry("version", value)?;
        }
        if let Some(value) = &self.provider {
            map.serialize_entry("provider", value)?;
        }
        map.serialize_entry("supportedInterfaces", &self.supported_interfaces())?;
        if let Some(value) = &self.icon_url {
            map.serialize_entry("iconUrl", value)?;
        }
        map.serialize_entry("capabilities", &self.capabilities)?;
        map.serialize_entry("defaultInputModes", &self.default_input_modes)?;
        map.serialize_entry("defaultOutputModes", &self.default_output_modes)?;
        map.serialize_entry("skills", &self.skills)?;
        if let Some(value) = &self.security_schemes {
            map.serialize_entry("securitySchemes", value)?;
        }
        if let Some(value) = &self.security {
            map.serialize_entry("securityRequirements", value)?;
        }
        if let Some(value) = &self.supports_authenticated_extended_card {
            map.serialize_entry("supportsAuthenticatedExtendedCard", value)?;
        }
        if let Some(value) = &self.signatures {
            map.serialize_entry("signatures", value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for AgentCard {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CardVisitor;

        impl<'de> serde::de::Visitor<'de> for CardVisitor {
            type Value = AgentCard;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an A2A agent card object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<AgentCard, A::Error> {
                let mut fields = serde_json::Map::new();
                while let Some(key) = access.next_key::<String>()? {
                    let value: Value = access.next_value()?;
                    fields.insert(key, value);
                }
                AgentCard::from_wire(Value::Object(fields)).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_map(CardVisitor)
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
    /// A2A-specific: the operation is not supported by this server (1.0 `UnsupportedOperationError`).
    pub const UNSUPPORTED_OPERATION: i64 = -32004;

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

    // Bound to the `A2AMethods` literals of `a2a-java-sdk` 1.4.0.Final: before the 1.0 migration
    // every constant was a 0.3 slash-separated name, so this fails on the 0.3 wire.
    #[test]
    fn wire_method_names_follow_a2a_1_0() {
        assert_eq!(methods::MESSAGE_SEND, "SendMessage");
        assert_eq!(methods::MESSAGE_STREAM, "SendStreamingMessage");
        assert_eq!(methods::TASKS_GET, "GetTask");
        assert_eq!(methods::TASKS_CANCEL, "CancelTask");
        assert_eq!(methods::TASKS_RESUBSCRIBE, "SubscribeToTask");
        assert_eq!(methods::PUSH_CONFIG_SET, "CreateTaskPushNotificationConfig");
        assert_eq!(methods::PUSH_CONFIG_GET, "GetTaskPushNotificationConfig");
        assert_eq!(methods::PUSH_CONFIG_LIST, "ListTaskPushNotificationConfigs");
        assert_eq!(methods::PUSH_CONFIG_DELETE, "DeleteTaskPushNotificationConfig");
        assert_eq!(methods::AGENT_CARD_GET, "GetExtendedAgentCard");
        // The 0.3 names survive only as dispatch aliases.
        assert_eq!(methods::LEGACY_MESSAGE_SEND, "message/send");
        assert_eq!(methods::LEGACY_TASKS_GET, "tasks/get");
        assert_ne!(methods::MESSAGE_SEND, methods::LEGACY_MESSAGE_SEND);
    }

    #[test]
    fn agent_card_defaults_and_round_trip() {
        let card = AgentCard::new("test-agent", "http://localhost:8080");
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["name"], "test-agent");
        assert_eq!(json["supportedInterfaces"][0]["url"], "http://localhost:8080");
        assert_eq!(json["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
        assert_eq!(json["supportedInterfaces"][0]["protocolVersion"], "1.0");
        assert_eq!(json["defaultInputModes"][0], "text/plain");
        let back: AgentCard = serde_json::from_value(json).unwrap();
        assert_eq!(back, card);
    }

    #[test]
    fn agent_card_serializes_the_a2a_1_0_shape() {
        let mut card = AgentCard::new("a2a-agent", "http://localhost:8080");
        card.additional_interfaces = Some(vec![AgentInterface {
            url: "http://localhost:9090".to_string(),
            transport: "GRPC".to_string(),
        }]);
        card.security = Some(vec![serde_json::Map::from_iter([(
            "apiKey".to_string(),
            serde_json::json!(["read"]),
        )])]);

        let json = serde_json::to_value(&card).unwrap();
        let interfaces = json["supportedInterfaces"].as_array().unwrap();
        assert_eq!(interfaces.len(), 2);
        assert_eq!(interfaces[0]["url"], "http://localhost:8080");
        assert_eq!(interfaces[0]["protocolBinding"], "JSONRPC");
        assert_eq!(interfaces[1]["url"], "http://localhost:9090");
        assert_eq!(interfaces[1]["protocolBinding"], "GRPC");
        assert_eq!(interfaces[1]["protocolVersion"], "1.0");
        assert!(interfaces[1].get("transport").is_none());
        assert_eq!(json["securityRequirements"][0]["apiKey"][0], "read");
        // The 0.3 top-level discovery keys are gone.
        for legacy in [
            "url",
            "preferredTransport",
            "additionalInterfaces",
            "protocolVersion",
            "security",
        ] {
            assert!(json.get(legacy).is_none(), "unexpected 0.3 key {legacy}");
        }

        let back: AgentCard = serde_json::from_value(json).unwrap();
        assert_eq!(back.url, "http://localhost:8080");
        assert_eq!(back.preferred_transport.as_deref(), Some("JSONRPC"));
        assert_eq!(back.additional_interfaces.as_ref().unwrap().len(), 1);
        assert_eq!(back.security, card.security);
    }

    #[test]
    fn agent_card_still_parses_a_0_3_card() {
        let legacy = serde_json::json!({
            "name": "legacy-agent",
            "url": "http://localhost:8080",
            "protocolVersion": "0.3.0",
            "preferredTransport": "JSONRPC",
            "additionalInterfaces": [
                { "url": "http://localhost:9090", "transport": "GRPC" }
            ],
            "capabilities": { "streaming": true },
            "defaultInputModes": ["text"],
            "defaultOutputModes": ["text"],
            "skills": [],
            "security": [ { "apiKey": ["read"] } ]
        });

        let card: AgentCard = serde_json::from_value(legacy).unwrap();
        assert_eq!(card.name, "legacy-agent");
        assert_eq!(card.url, "http://localhost:8080");
        assert_eq!(card.protocol_version.as_deref(), Some("0.3.0"));
        assert_eq!(card.preferred_transport.as_deref(), Some("JSONRPC"));
        assert_eq!(card.additional_interfaces.as_ref().unwrap()[0].transport, "GRPC");
        assert_eq!(
            card.security.as_ref().unwrap()[0]["apiKey"][0],
            serde_json::json!("read")
        );

        // …and is re-emitted in the 1.0 shape.
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
        assert!(json.get("url").is_none());
    }

    #[test]
    fn agent_card_serializes_documentation_url() {
        let mut card = AgentCard::new("test-agent", "http://localhost:8080");
        card.documentation_url = Some("https://docs.example/agent".into());
        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["documentationUrl"], "https://docs.example/agent");
        let back: AgentCard = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.documentation_url.as_deref(),
            Some("https://docs.example/agent")
        );
        // Absent when unset (never serialized as `null`).
        let plain = serde_json::to_value(AgentCard::new("x", "http://y")).unwrap();
        assert!(plain.get("documentationUrl").is_none());
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
