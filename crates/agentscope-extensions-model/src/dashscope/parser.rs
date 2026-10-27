//! Parses DashScope responses into AgentScope [`ChatResponse`]s.
//!
//! Mirrors `DashScopeResponseParser` and adds an incremental stream
//! accumulator modelled on core's OpenAI `StreamIncrementer`.

use agentscope_core::{
    ChatResponse, ChatUsage, ContentBlock, JsonMap, TextBlock, ThinkingBlock, ToolUseBlock,
};
use uuid::Uuid;

use super::dto::{DashScopeMessage, DashScopeResponse};

/// Placeholder tool name used for streamed argument fragments that arrive
/// without name/id metadata (same sentinel as Java).
pub const FRAGMENT_PLACEHOLDER: &str = "__fragment__";

fn synthetic_id(prefix: &str, index: usize) -> String {
    format!("tool_call_{prefix}_{index}")
}

/// Extracts text from a `DashScopeMessage` (string content or joined text parts).
fn message_text(message: &DashScopeMessage) -> Option<String> {
    message
        .content
        .as_ref()
        .map(|c| c.text_joined())
        .filter(|s| !s.is_empty())
}

/// Appends tool-use blocks derived from `message.tool_calls`.
fn add_tool_calls(message: &DashScopeMessage, blocks: &mut Vec<ContentBlock>) {
    let Some(calls) = &message.tool_calls else {
        return;
    };
    for (index, call) in calls.iter().enumerate() {
        let Some(function) = &call.function else {
            continue;
        };
        let name = function.name.trim();
        let args = function.arguments.clone();
        let id = call
            .id
            .clone()
            .unwrap_or_else(|| synthetic_id("call", index));

        if !name.is_empty() {
            // First chunk: complete metadata (name + id + partial arguments).
            let mut block = ToolUseBlock::new(id, name, JsonMap::new());
            block.content = args.clone();
            blocks.push(ContentBlock::ToolUse(block));
        } else if let Some(args) = args {
            // Subsequent chunks: argument fragments only (no name/id). A fragment
            // must not carry a synthesized id of its own — the accumulator merges
            // a continuation chunk into its owning tool call when the id is empty
            // (same contract as the Java/OpenAI/Anthropic parsers). A per-chunk
            // synthetic id would strand the fragment in its own nameless call.
            let mut block = ToolUseBlock::new(
                call.id.clone().unwrap_or_default(),
                FRAGMENT_PLACEHOLDER,
                JsonMap::new(),
            );
            block.content = Some(args);
            blocks.push(ContentBlock::ToolUse(block));
        }
    }
}

/// Merges a streamed tool-call fragment into the accumulated calls.
///
/// A named block (the first chunk of a call) starts or extends the call with the
/// matching id. A fragment (placeholder name) whose id matches an existing call
/// appends its arguments there; when the id is empty — DashScope omits it on
/// continuation chunks — the fragment appends to the most recent named call,
/// mirroring the OpenAI/Anthropic accumulators. Arguments are concatenated in
/// arrival order rather than overwriting each other.
fn merge_tool_block(calls: &mut Vec<ToolUseBlock>, block: ToolUseBlock) {
    let is_fragment = block.name == FRAGMENT_PLACEHOLDER;
    let target = if is_fragment {
        calls
            .iter_mut()
            .rev()
            .find(|c| c.name != FRAGMENT_PLACEHOLDER && (block.id.is_empty() || c.id == block.id))
    } else {
        calls.iter_mut().find(|c| c.id == block.id)
    };
    match target {
        Some(existing) => {
            if let Some(args) = block.content {
                match &mut existing.content {
                    Some(buf) => buf.push_str(&args),
                    None => existing.content = Some(args),
                }
            }
        }
        None => calls.push(block),
    }
}

fn usage_of(response: &DashScopeResponse) -> ChatUsage {
    match &response.usage {
        Some(u) => ChatUsage {
            input_tokens: u.input_tokens.unwrap_or(0).max(0) as u64,
            output_tokens: u.output_tokens.unwrap_or(0).max(0) as u64,
            cached_tokens: 0,
            time: 0.0,
            ..Default::default()
        },
        None => ChatUsage::default(),
    }
}

/// Parses a single (non-streaming or terminal) DashScope response.
///
/// Block order mirrors Java: thinking → text → tool calls.
pub fn parse_response(response: &DashScopeResponse) -> ChatResponse {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut finish_reason: Option<String> = None;

    if let Some(output) = &response.output {
        if let Some(choice) = output.first_choice() {
            if let Some(message) = &choice.message {
                if let Some(reasoning) = &message.reasoning_content {
                    if !reasoning.is_empty() {
                        blocks.push(ContentBlock::Thinking(ThinkingBlock::new(
                            reasoning.clone(),
                        )));
                    }
                }
                if let Some(text) = message_text(message) {
                    blocks.push(ContentBlock::Text(TextBlock::new(text)));
                }
                add_tool_calls(message, &mut blocks);
            }
            finish_reason = choice.finish_reason.clone();
        }
        if finish_reason.is_none() {
            finish_reason = output.finish_reason.clone();
        }
    }

    let id = response
        .request_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    ChatResponse::new(id, blocks, usage_of(response), None, finish_reason)
}

/// Accumulates streamed DashScope frames.
///
/// Text/thinking deltas are emitted as they arrive; tool calls, usage and the
/// finish reason are buffered and flushed in a single terminal response.
#[derive(Default)]
pub struct DashScopeStreamIncrementer {
    id: Option<String>,
    /// Accumulated tool calls, in arrival order. Fragments are merged into their
    /// owning call (by id, or the most recent named call when the id is empty)
    /// rather than keyed and overwritten by a synthesized per-chunk id.
    fragments: Vec<ToolUseBlock>,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

impl DashScopeStreamIncrementer {
    /// Creates an empty incrementer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes one frame, returning an incremental response for new text or
    /// thinking content.
    pub fn accumulate(&mut self, response: &DashScopeResponse) -> Option<ChatResponse> {
        if self.id.is_none() {
            self.id = response.request_id.clone().filter(|s| !s.trim().is_empty());
        }
        if response.usage.is_some() {
            let incoming = usage_of(response);
            self.usage = Some(crate::common::usage::merge_non_zero(
                &self.usage.unwrap_or_default(),
                &incoming,
            ));
        }

        let mut blocks: Vec<ContentBlock> = Vec::new();
        if let Some(output) = &response.output {
            if let Some(choice) = output.first_choice() {
                if let Some(message) = &choice.message {
                    if let Some(reasoning) = &message.reasoning_content {
                        if !reasoning.is_empty() {
                            blocks.push(ContentBlock::Thinking(ThinkingBlock::new(
                                reasoning.clone(),
                            )));
                        }
                    }
                    if let Some(text) = message_text(message) {
                        blocks.push(ContentBlock::Text(TextBlock::new(text)));
                    }
                    // Buffer tool-call fragments; flush them in `into_final`.
                    let mut tool_blocks = Vec::new();
                    add_tool_calls(message, &mut tool_blocks);
                    for block in tool_blocks {
                        if let ContentBlock::ToolUse(t) = block {
                            merge_tool_block(&mut self.fragments, t);
                        }
                    }
                }
                if let Some(reason) = &choice.finish_reason {
                    self.finish_reason = Some(reason.clone());
                }
            }
            if self.finish_reason.is_none() {
                self.finish_reason = output.finish_reason.clone();
            }
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

    /// Flushes buffered tool calls / usage / finish reason at end of stream.
    pub fn into_final(self) -> Option<ChatResponse> {
        if self.fragments.is_empty() && self.usage.is_none() && self.finish_reason.is_none() {
            return None;
        }
        let content: Vec<ContentBlock> = self
            .fragments
            .into_iter()
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

    fn resp(value: serde_json::Value) -> DashScopeResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn parses_thinking_text_and_usage_in_order() {
        let response = resp(serde_json::json!({
            "request_id": "req-1",
            "output": {
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "role": "assistant",
                        "reasoning_content": "let me think",
                        "content": "Hello!"
                    }
                }]
            },
            "usage": { "input_tokens": 7, "output_tokens": 3, "total_tokens": 10 }
        }));
        let parsed = parse_response(&response);
        assert_eq!(parsed.id, "req-1");
        assert!(
            matches!(&parsed.content[0], ContentBlock::Thinking(t) if t.thinking == "let me think")
        );
        assert!(matches!(&parsed.content[1], ContentBlock::Text(t) if t.text == "Hello!"));
        assert_eq!(parsed.usage.input_tokens, 7);
        assert_eq!(parsed.usage.output_tokens, 3);
        assert_eq!(parsed.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn parses_tool_calls_from_function_arguments_string() {
        let response = resp(serde_json::json!({
            "request_id": "req-2",
            "output": {
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_abc",
                            "type": "function",
                            "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" }
                        }]
                    }
                }]
            }
        }));
        let parsed = parse_response(&response);
        assert_eq!(parsed.content.len(), 1);
        match &parsed.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.id, "call_abc");
                assert_eq!(t.name, "get_weather");
                assert_eq!(t.content.as_deref(), Some("{\"city\":\"Paris\"}"));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn output_level_finish_reason_is_fallback() {
        let response = resp(serde_json::json!({
            "output": {
                "finish_reason": "length",
                "choices": [{ "message": { "content": "x" } }]
            }
        }));
        assert_eq!(
            parse_response(&response).finish_reason.as_deref(),
            Some("length")
        );
    }

    #[test]
    fn multimodal_content_parts_are_joined() {
        let response = resp(serde_json::json!({
            "output": {
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": [{ "text": "part one" }, { "text": "part two" }]
                    }
                }]
            }
        }));
        let parsed = parse_response(&response);
        assert!(
            matches!(&parsed.content[0], ContentBlock::Text(t) if t.text == "part one\npart two")
        );
    }

    #[test]
    fn missing_request_id_generates_one() {
        let parsed = parse_response(&resp(serde_json::json!({ "output": {} })));
        assert!(!parsed.id.is_empty());
    }

    #[test]
    fn stream_incrementer_emits_text_then_flushes_tools() {
        let mut inc = DashScopeStreamIncrementer::new();
        let a = resp(serde_json::json!({
            "request_id": "req-3",
            "output": { "choices": [{ "message": { "content": "Hel" } }] }
        }));
        let b = resp(serde_json::json!({
            "output": { "choices": [{ "message": { "content": "lo" } }] }
        }));
        let c = resp(serde_json::json!({
            "output": { "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "f", "arguments": "{}" }
                    }]
                }
            }] },
            "usage": { "input_tokens": 1, "output_tokens": 2 }
        }));

        assert!(
            matches!(&inc.accumulate(&a).unwrap().content[0], ContentBlock::Text(t) if t.text == "Hel")
        );
        assert!(
            matches!(&inc.accumulate(&b).unwrap().content[0], ContentBlock::Text(t) if t.text == "lo")
        );
        assert!(inc.accumulate(&c).is_none());

        let final_response = inc.into_final().unwrap();
        assert_eq!(final_response.usage.input_tokens, 1);
        assert_eq!(final_response.usage.output_tokens, 2);
        assert_eq!(final_response.finish_reason.as_deref(), Some("tool_calls"));
        assert!(matches!(&final_response.content[0], ContentBlock::ToolUse(t) if t.name == "f"));
    }

    #[test]
    fn fragment_chunks_use_placeholder_name() {
        let response = resp(serde_json::json!({
            "output": { "choices": [{ "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": { "name": "", "arguments": "{\"a\"" }
                }]
            } }] }
        }));
        let parsed = parse_response(&response);
        match &parsed.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.name, FRAGMENT_PLACEHOLDER);
                assert_eq!(t.id, "call_1");
                assert_eq!(t.content.as_deref(), Some("{\"a\""));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn fragment_without_id_does_not_synthesize_one() {
        // A continuation chunk that omits the id must not invent one; the empty
        // id is the contract the accumulator relies on to merge into its owner.
        let response = resp(serde_json::json!({
            "output": { "choices": [{ "message": {
                "content": "",
                "tool_calls": [{
                    "function": { "name": "", "arguments": "\"Paris\"}" }
                }]
            } }] }
        }));
        let parsed = parse_response(&response);
        match &parsed.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.name, FRAGMENT_PLACEHOLDER);
                assert!(
                    t.id.is_empty(),
                    "fragment id must stay empty, got {:?}",
                    t.id
                );
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn streamed_fragments_merge_into_their_owning_call() {
        // Fully realistic stream: first chunk carries id+name+partial args,
        // continuation chunks carry only argument fragments (id omitted).
        let mut inc = DashScopeStreamIncrementer::new();
        let c1 = resp(serde_json::json!({
            "request_id": "req-m",
            "output": { "choices": [{ "message": {
                "content": "",
                "tool_calls": [{
                    "id": "call_abc",
                    "function": { "name": "get_weather", "arguments": "{\"ci" }
                }]
            } }] }
        }));
        let c2 = resp(serde_json::json!({
            "output": { "choices": [{ "message": {
                "content": "",
                "tool_calls": [{
                    "function": { "name": "", "arguments": "ty\":\"Pa" }
                }]
            } }] }
        }));
        let c3 = resp(serde_json::json!({
            "output": { "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "function": { "name": "", "arguments": "ris\"}" }
                    }]
                }
            }] }
        }));

        assert!(inc.accumulate(&c1).is_none());
        assert!(inc.accumulate(&c2).is_none());
        assert!(inc.accumulate(&c3).is_none());

        let final_response = inc.into_final().unwrap();
        assert_eq!(
            final_response.content.len(),
            1,
            "one merged tool call expected"
        );
        match &final_response.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.id, "call_abc");
                assert_eq!(t.name, "get_weather");
                assert_eq!(t.content.as_deref(), Some("{\"city\":\"Paris\"}"));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn parallel_streamed_calls_are_merged_by_id() {
        // Two interleaved tool calls must stay distinct and each accumulate its
        // own fragments (id present on continuation chunks disambiguates them).
        let mut inc = DashScopeStreamIncrementer::new();
        let a1 = resp(serde_json::json!({
            "output": { "choices": [{ "message": { "tool_calls": [
                { "id": "call_a", "function": { "name": "f", "arguments": "{\"x\":" } },
                { "id": "call_b", "function": { "name": "g", "arguments": "{\"y\":" } }
            ] } }] }
        }));
        let a2 = resp(serde_json::json!({
            "output": { "choices": [{ "message": { "tool_calls": [
                { "id": "call_b", "function": { "name": "", "arguments": "2}" } },
                { "id": "call_a", "function": { "name": "", "arguments": "1}" } }
            ] } }] }
        }));

        assert!(inc.accumulate(&a1).is_none());
        assert!(inc.accumulate(&a2).is_none());

        let final_response = inc.into_final().unwrap();
        let mut calls: Vec<(String, Option<String>)> = final_response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse(t) => Some((t.id.clone(), t.content.clone())),
                _ => None,
            })
            .collect();
        calls.sort();
        assert_eq!(
            calls,
            vec![
                ("call_a".to_string(), Some("{\"x\":1}".to_string())),
                ("call_b".to_string(), Some("{\"y\":2}".to_string())),
            ]
        );
    }

    #[test]
    fn error_response_surfaces_code_and_message() {
        let response = resp(serde_json::json!({
            "code": "InvalidApiKey",
            "message": "bad key"
        }));
        // No output → empty content, no panic; code/message remain on the DTO.
        let parsed = parse_response(&response);
        assert!(parsed.content.is_empty());
        assert_eq!(response.code.as_deref(), Some("InvalidApiKey"));
    }
}
