//! AG-UI protocol data model, mirroring `io.agentscope.core.agui.model`.
//!
//! * [`AguiMessage`] - a conversation message (`content` is a [`MessageContent`] union).
//! * [`MessageContent`] - `Text(String)` or `Blocks(Vec<InputContent>)`, untagged on the wire.
//! * [`InputContent`] - a typed part discriminated by its `type` field.
//! * [`InputContentSource`] - `data` (inline base64) or `url`, discriminated by `type`.
//! * [`AguiToolCall`] / [`AguiFunctionCall`] - tool-call payloads.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A message in the AG-UI protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AguiMessage {
    /// Unique message id.
    pub id: String,
    /// Role: `user`, `assistant`, `system` or `tool`.
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
        }
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
    }
}
