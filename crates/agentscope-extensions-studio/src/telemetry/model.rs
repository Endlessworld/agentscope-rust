//! OTel message-part model types, mirroring the Java `telemetry.model` package.
//!
//! These types describe messages sent to / received from a model under the
//! OpenTelemetry GenAI semantic conventions. They serialize with a `type`
//! discriminator (`text`, `reasoning`, `tool_call`, `tool_call_response`,
//! `blob`, `file`, `uri`).
use serde::{Deserialize, Serialize};

/// Role of an entity that created a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    Developer,
}

impl Role {
    /// The wire value of this role.
    pub fn value(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::Developer => "developer",
        }
    }
}

/// A part of a message (text, reasoning, tool call, media, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessagePart {
    /// Text content sent to or received from the model.
    #[serde(rename = "text")]
    Text(TextPart),
    /// Reasoning/thinking content.
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningPart),
    /// A tool call requested by the model.
    #[serde(rename = "tool_call")]
    ToolCall(ToolCallRequestPart),
    /// A tool call result.
    #[serde(rename = "tool_call_response")]
    ToolCallResponse(ToolCallResponsePart),
    /// Inline binary data.
    #[serde(rename = "blob")]
    Blob(BlobPart),
    /// An externally referenced file by id.
    #[serde(rename = "file")]
    File(FilePart),
    /// An externally referenced file by URI.
    #[serde(rename = "uri")]
    Uri(UriPart),
}

/// Text part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    pub content: String,
}

impl TextPart {
    /// Creates a text part.
    pub fn create(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

/// Reasoning/thinking part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningPart {
    pub content: String,
}

impl ReasoningPart {
    /// Creates a reasoning part.
    pub fn create(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequestPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
}

impl ToolCallRequestPart {
    /// Creates a bare tool call part by name.
    pub fn create_by_name(name: impl Into<String>) -> Self {
        Self {
            id: None,
            name: name.into(),
            arguments: None,
        }
    }

    /// Creates a tool call part with id and arguments.
    pub fn create(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            id: Some(id.into()),
            name: name.into(),
            arguments: Some(arguments),
        }
    }
}

/// A tool call result sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallResponsePart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<serde_json::Value>,
}

impl ToolCallResponsePart {
    /// Creates a tool call response without an id.
    pub fn create(response: serde_json::Value) -> Self {
        Self {
            id: None,
            response: Some(response),
        }
    }

    /// Creates a tool call response with an id.
    pub fn create_with_id(id: impl Into<String>, response: serde_json::Value) -> Self {
        Self {
            id: Some(id.into()),
            response: Some(response),
        }
    }
}

/// Inline binary data (base64 when serialized).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobPart {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "mime_type")]
    pub mime_type: Option<String>,
    pub modality: String,
}

impl BlobPart {
    /// Creates a blob part.
    pub fn create(
        content: impl Into<String>,
        mime_type: Option<String>,
        modality: impl Into<String>,
    ) -> Self {
        Self {
            content: content.into(),
            mime_type,
            modality: modality.into(),
        }
    }
}

/// An externally referenced file by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilePart {
    #[serde(rename = "file_id")]
    pub file_id: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "mime_type")]
    pub mime_type: Option<String>,
    pub modality: String,
}

impl FilePart {
    /// Creates a file part.
    pub fn create(
        file_id: impl Into<String>,
        mime_type: Option<String>,
        modality: impl Into<String>,
    ) -> Self {
        Self {
            file_id: file_id.into(),
            mime_type,
            modality: modality.into(),
        }
    }
}

/// An externally referenced file by URI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UriPart {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "mime_type")]
    pub mime_type: Option<String>,
    pub modality: String,
}

impl UriPart {
    /// Creates a URI part.
    pub fn create(
        uri: impl Into<String>,
        mime_type: Option<String>,
        modality: impl Into<String>,
    ) -> Self {
        Self {
            uri: uri.into(),
            mime_type,
            modality: modality.into(),
        }
    }
}

/// An input message sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputMessage {
    pub role: String,
    pub parts: Vec<MessagePart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl InputMessage {
    /// Creates an input message from a role string.
    pub fn create(role: impl Into<String>, parts: Vec<MessagePart>, name: Option<String>) -> Self {
        Self {
            role: role.into(),
            parts,
            name,
        }
    }

    /// Creates an input message from a [`Role`].
    pub fn from_role(role: Role, parts: Vec<MessagePart>, name: Option<String>) -> Self {
        Self::create(role.value(), parts, name)
    }
}

/// An output message received from the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputMessage {
    pub role: String,
    pub parts: Vec<MessagePart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "finish_reason")]
    pub finish_reason: String,
}

impl OutputMessage {
    /// Creates an output message from a role string.
    pub fn create(
        role: impl Into<String>,
        parts: Vec<MessagePart>,
        name: Option<String>,
        finish_reason: impl Into<String>,
    ) -> Self {
        Self {
            role: role.into(),
            parts,
            name,
            finish_reason: finish_reason.into(),
        }
    }

    /// Creates an output message from a [`Role`].
    pub fn from_role(
        role: Role,
        parts: Vec<MessagePart>,
        name: Option<String>,
        finish_reason: impl Into<String>,
    ) -> Self {
        Self::create(role.value(), parts, name, finish_reason)
    }
}

/// System instructions sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemInstructions {
    pub parts: Vec<MessagePart>,
}

impl SystemInstructions {
    /// Creates system instructions.
    pub fn create(parts: Vec<MessagePart>) -> Self {
        Self { parts }
    }
}

/// A tool definition exposed to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ToolDefinition {
    /// Creates a tool definition by type and name.
    pub fn create(tool_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            tool_type: tool_type.into(),
            name: name.into(),
            description: None,
            parameters: None,
        }
    }

    /// Creates a fully-specified tool definition.
    pub fn create_full(
        tool_type: impl Into<String>,
        name: impl Into<String>,
        description: Option<String>,
        parameters: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Self {
        Self {
            tool_type: tool_type.into(),
            name: name.into(),
            description,
            parameters,
        }
    }
}
