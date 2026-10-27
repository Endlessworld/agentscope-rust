//! `QuartzScheduleAgentTask`, mirroring
//! `io.agentscope.extensions.scheduler.quartz.QuartzScheduleAgentTask`.
//!
//! Java wraps a Quartz `JobKey` + `Scheduler`. Here it wraps the tokio task
//! handle plus a control flag set; pause/resume/interrupt/status mirror the
//! Quartz job lifecycle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::Msg;

use crate::common::config::{RuntimeAgentConfig, ScheduleConfig};
use crate::common::scheduler::task::{AgentFactory, BaseScheduleAgentTask, ScheduleAgentTask};

/// Mutable status of a scheduled task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Scheduled and not paused.
    Scheduled,
    /// Paused; no further automatic executions.
    Paused,
    /// Cancelled / shut down.
    Cancelled,
}

/// Quartz-flavoured task handle.
pub struct QuartzScheduleAgentTask {
    inner: BaseScheduleAgentTask,
    name: String,
    factory: Arc<dyn AgentFactory>,
    runtime: Arc<RuntimeAgentConfig>,
    /// Initial input stored at scheduling time, mirroring the Quartz
    /// `JobDataMap#inputMsg` entry. Replayed on every automatic firing.
    initial_input: Option<Msg>,
    paused: AtomicBool,
    running: AtomicBool,
    cancelled: AtomicBool,
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
    ) -> Self {
        let inner = BaseScheduleAgentTask::new(runtime.clone(), schedule_config, factory.clone());
        Self {
            inner,
            name,
            factory,
            runtime,
            initial_input,
            paused: AtomicBool::new(false),
            running: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
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
            self.inner.schedule_config().clone(),
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

    /// The agent factory used for executions.
    pub fn factory(&self) -> Arc<dyn AgentFactory> {
        self.factory.clone()
    }

    /// Total executions (scheduled + manual), mirroring Java
    /// `BaseScheduleAgentTask#getExecutionCount`.
    pub fn execution_count(&self) -> u64 {
        self.inner.execution_count()
    }

    /// The initial input message stored at scheduling time, if any.
    pub fn initial_input(&self) -> Option<&Msg> {
        self.initial_input.as_ref()
    }

    /// Returns the current status.
    pub fn status(&self) -> TaskStatus {
        if self.cancelled.load(Ordering::SeqCst) {
            TaskStatus::Cancelled
        } else if self.paused.load(Ordering::SeqCst) {
            TaskStatus::Paused
        } else {
            TaskStatus::Scheduled
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

    /// Interrupts a currently running execution (best effort).
    pub(crate) fn interrupt(&self) {
        self.notify.notify_waiters();
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
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Agent(format!(
                "Cannot run a cancelled task: {}",
                self.name
            )));
        }
        self.running.store(true, Ordering::SeqCst);
        // Mirror `AgentQuartzJob`: replay the stored input on each firing.
        let messages = self
            .initial_input
            .clone()
            .map(|msg| vec![msg])
            .unwrap_or_default();
        let result = self.inner.run_with(messages).await;
        self.running.store(false, Ordering::SeqCst);
        result
    }

    /// Executes one run with explicit input messages.
    pub async fn execute_with(&self, messages: Vec<Msg>) -> Result<Msg> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Agent(format!(
                "Cannot run a cancelled task: {}",
                self.name
            )));
        }
        self.running.store(true, Ordering::SeqCst);
        let result = self.inner.run_with(messages).await;
        self.running.store(false, Ordering::SeqCst);
        result
    }

    /// Stops the job loop and waits for it to finish.
    pub(crate) async fn join(&self) {
        let handle = self.handle.lock().unwrap().take();
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
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
    async fn pause_resume_status_transitions() {
        let task = QuartzScheduleAgentTask::new(
            runtime("A"),
            ScheduleConfig::none(),
            "A".to_string(),
            Arc::new(EchoFactory),
            None,
        );
        assert_eq!(task.status(), TaskStatus::Scheduled);
        task.pause();
        assert_eq!(task.status(), TaskStatus::Paused);
        task.resume();
        assert_eq!(task.status(), TaskStatus::Scheduled);
        task.shutdown();
        assert_eq!(task.status(), TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn execute_increments_count() {
        let task = QuartzScheduleAgentTask::new(
            runtime("A"),
            ScheduleConfig::none(),
            "A".to_string(),
            Arc::new(EchoFactory),
            None,
        );
        let out = task.execute().await.unwrap();
        assert_eq!(out.text_content(), "echo:0");
        assert_eq!(task.execution_count(), 1);
    }

    #[tokio::test]
    async fn cancelled_execute_is_rejected() {
        let task = QuartzScheduleAgentTask::new(
            runtime("A"),
            ScheduleConfig::none(),
            "A".to_string(),
            Arc::new(EchoFactory),
            None,
        );
        task.shutdown();
        assert!(task.execute().await.is_err());
    }
}
