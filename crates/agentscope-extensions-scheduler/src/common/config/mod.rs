//! Scheduler configuration layer, mirroring
//! `io.agentscope.extensions.scheduler.config`.
//!
//! These types are the **shared abstraction layer** of this crate: both the
//! `quartz` and `xxljob` backends consume them. They are intentionally free of
//! any backend-specific types.

mod schedule;
mod spec;

pub use schedule::{ScheduleConfig, ScheduleConfigBuilder, ScheduleConfigError, ScheduleMode};
pub use spec::{
    AgentConfig, AgentConfigBuilder, AgentConfigError, ModelConfig, RuntimeAgentConfig,
    RuntimeAgentConfigBuilder, StaticModelConfig,
};
