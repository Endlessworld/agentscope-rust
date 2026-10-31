//! Shared abstraction layer for the scheduler extension.
//!
//! Mirrors `agentscope-extensions-scheduler-common`: configuration types plus the
//! `AgentScheduler` / `ScheduleAgentTask` contracts. Both the `quartz` and
//! `xxljob` backends build on these.

pub mod config;
pub mod scheduler;

pub use config::{
    AgentConfig, AgentConfigBuilder, AgentConfigError, ModelConfig, RuntimeAgentConfig,
    RuntimeAgentConfigBuilder, ScheduleConfig, ScheduleConfigBuilder, ScheduleConfigError,
    ScheduleMode, ScheduleZoneError, StaticModelConfig,
};
pub use scheduler::{
    AgentFactory, AgentScheduler, BaseScheduleAgentTask, NoopSchedulerLock, ScheduleAgentTask,
    SchedulerError, SchedulerLock, TaskControl,
};
