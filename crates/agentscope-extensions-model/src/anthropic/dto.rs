//! Anthropic Messages API wire DTOs (`POST {base_url}/v1/messages`).
//!
//! Java talks to the official `com.anthropic.client.okhttp.AnthropicOkHttpClient`
//! SDK, whose types (`MessageCreateParams`, `MessageParam`, `Message`, ...) are
//! not available in Rust. This adapter therefore re-declares the wire shape and
//! (de)serializes it with `serde`, exactly like the ollama/dashscope adapters do.
//!
//! Only the subsets actually sent/parsed are modelled; unknown fields are
//! ignored on deserialization so newer API revisions stay parseable.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `POST /v1/messages` request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AnthropicRequest {
    /// Model id, e.g. `claude-sonnet-4-5-20250929`.
    pub model: String,
    /// Required by the Anthropic API (unlike OpenAI).
    pub max_tokens: u32,
    /// Conversation messages. System prompts travel in [`Self::system`].
    pub messages: Vec<AnthropicMessage>,
    /// Whether to stream Server-Sent Events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// System prompt: either a plain string or a list of text blocks carrying
    /// `cache_control` markers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    /// Tool declarations: client tools (with `input_schema`) and built-in
    /// server tools (versioned `type`), in declaration order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicToolEntry>>,
    /// Tool-choice configuration (Anthropic nests `disable_parallel_tool_use`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Extended-thinking budget in tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinking>,
}

/// System prompt representation: a plain string, or text blocks (for caching).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// The common case: a single string.
    Text(String),
    /// Block form, used when the system prompt carries `cache_control`.
    Blocks(Vec<AnthropicTextBlock>),
}

/// Extended-thinking configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicThinking {
    /// Always `"enabled"` when present.
    #[serde(rename = "type")]
    pub kind: String,
    /// The thinking budget in tokens.
    pub budget_tokens: u32,
}

impl AnthropicThinking {
    /// Builds an `enabled` thinking config with the given budget.
    pub fn enabled(budget_tokens: u32) -> Self {
        Self {
            kind: "enabled".to_string(),
            budget_tokens,
        }
    }
}

/// A single conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessage {
    /// `user` or `assistant` (Anthropic has no `system` role in `messages`).
    pub role: String,
    /// Content blocks. Always a list in this adapter so `cache_control` can be
    /// attached to individual blocks.
    pub content: Vec<AnthropicContentBlock>,
}

/// A content block inside a request message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    /// Plain text.
    Text(AnthropicTextBlock),
    /// An image (base64 or remote URL source).
    Image(AnthropicImageBlock),
    /// An extended-thinking block. Only replayed on request when the
    /// signature from the original response is still available.
    Thinking(AnthropicThinkingBlock),
    /// A model-issued tool call.
    ToolUse(AnthropicToolUseBlock),
    /// A tool execution result (always inside a `user` message).
    ToolResult(AnthropicToolResultBlock),
    /// A call to a provider-executed (server) tool, e.g. `web_search`.
    ServerToolUse(AnthropicServerToolUseBlock),
    /// Result of a server-side web search.
    WebSearchToolResult(AnthropicServerToolResultBlock),
    /// Result of a server-side web fetch.
    WebFetchToolResult(AnthropicServerToolResultBlock),
    /// Result of server-side code execution.
    CodeExecutionToolResult(AnthropicServerToolResultBlock),
    /// Result of server-side bash execution.
    BashCodeExecutionToolResult(AnthropicServerToolResultBlock),
    /// Result of server-side text-editor execution.
    TextEditorCodeExecutionToolResult(AnthropicServerToolResultBlock),
    /// Result of the server-side tool-search tool.
    ToolSearchToolResult(AnthropicServerToolResultBlock),
}

/// A provider-executed tool call (`server_tool_use`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicServerToolUseBlock {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: serde_json::Map<String, Value>,
}

/// A provider-executed tool result block.
///
/// Anthropic gives every server tool its own result block type but they all
/// share the `{tool_use_id, content}` shape, where `content` is provider-typed
/// (a result array for web search, an object for code execution).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicServerToolResultBlock {
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}

/// Text block with optional prompt-cache marker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTextBlock {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

impl AnthropicTextBlock {
    /// Creates a plain text block without cache control.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache_control: None,
        }
    }
}

/// Extended-thinking block (`{"type":"thinking","thinking":...,"signature":...}`).
///
/// `signature` is only required when a thinking block is sent back to the API
/// (thinking + tool use); a response block always carries one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicThinkingBlock {
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Image block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicImageBlock {
    pub source: AnthropicImageSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

/// Image source: base64 payload or remote URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicImageSource {
    /// Inline base64 payload.
    Base64 { media_type: String, data: String },
    /// Remote URL fetched by Anthropic.
    Url { url: String },
}

/// Tool call emitted by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicToolUseBlock {
    pub id: String,
    pub name: String,
    /// Parsed arguments map.
    #[serde(default)]
    pub input: serde_json::Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

/// Tool result block, carried inside a `user` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicToolResultBlock {
    pub tool_use_id: String,
    pub content: Vec<AnthropicToolResultContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

/// Content of a tool result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolResultContent {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    /// Reference to a tool discovered by `tool_search` (deferred loading).
    ToolReference {
        tool_name: String,
    },
}

/// Ephemeral prompt-cache marker (`{"type":"ephemeral"}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicCacheControl {
    #[serde(rename = "type")]
    pub kind: String,
    /// Optional TTL (e.g. `"1h"`); omitted for the default 5-minute ephemeral.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

impl AnthropicCacheControl {
    /// Builds the default ephemeral marker, optionally with a TTL.
    pub fn ephemeral(ttl: Option<&str>) -> Self {
        Self {
            kind: "ephemeral".to_string(),
            ttl: ttl.filter(|t| !t.is_empty()).map(str::to_string),
        }
    }
}

/// Tool declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Marks the definition as deferred: it is only *loadable* through
    /// `tool_search` and a match is returned as a `tool_reference` block
    /// (mirrors the Java SDK's `Tool.deferLoading`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

/// One entry of the request `tools` array: a client tool or a built-in server
/// tool. Mirrors Java's `ToolUnion` (client tool vs. versioned server tool).
///
/// Untagged: a server-tool definition has no `input_schema`, so it cannot be
/// mistaken for [`AnthropicTool`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicToolEntry {
    /// Client-declared tool (name/description/input_schema).
    Client(AnthropicTool),
    /// Anthropic-hosted server tool (versioned `type` + `name`).
    Server(Value),
}

/// Tool-choice configuration. Anthropic nests `disable_parallel_tool_use`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolChoice {
    Auto {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    None,
    Any {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Tool {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Non-streaming `Message` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessageResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    pub usage: AnthropicUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Present on error payloads (`{"type":"error","error":{...}}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AnthropicErrorBody>,
}

/// Error envelope body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicErrorBody {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub message: String,
}

/// Token accounting. Anthropic reports cached tokens separately from
/// `input_tokens`, so callers must add them back (see [`AnthropicUsage::total_input`]).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// Extended-thinking token accounting (`output_tokens_details.thinking_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<AnthropicOutputTokensDetails>,
}

/// Token accounting for the extended-thinking portion of the output.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AnthropicOutputTokensDetails {
    #[serde(default)]
    pub thinking_tokens: u64,
}

impl AnthropicUsage {
    /// `input_tokens` plus cache read/creation tokens (the Java parser's invariant).
    pub fn total_input(&self) -> u64 {
        self.input_tokens
            + self.cache_read_input_tokens.unwrap_or(0)
            + self.cache_creation_input_tokens.unwrap_or(0)
    }

    /// Straightforward read-only cache hits.
    pub fn cached(&self) -> u64 {
        self.cache_read_input_tokens.unwrap_or(0)
    }

    /// Extended-thinking tokens (`output_tokens_details.thinking_tokens`).
    pub fn reasoning(&self) -> u64 {
        self.output_tokens_details
            .map(|details| details.thinking_tokens)
            .unwrap_or(0)
    }
}

/// A single Server-Sent Event frame of the streaming API.
///
/// `type` stays a plain string (mirroring the Java `RawMessageStreamEvent`
/// union): the parser reads the optional sub-objects that frame type carries.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AnthropicStreamEvent {
    #[serde(rename = "type", default)]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<AnthropicMessageResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_block: Option<AnthropicContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<AnthropicStreamDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<AnthropicUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AnthropicErrorBody>,
}

/// `delta` payload of `content_block_delta` / `message_delta` events.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AnthropicStreamDelta {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// `signature_delta`: an incremental fragment of the thinking block's
    /// signature. Anthropic streams the signature in pieces, so the parser
    /// concatenates them instead of overwriting (MODEL-18).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// `input_json_delta`: the partial JSON of a tool call's arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_required_fields_and_omits_absent_ones() {
        let request = AnthropicRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            max_tokens: 4096,
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: vec![AnthropicContentBlock::Text(AnthropicTextBlock::new("hi"))],
            }],
            stream: None,
            system: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking: None,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value["model"],
            serde_json::json!("claude-sonnet-4-5-20250929")
        );
        assert_eq!(value["max_tokens"], serde_json::json!(4096));
        assert!(!value.as_object().unwrap().contains_key("stream"));
        assert!(!value.as_object().unwrap().contains_key("thinking"));
        assert_eq!(
            value["messages"][0]["content"][0]["type"],
            serde_json::json!("text")
        );
    }

    #[test]
    fn system_prompt_supports_string_and_blocks() {
        let text: SystemPrompt = serde_json::from_value(serde_json::json!("be nice")).unwrap();
        assert_eq!(text, SystemPrompt::Text("be nice".into()));

        let blocks: SystemPrompt = serde_json::from_value(serde_json::json!([
            { "type": "text", "text": "be nice", "cache_control": { "type": "ephemeral", "ttl": "1h" } }
        ]))
        .unwrap();
        match blocks {
            SystemPrompt::Blocks(b) => {
                assert_eq!(b[0].text, "be nice");
                assert_eq!(
                    b[0].cache_control.as_ref().unwrap().ttl.as_deref(),
                    Some("1h")
                );
            }
            other => panic!("expected blocks, got {other:?}"),
        }
    }

    #[test]
    fn image_source_is_tagged_by_type() {
        let b64 = serde_json::to_value(AnthropicImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        })
        .unwrap();
        assert_eq!(b64["type"], serde_json::json!("base64"));
        assert_eq!(b64["media_type"], serde_json::json!("image/png"));

        let url = serde_json::to_value(AnthropicImageSource::Url {
            url: "https://x/a.png".into(),
        })
        .unwrap();
        assert_eq!(url["type"], serde_json::json!("url"));
    }

    #[test]
    fn tool_choice_serializes_disable_parallel_flag_only_when_set() {
        let none = serde_json::to_value(AnthropicToolChoice::None).unwrap();
        assert_eq!(none, serde_json::json!({ "type": "none" }));

        let auto = serde_json::to_value(AnthropicToolChoice::Auto {
            disable_parallel_tool_use: Some(true),
        })
        .unwrap();
        assert_eq!(auto["type"], serde_json::json!("auto"));
        assert_eq!(auto["disable_parallel_tool_use"], serde_json::json!(true));

        let specific = serde_json::to_value(AnthropicToolChoice::Tool {
            name: "f".into(),
            disable_parallel_tool_use: None,
        })
        .unwrap();
        assert!(!specific
            .as_object()
            .unwrap()
            .contains_key("disable_parallel_tool_use"));
    }

    #[test]
    fn usage_adds_cache_tokens_back_into_input() {
        let usage = AnthropicUsage {
            input_tokens: 10,
            output_tokens: 3,
            cache_creation_input_tokens: Some(2),
            cache_read_input_tokens: Some(5),
            ..Default::default()
        };
        assert_eq!(usage.total_input(), 17);
        assert_eq!(usage.cached(), 5);
        assert_eq!(usage.reasoning(), 0);
    }

    #[test]
    fn parses_stream_delta_variants() {
        let text: AnthropicStreamEvent = serde_json::from_value(serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "Hel" }
        }))
        .unwrap();
        assert_eq!(text.event_type, "content_block_delta");
        assert_eq!(text.delta.as_ref().unwrap().text.as_deref(), Some("Hel"));

        let json: AnthropicStreamEvent = serde_json::from_value(serde_json::json!({
            "type": "content_block_delta",
            "delta": { "type": "input_json_delta", "partial_json": "{\"a\":" }
        }))
        .unwrap();
        assert_eq!(json.delta.unwrap().partial_json.as_deref(), Some("{\"a\":"));
    }

    #[test]
    fn parses_tool_use_response_block() {
        let response: AnthropicMessageResponse = serde_json::from_value(serde_json::json!({
            "id": "msg_1",
            "content": [
                { "type": "text", "text": "let me check" },
                { "type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": { "city": "Paris" } }
            ],
            "usage": { "input_tokens": 12, "output_tokens": 4, "cache_read_input_tokens": 2 },
            "stop_reason": "tool_use"
        }))
        .unwrap();
        assert_eq!(response.id, "msg_1");
        assert_eq!(response.usage.total_input(), 14);
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
        match &response.content[1] {
            AnthropicContentBlock::ToolUse(t) => {
                assert_eq!(t.name, "get_weather");
                assert_eq!(t.input.get("city"), Some(&serde_json::json!("Paris")));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn parses_error_envelope() {
        let response: AnthropicMessageResponse = serde_json::from_value(serde_json::json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": "bad key" }
        }))
        .unwrap();
        let error = response.error.unwrap();
        assert_eq!(error.kind, "invalid_request_error");
        assert_eq!(error.message, "bad key");
    }
}
