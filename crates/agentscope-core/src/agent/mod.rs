//! Agent abstraction layer, mirroring `io.agentscope.core.agent`.

pub mod agent_traits;
pub mod lifecycle;
pub mod react_agent;
pub mod runtime_context;
pub mod stream_options;

pub use agent_traits::{Agent, CallableAgent, ObservableAgent};
pub use lifecycle::{CallGate, ExecutionGuard};
pub use react_agent::{ConfirmHandler, ReActAgent, ReActAgentBuilder};
pub use runtime_context::RuntimeContext;
pub use stream_options::{EventCategory, StreamOptions, StreamOptionsBuilder};
