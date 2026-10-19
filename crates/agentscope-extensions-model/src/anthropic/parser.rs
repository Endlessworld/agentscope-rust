//! Parses Anthropic Messages responses into AgentScope [`ChatResponse`]s.
//!
//! Mirrors `AnthropicResponseParser` (non-streaming `parseMessage` + streaming
//! event handling) and adds an incremental stream accumulator that emits
//! text/thinking deltas immediately while buffering tool-call fragments.

use std::collections::BTreeMap;

use agentscope_core::{
    ChatResponse, ChatUsage, ContentBlock, JsonMap, TextBlock, ThinkingBlock, ToolUseBlock,
};

use super::dto::{
    AnthropicContentBlock, AnthropicMessageResponse, AnthropicStreamEvent, AnthropicUsage,
};

/// Placeholder tool name used for streamed argument fragments that arrive
/// without name/id metadata (same sentinel as the Java `__fragment__`).
pub const FRAGMENT_PLACEHOLDER: &str = "__fragment__";

fn usage_of(usage: &AnthropicUsage) -> ChatUsage {
    ChatUsage {
        input_tokens: usage.total_input(),
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached(),
        cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
        ..Default::default()
    }
}

/// Converts request-side content blocks back into AgentScope blocks.
///
/// The Anthropic response reuses the request content-block shape, so the same
/// DTO covers both directions. Block order is thinking → text → tool use,
/// matching [`ChatResponse`] consumers.
pub fn blocks_of(content: &[AnthropicContentBlock]) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for block in content {
        match block {
            AnthropicContentBlock::Text(t) => {
                if !t.text.is_empty() {
                    blocks.push(ContentBlock::Text(TextBlock::new(t.text.clone())));
                }
            }
            AnthropicContentBlock::ToolUse(t) => {
                let raw = serde_json::Value::Object(t.input.clone()).to_string();
                let mut tool = ToolUseBlock::new(t.id.clone(), t.name.clone(), t.input.clone());
                tool.content = Some(raw);
                blocks.push(ContentBlock::ToolUse(tool));
            }
            // Images never appear in a response.
            AnthropicContentBlock::Image(_) | AnthropicContentBlock::ToolResult(_) => {}
        }
    }
    blocks
}

/// Parses a non-streaming `Message` response.
pub fn parse_message(response: &AnthropicMessageResponse) -> ChatResponse {
    let blocks = blocks_of(&response.content);
    ChatResponse::new(
        response.id.clone(),
        blocks,
        usage_of(&response.usage),
        None,
        response.stop_reason.clone(),
    )
}

/// Accumulates streamed Anthropic events.
///
/// * `content_block_delta` / `text_delta` and `thinking_delta` are yielded
///   immediately as incremental [`ChatResponse`]s.
/// * `content_block_start` registers an in-flight tool call (by stream index).
/// * `input_json_delta` appends to that tool call's raw argument buffer.
/// * `message_start` / `message_delta` update the accumulated usage; the tool
///   calls, usage and stop reason are flushed by [`Self::into_final`].
#[derive(Default)]
pub struct AnthropicStreamIncrementer {
    id: Option<String>,
    /// In-flight tool calls, keyed by their `content_block` stream index.
    tool_calls: BTreeMap<usize, ToolUseBlock>,
    position: BTreeMap<usize, usize>,
    usage: Option<ChatUsage>,
    stop_reason: Option<String>,
}

impl AnthropicStreamIncrementer {
    /// Creates an empty incrementer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes one event, returning an incremental response when the event
    /// carries new text/thinking content.
    pub fn accumulate(&mut self, event: &AnthropicStreamEvent) -> Option<ChatResponse> {
        if self.id.is_none() {
            self.id = event
                .message
                .as_ref()
                .map(|m| m.id.clone())
                .filter(|s| !s.is_empty());
        }

        // Usage is reported on message_start (prompt tokens) and again on
        // message_delta (cumulative output tokens). Merge rather than overwrite:
        // message_delta carries no input counts, so overwriting would drop them.
        if let Some(message) = &event.message {
            if event.event_type == "message_start" {
                self.usage = Some(usage_of(&message.usage));
            }
        }
        if let Some(usage) = &event.usage {
            let update = usage_of(usage);
            let mut merged = self.usage.unwrap_or_default();
            merged.output_tokens = update.output_tokens;
            if update.input_tokens > 0 {
                merged.input_tokens = update.input_tokens;
                merged.cached_tokens = update.cached_tokens;
            }
            merged.time = update.time;
            self.usage = Some(merged);
        }

        let mut blocks: Vec<ContentBlock> = Vec::new();
        match event.event_type.as_str() {
            "content_block_start" => {
                if let Some(AnthropicContentBlock::ToolUse(t)) = &event.content_block {
                    let index = event.index.unwrap_or(self.tool_calls.len());
                    let mut tool = ToolUseBlock::new(t.id.clone(), t.name.clone(), JsonMap::new());
                    tool.content = Some(String::new());
                    self.tool_calls.insert(index, tool);
                }
            }
            "content_block_delta" => {
                if let Some(delta) = &event.delta {
                    if let Some(text) = delta.text.as_ref().filter(|s| !s.is_empty()) {
                        blocks.push(ContentBlock::Text(TextBlock::new(text.clone())));
                    }
                    if let Some(thinking) = delta.thinking.as_ref().filter(|s| !s.is_empty()) {
                        blocks.push(ContentBlock::Thinking(ThinkingBlock::new(thinking.clone())));
                    }
                    if let Some(partial) = delta.partial_json.as_ref().filter(|s| !s.is_empty()) {
                        let index = event.index.unwrap_or(0);
                        let tool = self
                            .tool_calls
                            .entry(index)
                            .or_insert_with(|| fragment_tool(index));
                        let buffer = tool.content.get_or_insert_with(String::new);
                        buffer.push_str(partial);
                    }
                }
            }
            "message_delta" => {
                if let Some(delta) = &event.delta {
                    if let Some(reason) = &delta.stop_reason {
                        self.stop_reason = Some(reason.clone());
                    }
                }
            }
            _ => {}
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

    /// Flushes buffered tool calls / usage / stop reason at the end of the stream.
    pub fn into_final(mut self) -> Option<ChatResponse> {
        if self.tool_calls.is_empty() && self.usage.is_none() && self.stop_reason.is_none() {
            return None;
        }
        // Assign ordinal positions in stream-index order.
        for (ordinal, index) in self.tool_calls.keys().copied().enumerate() {
            self.position.insert(index, ordinal);
        }
        let content: Vec<ContentBlock> = self
            .tool_calls
            .into_iter()
            .map(|(_, mut tool)| {
                tool.input = parse_partial_json(tool.content.as_deref());
                ContentBlock::ToolUse(tool)
            })
            .collect();
        Some(ChatResponse::new(
            self.id.unwrap_or_default(),
            content,
            self.usage.unwrap_or_default(),
            None,
            self.stop_reason,
        ))
    }
}

fn fragment_tool(index: usize) -> ToolUseBlock {
    ToolUseBlock::new(
        format!("tool_call_{index}"),
        FRAGMENT_PLACEHOLDER,
        JsonMap::new(),
    )
}

/// Best-effort parse of a streamed tool-call argument buffer.
fn parse_partial_json(raw: Option<&str>) -> JsonMap {
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return JsonMap::new();
    };
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(value: serde_json::Value) -> AnthropicMessageResponse {
        serde_json::from_value(value).unwrap()
    }

    fn event(value: serde_json::Value) -> AnthropicStreamEvent {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn parses_text_and_tool_use_in_order() {
        let parsed = parse_message(&response(serde_json::json!({
            "id": "msg_1",
            "content": [
                { "type": "text", "text": "checking" },
                { "type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": { "city": "Paris" } }
            ],
            "usage": { "input_tokens": 10, "output_tokens": 4, "cache_read_input_tokens": 3 },
            "stop_reason": "tool_use"
        })));
        assert_eq!(parsed.id, "msg_1");
        assert_eq!(parsed.usage.input_tokens, 13);
        assert_eq!(parsed.usage.output_tokens, 4);
        assert_eq!(parsed.usage.cached_tokens, 3);
        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_use"));
        assert!(matches!(&parsed.content[0], ContentBlock::Text(t) if t.text == "checking"));
        match &parsed.content[1] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.id, "toolu_1");
                assert_eq!(t.name, "get_weather");
                assert_eq!(t.input.get("city"), Some(&serde_json::json!("Paris")));
                assert_eq!(t.content.as_deref(), Some("{\"city\":\"Paris\"}"));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn empty_text_blocks_are_skipped() {
        let parsed = parse_message(&response(serde_json::json!({
            "id": "msg",
            "content": [{ "type": "text", "text": "" }]
        })));
        assert!(parsed.content.is_empty());
    }

    #[test]
    fn stream_text_deltas_are_incremental() {
        let mut inc = AnthropicStreamIncrementer::new();
        let start = event(serde_json::json!({
            "type": "message_start",
            "message": { "id": "msg_9", "usage": { "input_tokens": 5, "cache_read_input_tokens": 1 } }
        }));
        assert!(inc.accumulate(&start).is_none());

        let a = event(serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "Hel" }
        }));
        let b = event(serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "lo" }
        }));
        let first = inc.accumulate(&a).unwrap();
        assert_eq!(first.id, "msg_9");
        assert!(matches!(&first.content[0], ContentBlock::Text(t) if t.text == "Hel"));
        assert!(matches!(&inc.accumulate(&b).unwrap().content[0], ContentBlock::Text(t) if t.text == "lo"));

        let final_response = inc.into_final().unwrap();
        assert_eq!(final_response.usage.input_tokens, 6);
    }

    #[test]
    fn stream_collects_tool_use_argument_fragments() {
        let mut inc = AnthropicStreamIncrementer::new();
        let start = event(serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": { "type": "tool_use", "id": "toolu_7", "name": "get_weather", "input": {} }
        }));
        assert!(inc.accumulate(&start).is_none());

        for partial in ["{\"ci", "ty\":\"P", "aris\"}"] {
            let delta = event(serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "input_json_delta", "partial_json": partial }
            }));
            assert!(inc.accumulate(&delta).is_none());
        }

        let end = event(serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "tool_use" },
            "usage": { "output_tokens": 9 }
        }));
        assert!(inc.accumulate(&end).is_none());

        let final_response = inc.into_final().unwrap();
        assert_eq!(final_response.finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(final_response.usage.output_tokens, 9);
        match &final_response.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.name, "get_weather");
                assert_eq!(t.input.get("city"), Some(&serde_json::json!("Paris")));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn stream_thinking_delta_is_emitted() {
        let mut inc = AnthropicStreamIncrementer::new();
        let delta = event(serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "thinking_delta", "thinking": "hmm" }
        }));
        let out = inc.accumulate(&delta).unwrap();
        assert!(matches!(&out.content[0], ContentBlock::Thinking(t) if t.thinking == "hmm"));
    }

    #[test]
    fn stream_without_metadata_flushes_nothing() {
        let mut inc = AnthropicStreamIncrementer::new();
        let ping = event(serde_json::json!({ "type": "ping" }));
        assert!(inc.accumulate(&ping).is_none());
        assert!(inc.into_final().is_none());
    }

    #[test]
    fn malformed_tool_arguments_degrade_to_empty_input() {
        let mut inc = AnthropicStreamIncrementer::new();
        let delta = event(serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": "{\"broken" }
        }));
        inc.accumulate(&delta);
        let final_response = inc.into_final().unwrap();
        match &final_response.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.name, FRAGMENT_PLACEHOLDER);
                assert!(t.input.is_empty());
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn usage_defaults_to_zero_when_absent() {
        let parsed = parse_message(&response(serde_json::json!({ "id": "m", "content": [] })));
        assert_eq!(parsed.usage, ChatUsage::default());
        assert!(parsed.finish_reason.is_none());
    }
}
