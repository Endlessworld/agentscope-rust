//! `AgentScheduler` trait, mirroring
//! `io.agentscope.extensions.scheduler.AgentScheduler`.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use agentscope_core::message::Msg;

use crate::common::config::{AgentConfig, ScheduleConfig};
use crate::common::scheduler::task::ScheduleAgentTask;

/// Errors surfaced by scheduler backends.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct SchedulerError(pub String);

impl SchedulerError {
    /// Creates a new error with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Core contract for scheduling agents, mirroring Java `AgentScheduler`.
#[async_trait]
pub trait AgentScheduler: Send + Sync {
    /// Schedules an agent using serializable metadata only.
    ///
    /// Implementations that need runtime objects should accept an
    /// [`crate::common::config::RuntimeAgentConfig`] via
    /// [`schedule_runtime`](Self::schedule_runtime).
    async fn schedule(
        &self,
        agent_config: AgentConfig,
        schedule_config: ScheduleConfig,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError>;

    /// Schedules an agent passing an initial input message.
    async fn schedule_with_input(
        &self,
        agent_config: AgentConfig,
        schedule_config: ScheduleConfig,
        input: Msg,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError> {
        let _ = input;
        let _ = (agent_config, schedule_config);
        Err(SchedulerError::new(
            "This scheduler implementation does not support initial input.",
        ))
    }

    /// Cancels and removes a scheduled task by name.
    async fn cancel(&self, name: &str) -> bool;

    /// Rust-idiomatic counterpart of Java's `cancel`.
    ///
    /// Java backends signal "cancel is not supported" by throwing
    /// `UnsupportedOperationException`; the boolean [`cancel`](Self::cancel) cannot
    /// express that, so backends that do not support cancellation override this
    /// method with an error. The default simply forwards to `cancel`.
    async fn try_cancel(&self, name: &str) -> Result<bool, SchedulerError> {
        Ok(self.cancel(name).await)
    }

    /// Retrieves a scheduled task by name.
    fn get_scheduled_agent(&self, name: &str) -> Option<Arc<dyn ScheduleAgentTask>>;

    /// Retrieves all scheduled tasks.
    fn all_schedule_agent_tasks(&self) -> Vec<Arc<dyn ScheduleAgentTask>>;

    /// Gracefully shuts the scheduler down.
    async fn shutdown(&self);

    /// The scheduler type identifier (e.g. `"quartz"`, `"xxl-job"`).
    fn scheduler_type(&self) -> &'static str;
}
