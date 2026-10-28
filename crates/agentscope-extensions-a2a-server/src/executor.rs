//! Agent execution properties and the agent-event → A2A output projection.
//!
//! Mirrors `io.agentscope.core.a2a.server.executor`:
//!
//! * [`AgentExecuteProperties`] - `executor.AgentExecuteProperties`
//! * [`AgentOutputProjector`] - the `BaseFluxEventHandler` conversion table
//!   (`convertToResponseMessage` / `generateRequiredEventTypes`)

use agentscope_core::event::AgentEvent;
use agentscope_core::message::{
    ContentBlock, DataBlock, HintBlock, JsonMap, Msg, MsgRole, Source, TextBlock, ThinkingBlock,
    ToolResultBlock,
};

/// Properties about agent execution.
///
/// Mirrors Java `io.agentscope.core.a2a.server.executor.AgentExecuteProperties`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentExecuteProperties {
    complete_with_message: bool,
    require_inner_message: bool,
}

impl AgentExecuteProperties {
    /// Whether a *non-blocking* task should complete with a message carrying the whole result.
    ///
    /// Blocking (`message/send`) requests always complete with a message, mirroring Java
    /// `completeWithMessage`.
    pub fn complete_with_message(&self) -> bool {
        self.complete_with_message
    }

    /// Whether inner events (tool progress, tool-result deltas, hints) are exposed to the client.
    pub fn require_inner_message(&self) -> bool {
        self.require_inner_message
    }

    /// Returns the Java-style fluent builder.
    pub fn builder() -> AgentExecutePropertiesBuilder {
        AgentExecutePropertiesBuilder::default()
    }
}

/// Builder for [`AgentExecuteProperties`].
#[derive(Debug, Default, Clone, Copy)]
pub struct AgentExecutePropertiesBuilder {
    properties: AgentExecuteProperties,
}

impl AgentExecutePropertiesBuilder {
    pub fn complete_with_message(mut self, complete_with_message: bool) -> Self {
        self.properties.complete_with_message = complete_with_message;
        self
    }

    pub fn require_inner_message(mut self, require_inner_message: bool) -> Self {
        self.properties.require_inner_message = require_inner_message;
        self
    }

    pub fn build(self) -> AgentExecuteProperties {
        self.properties
    }
}

/// Converts the events of one run into the `Msg`s A2A exposes, mirroring the conversion table in
/// Java `AgentScopeAgentExecutor.BaseFluxEventHandler`.
///
/// Required event types (Java `generateRequiredEventTypes`):
///
/// * always: `TEXT_BLOCK_DELTA`, `THINKING_BLOCK_DELTA`;
/// * only with [`AgentExecuteProperties::require_inner_message`]: `TOOL_PROGRESS`,
///   `TOOL_RESULT_TEXT_DELTA`, `TOOL_RESULT_DATA_DELTA`, `HINT_BLOCK`.
///
/// Java's executor consumes a delta stream, so `AGENT_RESULT` is not in its table. Rust
/// [`AgentRunner`](crate::AgentRunner)s may only produce the message-level result (the default
/// contract), so this projector falls back to `AGENT_RESULT` when nothing was streamed: the
/// first `AGENT_RESULT` is projected, and any later one is ignored so a runner that emits both
/// deltas and a result never duplicates its output.
#[derive(Debug, Clone)]
pub struct AgentOutputProjector {
    require_inner_message: bool,
    accumulated: Vec<Msg>,
    streamed_output: bool,
    result_projected: bool,
}

impl AgentOutputProjector {
    /// Creates a projector for the given execution properties.
    pub fn new(properties: AgentExecuteProperties) -> Self {
        Self {
            require_inner_message: properties.require_inner_message,
            accumulated: Vec::new(),
            streamed_output: false,
            result_projected: false,
        }
    }

    /// Projects one event into an output [`Msg`], or `None` when the event is not exposed.
    ///
    /// Mirrors Java `isNoResponseEvent` + `convertToResponseMessage`.
    pub fn project(&mut self, event: &AgentEvent) -> Option<Msg> {
        let msg = match event {
            AgentEvent::TextBlockDelta(data) => {
                self.streamed_output = true;
                Some(assistant_message(
                    data,
                    ContentBlock::Text(TextBlock::new(string_field(data, "delta"))),
                ))
            }
            AgentEvent::ThinkingBlockDelta(data) => {
                self.streamed_output = true;
                Some(assistant_message(
                    data,
                    ContentBlock::Thinking(ThinkingBlock::new(string_field(data, "delta"))),
                ))
            }
            AgentEvent::ToolProgress(data) if self.require_inner_message => {
                self.streamed_output = true;
                let content = data
                    .fields
                    .get("content")
                    .and_then(|value| serde_json::from_value::<ContentBlock>(value.clone()).ok());
                content.map(|content| tool_message(data, content))
            }
            AgentEvent::ToolResultTextDelta(data) if self.require_inner_message => {
                self.streamed_output = true;
                Some(tool_message(
                    data,
                    ContentBlock::Text(TextBlock::new(string_field(data, "delta"))),
                ))
            }
            AgentEvent::ToolResultDataDelta(data) if self.require_inner_message => {
                self.streamed_output = true;
                let source = data.fields.get("source").and_then(|value| {
                    serde_json::from_value::<Source>(value.clone())
                        .ok()
                        .map(|source| ContentBlock::Data(DataBlock::new(source)))
                });
                source.map(|content| tool_message(data, content))
            }
            AgentEvent::HintBlock(data) if self.require_inner_message => {
                self.streamed_output = true;
                Some(assistant_message(
                    data,
                    ContentBlock::Hint(HintBlock {
                        id: data
                            .fields
                            .get("block_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        hint: data
                            .fields
                            .get("hint")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        source: data
                            .fields
                            .get("hint_source")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    }),
                ))
            }
            // Fallback for runners that only implement the message-level contract.
            AgentEvent::AgentResult(data) if !self.streamed_output && !self.result_projected => {
                self.result_projected = true;
                self.project_result(data)
            }
            _ => None,
        };
        if let Some(msg) = &msg {
            self.accumulated.push(msg.clone());
        }
        msg
    }

    /// The `Msg`s projected so far, in emission order.
    pub fn accumulated(&self) -> &[Msg] {
        &self.accumulated
    }

    /// The accumulated output with streaming chunks compacted, mirroring Java
    /// `MessageConvertUtil.compactStreamingChunks`.
    pub fn compacted(&self) -> Vec<Msg> {
        crate::ServerMessageConvert::compact_streaming_chunks(&self.accumulated)
    }

    /// Whether any output was projected (false means the run produced no client-visible text).
    pub fn has_output(&self) -> bool {
        !self.accumulated.is_empty()
    }

    /// Projects an `AGENT_RESULT` payload: its `content` block array becomes one assistant `Msg`.
    fn project_result(&self, data: &agentscope_core::event::EventData) -> Option<Msg> {
        let blocks = data.fields.get("content")?.as_array()?;
        let mut content: Vec<ContentBlock> = Vec::new();
        for block in blocks {
            let Ok(block) = serde_json::from_value::<ContentBlock>(block.clone()) else {
                continue;
            };
            if self.exposes(&block) {
                content.push(block);
            }
        }
        if content.is_empty() {
            return None;
        }
        Some(
            Msg::builder()
                .id(result_message_id(data))
                .role(MsgRole::Assistant)
                .content(content)
                .metadata(data.metadata.clone())
                .build(),
        )
    }

    /// Block kinds the client may see: text and thinking always, inner blocks on request.
    fn exposes(&self, block: &ContentBlock) -> bool {
        match block {
            ContentBlock::Text(_) | ContentBlock::Thinking(_) => true,
            ContentBlock::ToolResult(_) | ContentBlock::Hint(_) => self.require_inner_message,
            _ => false,
        }
    }
}

/// Java `assistantMessage`: an assistant `Msg` keyed by the event's reply id.
fn assistant_message(data: &agentscope_core::event::EventData, content: ContentBlock) -> Msg {
    Msg::builder()
        .id(reply_id(data))
        .role(MsgRole::Assistant)
        .block(content)
        .metadata(data.metadata.clone())
        .build()
}

/// Java `toolResultMessage`: a tool `Msg` carrying one tool-result block.
fn tool_message(data: &agentscope_core::event::EventData, content: ContentBlock) -> Msg {
    Msg::builder()
        .id(reply_id(data))
        .role(MsgRole::Tool)
        .block(ContentBlock::ToolResult(ToolResultBlock {
            id: data
                .fields
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            name: data
                .fields
                .get("tool_name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            output: vec![content],
            metadata: JsonMap::new(),
            state: Default::default(),
        }))
        .metadata(data.metadata.clone())
        .build()
}

/// Java `event.getReplyId()`, falling back to the event id so a `Msg` always has an id.
fn reply_id(data: &agentscope_core::event::EventData) -> String {
    data.fields
        .get("reply_id")
        .and_then(|v| v.as_str())
        .unwrap_or(&data.id)
        .to_string()
}

/// `AGENT_RESULT` id source: the message result carries `msg_id`, else the event id.
fn result_message_id(data: &agentscope_core::event::EventData) -> String {
    data.fields
        .get("msg_id")
        .and_then(|v| v.as_str())
        .unwrap_or(&data.id)
        .to_string()
}

fn string_field(data: &agentscope_core::event::EventData, key: &str) -> String {
    data.fields
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::{AgentEvent, AgentEventType, EventData};

    fn delta(kind: AgentEventType, text: &str) -> AgentEvent {
        let mut fields = JsonMap::new();
        fields.insert(
            "reply_id".to_string(),
            serde_json::Value::String("reply-1".to_string()),
        );
        fields.insert(
            "delta".to_string(),
            serde_json::Value::String(text.to_string()),
        );
        AgentEvent::with_fields(kind, fields)
    }

    fn tool_delta(text: &str) -> AgentEvent {
        let mut fields = JsonMap::new();
        fields.insert(
            "tool_call_id".to_string(),
            serde_json::Value::String("call-1".to_string()),
        );
        fields.insert(
            "tool_name".to_string(),
            serde_json::Value::String("search".to_string()),
        );
        fields.insert(
            "delta".to_string(),
            serde_json::Value::String(text.to_string()),
        );
        AgentEvent::with_fields(AgentEventType::ToolResultTextDelta, fields)
    }

    fn result_event(text: &str) -> AgentEvent {
        let mut data = EventData::default();
        data.fields.insert(
            "msg_id".to_string(),
            serde_json::Value::String("msg-1".to_string()),
        );
        data.fields.insert(
            "content".to_string(),
            serde_json::json!([
                { "type": "text", "text": text },
                { "type": "thinking", "thinking": "hmm" },
                { "type": "tool_result", "id": "call-1", "name": "search",
                  "output": [{ "type": "text", "text": "inner" }] },
            ]),
        );
        AgentEvent::AgentResult(data)
    }

    #[test]
    fn properties_default_to_java_defaults_and_are_buildable() {
        let default = AgentExecuteProperties::default();
        assert!(!default.complete_with_message());
        assert!(!default.require_inner_message());

        let configured = AgentExecuteProperties::builder()
            .complete_with_message(true)
            .require_inner_message(true)
            .build();
        assert!(configured.complete_with_message());
        assert!(configured.require_inner_message());
    }

    #[test]
    fn text_and_thinking_deltas_are_always_projected() {
        let mut projector = AgentOutputProjector::new(AgentExecuteProperties::default());
        let text = projector
            .project(&delta(AgentEventType::TextBlockDelta, "Hel"))
            .expect("text delta is projected");
        assert_eq!(text.role, MsgRole::Assistant);
        assert_eq!(text.id, "reply-1");

        let thinking = projector
            .project(&delta(AgentEventType::ThinkingBlockDelta, "lo"))
            .expect("thinking delta is projected");
        assert_eq!(thinking.role, MsgRole::Assistant);
        // Both deltas share a reply id, so compaction folds them into one message.
        let compacted = projector.compacted();
        assert_eq!(compacted.len(), 1);
        assert_eq!(compacted[0].content.len(), 2);
    }

    #[test]
    fn inner_events_need_require_inner_message() {
        let mut quiet = AgentOutputProjector::new(AgentExecuteProperties::default());
        assert!(quiet.project(&tool_delta("tool output")).is_none());
        assert!(!quiet.has_output());

        let mut verbose = AgentOutputProjector::new(
            AgentExecuteProperties::builder()
                .require_inner_message(true)
                .build(),
        );
        let msg = verbose
            .project(&tool_delta("tool output"))
            .expect("inner event is projected when requested");
        assert_eq!(msg.role, MsgRole::Tool);
        match &msg.content[0] {
            ContentBlock::ToolResult(block) => {
                assert_eq!(block.id.as_deref(), Some("call-1"));
                assert_eq!(block.name.as_deref(), Some("search"));
            }
            other => panic!("expected a tool result block, got {other:?}"),
        }
    }

    #[test]
    fn agent_result_falls_back_only_when_nothing_was_streamed() {
        let mut projector = AgentOutputProjector::new(AgentExecuteProperties::default());
        let msg = projector
            .project(&result_event("final"))
            .expect("a result-only runner still produces output");
        assert_eq!(msg.id, "msg-1");
        // Inner blocks are filtered out without require_inner_message.
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(msg.content[1], ContentBlock::Thinking(_)));

        let mut streamed = AgentOutputProjector::new(AgentExecuteProperties::default());
        streamed.project(&delta(AgentEventType::TextBlockDelta, "a"));
        assert!(streamed.project(&result_event("final")).is_none());
        assert_eq!(streamed.accumulated().len(), 1);
    }

    #[test]
    fn compaction_merges_deltas_of_the_same_message() {
        let mut projector = AgentOutputProjector::new(AgentExecuteProperties::default());
        projector.project(&delta(AgentEventType::TextBlockDelta, "Hel"));
        projector.project(&delta(AgentEventType::TextBlockDelta, "lo"));
        let compacted = projector.compacted();
        assert_eq!(compacted.len(), 1);
        assert_eq!(
            compacted[0].content,
            vec![ContentBlock::Text(TextBlock::new("Hello"))]
        );
    }
}
