//! Shared scheduler abstractions and the default task implementation.
//!
//! Mirrors `io.agentscope.extensions.scheduler.{AgentScheduler, ScheduleAgentTask,
//! BaseScheduleAgentTask}`. This is the crate-internal shared layer consumed by
//! both backends.

pub mod scheduler;
pub mod task;

pub use scheduler::{AgentScheduler, SchedulerError};
pub use task::{AgentFactory, BaseScheduleAgentTask, ScheduleAgentTask, TaskControl};
