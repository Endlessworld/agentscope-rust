//! AG-UI protocol support, mirroring `io.agentscope.core.agui`.
//!
//! Sub-modules (mirroring the Java package layout):
//!
//! * [`event`] - the AG-UI event taxonomy ([`event::AguiEvent`]).
//! * [`model`] - the message/content and request model.
//! * [`converter`] - `Msg`/state/tool conversions.
//! * [`encoder`] - SSE framing for outgoing events.
//! * [`adapter`] - agent adaptation and event-conversion strategies.
//! * [`processor`] - request processing and interrupt resume coordination.
//! * [`registry`] - agent registration/lookup.
//! * [`runtime`] - request-scoped runtime context resolution.
//! * [`error`] / [`constants`] - AG-UI errors and interrupt wire constants.

pub mod adapter;
pub mod constants;
pub mod converter;
pub mod encoder;
pub mod error;
pub mod event;
pub mod model;
pub mod processor;
pub mod registry;
pub mod runtime;

pub use adapter::{
    AgentEventConverterRegistry, AguiAdapterConfig, AguiAdapterConfigBuilder, AguiAgentAdapter,
    AguiAgentAdapterFactory, AguiEventEnricher, AguiStreamContext, AguiStreamableAgent,
    DefaultAguiAgentAdapterFactory,
};
pub use constants::{
    INPUT_REQUIRED_INTERRUPT_REASON, INTERRUPT_KIND_PERMISSION_CONFIRM,
    METADATA_AGENTSCOPE_INTERRUPT_KIND, METADATA_REPLY_ID, METADATA_TOOL_CONTENT,
    METADATA_TOOL_INPUT, METADATA_TOOL_NAME, TOOL_CALL_INTERRUPT_REASON,
};
pub use converter::{AguiMessageConverter, AguiStateConverter, AguiToolConverter};
pub use encoder::AguiEventEncoder;
pub use error::{AguiError, AguiResult};
pub use event::{AguiEvent, AguiEventType, AguiEvents, Interrupt, JsonPatchOperation};
pub use model::{
    AguiContext, AguiFunctionCall, AguiMessage, AguiResume, AguiTool, AguiToolCall, InputContent,
    InputContentSource, MessageContent, RunAgentInput, ToolMergeMode,
};
pub use processor::{
    AgentResolver, AguiRequestProcessor, AguiRequestProcessorBuilder, AguiResumeCoordinator,
    ProcessResult,
};
pub use registry::{AgentFactory, AguiAgentRegistry};
pub use runtime::{
    AguiRequestBodyParser, AguiRuntimeContextRequest, AguiRuntimeContextResolver, AguiTransport,
};
