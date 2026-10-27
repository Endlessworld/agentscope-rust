//! Cross-protocol response encoding + a converter-route registry.
//!
//! `agentscope-extensions-model` is a **client** adapter set: every provider
//! converts the internal IR ([`Msg`]/[`ToolSchema`]/[`ChatResponse`]) into its
//! own wire format. To also serve a request *outwards* — e.g. expose an
//! OpenAI-compatible endpoint backed by a Claude/Gemini upstream — the reverse
//! direction is needed: encoding an IR [`ChatResponse`] back into a client
//! protocol.
//!
//! Because the IR sits in the middle, the full N×N matrix is covered by four
//! codecs (two directions each) rather than N×N hand-written converters. This
//! module provides the **outbound** half ([`encode_response`]) plus a
//! declarative route registry ([`BUILTIN_CONVERTERS`]) with quality grades and
//! explicit multi-hop paths, mirroring new-api's converter registry.

use agentscope_core::message::ContentBlock;
use agentscope_core::model::ChatResponse;

/// A client-facing protocol the gateway can encode a response into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayFormat {
    /// OpenAI Chat Completions.
    OpenAiChat,
    /// OpenAI Responses.
    OpenAiResponses,
    /// Anthropic Messages.
    ClaudeMessages,
    /// Google Gemini `generateContent`.
    Gemini,
}

impl RelayFormat {
    /// Stable wire identifier (also used in `ConverterSpec::id`).
    pub fn as_str(&self) -> &'static str {
        match self {
            RelayFormat::OpenAiChat => "openai_chat",
            RelayFormat::OpenAiResponses => "openai_responses",
            RelayFormat::ClaudeMessages => "claude_messages",
            RelayFormat::Gemini => "gemini_generate_content",
        }
    }
}

/// How lossy a protocol conversion is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConverterQuality {
    /// No meaningful loss.
    Good,
    /// Some fields may be dropped or approximated.
    Fair,
    /// Known to lose important information; prefer another route.
    Discouraged,
}

/// A declarative conversion route between two client protocols.
#[derive(Debug, Clone, Copy)]
pub struct ConverterSpec {
    /// Stable id, e.g. `claude_messages_to_openai_chat`.
    pub id: &'static str,
    /// Source protocol.
    pub from: RelayFormat,
    /// Target protocol.
    pub to: RelayFormat,
    /// Loss assessment.
    pub quality: ConverterQuality,
    /// Declared hop formats (inclusive of `from` and `to`). More than two means
    /// a multi-hop conversion through the IR.
    pub steps: &'static [RelayFormat],
}

const fn spec(
    id: &'static str,
    from: RelayFormat,
    to: RelayFormat,
    quality: ConverterQuality,
    steps: &'static [RelayFormat],
) -> ConverterSpec {
    ConverterSpec {
        id,
        from,
        to,
        quality,
        steps,
    }
}

/// The built-in route table. Because the internal IR is the pivot, Claude↔Gemini
/// and the Responses edges route through OpenAI Chat rather than being
/// hand-written, so they are graded conservatively.
pub const BUILTIN_CONVERTERS: &[ConverterSpec] = &[
    spec(
        "openai_chat_to_claude_messages",
        RelayFormat::OpenAiChat,
        RelayFormat::ClaudeMessages,
        ConverterQuality::Fair,
        &[RelayFormat::OpenAiChat, RelayFormat::ClaudeMessages],
    ),
    spec(
        "claude_messages_to_openai_chat",
        RelayFormat::ClaudeMessages,
        RelayFormat::OpenAiChat,
        ConverterQuality::Good,
        &[RelayFormat::ClaudeMessages, RelayFormat::OpenAiChat],
    ),
    spec(
        "openai_chat_to_openai_responses",
        RelayFormat::OpenAiChat,
        RelayFormat::OpenAiResponses,
        ConverterQuality::Good,
        &[RelayFormat::OpenAiChat, RelayFormat::OpenAiResponses],
    ),
    spec(
        "openai_responses_to_openai_chat",
        RelayFormat::OpenAiResponses,
        RelayFormat::OpenAiChat,
        ConverterQuality::Good,
        &[RelayFormat::OpenAiResponses, RelayFormat::OpenAiChat],
    ),
    spec(
        "openai_chat_to_gemini",
        RelayFormat::OpenAiChat,
        RelayFormat::Gemini,
        ConverterQuality::Fair,
        &[RelayFormat::OpenAiChat, RelayFormat::Gemini],
    ),
    spec(
        "gemini_to_openai_chat",
        RelayFormat::Gemini,
        RelayFormat::OpenAiChat,
        ConverterQuality::Fair,
        &[RelayFormat::Gemini, RelayFormat::OpenAiChat],
    ),
    spec(
        "claude_messages_to_gemini",
        RelayFormat::ClaudeMessages,
        RelayFormat::Gemini,
        ConverterQuality::Discouraged,
        &[
            RelayFormat::ClaudeMessages,
            RelayFormat::OpenAiChat,
            RelayFormat::Gemini,
        ],
    ),
    spec(
        "gemini_to_claude_messages",
        RelayFormat::Gemini,
        RelayFormat::ClaudeMessages,
        ConverterQuality::Discouraged,
        &[
            RelayFormat::Gemini,
            RelayFormat::OpenAiChat,
            RelayFormat::ClaudeMessages,
        ],
    ),
];

/// Looks up the route (if any) from `from` to `to`.
pub fn find_route(from: RelayFormat, to: RelayFormat) -> Option<&'static ConverterSpec> {
    BUILTIN_CONVERTERS
        .iter()
        .find(|s| s.from == from && s.to == to)
}

/// Encodes an IR [`ChatResponse`] into `format`'s **non-streaming** response body.
///
/// Returns the JSON the client protocol expects. Streaming encoding of the same
/// blocks can be layered on top by chunking the returned content array.
pub fn encode_response(
    format: RelayFormat,
    response: &ChatResponse,
    model: &str,
) -> serde_json::Value {
    match format {
        RelayFormat::OpenAiChat => encode_openai_chat(response, model),
        RelayFormat::ClaudeMessages => encode_claude_messages(response, model),
        RelayFormat::Gemini => encode_gemini(response),
        RelayFormat::OpenAiResponses => encode_openai_responses(response, model),
    }
}

/// The terminator sentinel a streaming client expects (`[DONE]` for OpenAI
/// dialects; Claude/Gemini close on their own `message_stop`/final chunk).
pub fn stream_terminator(format: RelayFormat) -> Option<&'static str> {
    match format {
        RelayFormat::OpenAiChat | RelayFormat::OpenAiResponses => Some("[DONE]"),
        RelayFormat::ClaudeMessages | RelayFormat::Gemini => None,
    }
}

/// Encodes an error into the client protocol's error envelope.
pub fn encode_error(format: RelayFormat, message: &str, error_type: &str) -> serde_json::Value {
    match format {
        RelayFormat::ClaudeMessages => serde_json::json!({
            "type": "error",
            "error": { "type": error_type, "message": message }
        }),
        RelayFormat::Gemini => serde_json::json!({
            "error": { "code": 0, "status": error_type, "message": message }
        }),
        _ => serde_json::json!({
            "error": { "message": message, "type": error_type, "code": serde_json::Value::Null }
        }),
    }
}

fn finish_reason(response: &ChatResponse, claude: bool, gemini: bool) -> String {
    let raw = response.finish_reason.as_deref().unwrap_or("stop");
    if gemini {
        return match raw {
            "length" => "MAX_TOKENS".into(),
            "tool_calls" | "function_call" => "STOP".into(),
            "content_filter" => "SAFETY".into(),
            _ => "STOP".into(),
        };
    }
    if claude {
        return match raw {
            "length" => "max_tokens".into(),
            "tool_calls" | "function_call" => "tool_use".into(),
            _ => "end_turn".into(),
        };
    }
    raw.to_string()
}

fn text_of(response: &ChatResponse) -> String {
    response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn tool_uses(response: &ChatResponse) -> Vec<&agentscope_core::message::ToolUseBlock> {
    response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse(t) => Some(t),
            _ => None,
        })
        .collect()
}

fn encode_openai_chat(response: &ChatResponse, model: &str) -> serde_json::Value {
    let tool_calls: Vec<serde_json::Value> = tool_uses(response)
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "type": "function",
                "function": {
                    "name": t.name,
                    "arguments": serde_json::to_string(&t.input).unwrap_or_else(|_| "{}".into()),
                }
            })
        })
        .collect();
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": text_of(response),
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = serde_json::Value::Array(tool_calls);
    }
    serde_json::json!({
        "id": response.id,
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason(response, false, false),
        }],
        "usage": {
            "prompt_tokens": response.usage.input_tokens,
            "completion_tokens": response.usage.output_tokens,
            "total_tokens": response.usage.total_tokens(),
        }
    })
}

fn encode_claude_messages(response: &ChatResponse, model: &str) -> serde_json::Value {
    let mut content: Vec<serde_json::Value> = Vec::new();
    let text = text_of(response);
    if !text.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": text}));
    }
    for t in tool_uses(response) {
        content.push(serde_json::json!({
            "type": "tool_use",
            "id": t.id,
            "name": t.name,
            "input": serde_json::Value::Object(t.input.clone()),
        }));
    }
    serde_json::json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": finish_reason(response, true, false),
        "usage": {
            "input_tokens": response.usage.input_tokens,
            "output_tokens": response.usage.output_tokens,
        }
    })
}

fn encode_gemini(response: &ChatResponse) -> serde_json::Value {
    let mut parts: Vec<serde_json::Value> = Vec::new();
    let text = text_of(response);
    if !text.is_empty() {
        parts.push(serde_json::json!({"text": text}));
    }
    for t in tool_uses(response) {
        parts.push(serde_json::json!({
            "functionCall": {"name": t.name, "args": serde_json::Value::Object(t.input.clone())}
        }));
    }
    serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": finish_reason(response, false, true),
        }],
        "usageMetadata": {
            "promptTokenCount": response.usage.input_tokens,
            "candidatesTokenCount": response.usage.output_tokens,
            "totalTokenCount": response.usage.total_tokens(),
        }
    })
}

fn encode_openai_responses(response: &ChatResponse, model: &str) -> serde_json::Value {
    let mut output: Vec<serde_json::Value> = Vec::new();
    let text = text_of(response);
    if !text.is_empty() {
        output.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }));
    }
    for t in tool_uses(response) {
        output.push(serde_json::json!({
            "type": "function_call",
            "call_id": t.id,
            "name": t.name,
            "arguments": serde_json::to_string(&t.input).unwrap_or_else(|_| "{}".into()),
        }));
    }
    serde_json::json!({
        "id": response.id,
        "object": "response",
        "model": model,
        "status": "completed",
        "output": output,
        "usage": {
            "input_tokens": response.usage.input_tokens,
            "output_tokens": response.usage.output_tokens,
            "total_tokens": response.usage.total_tokens(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{ChatUsage, JsonMap, TextBlock, ToolUseBlock};

    fn sample_response() -> ChatResponse {
        let input: JsonMap = serde_json::from_str(r#"{"city":"Beijing"}"#).unwrap();
        ChatResponse::new(
            "resp_1".to_string(),
            vec![
                ContentBlock::Text(TextBlock::new("hello")),
                ContentBlock::ToolUse(ToolUseBlock::new("call_1", "get_weather", input)),
            ],
            ChatUsage {
                input_tokens: 11,
                output_tokens: 4,
                cached_tokens: 0,
                time: 0.0,
                ..Default::default()
            },
            None,
            Some("tool_calls".to_string()),
        )
    }

    #[test]
    fn openai_encoding_has_tool_calls_and_usage() {
        let body = encode_response(RelayFormat::OpenAiChat, &sample_response(), "gpt-4o");
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "hello");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(body["usage"]["total_tokens"], 15);
    }

    #[test]
    fn claude_encoding_uses_content_blocks() {
        let body = encode_response(RelayFormat::ClaudeMessages, &sample_response(), "claude");
        assert_eq!(body["type"], "message");
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][1]["type"], "tool_use");
        assert_eq!(body["stop_reason"], "tool_use");
        assert_eq!(body["usage"]["input_tokens"], 11);
    }

    #[test]
    fn gemini_encoding_uses_parts_and_finish_reason() {
        let body = encode_response(RelayFormat::Gemini, &sample_response(), "gemini");
        assert_eq!(
            body["candidates"][0]["content"]["parts"][0]["text"],
            "hello"
        );
        assert_eq!(
            body["candidates"][0]["content"]["parts"][1]["functionCall"]["name"],
            "get_weather"
        );
        assert_eq!(body["usageMetadata"]["totalTokenCount"], 15);
    }

    #[test]
    fn route_registry_covers_edges_and_multi_hop() {
        assert_eq!(
            find_route(RelayFormat::ClaudeMessages, RelayFormat::OpenAiChat)
                .unwrap()
                .quality,
            ConverterQuality::Good
        );
        let multi = find_route(RelayFormat::ClaudeMessages, RelayFormat::Gemini).unwrap();
        assert_eq!(multi.steps.len(), 3, "claude -> openai -> gemini");
        assert_eq!(multi.quality, ConverterQuality::Discouraged);
        assert!(find_route(RelayFormat::Gemini, RelayFormat::ClaudeMessages).is_some());
    }

    #[test]
    fn terminators_and_errors_match_protocol() {
        assert_eq!(stream_terminator(RelayFormat::OpenAiChat), Some("[DONE]"));
        assert_eq!(stream_terminator(RelayFormat::ClaudeMessages), None);
        let err = encode_error(RelayFormat::ClaudeMessages, "boom", "overloaded_error");
        assert_eq!(err["type"], "error");
        assert_eq!(err["error"]["message"], "boom");
    }
}
