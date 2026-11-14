//! Agent abstraction layer, mirroring `io.agentscope.core.agent`.

pub mod agent_traits;
pub mod capability;
pub mod execution_host;
pub mod execution_position;
pub mod failover;
pub mod lifecycle;
pub mod react_agent;
pub mod run;
pub mod run_outcome;
pub mod runtime_context;
pub mod stream_options;
mod thread_key;

pub use execution_host::{
    execution_host, AgentExecutionHost, ExecutionBoundary, FinishContinuation,
    EXECUTION_HOST_CONTEXT_KEY,
};
pub use execution_position::{
    execution_position, AgentExecutionPosition, EXECUTION_POSITION_CONTEXT_KEY,
};
pub use run_outcome::{RunOutcome, RunOutcomeStatus};
pub use thread_key::ThreadKey;

pub use agent_traits::{Agent, CallableAgent, ObservableAgent};
pub use capability::{
    capability_surface_from_context, CapabilityResolver, CapabilitySurface,
    CAPABILITY_SURFACE_CONTEXT_KEY, CAPABILITY_SURFACE_RUNTIME_KEY,
};
pub use failover::FailoverListener;
pub use lifecycle::{CallGate, ExecutionGuard};
pub use react_agent::{ConfirmHandler, ReActAgent, ReActAgentBuilder};
pub use run::{
    generate_run_id, AgentRun, CancelSignal, RunControl, RunStatus, CANCEL_SIGNAL_CONTEXT_KEY,
    RUN_CONTROL_CONTEXT_KEY,
};
pub use runtime_context::{
    normalize_agent_id, Extensions, RuntimeContext, DEFAULT_AGENT_ID, MAX_AGENT_ID_LENGTH,
    MODEL_CONTEXT_KEY, MODEL_ID_CONTEXT_KEY,
};
pub use stream_options::{EventCategory, StreamOptions, StreamOptionsBuilder};
