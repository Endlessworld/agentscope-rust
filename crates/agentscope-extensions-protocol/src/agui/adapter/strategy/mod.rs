//! AgentScope -> AG-UI event conversion strategies, mirroring
//! `io.agentscope.core.agui.adapter.strategy`.
//!
//! * [`context`] - the per-run `AguiStreamContext` that keeps the stream well-formed.
//! * [`converters`] - the `AgentEventConverter` extension point and built-in converters.
//! * [`enricher`] - `AguiEventEnricher` post-processing (base properties, raw source data).
//! * [`registry`] - `AgentEventConverterRegistry`, mapping events to AG-UI events.

pub mod context;
pub mod converters;
pub mod enricher;
pub mod registry;

pub use context::{
    block_message_id, event_string, event_string_or_default, AguiStreamContext, TokenUsage,
    TokenUsageAccumulator, TokenUsageSnapshot,
};
pub use converters::{
    AgentEventConverter, AgentLifecycleEventConverter, CustomAgentEventConverter,
    ExternalExecutionEventConverter, ModelCallUsageEventConverter, PermissionConfirmEventConverter,
    RawAgentEventConverter, SubagentEventConverter, TextBlockEventConverter,
    ThinkingBlockEventConverter, ToolCallEventConverter, ToolResultEventConverter,
};
pub use enricher::{
    AguiEventEnricher, BaseEventPropertiesEnricher, RawEventPropertiesEnricher,
    SharedEventEnricher,
};
pub use registry::AgentEventConverterRegistry;
