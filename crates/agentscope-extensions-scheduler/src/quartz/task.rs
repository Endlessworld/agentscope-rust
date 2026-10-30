//! `QuartzScheduleAgentTask`, mirroring
//! `io.agentscope.extensions.scheduler.quartz.QuartzScheduleAgentTask`.
//!
//! Java wraps a Quartz `JobKey` + `Scheduler`. Here it wraps the tokio job handle
//! plus a control flag set; pause/resume/interrupt/status mirror the Quartz job
//! lifecycle.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::Msg;

use crate::common::config::{RuntimeAgentConfig, ScheduleConfig};
use crate::common::scheduler::task::{AgentFactory, BaseScheduleAgentTask, ScheduleAgentTask};
use crate::quartz::job::CronPlan;

/// Status of a scheduled task.
///
/// The first six variants mirror Quartz's `Trigger.TriggerState`, which is what
/// Java `QuartzAgentScheduler#getStatus` returns. `Cancelled` is the Rust-only
/// terminal state of a task that was explicitly cancelled (Java removes the job,
/// after which the trigger state reads `NONE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Quartz `TriggerState.NONE` — no trigger/job exists for the name.
    None,
    /// Scheduled and not paused (Quartz `NORMAL`).
    Normal,
    /// Paused; no further automatic executions (Quartz `PAUSED`).
    Paused,
    /// Exhausted: no future fire time (Quartz `COMPLETE`).
    Complete,
    /// The last execution failed (Quartz `ERROR`).
    Error,
    /// An execution is in flight (Quartz `BLOCKED`).
    Blocked,
    /// Cancelled / shut down (Rust-only; Java drops the job instead).
    Cancelled,
}

impl TaskStatus {
    /// The Quartz `TriggerState` name, for logging and cross-language tests.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::None => "NONE",
            TaskStatus::Normal => "NORMAL",
            TaskStatus::Paused => "PAUSED",
            TaskStatus::Complete => "COMPLETE",
            TaskStatus::Error => "ERROR",
            TaskStatus::Blocked => "BLOCKED",
            TaskStatus::Cancelled => "CANCELLED",
        }
    }

    /// Whether no further transition is expected.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::None | TaskStatus::Complete | TaskStatus::Cancelled
        )
    }
}

/// Quartz-flavoured task handle.
pub struct QuartzScheduleAgentTask {
    inner: BaseScheduleAgentTask,
    name: String,
    factory: Arc<dyn AgentFactory>,
    runtime: Arc<RuntimeAgentConfig>,
    schedule_config: ScheduleConfig,
    /// Initial input stored at scheduling time, mirroring the Quartz
    /// `JobDataMap#inputMsg` entry. Replayed on every automatic firing.
    initial_input: Option<Msg>,
    /// Parsed cron plan (CRON mode only); also backs the `next_fire_time` query.
    cron: Option<Arc<CronPlan>>,
    paused: AtomicBool,
    running: AtomicBool,
    cancelled: AtomicBool,
    /// No future fire time (Quartz marks such triggers `COMPLETE`).
    completed: AtomicBool,
    /// The last finished execution failed (Quartz `TriggerState.ERROR`).
    last_error: AtomicBool,
    /// Number of executions aborted by [`interrupt`](Self::interrupt).
    interrupt_count: AtomicU64,
    /// Monotonic interrupt generation, published to running executions.
    interrupt_tx: watch::Sender<u64>,
    /// Wakes the job loop when paused/resumed/interrupted.
    notify: Arc<Notify>,
    /// Wakes the job loop on cancel.
    cancel_notify: Arc<Notify>,
    handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl QuartzScheduleAgentTask {
    /// Creates a new task (job loop attached later via [`attach`](Self::attach)).
    pub(crate) fn new(
        runtime: Arc<RuntimeAgentConfig>,
        schedule_config: ScheduleConfig,
        name: String,
        factory: Arc<dyn AgentFactory>,
        initial_input: Option<Msg>,
        cron: Option<Arc<CronPlan>>,
    ) -> Self {
        let inner =
            BaseScheduleAgentTask::new(runtime.clone(), schedule_config.clone(), factory.clone());
        let (interrupt_tx, _rx) = watch::channel(0u64);
        Self {
            inner,
            name,
            factory,
            runtime,
            schedule_config,
            initial_input,
            cron,
            paused: AtomicBool::new(false),
            running: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            last_error: AtomicBool::new(false),
            interrupt_count: AtomicU64::new(0),
            interrupt_tx,
            notify: Arc::new(Notify::new()),
            cancel_notify: Arc::new(Notify::new()),
            handle: std::sync::Mutex::new(None),
        }
    }

    /// Replaces the factory used for executions.
    pub fn with_factory(mut self, factory: Arc<dyn AgentFactory>) -> Self {
        self.factory = factory;
        self.inner = BaseScheduleAgentTask::new(
            self.runtime.clone(),
            self.schedule_config.clone(),
            self.factory.clone(),
        );
        self
    }

    pub(crate) fn attach(&self, handle: JoinHandle<()>) {
        *self.handle.lock().unwrap() = Some(handle);
    }

    /// The runtime agent configuration backing this task.
    pub fn runtime(&self) -> &Arc<RuntimeAgentConfig> {
        &self.runtime
    }

    /// The agent configuration (Java `BaseScheduleAgentTask#getAgentConfig`).
    pub fn agent_config(&self) -> &RuntimeAgentConfig {
        self.inner.agent_config()
    }

    /// The schedule configuration (Java `BaseScheduleAgentTask#getScheduleConfig`).
    pub fn schedule_config(&self) -> &ScheduleConfig {
        &self.schedule_config
    }

    /// The parsed cron plan, if this task runs in CRON mode.
    pub(crate) fn cron_plan(&self) -> Option<&Arc<CronPlan>> {
        self.cron.as_ref()
    }

    /// The agent factory used for executions.
    pub fn factory(&self) -> Arc<dyn AgentFactory> {
        self.factory.clone()
    }

    /// Total executions (scheduled + manual), mirroring Java
    /// `BaseScheduleAgentTask#getExecutionCount`.
    pub fn execution_count(&self) -> u64 {
        self.inner.execution_count()
    }

    /// Number of executions aborted by [`interrupt`](Self::interrupt).
    pub fn interrupted_count(&self) -> u64 {
        self.interrupt_count.load(Ordering::SeqCst)
    }

    /// The initial input message stored at scheduling time, if any.
    pub fn initial_input(&self) -> Option<&Msg> {
        self.initial_input.as_ref()
    }

    /// Next fire time strictly after `from` (CRON mode only).
    ///
    /// This mirrors Quartz `Trigger#getNextFireTime` /
    /// `CronExpression#getNextValidTimeAfter` and is the same computation the
    /// job loop uses to sleep until the next firing.
    pub fn next_fire_time(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.cancelled.load(Ordering::SeqCst) || self.completed.load(Ordering::SeqCst) {
            return None;
        }
        self.cron.as_ref()?.next_fire_time(from)
    }

    /// Returns the current status.
    pub fn status(&self) -> TaskStatus {
        if self.cancelled.load(Ordering::SeqCst) {
            TaskStatus::Cancelled
        } else if self.paused.load(Ordering::SeqCst) {
            TaskStatus::Paused
        } else if self.completed.load(Ordering::SeqCst) {
            TaskStatus::Complete
        } else if self.running.load(Ordering::SeqCst) {
            TaskStatus::Blocked
        } else if self.last_error.load(Ordering::SeqCst) {
            TaskStatus::Error
        } else {
            TaskStatus::Normal
        }
    }

    /// Whether the task has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Whether the task is paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Whether a manual/scheduled execution is currently in flight.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Pauses the task.
    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Resumes the task and wakes the job loop.
    pub(crate) fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Interrupts the in-flight execution, if any.
    ///
    /// Mirrors Java's `InterruptableJob#interrupt`: the running execution is
    /// aborted (its future is dropped) and the job loop stops rescheduling.
    /// Returns `true` when an execution was actually in flight.
    pub(crate) fn interrupt(&self) -> bool {
        let was_running = self.running.load(Ordering::SeqCst);
        // `send_replace` never fails and keeps the value readable for receivers
        // created later (a plain `send` fails when nobody is subscribed).
        let next = self.interrupt_tx.borrow().wrapping_add(1);
        self.interrupt_tx.send_replace(next);
        self.notify.notify_waiters();
        was_running
    }

    /// Marks the schedule as exhausted (no future fire time).
    pub(crate) fn mark_completed(&self) {
        self.completed.store(true, Ordering::SeqCst);
    }

    /// Wakes the job loop because the task was cancelled.
    pub(crate) fn shutdown(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        self.do_cancel_flag();
        self.cancel_notify.notify_waiters();
        self.notify.notify_waiters();
    }

    fn do_cancel_flag(&self) {
        // Marks the base task cancelled (idempotent).
        self.inner.cancel();
    }

    /// Waits until the task is no longer paused or is cancelled.
    pub(crate) async fn wait_if_paused(&self) {
        while self.paused.load(Ordering::SeqCst) && !self.cancelled.load(Ordering::SeqCst) {
            self.notify.notified().await;
        }
    }

    /// Returns a future resolving when the task is cancelled.
    pub(crate) async fn cancelled(&self) {
        if self.cancelled.load(Ordering::SeqCst) {
            return;
        }
        self.cancel_notify.notified().await;
    }

    /// Executes one run immediately (manual trigger / job loop body).
    pub async fn execute(&self) -> Result<Msg> {
        // Mirror `AgentQuartzJob`: replay the stored input on each firing.
        let messages = self
            .initial_input
            .clone()
            .map(|msg| vec![msg])
            .unwrap_or_default();
        self.execute_with(messages).await
    }

    /// Executes one run with explicit input messages.
    ///
    /// The run is raced against the interrupt channel: an `interrupt()` issued
    /// while the factory is running drops the execution future and yields
    /// [`Error::Interrupted`], which is how Java's `InterruptableJob` aborts an
    /// in-flight job.
    pub async fn execute_with(&self, messages: Vec<Msg>) -> Result<Msg> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Agent(format!(
                "Cannot run a cancelled task: {}",
                self.name
            )));
        }
        let mut interrupts = self.interrupt_tx.subscribe();
        // Interrupts raised before this firing started must not abort it: Java
        // creates a fresh `AgentQuartzJob` (with `interrupted = false`) for every
        // firing, so only interrupts delivered while we run matter.
        interrupts.borrow_and_update();
        self.running.store(true, Ordering::SeqCst);
        let result = tokio::select! {
            biased;
            changed = interrupts.changed() => {
                let _ = changed;
                self.interrupt_count.fetch_add(1, Ordering::SeqCst);
                Err(Error::Interrupted(format!(
                    "Task execution interrupted: {}",
                    self.name
                )))
            }
            result = self.inner.run_with(messages) => result,
        };
        self.running.store(false, Ordering::SeqCst);
        match &result {
            Ok(_) => self.last_error.store(false, Ordering::SeqCst),
            Err(Error::Interrupted(_)) => {}
            Err(_) => self.last_error.store(true, Ordering::SeqCst),
        }
        result
    }

    /// Grace period [`join`](Self::join) grants an in-flight execution to finish
    /// before the job loop is aborted (Java waits indefinitely; a hard bound keeps
    /// a wedged provider call from hanging `shutdown`).
    pub(crate) const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    /// Stops the job loop and waits for it to finish.
    ///
    /// Mirrors Java `Scheduler.shutdown(true)` / `deleteJob`, neither of which
    /// interrupts a running job: the loop is cooperative, so once it observes
    /// [`shutdown`](Self::shutdown) it returns *after* the in-flight execution
    /// completed. Only [`interrupt`](Self::interrupt) abandons a running
    /// execution, exactly like Quartz `InterruptableJob`.
    pub(crate) async fn join(&self) {
        self.join_with_grace(Self::SHUTDOWN_GRACE).await
    }

    /// [`join`](Self::join) with an explicit grace period.
    ///
    /// The loop is asked to stop first (its `cancelled()`/`wait_if_paused()`
    /// selects win over the next sleep); the handle is aborted only if the
    /// in-flight execution outlives `grace`, so a wedged provider call cannot
    /// dead-wait the caller forever.
    pub(crate) async fn join_with_grace(&self, grace: std::time::Duration) {
        let handle = self.handle.lock().unwrap().take();
        if let Some(handle) = handle {
            let abort = handle.abort_handle();
            if tokio::time::timeout(grace, handle).await.is_err() {
                abort.abort();
            }
        }
    }
}

#[async_trait]
impl ScheduleAgentTask for QuartzScheduleAgentTask {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn id(&self) -> &str {
        self.inner.id()
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, messages: Vec<Msg>) -> Result<Msg> {
        self.execute_with(messages).await
    }

    fn cancel(&self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

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

    fn task(
        name: &str,
        factory: Arc<dyn AgentFactory>,
        schedule_config: ScheduleConfig,
    ) -> QuartzScheduleAgentTask {
        QuartzScheduleAgentTask::new(
            runtime(name),
            schedule_config,
            name.to_string(),
            factory,
            None,
            None,
        )
    }

    /// Factory used to observe in-flight state; sleeps long enough for the test
    /// to interrupt it.
    struct SlowFactory {
        started: AtomicUsize,
        sleep_ms: u64,
    }

    #[async_trait]
    impl AgentFactory for SlowFactory {
        async fn run(&self, _config: &RuntimeAgentConfig, _messages: Vec<Msg>) -> Result<Msg> {
            self.started.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
            Ok(Msg::assistant("slow-done"))
        }
    }

    struct FailingFactory;

    #[async_trait]
    impl AgentFactory for FailingFactory {
        async fn run(&self, _config: &RuntimeAgentConfig, _messages: Vec<Msg>) -> Result<Msg> {
            Err(Error::Agent("boom".into()))
        }
    }

    #[tokio::test]
    async fn pause_resume_status_transitions() {
        let task = task("A", Arc::new(EchoFactory), ScheduleConfig::none());
        assert_eq!(task.status(), TaskStatus::Normal);
        task.pause();
        assert_eq!(task.status(), TaskStatus::Paused);
        task.resume();
        assert_eq!(task.status(), TaskStatus::Normal);
        task.shutdown();
        assert_eq!(task.status(), TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn execute_increments_count() {
        let task = task("A", Arc::new(EchoFactory), ScheduleConfig::none());
        let out = task.execute().await.unwrap();
        assert_eq!(out.text_content(), "echo:0");
        assert_eq!(task.execution_count(), 1);
    }

    #[tokio::test]
    async fn cancelled_execute_is_rejected() {
        let task = task("A", Arc::new(EchoFactory), ScheduleConfig::none());
        task.shutdown();
        assert!(task.execute().await.is_err());
    }

    #[tokio::test]
    async fn config_accessors_are_exposed() {
        // Regression for tracker §7.4 (P1-21): Java exposes
        // `getScheduleConfig()` / `getAgentConfig()` / `getExecutionCount()`.
        let cfg = ScheduleConfig::builder().fixed_rate(250).build().unwrap();
        let task = task("A", Arc::new(EchoFactory), cfg.clone());
        assert_eq!(task.schedule_config(), &cfg);
        assert_eq!(task.schedule_config().fixed_rate(), Some(250));
        assert_eq!(task.agent_config().name(), "A");
        assert_eq!(task.agent_config().model_name(), "m");
        assert_eq!(task.runtime().name(), "A");

        task.execute().await.unwrap();
        task.execute().await.unwrap();
        assert_eq!(task.execution_count(), 2);
    }

    #[tokio::test]
    async fn status_reports_error_after_failed_execution() {
        let task = task("A", Arc::new(FailingFactory), ScheduleConfig::none());
        assert!(task.execute().await.is_err());
        assert_eq!(task.status(), TaskStatus::Error);
        assert!(!task.status().is_terminal());
    }

    #[tokio::test]
    async fn interrupt_aborts_in_flight_execution() {
        let factory = Arc::new(SlowFactory {
            started: AtomicUsize::new(0),
            sleep_ms: 5_000,
        });
        let task = Arc::new(task("A", factory.clone(), ScheduleConfig::none()));

        let running = task.clone();
        let handle = tokio::spawn(async move { running.execute().await });

        // Wait until the execution is really in flight.
        for _ in 0..200 {
            if task.is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(task.is_running(), "execution never started");
        // Quartz `BLOCKED` while a job occupies the trigger.
        assert_eq!(task.status(), TaskStatus::Blocked);

        assert!(task.interrupt(), "interrupt should report an in-flight run");

        let result = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("interrupt must end the in-flight execution immediately")
            .expect("task join");
        let err = result.expect_err("interrupted run must fail");
        assert!(matches!(err, Error::Interrupted(_)), "err was {err}");
        assert!(err.to_string().contains("interrupted"), "err was {err}");
        assert_eq!(task.interrupted_count(), 1);
        assert!(!task.is_running());
        // An interrupted run is not an error state (Java skips the reschedule).
        assert_eq!(task.status(), TaskStatus::Normal);
        assert_eq!(factory.started.load(Ordering::SeqCst), 1);
        // The execution was aborted, so it never reached the factory's return.
        assert_eq!(task.execution_count(), 0);
    }

    #[tokio::test]
    async fn interrupt_while_idle_does_not_abort_the_next_run() {
        // Java creates a fresh interruptable job per firing, so an interrupt that
        // arrives while nothing runs must not leak into the next execution.
        let task = Arc::new(task("A", Arc::new(EchoFactory), ScheduleConfig::none()));
        assert!(!task.interrupt(), "nothing was running");
        assert!(task.execute().await.is_ok());
        assert_eq!(task.execution_count(), 1);
        assert_eq!(task.interrupted_count(), 0);
    }

    #[tokio::test]
    async fn completed_task_is_terminal_and_has_no_next_fire_time() {
        let task = task("A", Arc::new(EchoFactory), ScheduleConfig::none());
        assert!(task.next_fire_time(Utc::now()).is_none());
        task.mark_completed();
        assert_eq!(task.status(), TaskStatus::Complete);
        assert!(task.status().is_terminal());
        assert_eq!(TaskStatus::None.as_str(), "NONE");
        assert!(TaskStatus::Cancelled.is_terminal());
        assert!(!TaskStatus::Blocked.is_terminal());
    }
}
