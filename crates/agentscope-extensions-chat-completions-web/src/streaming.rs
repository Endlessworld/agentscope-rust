//! Framework-agnostic streaming adapter converting agent events into
//! OpenAI-compatible [`ChatCompletionsChunk`]s, mirroring
//! `io.agentscope.core.chat.completions.streaming.ChatCompletionsStreamingAdapter`.
//!
//! The Java original consumes `Event` objects (message + type + `isLast`).
//! Rust has no direct equivalent in `agentscope-core`, so this crate defines a
//! minimal [`AgentStreamEvent`] view that adapters can build from the core event
//! stream (see tracker §4.2).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agentscope_core::message::{ContentBlock, GenerateReason, Msg};
use futures::stream::{Stream, StreamExt};

use super::builder::tool_use_block_to_tool_call;
use super::model::{ChatCompletionsChunk, ToolCall};

/// The subset of agent events relevant to chat-completion streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStreamEventType {
    /// A reasoning/model output event (text and tool calls).
    Reasoning,
    /// A tool execution result event.
    ToolResult,
}

/// A single agent stream event: a message plus its kind and stream position.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStreamEvent {
    /// Event kind.
    pub event_type: AgentStreamEventType,
    /// The message carried by this event.
    pub message: Msg,
    /// Whether this is the final (accumulated) event of the stream.
    pub is_last: bool,
}

impl AgentStreamEvent {
    /// Creates a non-final reasoning event.
    pub fn reasoning(message: Msg) -> Self {
        Self {
            event_type: AgentStreamEventType::Reasoning,
            message,
            is_last: false,
        }
    }

    /// Creates a non-final tool-result event.
    pub fn tool_result(message: Msg) -> Self {
        Self {
            event_type: AgentStreamEventType::ToolResult,
            message,
            is_last: false,
        }
    }

    /// Marks this event as the last one in the stream.
    pub fn last(mut self) -> Self {
        self.is_last = true;
        self
    }
}

/// Converts agent stream events into OpenAI-compatible chunks.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChatCompletionsStreamingAdapter;

impl ChatCompletionsStreamingAdapter {
    /// Creates a new adapter.
    pub fn new() -> Self {
        Self
    }

    /// Streams agent events as OpenAI-compatible chunks.
    ///
    /// When incremental reasoning chunks were observed, the text of the final
    /// accumulated reasoning event is dropped (it would otherwise be duplicated),
    /// while tool calls and the finish reason are preserved.
    pub fn stream<S>(
        &self,
        events: S,
        request_id: impl Into<String>,
        model: impl Into<String>,
    ) -> impl Stream<Item = ChatCompletionsChunk>
    where
        S: Stream<Item = AgentStreamEvent> + Send + 'static,
    {
        let request_id = Arc::new(request_id.into());
        let model = Arc::new(model.into());
        let seen_incremental = Arc::new(AtomicBool::new(false));
        // The adapter is `Copy`; moving a copy into the closure keeps the returned stream free of
        // the `&self` lifetime, which `impl Stream` (Rust 2021) cannot capture implicitly.
        let adapter = *self;

        events.flat_map(move |event| {
            let request_id = request_id.clone();
            let model = model.clone();
            let seen_incremental = seen_incremental.clone();

            let include_text = if event.event_type == AgentStreamEventType::Reasoning {
                if event.is_last && seen_incremental.load(Ordering::SeqCst) {
                    false
                } else {
                    if !event.is_last {
                        seen_incremental.store(true, Ordering::SeqCst);
                    }
                    true
                }
            } else {
                true
            };

            let chunks = adapter.convert_event(&event, &request_id, &model, include_text);
            futures::stream::iter(chunks)
        })
    }

    /// Converts one event into zero or more chunks, including text.
    pub fn convert_event_to_chunks(
        &self,
        event: &AgentStreamEvent,
        request_id: &str,
        model: &str,
    ) -> Vec<ChatCompletionsChunk> {
        self.convert_event(event, request_id, model, true)
    }

    fn convert_event(
        &self,
        event: &AgentStreamEvent,
        request_id: &str,
        model: &str,
        include_text: bool,
    ) -> Vec<ChatCompletionsChunk> {
        let msg = &event.message;
        if msg.content().is_empty() {
            return Vec::new();
        }

        let mut chunks = Vec::new();

        let text_content: String = msg
            .content()
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) if !t.text.is_empty() => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        if !text_content.is_empty() && include_text {
            chunks.push(ChatCompletionsChunk::text_chunk(
                request_id,
                model,
                text_content,
            ));
        }

        if event.event_type == AgentStreamEventType::Reasoning {
            let mut tool_calls = Vec::new();
            for block in msg.content() {
                if let ContentBlock::ToolUse(tool_use) = block {
                    // Streaming prefers the raw `content` fragment; an empty
                    // string lets clients keep accumulating later chunks.
                    let arguments_json = match tool_use.content.as_deref() {
                        Some(content) if !content.is_empty() => content.to_string(),
                        _ if !tool_use.input.is_empty() => {
                            serde_json::to_string(&tool_use.input).unwrap_or_else(|_| String::new())
                        }
                        _ => String::new(),
                    };
                    let call = ToolCall::with_index(
                        tool_calls.len() as i32,
                        tool_use.id.clone(),
                        tool_use.name.clone(),
                        arguments_json,
                    );
                    tool_calls.push(call);
                }
            }
            if !tool_calls.is_empty() {
                chunks.push(ChatCompletionsChunk::tool_call_chunk(
                    request_id, model, tool_calls,
                ));
            }
        }

        if event.event_type == AgentStreamEventType::ToolResult {
            for block in msg.content() {
                if let ContentBlock::ToolResult(result) = block {
                    chunks.push(ChatCompletionsChunk::tool_result_chunk(
                        request_id,
                        model,
                        result.id.as_deref(),
                        result.name.as_deref().unwrap_or(""),
                        &result.text_content(),
                    ));
                }
            }
        }

        if event.is_last {
            let finish_reason = match msg.generate_reason() {
                GenerateReason::ToolSuspended => "tool_calls",
                GenerateReason::MaxIterations => "length",
                _ => {
                    let has_tool_calls = msg
                        .content()
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolUse(_)));
                    if has_tool_calls {
                        "tool_calls"
                    } else {
                        "stop"
                    }
                }
            };
            chunks.push(ChatCompletionsChunk::finish_chunk(
                request_id,
                model,
                finish_reason,
            ));
        }

        chunks
    }

    /// Creates an error chunk rendered as assistant text.
    pub fn create_error_chunk(
        &self,
        error: Option<&str>,
        request_id: &str,
        model: &str,
    ) -> ChatCompletionsChunk {
        let message = error.unwrap_or("Unknown error occurred");
        ChatCompletionsChunk::text_chunk(request_id, model, format!("Error: {}", message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{ToolResultBlock, ToolUseBlock};
    use futures::StreamExt;
    use serde_json::json;

    fn collect(
        adapter: &ChatCompletionsStreamingAdapter,
        events: Vec<AgentStreamEvent>,
    ) -> Vec<ChatCompletionsChunk> {
        futures::executor::block_on(async move {
            adapter
                .stream(futures::stream::iter(events), "req-1", "gpt-4")
                .collect::<Vec<_>>()
                .await
        })
    }

    #[test]
    fn text_event_yields_text_chunk() {
        let adapter = ChatCompletionsStreamingAdapter::new();
        let event = AgentStreamEvent::reasoning(Msg::assistant("Hello"));
        let chunks = adapter.convert_event_to_chunks(&event, "req-1", "gpt-4");
        assert_eq!(chunks.len(), 1);
        let value = serde_json::to_value(&chunks[0]).unwrap();
        assert_eq!(value["choices"][0]["delta"]["content"], json!("Hello"));
    }

    #[test]
    fn last_event_appends_finish_chunk() {
        let adapter = ChatCompletionsStreamingAdapter::new();
        let event = AgentStreamEvent::reasoning(Msg::assistant("final")).last();
        let chunks = adapter.convert_event_to_chunks(&event, "req-1", "gpt-4");
        assert_eq!(chunks.len(), 2);
        let value = serde_json::to_value(&chunks[1]).unwrap();
        assert_eq!(value["choices"][0]["finish_reason"], json!("stop"));
    }

    #[test]
    fn reasoning_with_tool_use_emits_tool_call_chunk_with_index() {
        let adapter = ChatCompletionsStreamingAdapter::new();
        let msg = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call_1",
            "get_weather",
            serde_json::from_value(json!({"city": "Hangzhou"})).unwrap(),
        ))]);
        let event = AgentStreamEvent::reasoning(msg);
        let chunks = adapter.convert_event_to_chunks(&event, "req", "gpt-4");
        assert_eq!(chunks.len(), 1);
        let value = serde_json::to_value(&chunks[0]).unwrap();
        assert_eq!(
            value["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(0)
        );
        assert_eq!(
            value["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            json!("get_weather")
        );
    }

    #[test]
    fn tool_result_event_emits_prefixed_text_chunk() {
        let adapter = ChatCompletionsStreamingAdapter::new();
        let result = ToolResultBlock::new(
            "call_1",
            "get_weather",
            vec![ContentBlock::Text(
                agentscope_core::message::TextBlock::new("sunny"),
            )],
        );
        let msg = Msg::builder()
            .role(agentscope_core::message::MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(result)])
            .build();
        let event = AgentStreamEvent::tool_result(msg);
        let chunks = adapter.convert_event_to_chunks(&event, "req", "gpt-4");
        assert_eq!(chunks.len(), 1);
        let value = serde_json::to_value(&chunks[0]).unwrap();
        assert_eq!(
            value["choices"][0]["delta"]["content"],
            json!("[Tool: get_weather] sunny")
        );
    }

    #[test]
    fn final_accumulated_text_is_deduplicated_when_incremental_seen() {
        let adapter = ChatCompletionsStreamingAdapter::new();
        let events = vec![
            AgentStreamEvent::reasoning(Msg::assistant("Hel")),
            AgentStreamEvent::reasoning(Msg::assistant("lo")),
            AgentStreamEvent::reasoning(Msg::assistant("Hello")).last(),
        ];
        let chunks = collect(&adapter, events);
        let texts: Vec<String> = chunks
            .iter()
            .filter_map(|c| {
                c.choices
                    .as_ref()
                    .and_then(|cs| cs.first())
                    .and_then(|ch| ch.delta.as_ref())
                    .and_then(|d| d.content.clone())
            })
            .collect();
        assert_eq!(texts, vec!["Hel".to_string(), "lo".to_string()]);
        // finish chunk still present
        let last = chunks.last().unwrap();
        assert_eq!(
            last.choices.as_ref().unwrap()[0].finish_reason.as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn max_iterations_finish_reason_is_length() {
        let adapter = ChatCompletionsStreamingAdapter::new();
        let msg = Msg::builder()
            .role(agentscope_core::message::MsgRole::Assistant)
            .text_content("partial")
            .generate_reason(GenerateReason::MaxIterations)
            .build();
        let event = AgentStreamEvent::reasoning(msg).last();
        let chunks = adapter.convert_event_to_chunks(&event, "req", "m");
        let last = chunks.last().unwrap();
        assert_eq!(
            last.choices.as_ref().unwrap()[0].finish_reason.as_deref(),
            Some("length")
        );
    }

    #[test]
    fn create_error_chunk_renders_message() {
        let adapter = ChatCompletionsStreamingAdapter::new();
        let chunk = adapter.create_error_chunk(Some("boom"), "req", "m");
        let value = serde_json::to_value(&chunk).unwrap();
        assert_eq!(
            value["choices"][0]["delta"]["content"],
            json!("Error: boom")
        );
    }

    #[test]
    fn empty_message_yields_no_chunks() {
        let adapter = ChatCompletionsStreamingAdapter::new();
        let event = AgentStreamEvent::reasoning(Msg::assistant(""));
        assert!(adapter
            .convert_event_to_chunks(&event, "req", "m")
            .is_empty());
    }
}
