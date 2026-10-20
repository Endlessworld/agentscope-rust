//! Middleware layer for intercepting agent execution.
//! Mirrors `io.agentscope.core.middleware`.

pub mod input;
pub mod middleware;
pub mod task_reminder;

pub use input::{
    ActingInput, ActingResultsHandle, ActingResultsSnapshot, AgentInput, EventStream, ModelCallInput,
    ReasoningInput,
};
pub use middleware::{ExtensionPoint, Middleware, MiddlewareChain, Next};
pub use task_reminder::TaskReminderMiddleware;
