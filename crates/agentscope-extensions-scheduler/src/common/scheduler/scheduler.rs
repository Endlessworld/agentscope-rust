//! `AgentScheduler` trait, mirroring
//! `io.agentscope.extensions.scheduler.AgentScheduler`.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use agentscope_core::message::Msg;

use crate::common::config::{AgentConfig, ScheduleConfig};
use crate::common::scheduler::task::ScheduleAgentTask;

/// Errors surfaced by scheduler backends.
///
/// Java distinguishes three failure kinds — `IllegalArgumentException` for bad
/// arguments, `UnsupportedOperationException` for operations a backend does not
/// implement and plain `RuntimeException` for backend failures. The variants
/// keep that distinction so callers can branch on it, while the `Display` text
/// stays byte-identical to the Java messages.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SchedulerError {
    /// The caller passed an invalid argument (Java `IllegalArgumentException`).
    #[error("{0}")]
    InvalidArgument(String),
    /// The backend does not support the operation (Java
    /// `UnsupportedOperationException`).
    #[error("{0}")]
    Unsupported(String),
    /// The backend failed while executing (Java `RuntimeException`).
    #[error("{0}")]
    Backend(String),
    /// The scheduler was already shut down.
    ///
    /// Java has no dedicated type for this (it surfaces as a
    /// `RuntimeException`), but a caller that ignores it would schedule onto a
    /// dead scheduler, so it gets its own variant.
    #[error("Scheduler has been shut down")]
    Shutdown,
}

impl SchedulerError {
    /// Creates an [`SchedulerError::InvalidArgument`].
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    /// Creates an [`SchedulerError::Unsupported`].
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    /// Creates an [`SchedulerError::Backend`].
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend(message.into())
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
        Err(SchedulerError::unsupported(
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
