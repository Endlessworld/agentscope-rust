//! XXL-Job backend, mirroring
//! `io.agentscope.extensions.scheduler.xxljob.XxlJobAgentScheduler`.
//!
//! Java integrates with the external XXL-Job executor and only *registers* a
//! `JobHandler` per agent; the actual cron schedule lives in the XXL-Job admin
//! console. Rust has no xxl-job SDK, so we provide an equivalent in-process
//! `JobHandler` registry plus a local `trigger` used by tests and by a host
//! application that bridges an external scheduler. The executor protocol itself
//! (registry/beat/run/idleBeat/kill/log endpoints) stays in the host: this crate
//! only defines its contract in [`bridge`].
//!
//! Semantics preserved from Java:
//! * only `ScheduleMode::NONE` is accepted at registration time;
//! * registration is keyed by agent name (= JobHandler name);
//! * the trigger reads the dispatch metadata from [`XxlJobContext`]
//!   (`getJobParam()`, sharding info, log id) and writes back through
//!   [`XxlJobHelper`] (`log`, `handleSuccess` / `handleFail`);
//! * `cancel` is **unsupported** — see [`XxlJobAgentScheduler::try_cancel`], the
//!   error-carrying equivalent of Java's `UnsupportedOperationException`;
//! * `shutdown` clears all handlers (Java additionally destroys the executor).

pub mod bridge;
pub mod context;
mod log;

pub use bridge::{DispatchRequest, ExecuteResponse, ExecutorBridge};
pub use context::{XxlJobContext, XxlJobHandleCode, XxlJobHandleResult, XxlJobHelper};
pub use log::XxlJobLogMiddleware;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use agentscope_core::message::Msg;

use crate::common::config::{AgentConfig, RuntimeAgentConfig, ScheduleConfig, ScheduleMode};
use crate::common::scheduler::scheduler::{AgentScheduler, SchedulerError};
use crate::common::scheduler::task::{AgentFactory, BaseScheduleAgentTask, ScheduleAgentTask};

/// The exact message Java throws from `cancel`.
const CANCEL_UNSUPPORTED: &str = "Cancel operation is not supported by XxlJobAgentScheduler. \
     Please use XXL-Job admin console to pause or delete the job.";

/// A registered job handler for one agent.
struct JobHandler {
    task: Arc<BaseScheduleAgentTask>,
}

/// AgentScheduler implementation backed by an in-process XXL-Job-style registry.
pub struct XxlJobAgentScheduler {
    factory: Arc<dyn AgentFactory>,
    handlers: RwLock<HashMap<String, JobHandler>>,
    shutdown: AtomicBool,
}

impl XxlJobAgentScheduler {
    /// Creates a new scheduler with the given agent factory.
    pub fn new(factory: Arc<dyn AgentFactory>) -> Self {
        Self {
            factory,
            handlers: RwLock::new(HashMap::new()),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Registers using a runtime config (carries model/toolkit for execution).
    pub fn schedule_runtime(
        &self,
        runtime: Arc<RuntimeAgentConfig>,
        schedule_config: ScheduleConfig,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(SchedulerError::new("Scheduler has been shut down"));
        }
        if schedule_config.schedule_mode() != ScheduleMode::NONE {
            return Err(SchedulerError::new(
                "XXL-Job scheduler implementation currently only supports scheduleType 'none'. \
                 Other types are not supported yet.",
            ));
        }
        let name = runtime.name().to_string();
        if name.trim().is_empty() {
            return Err(SchedulerError::new("Agent name must not be null or empty"));
        }
        if let Some(existing) = self.get_scheduled_agent(&name) {
            return Ok(existing);
        }

        // Java appends its `PostReasoningEvent` -> `XxlJobHelper.log` hook to the
        // caller's hooks list (`XxlJobAgentScheduler:189-201`). Rust configs are
        // immutable, so the effective config is a copy carrying the extra
        // middleware; a factory that builds its agent from this config's
        // `middleware()` chain gets every reasoning message logged.
        let runtime = Arc::new(runtime.with_middleware(XxlJobLogMiddleware::shared()));

        let task = Arc::new(BaseScheduleAgentTask::new(
            runtime,
            schedule_config,
            self.factory.clone(),
        ));
        self.handlers
            .write()
            .unwrap()
            .insert(name.clone(), JobHandler { task: task.clone() });
        tracing::info!(handler = %name, "registered XXL-Job handler");
        Ok(task)
    }

    /// Java's single-argument `schedule(AgentConfig)` overload: registers with the
    /// default schedule config (`ScheduleMode::NONE`).
    pub fn schedule_default(
        &self,
        agent_config: AgentConfig,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError> {
        let runtime = RuntimeAgentConfig::from(agent_config);
        self.schedule_runtime(Arc::new(runtime), ScheduleConfig::none())
    }

    /// Triggers a registered handler locally (stand-in for an XXL-Job dispatch),
    /// passing `job_param` as the user message — mirrors Java `executeAgent`.
    ///
    /// The dispatch runs inside a fresh [`XxlJobContext`] so the same helper API
    /// as in Java (`XxlJobHelper::log`, `handle_success`, `get_job_param`, …) is
    /// available to the factory. Use [`trigger_with_context`](Self::trigger_with_context)
    /// to supply dispatch metadata (job id, sharding, log id) explicitly.
    pub async fn trigger(&self, name: &str, job_param: &str) -> Result<Msg, SchedulerError> {
        let context = Arc::new(XxlJobContext::new(0, job_param));
        self.trigger_with_context(name, context).await
    }

    /// Triggers a handler with an explicit XXL-Job dispatch context.
    ///
    /// Mirrors Java `executeAgent(BaseScheduleAgentTask)`: the job parameter
    /// becomes the user message, the execution runs inside the dispatch context,
    /// each reasoning step is logged through [`XxlJobLogMiddleware`], and
    /// `handleSuccess` / `handleFail` is reported.
    pub async fn trigger_with_context(
        &self,
        name: &str,
        context: Arc<XxlJobContext>,
    ) -> Result<Msg, SchedulerError> {
        let task = self
            .get_scheduled_agent(name)
            .ok_or_else(|| SchedulerError::new(format!("No such handler: {name}")))?;
        let task_name = name.to_string();
        let input = Msg::user(context.job_param().to_string());
        let context_for_run = context.clone();

        XxlJobContext::scope(context_for_run, async move {
            tracing::info!(
                handler = %task_name,
                job_id = XxlJobHelper::get_job_id().unwrap_or_default(),
                shard = XxlJobHelper::get_shard_index().unwrap_or_default(),
                shards = XxlJobHelper::get_shard_total().unwrap_or_default(),
                log_id = XxlJobHelper::get_log_id().unwrap_or_default(),
                "executing XXL-Job handler"
            );
            match task.run(vec![input]).await {
                Ok(reply) => {
                    // Java logs each reasoning step through the injected hook
                    // (`XxlJobLogMiddleware`) and only reports the outcome here; the
                    // final reply itself is not written to the dispatch log.
                    XxlJobHelper::handle_success(format!(
                        "Successfully executed scheduled task '{task_name}'"
                    ));
                    Ok(reply)
                }
                Err(err) => {
                    let message = format!("Task execution failed: {task_name}: {err}");
                    XxlJobHelper::handle_fail(message.clone());
                    Err(SchedulerError::new(message))
                }
            }
        })
        .await
    }

    /// Cancels a handler.
    ///
    /// Java throws `UnsupportedOperationException` unconditionally; this is the
    /// faithful Rust counterpart used by callers that need to distinguish
    /// "unsupported" from "not found".
    pub fn try_cancel(&self, _name: &str) -> Result<(), SchedulerError> {
        Err(SchedulerError::new(CANCEL_UNSUPPORTED))
    }

    /// Lists registered handler names.
    pub fn registered_handlers(&self) -> Vec<String> {
        self.handlers.read().unwrap().keys().cloned().collect()
    }
}

#[async_trait]
impl AgentScheduler for XxlJobAgentScheduler {
    async fn schedule(
        &self,
        agent_config: AgentConfig,
        schedule_config: ScheduleConfig,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError> {
        let runtime = RuntimeAgentConfig::from(agent_config);
        self.schedule_runtime(Arc::new(runtime), schedule_config)
    }

    async fn cancel(&self, _name: &str) -> bool {
        // Java throws UnsupportedOperationException. The boolean trait method
        // cannot express that, so it logs and returns `false`; callers that need
        // the Java semantics use `try_cancel` (see `AgentScheduler::try_cancel`).
        tracing::warn!(
            "Cancel operation is not supported by XxlJobAgentScheduler; \
             use the XXL-Job admin console to pause or delete the job."
        );
        false
    }

    async fn try_cancel(&self, name: &str) -> Result<bool, SchedulerError> {
        self.try_cancel(name).map(|()| true)
    }

    fn get_scheduled_agent(&self, name: &str) -> Option<Arc<dyn ScheduleAgentTask>> {
        if name.trim().is_empty() {
            return None;
        }
        self.handlers
            .read()
            .unwrap()
            .get(name)
            .map(|h| h.task.clone() as Arc<dyn ScheduleAgentTask>)
    }

    fn all_schedule_agent_tasks(&self) -> Vec<Arc<dyn ScheduleAgentTask>> {
        self.handlers
            .read()
            .unwrap()
            .values()
            .map(|h| h.task.clone() as Arc<dyn ScheduleAgentTask>)
            .collect()
    }

    async fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        self.handlers.write().unwrap().clear();
        tracing::info!("XxlJobAgentScheduler shutdown completed");
    }

    fn scheduler_type(&self) -> &'static str {
        "xxl-job"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use agentscope_core::error::{Error, Result as CoreResult};

    use crate::test_support::EchoFactory;

    fn runtime(name: &str) -> Arc<RuntimeAgentConfig> {
        Arc::new(
            RuntimeAgentConfig::builder()
                .name(name)
                .model_name("m")
                .sys_prompt("sys")
                .model(crate::test_support::stub_model("reasoned"))
                .build()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn schedule_appends_the_reasoning_log_middleware() {
        // Java appends its `PostReasoningEvent` logging hook to the config at
        // registration time; the Rust equivalent is a middleware appended to the
        // effective (copied) runtime config.
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        let input = runtime("A");
        assert!(input.middleware().is_empty());
        let task = sched.schedule_runtime(input.clone(), ScheduleConfig::none()).unwrap();

        let base = task
            .as_any()
            .downcast_ref::<crate::common::scheduler::BaseScheduleAgentTask>()
            .expect("xxl-job stores a BaseScheduleAgentTask");
        assert_eq!(base.agent_config().middleware().len(), 1);
        // The caller's config is left untouched (Rust configs are immutable).
        assert!(input.middleware().is_empty());
    }

    /// Factory that builds its agent the way production does: the model and the
    /// middleware chain come from the runtime config, so the hook attached by
    /// `schedule_runtime` actually runs.
    struct ConfigDrivenFactory;

    #[async_trait]
    impl AgentFactory for ConfigDrivenFactory {
        async fn run(&self, config: &RuntimeAgentConfig, messages: Vec<Msg>) -> CoreResult<Msg> {
            let mut chain = agentscope_core::middleware::MiddlewareChain::new();
            for mw in config.middleware() {
                chain.add_arc(mw.clone());
            }
            let agent = agentscope_core::agent::ReActAgent::builder()
                .name(config.name())
                .sys_prompt(config.sys_prompt().unwrap_or_default())
                .model(config.model().cloned().expect("model configured"))
                .middleware_chain(chain)
                .build()?;
            agent.call(messages).await
        }
    }

    #[tokio::test]
    async fn dispatch_logs_each_reasoning_step_through_the_attached_middleware() {
        // End-to-end through the real assembly path: scheduler -> task -> factory
        // -> ReActAgent -> core `on_reasoning_result` -> `XxlJobHelper.log`.
        let sched = XxlJobAgentScheduler::new(Arc::new(ConfigDrivenFactory));
        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none())
            .unwrap();

        let ctx = Arc::new(XxlJobContext::new(5, "hello").with_log_id(11));
        let reply = sched.trigger_with_context("A", ctx.clone()).await.unwrap();
        assert_eq!(reply.text_content(), "reasoned");

        // Java: every `PostReasoningEvent` reasoning message goes to the job log.
        assert_eq!(ctx.logs(), vec!["reasoned".to_string()]);
        let handle = ctx.handle_result().expect("handle result reported");
        assert_eq!(handle.code(), XxlJobHandleCode::Success);
    }

    #[tokio::test]
    async fn registers_handler_and_triggers() {
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        sched
            .schedule_runtime(runtime("MyAgent"), ScheduleConfig::none())
            .unwrap();
        assert_eq!(sched.registered_handlers(), vec!["MyAgent".to_string()]);
        let out = sched.trigger("MyAgent", "payload").await.unwrap();
        assert!(out.text_content().starts_with("echo:"));
        assert_eq!(sched.scheduler_type(), "xxl-job");
    }

    #[tokio::test]
    async fn rejects_non_none_mode() {
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        let cfg = ScheduleConfig::builder()
            .cron("0 0 8 * * ?")
            .build()
            .unwrap();
        assert!(sched.schedule_runtime(runtime("A"), cfg).is_err());
    }

    #[tokio::test]
    async fn duplicate_registration_returns_existing() {
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        let a = sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none())
            .unwrap();
        let b = sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none())
            .unwrap();
        assert_eq!(a.id(), b.id());
    }

    #[tokio::test]
    async fn cancel_is_unsupported_shutdown_clears() {
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none())
            .unwrap();
        assert!(!sched.cancel("A").await);
        assert!(sched.get_scheduled_agent("A").is_some());
        sched.shutdown().await;
        assert!(sched.all_schedule_agent_tasks().is_empty());
    }

    #[tokio::test]
    async fn trigger_unknown_handler_errors() {
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        assert!(sched.trigger("missing", "").await.is_err());
    }

    #[tokio::test]
    async fn try_cancel_mirrors_java_unsupported_operation() {
        // Java throws `UnsupportedOperationException` for cancel regardless of
        // whether the handler exists or has already run.
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        let err = sched.try_cancel("never-registered").unwrap_err();
        assert!(
            err.to_string()
                .contains("Cancel operation is not supported"),
            "err was {err}"
        );
        assert!(
            err.to_string().contains("XXL-Job admin console"),
            "err was {err}"
        );

        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none())
            .unwrap();
        sched.trigger("A", "p").await.unwrap();
        let err = sched.try_cancel("A").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Cancel operation is not supported by XxlJobAgentScheduler. \
             Please use XXL-Job admin console to pause or delete the job."
        );
        // The trait-level counterpart reports the same error.
        assert!(AgentScheduler::try_cancel(&sched, "A").await.is_err());
    }

    /// Factory that records the messages it received and can run a closure inside
    /// the dispatch context (to exercise `XxlJobHelper`).
    struct ContextFactory {
        seen: Mutex<Vec<String>>,
        fail: bool,
    }

    #[async_trait]
    impl AgentFactory for ContextFactory {
        async fn run(&self, _config: &RuntimeAgentConfig, messages: Vec<Msg>) -> CoreResult<Msg> {
            let text = messages
                .first()
                .map(|m| m.text_content())
                .unwrap_or_default();
            self.seen.lock().unwrap().push(text.clone());
            // The helper API must resolve the dispatch of the running execution.
            assert_eq!(
                XxlJobHelper::get_job_param().as_deref(),
                Some(text.as_str())
            );
            XxlJobHelper::log(format!("received: {text}"));
            if self.fail {
                return Err(Error::Agent("factory boom".into()));
            }
            Ok(Msg::assistant(format!("reply:{text}")))
        }
    }

    #[tokio::test]
    async fn trigger_passes_job_param_and_reports_success() {
        let factory = Arc::new(ContextFactory {
            seen: Mutex::new(Vec::new()),
            fail: false,
        });
        let sched = XxlJobAgentScheduler::new(factory.clone());
        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none())
            .unwrap();

        let ctx = Arc::new(
            XxlJobContext::new(9, "hello")
                .with_sharding(0, 2)
                .with_log_id(77),
        );
        let out = sched.trigger_with_context("A", ctx.clone()).await.unwrap();
        assert_eq!(out.text_content(), "reply:hello");
        assert_eq!(factory.seen.lock().unwrap().as_slice(), ["hello"]);

        // §XxlJobHelper.handleSuccess + log protocol.
        let handle = ctx.handle_result().expect("handle result reported");
        assert_eq!(handle.code(), XxlJobHandleCode::Success);
        assert_eq!(handle.message(), "Successfully executed scheduled task 'A'");
        let logs = ctx.logs();
        assert!(
            logs.iter().any(|l| l == "received: hello"),
            "logs: {logs:?}"
        );
        // The final reply is no longer copied into the log by the scheduler: Java
        // logs each reasoning step through its hook (see
        // `dispatch_logs_each_reasoning_step_through_the_attached_middleware`), and
        // this factory does not run an agent, so nothing else is logged.
        assert!(!logs.iter().any(|l| l == "reply:hello"), "logs: {logs:?}");
        // The context keeps the dispatch metadata available after the run.
        assert_eq!(ctx.job_id(), 9);
        assert_eq!(ctx.shard_index(), 0);
        assert_eq!(ctx.shard_total(), 2);
        assert_eq!(ctx.log_id(), 77);
    }

    #[tokio::test]
    async fn trigger_reports_handle_fail_on_error() {
        let factory = Arc::new(ContextFactory {
            seen: Mutex::new(Vec::new()),
            fail: true,
        });
        let sched = XxlJobAgentScheduler::new(factory);
        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none())
            .unwrap();

        let ctx = Arc::new(XxlJobContext::new(1, "boom-param"));
        let err = sched
            .trigger_with_context("A", ctx.clone())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Task execution failed: A"),
            "err was {err}"
        );

        let handle = ctx.handle_result().expect("handle result reported");
        assert_eq!(handle.code(), XxlJobHandleCode::Fail);
        assert!(handle.message().contains("Task execution failed: A"));
    }
}
