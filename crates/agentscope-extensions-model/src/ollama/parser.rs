//! Parses Ollama responses into AgentScope [`ChatResponse`]s.
//!
//! Mirrors `OllamaResponseParser` and adds an incremental stream accumulator
//! modelled on core's OpenAI `StreamIncrementer`.

use std::collections::BTreeMap;

use agentscope_core::{ChatResponse, ChatUsage, ContentBlock, JsonMap, TextBlock, ToolUseBlock};
use uuid::Uuid;

use super::dto::{OllamaFunction, OllamaResponse};

fn function_input(function: &OllamaFunction) -> JsonMap {
    match &function.arguments {
        Some(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        None => JsonMap::new(),
    }
}

/// Parses a single (non-streaming or final) Ollama response.
pub fn parse_response(response: &OllamaResponse) -> ChatResponse {
    let mut content: Vec<ContentBlock> = Vec::new();

    if let Some(message) = &response.message {
        if let Some(text) = &message.content {
            if !text.is_empty() {
                content.push(ContentBlock::Text(TextBlock::new(text.clone())));
            }
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                let input = function_input(&call.function);
                let raw = if input.is_empty() {
                    "{}".to_string()
                } else {
                    serde_json::Value::Object(input.clone()).to_string()
                };
                // Ollama does not return a tool-call id, so we synthesise one
                // (same behaviour as the Java parser).
                let mut block = ToolUseBlock::new(
                    Uuid::new_v4().to_string(),
                    call.function.name.clone(),
                    input,
                );
                block.content = Some(raw);
                content.push(ContentBlock::ToolUse(block));
            }
        }
    }

    // Ollama reports durations in nanoseconds.
    let time = response
        .total_duration
        .map(|d| d as f64 / 1e9)
        .unwrap_or(0.0);
    let usage = ChatUsage {
        input_tokens: response.prompt_eval_count.unwrap_or(0).max(0) as u64,
        output_tokens: response.eval_count.unwrap_or(0).max(0) as u64,
        cached_tokens: 0,
        time,
        ..Default::default()
    };

    let mut metadata = JsonMap::new();
    let mut put = |key: &str, value: serde_json::Value| {
        metadata.insert(key.to_string(), value);
    };
    if let Some(model) = &response.model {
        put("model", serde_json::json!(model));
    }
    if let Some(v) = &response.created_at {
        put("created_at", serde_json::json!(v));
    }
    if let Some(v) = response.total_duration {
        put("total_duration", serde_json::json!(v));
    }
    if let Some(v) = response.load_duration {
        put("load_duration", serde_json::json!(v));
    }
    if let Some(v) = response.prompt_eval_count {
        put("prompt_eval_count", serde_json::json!(v));
    }
    if let Some(v) = response.prompt_eval_duration {
        put("prompt_eval_duration", serde_json::json!(v));
    }
    if let Some(v) = response.eval_count {
        put("eval_count", serde_json::json!(v));
    }
    if let Some(v) = response.eval_duration {
        put("eval_duration", serde_json::json!(v));
    }
    if let Some(v) = response.done {
        put("done", serde_json::json!(v));
    }

    let finish_reason = response.done_reason.clone().or_else(|| {
        if response.done == Some(true) {
            Some("stop".to_string())
        } else {
            None
        }
    });

    ChatResponse::new(
        Uuid::new_v4().to_string(),
        content,
        usage,
        Some(metadata),
        finish_reason,
    )
}

/// Accumulates streamed Ollama frames.
///
/// Text content is emitted incrementally (one [`ChatResponse`] per non-empty
/// delta); tool calls, usage and finish reason are accumulated and flushed as a
/// single terminal [`ChatResponse`].
#[derive(Default)]
pub struct OllamaStreamIncrementer {
    id: Option<String>,
    tool_calls: BTreeMap<usize, ToolUseBlock>,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

impl OllamaStreamIncrementer {
    /// Creates an empty incrementer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes one frame, returning an incremental response when the frame
    /// carries new text/thinking content.
    pub fn accumulate(&mut self, response: &OllamaResponse) -> Option<ChatResponse> {
        if self.id.is_none() {
            self.id = Some(Uuid::new_v4().to_string());
        }
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if let Some(message) = &response.message {
            if let Some(text) = &message.content {
                if !text.is_empty() {
                    blocks.push(ContentBlock::Text(TextBlock::new(text.clone())));
                }
            }
            if let Some(calls) = &message.tool_calls {
                for (index, call) in calls.iter().enumerate() {
                    let input = function_input(&call.function);
                    self.tool_calls.insert(
                        index,
                        ToolUseBlock::new(
                            Uuid::new_v4().to_string(),
                            call.function.name.clone(),
                            input,
                        ),
                    );
                }
            }
        }
        if response.prompt_eval_count.is_some() || response.eval_count.is_some() {
            let incoming = ChatUsage {
                input_tokens: response.prompt_eval_count.unwrap_or(0).max(0) as u64,
                output_tokens: response.eval_count.unwrap_or(0).max(0) as u64,
                cached_tokens: 0,
                time: response
                    .total_duration
                    .map(|d| d as f64 / 1e9)
                    .unwrap_or(0.0),
                ..Default::default()
            };
            self.usage = Some(crate::common::usage::merge_non_zero(
                &self.usage.unwrap_or_default(),
                &incoming,
            ));
        }
        if let Some(reason) = &response.done_reason {
            self.finish_reason = Some(reason.clone());
        } else if response.done == Some(true) {
            self.finish_reason = Some("stop".to_string());
        }

        if blocks.is_empty() {
            None
        } else {
            Some(ChatResponse::new(
                self.id.clone().unwrap_or_default(),
                blocks,
                ChatUsage::default(),
                None,
                None,
            ))
        }
    }

    /// Flushes accumulated tool calls / usage / finish reason at end of stream.
    pub fn into_final(self) -> Option<ChatResponse> {
        if self.tool_calls.is_empty() && self.usage.is_none() && self.finish_reason.is_none() {
            return None;
        }
        let content: Vec<ContentBlock> = self
            .tool_calls
            .into_values()
            .map(ContentBlock::ToolUse)
            .collect();
        Some(ChatResponse::new(
            self.id.unwrap_or_default(),
            content,
            self.usage.unwrap_or_default(),
            None,
            self.finish_reason,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp_json(value: serde_json::Value) -> OllamaResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn parses_text_and_usage_and_metadata() {
        let response = resp_json(serde_json::json!({
            "model": "llama3",
            "created_at": "2024-01-01T00:00:00Z",
            "message": { "role": "assistant", "content": "Hello!" },
            "done": true,
            "done_reason": "stop",
            "total_duration": 2_000_000_000_i64,
            "prompt_eval_count": 10,
            "eval_count": 20
        }));
        let parsed = parse_response(&response);
        assert_eq!(parsed.content.len(), 1);
        assert!(matches!(&parsed.content[0], ContentBlock::Text(t) if t.text == "Hello!"));
        assert_eq!(parsed.usage.input_tokens, 10);
        assert_eq!(parsed.usage.output_tokens, 20);
        assert_eq!(parsed.usage.time, 2.0);
        assert_eq!(parsed.finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            parsed.metadata.get("model"),
            Some(&serde_json::json!("llama3"))
        );
        assert_eq!(parsed.metadata.get("done"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn parses_tool_calls_with_generated_id() {
        let response = resp_json(serde_json::json!({
            "done": true,
            "message": {
                "role": "assistant",
                "tool_calls": [
                    { "function": { "name": "get_weather", "arguments": { "city": "Paris" } } }
                ]
            }
        }));
        let parsed = parse_response(&response);
        match &parsed.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.name, "get_weather");
                assert_eq!(t.input.get("city"), Some(&serde_json::json!("Paris")));
                assert_eq!(t.content.as_deref(), Some("{\"city\":\"Paris\"}"));
                assert!(!t.id.is_empty());
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn done_true_without_reason_falls_back_to_stop() {
        let response = resp_json(serde_json::json!({ "done": true }));
        assert_eq!(
            parse_response(&response).finish_reason.as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn stream_incrementer_emits_text_then_flushes_tools_and_usage() {
        let mut inc = OllamaStreamIncrementer::new();
        let a = resp_json(serde_json::json!({ "message": { "content": "Hel" }, "done": false }));
        let b = resp_json(serde_json::json!({ "message": { "content": "lo" }, "done": false }));
        let c = resp_json(serde_json::json!({
            "message": {
                "content": "",
                "tool_calls": [{ "function": { "name": "f", "arguments": {} } }]
            },
            "done": true,
            "prompt_eval_count": 3,
            "eval_count": 5
        }));

        assert!(
            matches!(&inc.accumulate(&a).unwrap().content[0], ContentBlock::Text(t) if t.text == "Hel")
        );
        assert!(
            matches!(&inc.accumulate(&b).unwrap().content[0], ContentBlock::Text(t) if t.text == "lo")
        );
        // The final frame carries no incremental text.
        assert!(inc.accumulate(&c).is_none());

        let final_response = inc.into_final().unwrap();
        assert_eq!(final_response.content.len(), 1);
        assert!(matches!(&final_response.content[0], ContentBlock::ToolUse(t) if t.name == "f"));
        assert_eq!(final_response.usage.input_tokens, 3);
        assert_eq!(final_response.usage.output_tokens, 5);
        assert_eq!(final_response.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn stream_incrementer_returns_none_when_nothing_to_flush() {
        let mut inc = OllamaStreamIncrementer::new();
        let frame = resp_json(serde_json::json!({ "message": { "content": "x" }, "done": false }));
        assert!(inc.accumulate(&frame).is_some());
        assert!(inc.into_final().is_none());
    }
}
