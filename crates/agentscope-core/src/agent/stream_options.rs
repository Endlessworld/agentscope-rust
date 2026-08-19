//! Streaming configuration and event filtering for fine-grained agent events,
//! mirroring the Java `StreamOptions` (v2 `streamEvents` surface).

use std::collections::HashSet;

use crate::event::{AgentEvent, AgentEventType};

/// Coarse event category used to select which event families to stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventCategory {
    All,
    Agent,
    ModelCall,
    Text,
    Thinking,
    Data,
    ToolCall,
    ToolResult,
    Summary,
    Interrupt,
    Subagent,
    Custom,
}

impl EventCategory {
    /// Maps a fine-grained event to its coarse category.
    pub fn of(event: &AgentEvent) -> EventCategory {
        match event.event_type() {
            AgentEventType::AgentStart
            | AgentEventType::AgentEnd
            | AgentEventType::AgentResult => EventCategory::Agent,
            AgentEventType::ModelCallStart | AgentEventType::ModelCallEnd => EventCategory::ModelCall,
            AgentEventType::TextBlockStart
            | AgentEventType::TextBlockDelta
            | AgentEventType::TextBlockEnd => EventCategory::Text,
            AgentEventType::ThinkingBlockStart
            | AgentEventType::ThinkingBlockDelta
            | AgentEventType::ThinkingBlockEnd => EventCategory::Thinking,
            AgentEventType::DataBlockStart
            | AgentEventType::DataBlockDelta
            | AgentEventType::DataBlockEnd => EventCategory::Data,
            AgentEventType::ToolCallStart
            | AgentEventType::ToolCallDelta
            | AgentEventType::ToolCallEnd => EventCategory::ToolCall,
            AgentEventType::ToolResultStart
            | AgentEventType::ToolResultTextDelta
            | AgentEventType::ToolResultDataDelta
            | AgentEventType::ToolResultEnd => EventCategory::ToolResult,
            AgentEventType::ExceedMaxIters => EventCategory::Summary,
            AgentEventType::RequireUserConfirm
            | AgentEventType::RequireExternalExecution
            | AgentEventType::UserConfirmResult
            | AgentEventType::ExternalExecutionResult
            | AgentEventType::RequestStop => EventCategory::Interrupt,
            AgentEventType::SubagentExposed => EventCategory::Subagent,
            AgentEventType::HintBlock
            | AgentEventType::Custom
            | AgentEventType::AllToolsDenied => EventCategory::Custom,
        }
    }
}

/// Configuration controlling which events a stream emits, mirroring Java `StreamOptions`.
#[derive(Debug, Clone)]
pub struct StreamOptions {
    event_types: HashSet<EventCategory>,
    incremental: bool,
    include_reasoning_chunk: bool,
    include_reasoning_result: bool,
    include_acting_chunk: bool,
    include_summary_chunk: bool,
    include_summary_result: bool,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self::defaults()
    }
}

impl StreamOptions {
    /// All event types, incremental mode, all sub-emissions included.
    pub fn defaults() -> Self {
        let mut event_types = HashSet::new();
        event_types.insert(EventCategory::All);
        Self {
            event_types,
            incremental: true,
            include_reasoning_chunk: true,
            include_reasoning_result: true,
            include_acting_chunk: true,
            include_summary_chunk: true,
            include_summary_result: true,
        }
    }

    /// Creates a new builder.
    pub fn builder() -> StreamOptionsBuilder {
        StreamOptionsBuilder::default()
    }

    /// Whether incremental (delta-only) delivery is enabled.
    pub fn incremental(&self) -> bool {
        self.incremental
    }

    /// The configured coarse event categories.
    pub fn event_types(&self) -> &HashSet<EventCategory> {
        &self.event_types
    }

    /// Whether this event should be emitted given the configured options.
    pub fn should_stream(&self, event: &AgentEvent) -> bool {
        let cat = EventCategory::of(event);
        if !(self.event_types.contains(&EventCategory::All)
            || self.event_types.contains(&cat))
        {
            return false;
        }
        match event.event_type() {
            AgentEventType::ThinkingBlockStart
            | AgentEventType::ThinkingBlockDelta => self.include_reasoning_chunk,
            AgentEventType::ThinkingBlockEnd => self.include_reasoning_result,
            AgentEventType::ToolResultStart
            | AgentEventType::ToolResultTextDelta
            | AgentEventType::ToolResultDataDelta => self.include_acting_chunk,
            AgentEventType::ExceedMaxIters => self.include_summary_result,
            _ => true,
        }
    }
}

/// Builder for [`StreamOptions`].
#[derive(Debug, Clone, Default)]
pub struct StreamOptionsBuilder {
    event_types: Option<HashSet<EventCategory>>,
    incremental: Option<bool>,
    include_reasoning_chunk: Option<bool>,
    include_reasoning_result: Option<bool>,
    include_acting_chunk: Option<bool>,
    include_summary_chunk: Option<bool>,
    include_summary_result: Option<bool>,
}

impl StreamOptionsBuilder {
    /// Selects the coarse categories to stream (defaults to all).
    pub fn event_types(mut self, types: &[EventCategory]) -> Self {
        self.event_types = Some(types.iter().copied().collect());
        self
    }
    pub fn incremental(mut self, v: bool) -> Self {
        self.incremental = Some(v);
        self
    }
    pub fn include_reasoning_chunk(mut self, v: bool) -> Self {
        self.include_reasoning_chunk = Some(v);
        self
    }
    pub fn include_reasoning_result(mut self, v: bool) -> Self {
        self.include_reasoning_result = Some(v);
        self
    }
    pub fn include_acting_chunk(mut self, v: bool) -> Self {
        self.include_acting_chunk = Some(v);
        self
    }
    pub fn include_summary_chunk(mut self, v: bool) -> Self {
        self.include_summary_chunk = Some(v);
        self
    }
    pub fn include_summary_result(mut self, v: bool) -> Self {
        self.include_summary_result = Some(v);
        self
    }
    pub fn build(self) -> StreamOptions {
        let d = StreamOptions::defaults();
        StreamOptions {
            event_types: self.event_types.unwrap_or(d.event_types),
            incremental: self.incremental.unwrap_or(d.incremental),
            include_reasoning_chunk: self.include_reasoning_chunk.unwrap_or(d.include_reasoning_chunk),
            include_reasoning_result: self.include_reasoning_result.unwrap_or(d.include_reasoning_result),
            include_acting_chunk: self.include_acting_chunk.unwrap_or(d.include_acting_chunk),
            include_summary_chunk: self.include_summary_chunk.unwrap_or(d.include_summary_chunk),
            include_summary_result: self.include_summary_result.unwrap_or(d.include_summary_result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentEvent;

    #[test]
    fn category_mapping() {
        assert_eq!(
            EventCategory::of(&AgentEvent::new(AgentEventType::ThinkingBlockDelta)),
            EventCategory::Thinking
        );
        assert_eq!(
            EventCategory::of(&AgentEvent::new(AgentEventType::ToolResultEnd)),
            EventCategory::ToolResult
        );
        assert_eq!(
            EventCategory::of(&AgentEvent::new(AgentEventType::AgentStart)),
            EventCategory::Agent
        );
        assert_eq!(
            EventCategory::of(&AgentEvent::new(AgentEventType::ExceedMaxIters)),
            EventCategory::Summary
        );
    }

    #[test]
    fn defaults_stream_everything() {
        let opts = StreamOptions::defaults();
        assert!(opts.should_stream(&AgentEvent::new(AgentEventType::AgentStart)));
        assert!(opts.should_stream(&AgentEvent::new(AgentEventType::ToolResultEnd)));
        assert!(opts.should_stream(&AgentEvent::new(AgentEventType::ThinkingBlockDelta)));
        assert!(opts.incremental());
    }

    #[test]
    fn category_filter_hides_other_types() {
        let opts = StreamOptions::builder()
            .event_types(&[EventCategory::ToolResult])
            .build();
        assert!(opts.should_stream(&AgentEvent::new(AgentEventType::ToolResultEnd)));
        assert!(!opts.should_stream(&AgentEvent::new(AgentEventType::AgentStart)));
        assert!(!opts.should_stream(&AgentEvent::new(AgentEventType::ThinkingBlockDelta)));
    }

    #[test]
    fn reasoning_subtype_flags() {
        let opts = StreamOptions::builder()
            .event_types(&[EventCategory::Thinking])
            .include_reasoning_chunk(false)
            .build();
        assert!(!opts.should_stream(&AgentEvent::new(AgentEventType::ThinkingBlockDelta)));
        assert!(opts.should_stream(&AgentEvent::new(AgentEventType::ThinkingBlockEnd)));
    }

    #[test]
    fn acting_chunk_flag() {
        let opts = StreamOptions::builder()
            .event_types(&[EventCategory::ToolResult])
            .include_acting_chunk(false)
            .build();
        assert!(!opts.should_stream(&AgentEvent::new(AgentEventType::ToolResultTextDelta)));
        assert!(opts.should_stream(&AgentEvent::new(AgentEventType::ToolResultEnd)));
    }
}
