//! Middleware layer for intercepting agent execution.
//! Mirrors `io.agentscope.core.middleware`.

pub mod input;
pub mod middleware;
pub mod task_context_projection;
pub mod task_reminder;

pub use input::{
    ActingInput, ActingResultsHandle, ActingResultsSnapshot, AgentInput, EventStream, ModelCallInput,
    ReasoningInput,
};
pub use middleware::{ExtensionPoint, Middleware, MiddlewareChain, Next};
pub use task_context_projection::{
    project, project_all, TASK_CONTEXT_STATE_CONTEXT_KEY, TASK_STATE_MESSAGE_ID,
    TASK_STATE_REMINDER_KIND,
};
pub use task_reminder::TaskReminderMiddleware;
