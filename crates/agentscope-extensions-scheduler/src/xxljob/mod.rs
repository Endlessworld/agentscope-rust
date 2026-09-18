//! XXL-Job backend, mirroring
//! `io.agentscope.extensions.scheduler.xxljob.XxlJobAgentScheduler`.
//!
//! Java integrates with the external XXL-Job executor and only *registers* a
//! `JobHandler` per agent; the actual cron schedule lives in the XXL-Job admin
//! console. Rust has no xxl-job SDK, so we provide an equivalent in-process
//! `JobHandler` registry plus a local `trigger(name, params)` used by tests and
//! by a host application that bridges an external scheduler.
//!
//! Semantics preserved from Java:
//! * only `ScheduleMode::NONE` is accepted at registration time;
//! * registration is keyed by agent name (= JobHandler name);
//! * `cancel` is **unsupported** and returns an error;
//! * `shutdown` clears all handlers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use agentscope_core::message::Msg;

use crate::common::config::{AgentConfig, RuntimeAgentConfig, ScheduleConfig, ScheduleMode};
use crate::common::scheduler::scheduler::{AgentScheduler, SchedulerError};
use crate::common::scheduler::task::{AgentFactory, BaseScheduleAgentTask, ScheduleAgentTask};

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
        Self { factory, handlers: RwLock::new(HashMap::new()), shutdown: AtomicBool::new(false) }
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

    /// Triggers a registered handler locally (stand-in for an XXL-Job dispatch),
    /// passing `job_param` as the user message — mirrors Java `executeAgent`.
    pub async fn trigger(&self, name: &str, job_param: &str) -> Result<Msg, SchedulerError> {
        let task = self
            .get_scheduled_agent(name)
            .ok_or_else(|| SchedulerError::new(format!("No such handler: {name}")))?;
        let input = Msg::user(job_param.to_string());
        task.run(vec![input])
            .await
            .map_err(|e| SchedulerError::new(e.to_string()))
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
        // Java throws UnsupportedOperationException; Rust has no exceptions, so
        // we surface the unsupported operation via `false`.
        tracing::warn!(
            "Cancel operation is not supported by XxlJobAgentScheduler; \
             use the XXL-Job admin console to pause or delete the job."
        );
        false
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
    use crate::test_support::EchoFactory;

    fn runtime(name: &str) -> Arc<RuntimeAgentConfig> {
        Arc::new(
            RuntimeAgentConfig::builder()
                .name(name)
                .model_name("m")
                .sys_prompt("sys")
                .build()
                .unwrap(),
        )
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
        let cfg = ScheduleConfig::builder().cron("0 0 8 * * ?").build().unwrap();
        assert!(sched.schedule_runtime(runtime("A"), cfg).is_err());
    }

    #[tokio::test]
    async fn duplicate_registration_returns_existing() {
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        let a = sched.schedule_runtime(runtime("A"), ScheduleConfig::none()).unwrap();
        let b = sched.schedule_runtime(runtime("A"), ScheduleConfig::none()).unwrap();
        assert_eq!(a.id(), b.id());
    }

    #[tokio::test]
    async fn cancel_is_unsupported_shutdown_clears() {
        let sched = XxlJobAgentScheduler::new(Arc::new(EchoFactory));
        sched.schedule_runtime(runtime("A"), ScheduleConfig::none()).unwrap();
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
}
