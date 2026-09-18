//! agentscope-extensions-scheduler — scheduled agent execution.
//!
//! Rust port of `agentscope-extensions-scheduler` (wave C), composed of three
//! Java submodules mapped to crate modules:
//! * [`common`]    — shared config + `AgentScheduler` / `ScheduleAgentTask` contracts
//!   (the crate-internal shared layer).
//! * [`quartz`]    — Quartz-equivalent scheduler built on tokio timers.
//! * [`xxljob`]    — XXL-Job-style job handler registry.
//!
//! See `docs/extensions/SCHEDULER__GAP_TRACKER.md` for migration status.

pub mod common;
pub mod quartz;
pub mod xxljob;

pub use common::{
    AgentConfig, AgentConfigBuilder, AgentConfigError, AgentFactory, AgentScheduler,
    BaseScheduleAgentTask, ModelConfig, RuntimeAgentConfig, RuntimeAgentConfigBuilder,
    ScheduleAgentTask, ScheduleConfig, ScheduleConfigBuilder, ScheduleConfigError, ScheduleMode,
    SchedulerError, StaticModelConfig, TaskControl,
};
pub use quartz::{
    CronExpression, CronParseError, QuartzAgentScheduler, QuartzScheduleAgentTask,
    SchedulerRegistry, TaskStatus,
};
pub use xxljob::XxlJobAgentScheduler;

/// Test-only helpers shared across module unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use agentscope_core::error::Result;
    use agentscope_core::message::Msg;

    use crate::common::config::RuntimeAgentConfig;
    use crate::common::scheduler::AgentFactory;

    /// An [`AgentFactory`] that echoes the number of input messages, for tests.
    pub(crate) struct EchoFactory;

    #[async_trait::async_trait]
    impl AgentFactory for EchoFactory {
        async fn run(&self, _config: &RuntimeAgentConfig, messages: Vec<Msg>) -> Result<Msg> {
            Ok(Msg::assistant(format!("echo:{}", messages.len())))
        }
    }
}
