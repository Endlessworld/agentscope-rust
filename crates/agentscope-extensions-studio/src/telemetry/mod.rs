//! OpenTelemetry GenAI telemetry for AgentScope, mirroring
//! `io.agentscope.core.tracing.telemetry`.
//!
//! Scope of this migration:
//! - OTel GenAI / AgentScope semantic-convention attribute keys ([`gen_ai_attributes`],
//!   [`agent_scope_attributes`]).
//! - The OTel message-part model ([`model`]).
//! - Pure attribute extractors ([`extractors`]).
//! - Streaming response aggregation ([`aggregator`]).
//!
//! The full OTLP exporter / [`TelemetryTracer`] span pipeline is deliberately
//! deferred (tracker §4.2): it would require an `opentelemetry` workspace dependency,
//! which is owned by the aggregation agent. Extracted attributes are returned as
//! plain `(String, serde_json::Value)` pairs so the logic stays testable offline.

pub mod agent_scope_attributes;
pub mod aggregator;
pub mod extractors;
pub mod gen_ai_attributes;
pub mod model;

pub use aggregator::StreamChatResponseAggregator;
pub use extractors::{
    get_agent_request_attributes, get_agent_response_attributes, get_common_attributes,
    get_format_request_attributes, get_format_response_attributes, get_function_name,
    get_llm_request_attributes, get_llm_response_attributes, get_tool_request_attributes,
    get_tool_response_attributes, Attribute, Attributes, FormatterConverter, ProviderNameConverter,
};
pub use model::{
    BlobPart, FilePart, InputMessage, MessagePart, OutputMessage, ReasoningPart, Role,
    SystemInstructions, TextPart, ToolCallRequestPart, ToolCallResponsePart, ToolDefinition, UriPart,
};
