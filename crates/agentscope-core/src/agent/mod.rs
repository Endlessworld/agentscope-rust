//! Agent abstraction layer, mirroring `io.agentscope.core.agent`.

pub mod agent_traits;
pub mod lifecycle;
pub mod react_agent;
pub mod run;
pub mod runtime_context;
pub mod stream_options;

pub use agent_traits::{Agent, CallableAgent, ObservableAgent};
pub use lifecycle::{CallGate, ExecutionGuard};
pub use react_agent::{ConfirmHandler, ReActAgent, ReActAgentBuilder};
pub use run::{
    generate_run_id, AgentRun, CancelSignal, RunControl, RunStatus, CANCEL_SIGNAL_CONTEXT_KEY,
    RUN_CONTROL_CONTEXT_KEY,
};
pub use runtime_context::{
    normalize_agent_id, Extensions, RuntimeContext, DEFAULT_AGENT_ID, MAX_AGENT_ID_LENGTH,
};
pub use stream_options::{EventCategory, StreamOptions, StreamOptionsBuilder};
