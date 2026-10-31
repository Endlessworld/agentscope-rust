//! Quartz-style scheduler backend, mirroring
//! `io.agentscope.extensions.scheduler.quartz`.
//!
//! Java uses the Quartz library (Job/Trigger). Rust has no Quartz binding, so we
//! reproduce the same observable semantics with tokio tasks:
//! * `CRON`        — evaluated by the crate-local cron parser and fired at the
//!   exact `next_fire_time` of the configured zone (`ScheduleConfig::zone_id`,
//!   UTC when unset).
//! * `FIXED_RATE`  — `tokio::time::interval`, constant interval between starts.
//! * `FIXED_DELAY` — `sleep(delay)` after each completion (sequential).
//!
//! `pause` / `resume` / `interrupt` / `getStatus` are preserved.

pub mod cron;
mod job;
mod registry;
mod task;

pub use cron::{CronExpression, CronParseError};
pub use registry::{scheduler_registry, RegistryEntry, SchedulerRegistry};
pub use task::{QuartzScheduleAgentTask, TaskStatus};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;

use agentscope_core::message::Msg;

use crate::common::config::{AgentConfig, RuntimeAgentConfig, ScheduleConfig, ScheduleMode};
use crate::common::scheduler::lock::{NoopSchedulerLock, SchedulerLock};
use crate::common::scheduler::scheduler::{AgentScheduler, SchedulerError};
use crate::common::scheduler::task::{AgentFactory, ScheduleAgentTask};

use self::job::{spawn_job, CronPlan};

/// AgentScheduler implementation backed by tokio timers (Quartz-equivalent).
pub struct QuartzAgentScheduler {
    scheduler_id: String,
    factory: Arc<dyn AgentFactory>,
    /// Cross-instance lock consulted before each automatic firing.
    ///
    /// Defaults to [`NoopSchedulerLock`] (single-instance premise); a host that
    /// runs several instances injects a real one via
    /// [`QuartzAgentSchedulerBuilder::scheduler_lock`].
    lock: Arc<dyn SchedulerLock>,
    /// Cap on simultaneous scheduled executions, standing in for Quartz's
    /// thread pool. Manual `trigger`/`run` calls do not take a permit.
    concurrency: Arc<Semaphore>,
    /// name -> running job handle
    jobs: RwLock<HashMap<String, Arc<QuartzScheduleAgentTask>>>,
    shutdown: AtomicBool,
}

impl QuartzAgentScheduler {
    /// The default concurrency cap, matching Quartz's
    /// `org.quartz.threadPool.threadCount` default of 10.
    pub const DEFAULT_MAX_CONCURRENCY: usize = 10;
    /// Creates a new scheduler with the given agent factory.
    ///
    /// The instance is **not** registered in the process-wide registry yet: Java
    /// registers in the constructor, Rust needs a shared owner for the weak
    /// handle. Use [`new_shared`](Self::new_shared) (or
    /// [`register_self`](Self::register_self)) when the registry entry is wanted,
    /// e.g. so a job can look its scheduler back up by id.
    pub fn new(factory: Arc<dyn AgentFactory>) -> Self {
        Self::with_id(factory, uuid::Uuid::new_v4().to_string())
    }

    /// Creates a scheduler with an explicit id (see [`Self::builder`]).
    fn with_id(factory: Arc<dyn AgentFactory>, scheduler_id: String) -> Self {
        Self {
            scheduler_id,
            factory,
            lock: Arc::new(NoopSchedulerLock),
            concurrency: Arc::new(Semaphore::new(Self::DEFAULT_MAX_CONCURRENCY)),
            jobs: RwLock::new(HashMap::new()),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Creates the scheduler and registers it in the process-wide registry.
    ///
    /// Mirror of Java's constructor (`QuartzAgentSchedulerRegistry.register`).
    pub fn new_shared(factory: Arc<dyn AgentFactory>) -> Arc<Self> {
        Arc::new(Self::new(factory)).register_self()
    }

    /// Registers this scheduler in the process-wide registry and returns the handle.
    pub fn register_self(self: Arc<Self>) -> Arc<Self> {
        scheduler_registry().register(&self);
        self
    }

    /// Creates a builder, mirroring Java `QuartzAgentScheduler.builder()`.
    ///
    /// Java's `Builder#autoStart` has no Rust counterpart (a tokio job loop starts
    /// with its task, there is no scheduler-wide start/standby switch); a custom
    /// Quartz `Scheduler` cannot be injected either. What remains is the
    /// deterministic `schedulerId` (registry lookups across restarts) plus the two
    /// host seams: the cross-instance [`SchedulerLock`] and the concurrency limiter
    /// that stands in for Quartz's thread pool.
    pub fn builder(factory: Arc<dyn AgentFactory>) -> QuartzAgentSchedulerBuilder {
        QuartzAgentSchedulerBuilder {
            factory,
            scheduler_id: None,
            lock: None,
            concurrency: None,
        }
    }

    /// Java's single-argument `schedule(AgentConfig)` overload: schedules with the
    /// default schedule config (`ScheduleMode::NONE`, manual triggering only).
    pub async fn schedule_default(
        &self,
        agent_config: AgentConfig,
    ) -> Result<Arc<dyn ScheduleAgentTask>, SchedulerError> {
        self.schedule(agent_config, ScheduleConfig::none()).await
    }

    /// The unique id of this scheduler instance.
    pub fn scheduler_id(&self) -> &str {
        &self.scheduler_id
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
            cron.clone(),
        ));

        self.jobs
            .write()
            .unwrap()
            .insert(name.clone(), task.clone());

        let handle = spawn_job(self.context(), task.clone(), schedule_config.clone());
        task.attach(handle);

        tracing::info!(task = %name, mode = schedule_config.schedule_mode().as_str(), "scheduled task");
        Ok(task)
    }

    fn context(&self) -> JobContext {
        JobContext {
            scheduler_id: self.scheduler_id.clone(),
            lock: self.lock.clone(),
            semaphore: self.concurrency.clone(),
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
    ///
    /// Mirrors Java `Scheduler#interrupt(JobKey)`: the in-flight execution is
    /// aborted and `true` is returned; when no execution is running (or the task
    /// does not exist) `false` is returned.
    pub fn interrupt(&self, name: &str) -> bool {
        match self.get_scheduled_agent(name) {
            Some(t) => match t.as_any().downcast_ref::<QuartzScheduleAgentTask>() {
                Some(q) => q.interrupt(),
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

    /// Java `getStatus` analogue: the Quartz `TriggerState`, `NONE` when the task
    /// is unknown (Java returns `null` there; Rust uses the `None` variant).
    pub fn trigger_state(&self, name: &str) -> TaskStatus {
        self.get_status(name).unwrap_or(TaskStatus::None)
    }

    /// Next fire time of a CRON task, as an absolute instant.
    ///
    /// Mirrors Quartz `Trigger#getNextFireTime` /
    /// `CronExpression#getNextValidTimeAfter`. Returns `None` for non-CRON tasks,
    /// unknown names and schedules with no future firing.
    pub fn next_fire_time(&self, name: &str) -> Option<DateTime<Utc>> {
        let task = self.get_scheduled_agent(name)?;
        let quart_task = task.as_any().downcast_ref::<QuartzScheduleAgentTask>()?;
        quart_task.next_fire_time(Utc::now())
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

/// Builder for [`QuartzAgentScheduler`], mirroring Java's
/// `QuartzAgentScheduler.Builder` (only the parts that have a Rust meaning).
pub struct QuartzAgentSchedulerBuilder {
    factory: Arc<dyn AgentFactory>,
    scheduler_id: Option<String>,
    lock: Option<Arc<dyn SchedulerLock>>,
    concurrency: Option<Arc<Semaphore>>,
}

impl QuartzAgentSchedulerBuilder {
    /// Sets a deterministic scheduler id (Java `Builder#schedulerId`).
    pub fn scheduler_id(mut self, id: impl Into<String>) -> Self {
        self.scheduler_id = Some(id.into());
        self
    }

    /// Injects the cross-instance lock consulted before every automatic firing.
    ///
    /// The default is [`NoopSchedulerLock`], i.e. the documented single-instance
    /// premise. Inject a storage-backed implementation (JDBC/Redis) when several
    /// scheduler processes share the same schedules.
    pub fn scheduler_lock(mut self, lock: Arc<dyn SchedulerLock>) -> Self {
        self.lock = Some(lock);
        self
    }

    /// Injects an explicit concurrency limiter (Java's Quartz thread pool).
    ///
    /// The semaphore is shared by every task of this scheduler and bounds the
    /// *automatic* firings; a firing waits for a permit, exactly like a Quartz
    /// trigger waits for a worker thread. Manual `trigger`/`run` calls bypass it.
    pub fn concurrency_limiter(mut self, semaphore: Arc<Semaphore>) -> Self {
        self.concurrency = Some(semaphore);
        self
    }

    /// Sets the maximum number of simultaneous executions (default
    /// [`QuartzAgentScheduler::DEFAULT_MAX_CONCURRENCY`] = 10, matching Quartz).
    pub fn max_concurrency(mut self, max: usize) -> Self {
        self.concurrency = Some(Arc::new(Semaphore::new(max.max(1))));
        self
    }

    /// Builds the scheduler (not registered; see
    /// [`QuartzAgentScheduler::new_shared`]).
    pub fn build(self) -> QuartzAgentScheduler {
        let id = self
            .scheduler_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut scheduler = QuartzAgentScheduler::with_id(self.factory, id);
        if let Some(lock) = self.lock {
            scheduler.lock = lock;
        }
        if let Some(concurrency) = self.concurrency {
            scheduler.concurrency = concurrency;
        }
        scheduler
    }

    /// Builds the scheduler and registers it in the process-wide registry.
    pub fn build_shared(self) -> Arc<QuartzAgentScheduler> {
        Arc::new(self.build()).register_self()
    }
}

/// Parses the cron expression and resolves its time zone, mirroring Java's
/// `CronScheduleBuilder.cronSchedule(...)` + `csb.inTimeZone(...)`.
///
/// Returns `None` for non-CRON modes. Called before the task is registered so a
/// bad expression or zone id has no side effects (Java throws before
/// `tasks.put(...)`).
fn resolve_cron(schedule_config: &ScheduleConfig) -> Result<Option<Arc<CronPlan>>, SchedulerError> {
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
    Ok(Some(Arc::new(CronPlan {
        expression: parsed,
        offset,
    })))
}

/// Context handed to a spawned job loop.
#[derive(Clone)]
pub(crate) struct JobContext {
    pub(crate) scheduler_id: String,
    /// Cross-instance lock consulted before each firing.
    pub(crate) lock: Arc<dyn SchedulerLock>,
    /// Concurrency cap shared by every task of the scheduler.
    pub(crate) semaphore: Arc<Semaphore>,
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
                // Java `scheduler.deleteJob(taskKey)` drops the job and its trigger;
                // the Rust equivalent must also stop and await the job loop, or the
                // loop could keep firing after `cancel` already returned `true`.
                task.shutdown();
                task.join().await;
                tracing::info!(task = %name, "cancelled task");
                true
            }
            None => {
                tracing::warn!(task = %name, "attempt to cancel non-existent task");
                false
            }
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
            // Java `scheduler.shutdown(true)` waits for running jobs; awaiting the
            // job-loop handle is the Rust equivalent.
            task.shutdown();
            task.join().await;
        }
        scheduler_registry().unregister(&self.scheduler_id);
        tracing::info!("QuartzAgentScheduler shutdown completed");
    }

    fn scheduler_type(&self) -> &'static str {
        "quartz"
    }
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
                .model(crate::test_support::stub_model("ok"))
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
    async fn builder_sets_a_deterministic_scheduler_id() {
        // Java `Builder#schedulerId`: the id is what a job uses to find its
        // scheduler in the registry, so it must be settable and stable.
        let sched = QuartzAgentScheduler::builder(Arc::new(EchoFactory))
            .scheduler_id("fixed-id")
            .build_shared();
        assert_eq!(sched.scheduler_id(), "fixed-id");
        assert!(scheduler_registry().get_handle("fixed-id").is_some());

        sched.shutdown().await;
        assert!(scheduler_registry().get_handle("fixed-id").is_none());
    }

    #[tokio::test]
    async fn schedule_default_uses_none_mode_and_needs_a_runtime_model() {
        // Java's single-arg `schedule(AgentConfig)` builds the model from its
        // `ModelConfig`; Rust has no provider registry in this crate, so the
        // serializable path schedules fine and fails at execution time with the
        // same `Model cannot be null.` message Java would raise for a missing model.
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        let cfg = AgentConfig::builder()
            .name("A")
            .model_name("m")
            .sys_prompt("sys")
            .build()
            .unwrap();
        let task = sched.schedule_default(cfg).await.unwrap();
        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();
        assert_eq!(qt.schedule_config().schedule_mode(), ScheduleMode::NONE);
        let err = task.run_empty().await.unwrap_err();
        // `Error::Agent` renders as `Agent error: {0}`; the Java text is the payload.
        match err {
            agentscope_core::error::Error::Agent(message) => {
                assert_eq!(message, "Model cannot be null.")
            }
            other => panic!("expected an agent error, got {other:?}"),
        }
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
        assert_eq!(sched.get_status("A"), Some(TaskStatus::Normal));
    }

    #[tokio::test]
    async fn shutdown_clears_registry_and_tasks() {
        let sched = QuartzAgentScheduler::new_shared(Arc::new(EchoFactory));
        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none(), None)
            .await
            .unwrap();
        let id = sched.scheduler_id().to_string();
        // Java registers in the constructor; the shared constructor keeps that.
        assert!(scheduler_registry().get_handle(&id).is_some());

        sched.shutdown().await;
        assert!(sched.all_schedule_agent_tasks().is_empty());
        assert!(
            scheduler_registry().get_handle(&id).is_none(),
            "shutdown must unregister the scheduler"
        );
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

    #[tokio::test]
    async fn next_fire_time_query_for_cron_task() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        sched
            .schedule_runtime(
                runtime("A"),
                ScheduleConfig::builder()
                    .cron("0 0 0 1 1 ? 2099")
                    .build()
                    .unwrap(),
                None,
            )
            .await
            .unwrap();

        // Quartz `Trigger#getNextFireTime` analogue: the query goes through the
        // same `next_after` computation the job loop uses.
        let next = sched
            .next_fire_time("A")
            .expect("cron task has a next fire");
        assert_eq!(
            next.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2099-01-01 00:00:00"
        );
        // Unknown names and non-CRON tasks have no next fire time.
        assert!(sched.next_fire_time("missing").is_none());
        sched
            .schedule_runtime(runtime("B"), ScheduleConfig::none(), None)
            .await
            .unwrap();
        assert!(sched.next_fire_time("B").is_none());

        sched.shutdown().await;
    }

    #[tokio::test]
    async fn trigger_state_covers_quartz_states() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        assert_eq!(sched.trigger_state("missing"), TaskStatus::None);
        assert!(sched.trigger_state("missing").is_terminal());

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
        assert_eq!(sched.trigger_state("A"), TaskStatus::Normal);

        assert!(sched.pause("A"));
        assert_eq!(sched.trigger_state("A"), TaskStatus::Paused);
        assert!(!sched.trigger_state("A").is_terminal());

        assert!(sched.resume("A"));
        assert_eq!(sched.trigger_state("A"), TaskStatus::Normal);

        assert!(sched.cancel("A").await);
        // Java removes the task, after which `getStatus` reports NONE.
        assert_eq!(sched.trigger_state("A"), TaskStatus::None);
        assert_eq!(sched.get_status("A"), None);

        sched.shutdown().await;
    }

    #[tokio::test]
    async fn interrupt_reports_whether_a_run_was_in_flight() {
        let sched = QuartzAgentScheduler::new(Arc::new(EchoFactory));
        sched
            .schedule_runtime(runtime("A"), ScheduleConfig::none(), None)
            .await
            .unwrap();
        // Nothing is running, so Quartz would not report an interrupted job.
        assert!(!sched.interrupt("A"));
        assert!(!sched.interrupt("missing"));

        // A manual trigger completes immediately, so an interrupt after it also
        // finds nothing in flight, but the task itself still exists.
        sched.trigger("A").await.unwrap();
        assert!(!sched.interrupt("A"));

        sched.shutdown().await;
    }

    #[tokio::test]
    async fn cron_loop_fires_on_next_fire_time() {
        // End-to-end: the CRON job loop sleeps until `next_fire_time` (i.e.
        // `next_after` is on the production path) and really fires the task.
        let sched = Arc::new(QuartzAgentScheduler::new(Arc::new(EchoFactory)));
        let cfg = ScheduleConfig::builder()
            .cron("* * * * * ?")
            .build()
            .unwrap();
        let task = sched
            .schedule_runtime(runtime("EverySecond"), cfg, None)
            .await
            .unwrap();
        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();

        // The query API and the job loop share the same computation.
        assert!(sched.next_fire_time("EverySecond").is_some());

        for _ in 0..150 {
            if qt.execution_count() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            qt.execution_count() >= 2,
            "cron loop did not fire twice (count {})",
            qt.execution_count()
        );

        sched.shutdown().await;
    }

    /// Factory that sleeps, so an execution can be observed in flight.
    struct SlowEcho {
        started: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl AgentFactory for SlowEcho {
        async fn run(
            &self,
            _config: &RuntimeAgentConfig,
            _messages: Vec<Msg>,
        ) -> agentscope_core::error::Result<Msg> {
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(Msg::assistant("slow"))
        }
    }

    #[tokio::test]
    async fn cancel_waits_for_the_in_flight_execution_to_finish() {
        // Java `deleteJob` drops the job and its trigger but never interrupts a
        // running execution (only `Scheduler.interrupt` does), and the Rust
        // counterpart waits for the loop to stop: the in-flight run therefore
        // completes and is counted, and no further firing happens afterwards.
        let factory = Arc::new(SlowEcho {
            started: std::sync::atomic::AtomicUsize::new(0),
        });
        let sched = Arc::new(QuartzAgentScheduler::new(factory.clone()));
        let cfg = ScheduleConfig::builder()
            .fixed_delay(10)
            .initial_delay(1)
            .build()
            .unwrap();
        let task = sched
            .schedule_runtime(runtime("Slow"), cfg, None)
            .await
            .unwrap();
        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();

        for _ in 0..100 {
            if qt.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(qt.is_running(), "execution never started");
        assert_eq!(qt.status(), TaskStatus::Blocked);

        assert!(sched.cancel("Slow").await);
        assert!(qt.is_cancelled());
        // `cancel` returned only after the loop stopped, so the run that was in
        // flight completed and was counted, and the loop did not fire again.
        assert_eq!(
            qt.execution_count(),
            1,
            "cancel must wait for the in-flight execution instead of discarding it"
        );
        assert!(!qt.is_running());
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(qt.execution_count(), 1, "the loop kept firing after cancel");

        sched.shutdown().await;
    }

    #[tokio::test]
    async fn fixed_rate_cancel_waits_for_the_in_flight_execution() {
        // FIXED_RATE used to race the firing itself against `cancelled()` inside one
        // `select!`, so a cancel dropped the execution future mid-run (the run was
        // never counted). Only the wait for the next tick may be raced, exactly like
        // the FIXED_DELAY/CRON branches, so the in-flight run completes.
        let sched = Arc::new(QuartzAgentScheduler::new(Arc::new(SlowEcho {
            started: std::sync::atomic::AtomicUsize::new(0),
        })));
        let cfg = ScheduleConfig::builder()
            .fixed_rate(50)
            .initial_delay(1)
            .build()
            .unwrap();
        let task = sched.schedule_runtime(runtime("Rate"), cfg, None).await.unwrap();
        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();

        for _ in 0..100 {
            if qt.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(qt.is_running(), "execution never started");

        assert!(sched.cancel("Rate").await);
        assert_eq!(
            qt.execution_count(),
            1,
            "the FIXED_RATE in-flight execution must complete, not be dropped"
        );
        assert!(!qt.is_running());

        sched.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_waits_for_the_in_flight_execution() {
        // Java `scheduler.shutdown(true)` waits for running jobs; the Rust
        // equivalent must not abort the execution that is in flight.
        let sched = Arc::new(QuartzAgentScheduler::new(Arc::new(SlowEcho {
            started: std::sync::atomic::AtomicUsize::new(0),
        })));
        let cfg = ScheduleConfig::builder()
            .fixed_delay(10)
            .initial_delay(1)
            .build()
            .unwrap();
        let task = sched
            .schedule_runtime(runtime("Slow"), cfg, None)
            .await
            .unwrap();
        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();

        for _ in 0..100 {
            if qt.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(qt.is_running(), "execution never started");

        sched.shutdown().await;
        assert_eq!(
            qt.execution_count(),
            1,
            "shutdown must let the in-flight execution finish (Java shutdown(true))"
        );
        assert!(sched.all_schedule_agent_tasks().is_empty());
    }

    #[tokio::test]
    async fn cancel_waits_for_the_job_loop_to_stop() {
        let sched = Arc::new(QuartzAgentScheduler::new(Arc::new(EchoFactory)));
        let cfg = ScheduleConfig::builder()
            .fixed_rate(10)
            .initial_delay(1)
            .build()
            .unwrap();
        let task = sched
            .schedule_runtime(runtime("A"), cfg, None)
            .await
            .unwrap();
        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();

        for _ in 0..100 {
            if qt.execution_count() >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(qt.execution_count() >= 1, "task never fired");

        assert!(sched.cancel("A").await);
        let after_cancel = qt.execution_count();
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert_eq!(
            qt.execution_count(),
            after_cancel,
            "job loop kept firing after cancel returned"
        );
        assert!(qt.is_cancelled());
        assert!(!sched.cancel("A").await);

        sched.shutdown().await;
    }

    /// Factory standing in for a host with a provider registry: it turns the
    /// serializable `model_name` into a `Model` and runs from the config it is
    /// handed (see `AgentFactory::resolve_model`).
    struct ModelResolvingFactory {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl AgentFactory for ModelResolvingFactory {
        fn resolve_model(
            &self,
            config: &RuntimeAgentConfig,
        ) -> Option<Arc<dyn agentscope_core::model::Model>> {
            (config.model_name() == "qwen-max").then(|| crate::test_support::stub_model("ok"))
        }

        async fn run(
            &self,
            config: &RuntimeAgentConfig,
            _messages: Vec<Msg>,
        ) -> agentscope_core::error::Result<Msg> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // The factory-resolved model is injected into the config, so building
            // the agent from `config.model()` works exactly like Java.
            assert_eq!(
                config.model().expect("resolved model injected").model_name(),
                "stub-model"
            );
            Ok(Msg::assistant("ran"))
        }
    }

    #[tokio::test]
    async fn schedule_default_runs_with_a_factory_resolved_model() {
        // SCHED-01 acceptance: `schedule(AgentConfig)` only carries a model *name*
        // (`RuntimeAgentConfig::from` has `model: None`). Before
        // `AgentFactory::resolve_model` the run aborted with
        // `Model cannot be null.` before the host factory was ever consulted.
        let factory = Arc::new(ModelResolvingFactory {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let sched = QuartzAgentScheduler::new(factory.clone());
        let cfg = AgentConfig::builder()
            .name("A")
            .model_name("qwen-max")
            .sys_prompt("sys")
            .build()
            .unwrap();

        let task = sched.schedule(cfg, ScheduleConfig::none()).await.unwrap();
        task.run_empty().await.unwrap();

        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();
        assert_eq!(qt.execution_count(), 1);
        assert_eq!(factory.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Lock that records the keys it is shown and grants or denies them.
    struct RecordingLock {
        granted: std::sync::Mutex<Vec<String>>,
        released: std::sync::Mutex<Vec<String>>,
        grant: bool,
        acquired: tokio::sync::Notify,
        released_signal: tokio::sync::Notify,
    }

    impl RecordingLock {
        fn new(grant: bool) -> Arc<Self> {
            Arc::new(Self {
                granted: std::sync::Mutex::new(Vec::new()),
                released: std::sync::Mutex::new(Vec::new()),
                grant,
                acquired: tokio::sync::Notify::new(),
                released_signal: tokio::sync::Notify::new(),
            })
        }
    }

    #[async_trait]
    impl SchedulerLock for RecordingLock {
        async fn try_acquire(&self, key: &str) -> bool {
            self.granted.lock().unwrap().push(key.to_string());
            self.acquired.notify_one();
            self.grant
        }

        async fn release(&self, key: &str) {
            self.released.lock().unwrap().push(key.to_string());
            self.released_signal.notify_one();
        }
    }

    #[tokio::test]
    async fn injected_scheduler_lock_gates_every_firing() {
        // SCHED-05: the default lock is a no-op (single-instance premise); an
        // injected one must be consulted before the firing and released after it.
        let lock = RecordingLock::new(true);
        let sched = Arc::new(
            QuartzAgentScheduler::builder(Arc::new(EchoFactory))
                .scheduler_lock(lock.clone())
                .build(),
        );
        let cfg = ScheduleConfig::builder()
            .fixed_rate(60_000)
            .initial_delay(1)
            .build()
            .unwrap();
        let task = sched
            .schedule_runtime(runtime("Locked"), cfg, None)
            .await
            .unwrap();
        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();

        // `notify_one` stores a permit when nobody waits yet, so registering the
        // waiter after `schedule_runtime` cannot miss the first acquisition.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            lock.acquired.notified(),
        )
        .await
        .expect("the injected lock was never consulted");

        let granted = lock.granted.lock().unwrap().clone();
        assert_eq!(granted.len(), 1, "one firing, one lock acquisition");
        assert!(
            granted[0].starts_with(sched.scheduler_id()),
            "the key must be namespaced by the scheduler id: {}",
            granted[0]
        );
        assert!(
            granted[0].contains("Locked") && granted[0].contains(":rate:"),
            "unexpected slot key: {}",
            granted[0]
        );

        // Wait for the release: it happens after the execution completed, so the
        // run is counted and there is no cancel/execute race.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            lock.released_signal.notified(),
        )
        .await
        .expect("the granted slot was never released");
        assert_eq!(
            lock.released.lock().unwrap().clone(),
            granted,
            "the granted slot must be released after the firing"
        );
        assert_eq!(qt.execution_count(), 1);

        assert!(sched.cancel("Locked").await);
    }

    #[tokio::test]
    async fn a_denied_slot_skips_the_execution() {
        // Two instances share a schedule; the lock owner is the other node, so
        // this one must not run (and must not release a lock it never held).
        let lock = RecordingLock::new(false);
        let sched = Arc::new(
            QuartzAgentScheduler::builder(Arc::new(EchoFactory))
                .scheduler_lock(lock.clone())
                .build(),
        );
        let cfg = ScheduleConfig::builder()
            .fixed_delay(60_000)
            .initial_delay(1)
            .build()
            .unwrap();
        let task = sched
            .schedule_runtime(runtime("Denied"), cfg, None)
            .await
            .unwrap();
        let qt = task
            .as_any()
            .downcast_ref::<QuartzScheduleAgentTask>()
            .unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            lock.acquired.notified(),
        )
        .await
        .expect("the injected lock was never consulted");

        assert_eq!(qt.execution_count(), 0, "a denied slot must not execute");
        assert!(lock.granted.lock().unwrap()[0].contains(":delay:"));
        assert!(
            lock.released.lock().unwrap().is_empty(),
            "a denied slot is never released"
        );

        sched.shutdown().await;
    }

    /// Factory whose executions block on a shared gate (no timing involved).
    struct GatedFactory {
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
        started: std::sync::atomic::AtomicUsize,
        progress: tokio::sync::watch::Sender<usize>,
        gate: Arc<Semaphore>,
    }

    #[async_trait]
    impl AgentFactory for GatedFactory {
        async fn run(
            &self,
            _config: &RuntimeAgentConfig,
            _messages: Vec<Msg>,
        ) -> agentscope_core::error::Result<Msg> {
            use std::sync::atomic::Ordering as AtomicOrdering;
            let active = self.active.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_active.fetch_max(active, AtomicOrdering::SeqCst);
            let started = self.started.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let _ = self.progress.send(started);
            // Blocks until the test opens the gate.
            let permit = self.gate.acquire().await.expect("gate stays open");
            drop(permit);
            self.active.fetch_sub(1, AtomicOrdering::SeqCst);
            Ok(Msg::assistant("gated"))
        }
    }

    #[tokio::test]
    async fn concurrency_limit_bounds_simultaneous_executions() {
        // SCHED-09: Quartz bounds concurrent jobs with its thread pool (default
        // 10); without a limiter every task's tokio loop fires immediately.
        let gate = Arc::new(Semaphore::new(0));
        let (progress, mut rx) = tokio::sync::watch::channel(0usize);
        let factory = Arc::new(GatedFactory {
            active: std::sync::atomic::AtomicUsize::new(0),
            max_active: std::sync::atomic::AtomicUsize::new(0),
            started: std::sync::atomic::AtomicUsize::new(0),
            progress,
            gate: gate.clone(),
        });
        let sched = Arc::new(
            QuartzAgentScheduler::builder(factory.clone())
                .max_concurrency(2)
                .build(),
        );
        for name in ["A", "B", "C"] {
            let cfg = ScheduleConfig::builder()
                .fixed_delay(60_000)
                .initial_delay(1)
                .build()
                .unwrap();
            sched
                .schedule_runtime(runtime(name), cfg, None)
                .await
                .unwrap();
        }

        // Exactly the configured two executions get in; the third waits for a
        // permit (without the limiter all three would already be running).
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut started = *rx.borrow_and_update();
            while started < 2 {
                rx.changed().await.expect("factory dropped");
                started = *rx.borrow_and_update();
            }
        })
        .await
        .expect("two executions never started");
        assert_eq!(factory.active.load(std::sync::atomic::Ordering::SeqCst), 2);

        // Opening the gate lets the queued firing in; the peak must stay at the
        // configured limit.
        gate.add_permits(3);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut started = *rx.borrow_and_update();
            while started < 3 {
                rx.changed().await.expect("factory dropped");
                started = *rx.borrow_and_update();
            }
        })
        .await
        .expect("the queued execution never started");
        assert_eq!(
            factory.max_active.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the concurrency limit was exceeded"
        );

        sched.shutdown().await;
    }
}
