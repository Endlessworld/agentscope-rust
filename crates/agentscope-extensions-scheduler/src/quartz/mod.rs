//! Quartz-style scheduler backend, mirroring
//! `io.agentscope.extensions.scheduler.quartz`.
//!
//! Java uses the Quartz library (Job/Trigger). Rust has no Quartz binding, so we
//! reproduce the same observable semantics with tokio tasks:
//! * `CRON`        — evaluated by the crate-local cron parser, ticking every
//!   second and comparing against the wall clock of `ScheduleConfig::zone_id`
//!   (UTC when unset).
//! * `FIXED_RATE`  — `tokio::time::interval`, constant interval between starts.
//! * `FIXED_DELAY` — `sleep(delay)` after each completion (sequential).
//!
//! `pause` / `resume` / `interrupt` / `getStatus` are preserved.

pub mod cron;
mod job;
mod registry;
mod task;

pub use cron::{CronExpression, CronParseError};
pub use registry::{scheduler_registry, SchedulerRegistry};
pub use task::{QuartzScheduleAgentTask, TaskStatus};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use tokio::sync::Notify;

use agentscope_core::message::Msg;

use crate::common::config::{AgentConfig, RuntimeAgentConfig, ScheduleConfig, ScheduleMode};
use crate::common::scheduler::scheduler::{AgentScheduler, SchedulerError};
use crate::common::scheduler::task::{AgentFactory, ScheduleAgentTask};

use self::job::{spawn_job, CronPlan};

/// AgentScheduler implementation backed by tokio timers (Quartz-equivalent).
pub struct QuartzAgentScheduler {
    scheduler_id: String,
    factory: Arc<dyn AgentFactory>,
    /// name -> running job handle
    jobs: RwLock<HashMap<String, Arc<QuartzScheduleAgentTask>>>,
    shutdown: AtomicBool,
    next_seq: AtomicU64,
}

impl QuartzAgentScheduler {
    /// Creates a new scheduler with the given agent factory.
    pub fn new(factory: Arc<dyn AgentFactory>) -> Self {
        let scheduler_id = uuid::Uuid::new_v4().to_string();
        let sched = Self {
            scheduler_id,
            factory,
            jobs: RwLock::new(HashMap::new()),
            shutdown: AtomicBool::new(false),
            next_seq: AtomicU64::new(0),
        };
        scheduler_registry().register(sched.scheduler_id.clone(), sched.self_handle_hint());
        sched
    }

    /// The unique id of this scheduler instance.
    pub fn scheduler_id(&self) -> &str {
        &self.scheduler_id
    }

    // Registry stores task names keyed by scheduler id; the handle hint is a
    // plain name so the job loop can locate the scheduler without a strong `Arc`.
    fn self_handle_hint(&self) -> String {
        self.scheduler_id.clone()
    }

    /// Schedules using a runtime configuration (carries model/toolkit/middleware).
    pub async fn schedule_runtime(
        &self,
        runtime: Arc<RuntimeAgentConfig>,
        schedule_config: ScheduleConfig,
        input: Option<Msg>,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(SchedulerError::new("Scheduler has been shut down"));
        }
        schedule_config
            .validate()
            .map_err(|e| SchedulerError::new(e.to_string()))?;

        let name = runtime.name().to_string();
        if name.trim().is_empty() {
            return Err(SchedulerError::new("Agent name must not be null or empty"));
        }
        // Already scheduled -> return existing task (mirrors Java warning path).
        if let Some(existing) = self.get_scheduled_agent(&name) {
            return Ok(existing);
        }

        // Java keeps the initial message in the Quartz JobDataMap
        // (`QuartzAgentScheduler.java:233-241`) and reads it back on every
        // firing (`AgentQuartzJob.java:67-101`). The Rust task owns the same
        // payload so both the job loop and manual triggers honour it.
        //
        // Cron + time zone are resolved *before* the task is registered so that
        // an invalid expression/zone fails the call without side effects (Java
        // throws `RuntimeException("CronExpression '…' is invalid.")` and never
        // reaches `tasks.put(...)`).
        let cron = resolve_cron(&schedule_config)?;

        let task = Arc::new(QuartzScheduleAgentTask::new(
            runtime,
            schedule_config.clone(),
            name.clone(),
            self.factory.clone(),
            input,
        ));

        self.jobs
            .write()
            .unwrap()
            .insert(name.clone(), task.clone());

        let handle = spawn_job(self.context(), task.clone(), schedule_config.clone(), cron);
        task.attach(handle);

        tracing::info!(task = %name, mode = schedule_config.schedule_mode().as_str(), "scheduled task");
        Ok(task)
    }

    fn context(&self) -> JobContext {
        JobContext {
            scheduler_id: self.scheduler_id.clone(),
        }
    }

    /// Pauses a task, if present.
    pub fn pause(&self, name: &str) -> bool {
        match self.get_scheduled_agent(name) {
            Some(t) => match t.as_any().downcast_ref::<QuartzScheduleAgentTask>() {
                Some(q) => {
                    q.pause();
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Resumes a paused task, if present.
    pub fn resume(&self, name: &str) -> bool {
        match self.get_scheduled_agent(name) {
            Some(t) => match t.as_any().downcast_ref::<QuartzScheduleAgentTask>() {
                Some(q) => {
                    q.resume();
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Sends an interrupt to a running execution.
    pub fn interrupt(&self, name: &str) -> bool {
        match self.get_scheduled_agent(name) {
            Some(t) => match t.as_any().downcast_ref::<QuartzScheduleAgentTask>() {
                Some(q) => {
                    q.interrupt();
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Returns the current status of a task, if present.
    pub fn get_status(&self, name: &str) -> Option<TaskStatus> {
        self.get_scheduled_agent(name).and_then(|t| {
            t.as_any()
                .downcast_ref::<QuartzScheduleAgentTask>()
                .map(|q| q.status())
        })
    }

    /// Triggers one execution of a task by name (manual run).
    pub async fn trigger(&self, name: &str) -> Result<Msg, SchedulerError> {
        let task = self
            .get_scheduled_agent(name)
            .ok_or_else(|| SchedulerError::new(format!("No such task: {name}")))?;
        let q = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .ok_or_else(|| SchedulerError::new("Unexpected task type"))?;
        q.execute()
            .await
            .map_err(|e| SchedulerError::new(e.to_string()))
    }
}

/// Parses the cron expression and resolves its time zone, mirroring Java's
/// `CronScheduleBuilder.cronSchedule(...)` + `csb.inTimeZone(...)`.
///
/// Returns `None` for non-CRON modes. Called before the task is registered so a
/// bad expression or zone id has no side effects (Java throws before
/// `tasks.put(...)`).
fn resolve_cron(schedule_config: &ScheduleConfig) -> Result<Option<CronPlan>, SchedulerError> {
    if schedule_config.schedule_mode() != ScheduleMode::CRON {
        return Ok(None);
    }
    let expression = schedule_config.cron_expression().unwrap_or_default();
    let parsed = CronExpression::parse(expression).map_err(|e| {
        SchedulerError::new(format!("CronExpression '{expression}' is invalid: {e}"))
    })?;
    let offset = schedule_config.zone_offset().map_err(|e| {
        SchedulerError::new(format!(
            "CronExpression '{expression}' has an invalid time zone: {e}"
        ))
    })?;
    Ok(Some(CronPlan {
        expression: parsed,
        offset,
    }))
}

/// Context handed to a spawned job loop.
#[derive(Clone)]
pub(crate) struct JobContext {
    pub(crate) scheduler_id: String,
}

impl JobContext {
    /// Runs a single execution of the task located via the registry.
    pub(crate) async fn run_task(
        &self,
        task: &Arc<QuartzScheduleAgentTask>,
    ) -> agentscope_core::error::Result<Msg> {
        // The registry is consulted to mirror the Java job's indirection; the
        // task itself already holds its runtime config + factory.
        let _ = scheduler_registry().contains(self.scheduler_id.as_str());
        task.execute().await
    }
}

#[async_trait]
impl AgentScheduler for QuartzAgentScheduler {
    async fn schedule(
        &self,
        agent_config: AgentConfig,
        schedule_config: ScheduleConfig,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError> {
        let runtime = RuntimeAgentConfig::from(agent_config.clone());
        self.schedule_runtime(Arc::new(runtime), schedule_config, None)
            .await
    }

    async fn schedule_with_input(
        &self,
        agent_config: AgentConfig,
        schedule_config: ScheduleConfig,
        input: Msg,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError> {
        let runtime = RuntimeAgentConfig::from(agent_config);
        self.schedule_runtime(Arc::new(runtime), schedule_config, Some(input))
            .await
    }

    async fn cancel(&self, name: &str) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        let removed = self.jobs.write().unwrap().remove(name);
        match removed {
            Some(task) => {
                task.shutdown();
                tracing::info!(task = %name, "cancelled task");
                true
            }
            None => false,
        }
    }

    fn get_scheduled_agent(&self, name: &str) -> Option<Arc<dyn ScheduleAgentTask>> {
        self.jobs
            .read()
            .unwrap()
            .get(name)
            .map(|t| t.clone() as Arc<dyn ScheduleAgentTask>)
    }

    fn all_schedule_agent_tasks(&self) -> Vec<Arc<dyn ScheduleAgentTask>> {
        self.jobs
            .read()
            .unwrap()
            .values()
            .map(|t| t.clone() as Arc<dyn ScheduleAgentTask>)
            .collect()
    }

    async fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        let tasks: Vec<_> = self.jobs.write().unwrap().drain().map(|(_, v)| v).collect();
        for task in tasks {
            task.shutdown();
        }
        scheduler_registry().unregister(&self.scheduler_id);
        tracing::info!("QuartzAgentScheduler shutdown completed");
    }

    fn scheduler_type(&self) -> &'static str {
        "quartz"
    }
}

/// Marker trait so `Arc<dyn ScheduleAgentTask>` can be downcast to the concrete
/// backend task. Implemented for every scheduler task in this crate.
pub(crate) trait AsAny {
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Notify helper reused by the job loop for interrupt-driven wakeups.
pub(crate) fn interrupt_notify() -> Arc<Notify> {
    Arc::new(Notify::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::ScheduleMode;
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
    async fn schedule_and_query() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        let task = sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none(), None)
            .await
            .unwrap();
        assert_eq!(task.name(), "A");
        assert!(sched.get_scheduled_agent("A").is_some());
        assert_eq!(sched.all_schedule_agent_tasks().len(), 1);
        assert_eq!(sched.scheduler_type(), "quartz");
    }

    #[tokio::test]
    async fn duplicate_schedule_returns_existing() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        let a = sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none(), None)
            .await
            .unwrap();
        let b = sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none(), None)
            .await
            .unwrap();
        assert_eq!(a.id(), b.id());
        assert_eq!(sched.all_schedule_agent_tasks().len(), 1);
    }

    #[tokio::test]
    async fn cancel_removes_task() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none(), None)
            .await
            .unwrap();
        assert!(sched.cancel("A").await);
        assert!(sched.get_scheduled_agent("A").is_none());
        assert!(!sched.cancel("A").await);
    }

    #[tokio::test]
    async fn pause_resume_and_status() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        sched
            .schedule_runtime(
                runtime("A"),
                ScheduleConfig::builder()
                    .fixed_rate(100_000)
                    .build()
                    .unwrap(),
                None,
            )
            .await
            .unwrap();
        assert!(sched.pause("A"));
        assert_eq!(sched.get_status("A"), Some(TaskStatus::Paused));
        assert!(sched.resume("A"));
        assert_eq!(sched.get_status("A"), Some(TaskStatus::Scheduled));
    }

    #[tokio::test]
    async fn shutdown_clears_registry_and_tasks() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none(), None)
            .await
            .unwrap();
        sched.shutdown().await;
        assert!(sched.all_schedule_agent_tasks().is_empty());
        assert!(sched
            .schedule_runtime(runtime("B"), ScheduleConfig::none(), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn reject_invalid_fixed_rate() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        let bad = ScheduleConfig {
            schedule_mode: ScheduleMode::FIXED_RATE,
            fixed_rate: Some(0),
            ..Default::default()
        };
        assert!(sched
            .schedule_runtime(runtime("A"), bad, None)
            .await
            .is_err());
    }

    fn expect_err<T>(result: Result<T, SchedulerError>, what: &str) -> SchedulerError {
        match result {
            Err(e) => e,
            Ok(_) => panic!("expected {what} to fail"),
        }
    }

    #[tokio::test]
    async fn invalid_cron_fails_without_registering_task() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));

        // Quartz rejects both day selectors (`*` counts as specified).
        let cfg = ScheduleConfig::builder()
            .cron("0 0 9 * * MON-FRI")
            .build()
            .unwrap();
        let err = expect_err(
            sched.schedule_runtime(runtime("A"), cfg, None).await,
            "day-field conflict",
        );
        assert!(err.to_string().contains("is invalid"), "err was {err}");
        assert!(sched.get_scheduled_agent("A").is_none());
        assert!(sched.all_schedule_agent_tasks().is_empty());

        // `0` is not a Quartz weekday.
        let cfg = ScheduleConfig::builder()
            .cron("0 0 8 * * 0")
            .build()
            .unwrap();
        assert!(sched
            .schedule_runtime(runtime("A"), cfg, None)
            .await
            .is_err());
        assert!(sched.all_schedule_agent_tasks().is_empty());
    }

    #[tokio::test]
    async fn invalid_zone_id_fails_without_registering_task() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        let cfg = ScheduleConfig::builder()
            .cron("0 0 8 * * ?")
            .zone_id("Mars/Olympus")
            .build()
            .unwrap();
        let err = expect_err(
            sched.schedule_runtime(runtime("A"), cfg, None).await,
            "unknown zone id",
        );
        assert!(err.to_string().contains("Mars/Olympus"), "err was {err}");
        assert!(sched.get_scheduled_agent("A").is_none());
    }

    #[tokio::test]
    async fn cron_with_zone_id_is_scheduled() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        let cfg = ScheduleConfig::builder()
            .cron("0 0 8 * * ?")
            .zone_id("Asia/Shanghai")
            .build()
            .unwrap();
        let task = sched
            .schedule_runtime(runtime("A"), cfg, None)
            .await
            .unwrap();
        assert_eq!(task.name(), "A");
        assert!(sched.cancel("A").await);
    }
}
