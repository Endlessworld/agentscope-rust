//! agentscope-extensions-scheduler — scheduled agent execution.
//!
//! Rust port of `agentscope-extensions-scheduler` (wave C), composed of three
//! Java submodules mapped to crate modules:
//! * [`common`]    — shared config + `AgentScheduler` / `ScheduleAgentTask` contracts
//!   (the crate-internal shared layer).
//! * [`quartz`]    — Quartz-equivalent scheduler built on tokio timers.
//! * [`xxljob`]    — XXL-Job-style job handler registry, plus the host bridge
//!   contract for the executor protocol ([`ExecutorBridge`]).
//!
//! # Deployment premise
//!
//! * **One scheduler instance per schedule.** Tasks live in a process-local map
//!   with no shared store, so two instances scheduling the same CRON would both
//!   fire it (tracker `SCHED-05`). A host that really runs several instances
//!   injects a [`SchedulerLock`] backed by its storage layer
//!   (`QuartzAgentSchedulerBuilder::scheduler_lock`); the default
//!   [`NoopSchedulerLock`] grants every slot, which *is* the single-instance
//!   premise. Manual `trigger`/`run` calls always execute locally, like Java
//!   `triggerJob`.
//! * **Concurrency is bounded** by a semaphore (tracker `SCHED-09`): scheduled
//!   firings of one scheduler run at most
//!   [`QuartzAgentScheduler::DEFAULT_MAX_CONCURRENCY`] (10, Quartz's default
//!   thread-pool size) at a time unless the builder sets another limit. Manual
//!   `trigger`/`run` calls are not throttled.
//! * **xxl-job transport belongs to the host** (tracker `SCHED-06`): the crate
//!   embeds no HTTP server and exposes [`ExecutorBridge`] with the
//!   [`DispatchRequest`] → `XxlJobContext` field mapping instead.
//!
//! See `docs/extensions/SCHEDULER__GAP_TRACKER.md` for migration status.

pub mod common;
pub mod quartz;
pub mod xxljob;

pub use common::{
    AgentConfig, AgentConfigBuilder, AgentConfigError, AgentFactory, AgentScheduler,
    BaseScheduleAgentTask, ModelConfig, NoopSchedulerLock, RuntimeAgentConfig,
    RuntimeAgentConfigBuilder, ScheduleAgentTask, ScheduleConfig, ScheduleConfigBuilder,
    ScheduleConfigError, ScheduleMode, ScheduleZoneError, SchedulerError, SchedulerLock,
    StaticModelConfig, TaskControl,
};
pub use quartz::{
    CronExpression, CronParseError, QuartzAgentScheduler, QuartzAgentSchedulerBuilder,
    QuartzScheduleAgentTask, SchedulerRegistry, TaskStatus,
};
pub use xxljob::{
    DispatchRequest, ExecuteResponse, ExecutorBridge, XxlJobAgentScheduler, XxlJobContext,
    XxlJobHandleCode, XxlJobHandleResult, XxlJobHelper, XxlJobLogMiddleware,
};

/// Test-only helpers shared across module unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use agentscope_core::error::Result;
    use agentscope_core::message::Msg;
    use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};

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

    /// A model that never reaches a provider.
    ///
    /// Execution-time validation requires a non-null model (Java
    /// `Model cannot be null.`), so every test config carries this stub. The
    /// factory-based tests never call it; only the tests that build a real
    /// `ReActAgent` stream from it.
    pub(crate) struct StubModel {
        reply: String,
    }

    impl StubModel {
        pub(crate) fn new(reply: impl Into<String>) -> Self {
            Self {
                reply: reply.into(),
            }
        }
    }

    impl Model for StubModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> futures::stream::BoxStream<'_, Result<ChatResponse>> {
            let reply = self.reply.clone();
            Box::pin(futures::stream::once(async move {
                Ok(ChatResponse::builder()
                    .content(vec![agentscope_core::message::ContentBlock::Text(
                        agentscope_core::message::TextBlock::new(reply),
                    )])
                    .build())
            }))
        }

        fn model_name(&self) -> &str {
            "stub-model"
        }
    }

    /// A ready-to-use stub model behind the `Arc` every config expects.
    pub(crate) fn stub_model(reply: &str) -> std::sync::Arc<dyn Model> {
        std::sync::Arc::new(StubModel::new(reply))
    }
}
