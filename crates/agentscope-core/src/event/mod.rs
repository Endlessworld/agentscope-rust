//! Fine-grained agent events, mirroring `io.agentscope.core.event`.

pub mod emitter;
pub mod event;
pub mod event_type;

pub use emitter::{AgentEventEmitter, ConfirmResult, CONTEXT_KEY, FORWARDING_CONTEXT_KEY};
pub use event::{AgentEvent, EventData};
pub use event_type::AgentEventType;
