//! AG-UI agent adaptation, mirroring `io.agentscope.core.agui.adapter`.
//!
//! * [`config`] - `AguiAdapterConfig` (tool merge mode, event toggles, timeouts).
//! * [`strategy`] - `AguiStreamContext`, event converters and enrichers.
//! * [`agent_adapter`] - `AguiAgentAdapter` / `AguiAgentAdapterFactory`.

pub mod agent_adapter;
pub mod config;
pub mod strategy;

pub use agent_adapter::{
    build_runtime_context, map_error_code, put_resume_interrupts, resume_interrupts,
    AguiAgentAdapter, AguiAgentAdapterFactory, AguiEventStream, AguiRunStream, AguiStreamableAgent,
    DefaultAguiAgentAdapterFactory, RUNTIME_CONTEXT_CONTEXT_KEY,
    RUNTIME_CONTEXT_FORWARDED_PROPS_KEY, RUNTIME_CONTEXT_MESSAGES_KEY,
    RUNTIME_CONTEXT_RESUME_INTERRUPTS_KEY, RUNTIME_CONTEXT_RESUME_KEY, RUNTIME_CONTEXT_RUN_ID_KEY,
    RUNTIME_CONTEXT_STATE_KEY, RUNTIME_CONTEXT_THREAD_ID_KEY, RUNTIME_CONTEXT_TOOLS_KEY,
};
pub use config::{
    AguiAdapterConfig, AguiAdapterConfigBuilder, DEFAULT_RUN_TIMEOUT,
};
pub use strategy::{
    AgentEventConverter, AgentEventConverterRegistry, AguiEventEnricher, AguiStreamContext,
    BaseEventPropertiesEnricher, RawEventPropertiesEnricher, TokenUsage, TokenUsageAccumulator,
    TokenUsageSnapshot,
};
