//! OpenAI-compatible Chat Completions wire model, mirroring
//! `io.agentscope.core.chat.completions.model` in the Java SDK.
//!
//! All DTOs use Jackson's `NON_NULL` semantics: optional fields are omitted from
//! the JSON payload when absent. Field names match the OpenAI JSON wire format
//! exactly (`tool_calls`, `tool_call_id`, `finish_reason`, `prompt_tokens`, ...).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A single function invocation inside a [`ToolCall`].
///
/// JSON:
/// ```json
/// {"name": "get_weather", "arguments": "{\"city\":\"Hangzhou\"}"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCall {
    /// Function name.
    pub name: String,
    /// JSON-encoded arguments string (as produced by the model).
    pub arguments: String,
}

impl FunctionCall {
    /// Creates a new function call.
    pub fn new(name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            arguments: arguments.into(),
        }
    }
}

/// A tool call in OpenAI-compatible format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct ToolCall {
    /// Index in the `tool_calls` array (streaming only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i32>,
    /// Unique identifier for this tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Tool type; always `"function"`.
    #[serde(default = "default_function_type")]
    pub r#type: String,
    /// The function invocation, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCall>,
}

fn default_function_type() -> String {
    "function".to_string()
}

impl ToolCall {
    /// Creates a tool call without an index (non-streaming).
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            index: None,
            id: Some(id.into()),
            r#type: "function".to_string(),
            function: Some(FunctionCall::new(name, arguments)),
        }
    }

    /// Creates a tool call carrying a streaming index.
    pub fn with_index(
        index: i32,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            index: Some(index),
            id: Some(id.into()),
            r#type: "function".to_string(),
            function: Some(FunctionCall::new(name, arguments)),
        }
    }
}

/// Chat message representation for the Chat Completions API.
///
/// Mirrors `io.agentscope.core.chat.completions.model.ChatMessage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct ChatMessage {
    /// Message role: `user`, `assistant`, `system` or `tool`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Plain-text content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool calls made by the assistant (assistant role only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// ID of the tool call this message responds to (tool role only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Name of the tool (tool role only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    /// Creates a simple text message.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Some(role.into()),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    /// Creates an assistant message carrying tool calls.
    pub fn assistant_with_tool_calls(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Some("assistant".to_string()),
            content,
            tool_calls: Some(tool_calls),
            ..Default::default()
        }
    }

    /// Creates a tool result message.
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Some("tool".to_string()),
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            tool_calls: None,
        }
    }
}

/// One entry of the `choices` array, used both for full responses (`message`)
/// and streaming deltas (`delta`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct ChatChoice {
    /// Index of the choice in the list.
    #[serde(default)]
    pub index: i32,
    /// Full message (non-streaming responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<ChatMessage>,
    /// Incremental message (streaming responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<ChatMessage>,
    /// Why the model stopped generating (`stop`, `tool_calls`, `length`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Log-probability payload (opaque; pass-through).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Value>,
}

impl ChatChoice {
    /// Creates a choice carrying a full message.
    pub fn message(index: i32, message: ChatMessage, finish_reason: Option<String>) -> Self {
        Self {
            index,
            message: Some(message),
            finish_reason,
            ..Default::default()
        }
    }

    /// Creates a choice carrying a streaming delta.
    pub fn delta(index: i32, delta: ChatMessage, finish_reason: Option<String>) -> Self {
        Self {
            index,
            delta: Some(delta),
            finish_reason,
            ..Default::default()
        }
    }
}

/// Token usage block of a chat completion response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct Usage {
    /// Number of prompt tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i32>,
    /// Number of completion tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i32>,
    /// Total number of tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i32>,
}

/// A chat completion response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ChatCompletionsResponse {
    /// Request identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Object kind; always `"chat.completion"`.
    #[serde(default = "default_completion_object")]
    pub object: String,
    /// Unix epoch seconds when the response was created.
    #[serde(default)]
    pub created: i64,
    /// Model name echoed from the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Generated choices.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<ChatChoice>>,
    /// Token usage, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

fn default_completion_object() -> String {
    "chat.completion".to_string()
}

impl Default for ChatCompletionsResponse {
    fn default() -> Self {
        Self {
            id: None,
            object: default_completion_object(),
            created: 0,
            model: None,
            choices: None,
            usage: None,
        }
    }
}

/// A streaming chat completion chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ChatCompletionsChunk {
    /// Request identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Object kind; always `"chat.completion.chunk"`.
    #[serde(default = "default_chunk_object")]
    pub object: String,
    /// Unix epoch seconds when the chunk was created.
    #[serde(default)]
    pub created: i64,
    /// Model name echoed from the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Chunk choices carrying deltas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<ChatChoice>>,
}

fn default_chunk_object() -> String {
    "chat.completion.chunk".to_string()
}

impl ChatCompletionsChunk {
    /// Creates an empty chunk for the given request/model.
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            object: default_chunk_object(),
            created: 0,
            model: Some(model.into()),
            choices: None,
        }
    }

    /// Creates a text delta chunk.
    pub fn text_chunk(id: &str, model: &str, content: impl Into<String>) -> Self {
        let delta = ChatMessage {
            role: Some("assistant".to_string()),
            content: Some(content.into()),
            ..Default::default()
        };
        let mut chunk = Self::new(id, model);
        chunk.choices = Some(vec![ChatChoice::delta(0, delta, None)]);
        chunk
    }

    /// Creates a tool-call delta chunk.
    pub fn tool_call_chunk(id: &str, model: &str, tool_calls: Vec<ToolCall>) -> Self {
        let delta = ChatMessage {
            role: Some("assistant".to_string()),
            tool_calls: Some(tool_calls),
            ..Default::default()
        };
        let mut chunk = Self::new(id, model);
        chunk.choices = Some(vec![ChatChoice::delta(0, delta, None)]);
        chunk
    }

    /// Creates a tool-result chunk.
    ///
    /// Tool results are rendered as assistant text prefixed with the tool name,
    /// because OpenAI streaming deltas only allow the `assistant`/`user` roles.
    pub fn tool_result_chunk(
        id: &str,
        model: &str,
        _tool_call_id: Option<&str>,
        tool_name: &str,
        content: &str,
    ) -> Self {
        Self::text_chunk(id, model, format!("[Tool: {}] {}", tool_name, content))
    }

    /// Creates a chunk carrying only a `finish_reason`.
    pub fn finish_chunk(id: &str, model: &str, finish_reason: impl Into<String>) -> Self {
        let mut chunk = Self::new(id, model);
        chunk.choices = Some(vec![ChatChoice::delta(
            0,
            ChatMessage::default(),
            Some(finish_reason.into()),
        )]);
        chunk
    }
}

/// An OpenAI tool definition usable in requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OpenAITool {
    /// Tool type; always `"function"`.
    #[serde(default = "default_function_type")]
    pub r#type: String,
    /// The function definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<OpenAIToolFunction>,
}

impl Default for OpenAITool {
    fn default() -> Self {
        Self {
            r#type: default_function_type(),
            function: None,
        }
    }
}

impl OpenAITool {
    /// Creates a tool wrapping the given function definition.
    pub fn new(function: OpenAIToolFunction) -> Self {
        Self {
            r#type: default_function_type(),
            function: Some(function),
        }
    }
}

/// The function definition inside an [`OpenAITool`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpenAIToolFunction {
    /// Function name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Map<String, Value>>,
    /// Whether strict schema validation is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

impl OpenAIToolFunction {
    /// Creates a function definition with a name and description.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            description: Some(description.into()),
            parameters: None,
            strict: None,
        }
    }
}

/// A chat completion request, matching the OpenAI Chat Completions API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct ChatCompletionsRequest {
    /// Requested model name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Conversation history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<ChatMessage>>,
    /// Whether to stream the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAITool>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_message_serializes_tool_calls_and_tool_call_id_with_snake_case() {
        let msg = ChatMessage::assistant_with_tool_calls(
            Some("Let me check".to_string()),
            vec![ToolCall::new(
                "call_abc",
                "get_weather",
                "{\"city\":\"Hangzhou\"}",
            )],
        );
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["role"], json!("assistant"));
        assert_eq!(value["content"], json!("Let me check"));
        assert_eq!(value["tool_calls"][0]["id"], json!("call_abc"));
        assert_eq!(value["tool_calls"][0]["type"], json!("function"));
        assert_eq!(
            value["tool_calls"][0]["function"]["name"],
            json!("get_weather")
        );
        assert_eq!(
            value["tool_calls"][0]["function"]["arguments"],
            json!("{\"city\":\"Hangzhou\"}")
        );
        // non-null omission
        assert!(value.get("tool_call_id").is_none());
        assert!(value.get("name").is_none());
    }

    #[test]
    fn tool_result_message_round_trips() {
        let msg = ChatMessage::tool_result("call_abc", "get_weather", "{\"temp\": 25}");
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            text,
            "{\"role\":\"tool\",\"content\":\"{\\\"temp\\\": 25}\",\"tool_call_id\":\"call_abc\",\"name\":\"get_weather\"}"
        );
        let back: ChatMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn tool_call_index_is_serialized_for_streaming() {
        let call = ToolCall::with_index(0, "call_1", "get_weather", "{}");
        let value = serde_json::to_value(&call).unwrap();
        assert_eq!(value["index"], json!(0));
        assert_eq!(value["id"], json!("call_1"));
        assert_eq!(value["type"], json!("function"));
    }

    #[test]
    fn openai_tool_uses_camel_case_function_fields() {
        let mut function = OpenAIToolFunction::new("get_weather", "Get the current weather");
        function.parameters = Some(
            serde_json::from_value(json!({
                "type": "object",
                "properties": {"location": {"type": "string"}},
                "required": ["location"]
            }))
            .unwrap(),
        );
        function.strict = Some(true);
        let tool = OpenAITool::new(function);
        let value = serde_json::to_value(&tool).unwrap();
        assert_eq!(value["type"], json!("function"));
        assert_eq!(value["function"]["name"], json!("get_weather"));
        assert_eq!(value["function"]["strict"], json!(true));
        assert_eq!(value["function"]["parameters"]["type"], json!("object"));
    }

    #[test]
    fn response_object_defaults_and_usage_snake_case() {
        let mut response = ChatCompletionsResponse {
            id: Some("chatcmpl-1".to_string()),
            created: 1234567890,
            model: Some("gpt-4".to_string()),
            ..Default::default()
        };
        response.choices = Some(vec![ChatChoice::message(
            0,
            ChatMessage::new("assistant", "Hello!"),
            Some("stop".to_string()),
        )]);
        response.usage = Some(Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
        });
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["object"], json!("chat.completion"));
        assert_eq!(value["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(value["choices"][0]["message"]["content"], json!("Hello!"));
        assert_eq!(value["usage"]["prompt_tokens"], json!(10));
        assert_eq!(value["usage"]["completion_tokens"], json!(5));
        assert_eq!(value["usage"]["total_tokens"], json!(15));
        // `delta` must be omitted when only `message` is set
        assert!(value["choices"][0].get("delta").is_none());
    }

    #[test]
    fn chunk_object_default_and_text_chunk_shape() {
        let chunk = ChatCompletionsChunk::text_chunk("req-1", "gpt-4", "Hi");
        let value = serde_json::to_value(&chunk).unwrap();
        assert_eq!(value["object"], json!("chat.completion.chunk"));
        assert_eq!(value["id"], json!("req-1"));
        assert_eq!(value["model"], json!("gpt-4"));
        assert_eq!(value["choices"][0]["index"], json!(0));
        assert_eq!(value["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(value["choices"][0]["delta"]["content"], json!("Hi"));
    }

    #[test]
    fn tool_result_chunk_prefixes_tool_name_as_assistant() {
        let chunk = ChatCompletionsChunk::tool_result_chunk(
            "req-1",
            "gpt-4",
            Some("call_1"),
            "get_weather",
            "sunny",
        );
        let value = serde_json::to_value(&chunk).unwrap();
        assert_eq!(value["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(
            value["choices"][0]["delta"]["content"],
            json!("[Tool: get_weather] sunny")
        );
    }

    #[test]
    fn finish_chunk_carries_finish_reason_only() {
        let chunk = ChatCompletionsChunk::finish_chunk("req-1", "gpt-4", "tool_calls");
        let value = serde_json::to_value(&chunk).unwrap();
        assert_eq!(value["choices"][0]["finish_reason"], json!("tool_calls"));
        let delta = &value["choices"][0]["delta"];
        assert_eq!(delta, &json!({}));
    }

    #[test]
    fn request_deserializes_openai_sample() {
        let raw = r#"{
            "model": "gpt-4",
            "stream": true,
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "tool_calls": [{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "name": "f", "content": "ok"}
            ],
            "tools": [{"type": "function", "function": {"name": "f", "description": "d"}}]
        }"#;
        let request: ChatCompletionsRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(request.model.as_deref(), Some("gpt-4"));
        assert_eq!(request.stream, Some(true));
        let messages = request.messages.unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1].tool_calls.as_ref().unwrap()[0].id.as_deref(),
            Some("c1")
        );
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(
            request.tools.unwrap()[0]
                .function
                .as_ref()
                .unwrap()
                .name
                .as_deref(),
            Some("f")
        );
    }
}
