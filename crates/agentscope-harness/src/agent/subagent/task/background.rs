//! Wraps a tokio task to track background subagent task execution with status, result,
//! error and lifecycle metadata, mirroring `BackgroundTask`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::now_millis;
use super::status::TaskStatus;

/// Inner shared state updated by the background task and read by the handle.
struct TaskState {
    status: Mutex<TaskInner>,
    condvar: Condvar,
}

enum TaskInner {
    Running,
    Done(Result<String, String>),
}

/// Wraps an async execution to track a background subagent task.
pub struct BackgroundTask {
    task_id: String,
    agent_id: String,
    created_at: u64,
    last_checked_at: AtomicU64,
    cancelled: AtomicBool,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Host-visible session id of the subagent this run belongs to (`sub-<uuid>`).
    ///
    /// Reconstructed from the durable record so a host replaying a session can name the same
    /// child the live client saw. `None` for tasks that are not subagent turns (and for records
    /// written before the field existed).
    sub_session_id: Mutex<Option<String>>,
    state: Arc<TaskState>,
}

impl BackgroundTask {
    fn new(
        task_id: String,
        agent_id: String,
        handle: Option<tokio::task::JoinHandle<()>>,
    ) -> Arc<Self> {
        Self::new_at(task_id, agent_id, now_millis(), handle)
    }

    fn new_at(
        task_id: String,
        agent_id: String,
        created_at: u64,
        handle: Option<tokio::task::JoinHandle<()>>,
    ) -> Arc<Self> {
        let now = now_millis();
        Arc::new(Self {
            task_id,
            agent_id,
            created_at,
            last_checked_at: AtomicU64::new(now),
            cancelled: AtomicBool::new(false),
            handle: Mutex::new(handle),
            sub_session_id: Mutex::new(None),
            state: Arc::new(TaskState {
                status: Mutex::new(TaskInner::Running),
                condvar: Condvar::new(),
            }),
        })
    }

    /// Creates a controlled running task with no backing execution yet. The caller drives it via
    /// `attach_handle` + `set_done`. Used by the task repository to run side effects (persist,
    /// callback) on completion.
    pub fn controlled(task_id: impl Into<String>, agent_id: impl Into<String>) -> Arc<Self> {
        Self::new(task_id.into(), agent_id.into(), None)
    }

    /// Attaches the backing execution handle (used by `cancel` to abort).
    pub fn attach_handle(&self, handle: tokio::task::JoinHandle<()>) {
        *self.handle.lock().unwrap() = Some(handle);
    }

    /// Records the terminal outcome and wakes any waiters.
    pub fn set_done(&self, result: Result<String, String>) {
        let mut st = self.state.status.lock().unwrap();
        *st = TaskInner::Done(result);
        self.state.condvar.notify_all();
    }

    /// Creates a synthetic task already in a terminal state (reconstructed from a workspace record).
    pub fn from_result(
        task_id: impl Into<String>,
        agent_id: impl Into<String>,
        result: Result<String, String>,
    ) -> Arc<Self> {
        let bt = Self::new(task_id.into(), agent_id.into(), None);
        bt.set_done(result);
        bt
    }

    /// Rebuilds a task from its durable record, preserving the persisted `created_at` and the
    /// exact lifecycle status.
    ///
    /// Both matter for reporting: a reconstructed task that starts from "now" makes every
    /// listing claim the task was just created, and collapsing `Cancelled` into a failed result
    /// makes an explicit cancellation indistinguishable from an execution error.
    pub fn from_record(
        task_id: impl Into<String>,
        agent_id: impl Into<String>,
        created_at: u64,
        status: TaskStatus,
        result: Option<String>,
        error_message: Option<String>,
    ) -> Arc<Self> {
        let bt = Self::new_at(task_id.into(), agent_id.into(), created_at, None);
        match status {
            TaskStatus::Completed => bt.set_done(Ok(result.unwrap_or_default())),
            TaskStatus::Cancelled => {
                bt.cancelled.store(true, Ordering::Relaxed);
                bt.set_done(Err(
                    error_message.unwrap_or_else(|| "task cancelled".to_string())
                ));
            }
            TaskStatus::Failed => {
                bt.set_done(Err(error_message.unwrap_or_else(|| "Failed".to_string())))
            }
            // Nothing is executing any more, but the record is the only truth left: report the
            // task exactly as the record describes it instead of inventing a failure.
            TaskStatus::Pending | TaskStatus::Running => {}
        }
        bt
    }

    /// Spawns a blocking local execution and tracks it (self-contained).
    pub fn spawn_local<F>(
        task_id: impl Into<String>,
        agent_id: impl Into<String>,
        execution: F,
    ) -> Arc<Self>
    where
        F: Fn() -> agentscope_core::error::Result<String> + Send + Sync + 'static,
    {
        let task_id = task_id.into();
        let agent_id = agent_id.into();
        let state = Arc::new(TaskState {
            status: Mutex::new(TaskInner::Running),
            condvar: Condvar::new(),
        });
        let state2 = state.clone();
        let handle = tokio::task::spawn(async move {
            #[allow(clippy::redundant_closure)]
            let outcome = tokio::task::spawn_blocking(move || execution()).await;
            let result = match outcome {
                Ok(Ok(r)) => Ok(r),
                Ok(Err(e)) => Err(e.to_string()),
                Err(je) => Err(format!("task execution panicked: {je}")),
            };
            let mut st = state2.status.lock().unwrap();
            *st = TaskInner::Done(result);
            state2.condvar.notify_all();
        });
        Arc::new(Self {
            task_id,
            agent_id,
            created_at: now_millis(),
            last_checked_at: AtomicU64::new(now_millis()),
            cancelled: AtomicBool::new(false),
            handle: Mutex::new(Some(handle)),
            sub_session_id: Mutex::new(None),
            state,
        })
    }

    /// Adopts an already-running tokio task and tracks its completion (self-contained).
    pub fn adopt(
        task_id: impl Into<String>,
        agent_id: impl Into<String>,
        future: tokio::task::JoinHandle<agentscope_core::error::Result<String>>,
    ) -> Arc<Self> {
        let task_id = task_id.into();
        let agent_id = agent_id.into();
        let state = Arc::new(TaskState {
            status: Mutex::new(TaskInner::Running),
            condvar: Condvar::new(),
        });
        let state2 = state.clone();
        let handle = tokio::task::spawn(async move {
            let result = match future.await {
                Ok(Ok(r)) => Ok(r),
                Ok(Err(e)) => Err(e.to_string()),
                Err(je) => Err(format!("task join error: {je}")),
            };
            let mut st = state2.status.lock().unwrap();
            *st = TaskInner::Done(result);
            state2.condvar.notify_all();
        });
        Arc::new(Self {
            task_id,
            agent_id,
            created_at: now_millis(),
            last_checked_at: AtomicU64::new(now_millis()),
            cancelled: AtomicBool::new(false),
            handle: Mutex::new(Some(handle)),
            sub_session_id: Mutex::new(None),
            state,
        })
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The host-visible `sub-<uuid>` session this run belongs to, when the record carries one.
    pub fn sub_session_id(&self) -> Option<String> {
        self.sub_session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Restores the child session id recorded when the task was created.
    pub fn set_sub_session_id(&self, session_id: Option<String>) {
        *self
            .sub_session_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = session_id;
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn last_checked_at(&self) -> u64 {
        self.last_checked_at.load(Ordering::Relaxed)
    }

    pub fn update_last_checked_at(&self) {
        self.last_checked_at.store(now_millis(), Ordering::Relaxed);
    }

    /// Restores the last observed timestamp from a durable record.
    ///
    /// A reconstructed task otherwise claims it was "checked" at load time, which makes a replay
    /// report every finished subagent as having completed just now.
    pub fn set_last_checked_at(&self, ts: u64) {
        self.last_checked_at.store(ts, Ordering::Relaxed);
    }

    pub fn is_completed(&self) -> bool {
        // A cancellation is terminal for observation: no result will ever be reported for the
        // task. The backing execution may still be unwinding (see `is_execution_finished`).
        self.cancelled.load(Ordering::Relaxed)
            || matches!(*self.state.status.lock().unwrap(), TaskInner::Done(_))
    }

    /// Returns the structured `TaskStatus`, reflecting the underlying execution and any
    /// explicit cancellation.
    pub fn get_task_status(&self) -> TaskStatus {
        if self.cancelled.load(Ordering::Relaxed) {
            return TaskStatus::Cancelled;
        }
        match &*self.state.status.lock().unwrap() {
            TaskInner::Running => TaskStatus::Running,
            TaskInner::Done(Ok(_)) => TaskStatus::Completed,
            TaskInner::Done(Err(_)) => TaskStatus::Failed,
        }
    }

    /// Human-readable status string.
    pub fn get_status_string(&self) -> String {
        let ts = self.get_task_status();
        if ts == TaskStatus::Failed {
            match self.get_error() {
                Some(e) => format!("Failed: {e}"),
                None => "Failed: Unknown error".to_string(),
            }
        } else {
            ts.as_str().to_string()
        }
    }

    /// Non-blocking result peek.
    pub fn get_result(&self) -> Option<String> {
        match &*self.state.status.lock().unwrap() {
            TaskInner::Done(Ok(r)) => Some(r.clone()),
            _ => None,
        }
    }

    /// Non-blocking error peek.
    pub fn get_error(&self) -> Option<String> {
        if self.cancelled.load(Ordering::Relaxed)
            && !matches!(*self.state.status.lock().unwrap(), TaskInner::Done(_))
        {
            // Same wording a reconstructed record reports, so a live cancellation and a
            // replay of one read the same.
            return Some("task cancelled".to_string());
        }
        match &*self.state.status.lock().unwrap() {
            TaskInner::Done(Err(e)) => Some(e.clone()),
            _ => None,
        }
    }

    /// Marks the task cancelled and aborts the backing execution, if any.
    ///
    /// A blocking local execution runs on the blocking pool, which `abort()` cannot stop: the
    /// wrapper task is gone but the closure keeps running. Recording `Done` here used to lie
    /// about that — callers were told the work had terminated when all that happened is that
    /// cancellation was requested. The task therefore stays *not finished*
    /// (`is_execution_finished`) while `get_task_status` already reports `Cancelled`, and only
    /// the real completion path records `Done`.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.lock().unwrap().take() {
            h.abort();
        }
        // Wake waiters: the status changed from Running to Cancelled even though the work may
        // still be unwinding.
        self.state.condvar.notify_all();
    }

    /// Whether cancellation was requested (and no result will be reported).
    pub fn is_cancellation_requested(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Whether the backing execution actually reached a terminal state.
    ///
    /// A cancelled blocking task can stay `false` for as long as its closure runs.
    pub fn is_execution_finished(&self) -> bool {
        matches!(*self.state.status.lock().unwrap(), TaskInner::Done(_))
    }

    /// Blocks until the task completes or the timeout elapses.
    pub fn wait_for_completion(&self, timeout_ms: u64) -> bool {
        let timeout = Duration::from_millis(timeout_ms);
        let mut st = self.state.status.lock().unwrap();
        let start = std::time::Instant::now();
        while matches!(*st, TaskInner::Running) {
            // Cancellation settles the observation even when the blocking work keeps running;
            // waiting for it would park the caller for as long as an unkillable closure takes.
            if self.cancelled.load(Ordering::Relaxed) {
                return true;
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return false;
            }
            let (guard, _) = self
                .state
                .condvar
                .wait_timeout(st, timeout - elapsed)
                .unwrap();
            st = guard;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_blocked_task_does_not_claim_its_execution_ended() {
        // Regression: `cancel()` used to write `Done(Err("task cancelled"))` immediately. A
        // local task runs on the blocking pool, which `abort()` cannot stop, so that made the
        // tool report a terminated execution while the closure was still running.
        let task = BackgroundTask::controlled("t1", "sub");
        task.cancel();
        assert_eq!(task.get_task_status(), TaskStatus::Cancelled);
        assert!(task.is_cancellation_requested());
        assert!(
            task.is_completed(),
            "cancellation is terminal for observation"
        );
        assert!(
            !task.is_execution_finished(),
            "the blocking closure was never stopped, so it has not finished"
        );
        assert_eq!(task.get_error().as_deref(), Some("task cancelled"));

        // The real completion still lands, and it does not resurrect the task as successful.
        task.set_done(Ok("late result".into()));
        assert!(task.is_execution_finished());
        assert_eq!(task.get_task_status(), TaskStatus::Cancelled);
    }

    #[test]
    fn from_record_preserves_created_at() {
        // Regression: a record rebuilt for a listing used to start from "now", so every
        // `task_list` reported the task as created at the moment of the listing.
        let created = 1_700_000_000_000u64;
        let task = BackgroundTask::from_record(
            "t1",
            "sub",
            created,
            TaskStatus::Completed,
            Some("done".into()),
            None,
        );
        assert_eq!(task.created_at(), created);
        assert_eq!(task.get_task_status(), TaskStatus::Completed);
        assert_eq!(task.get_result().as_deref(), Some("done"));
    }

    #[test]
    fn from_record_keeps_a_cancelled_task_cancelled() {
        // Regression: non-completed records were all folded into `Failed`, so an explicit
        // cancellation was indistinguishable from an execution error.
        let task = BackgroundTask::from_record(
            "t1",
            "sub",
            1_700_000_000_000,
            TaskStatus::Cancelled,
            None,
            None,
        );
        assert_eq!(task.get_task_status(), TaskStatus::Cancelled);
        assert_eq!(task.get_status_string(), "Cancelled");
        assert_eq!(task.get_error().as_deref(), Some("task cancelled"));
    }

    #[test]
    fn from_record_keeps_failure_details() {
        let task = BackgroundTask::from_record(
            "t1",
            "sub",
            1_700_000_000_000,
            TaskStatus::Failed,
            None,
            Some("boom".into()),
        );
        assert_eq!(task.get_task_status(), TaskStatus::Failed);
        assert_eq!(task.get_error().as_deref(), Some("boom"));
    }

    #[test]
    fn from_record_does_not_turn_a_live_record_into_a_failure() {
        let task = BackgroundTask::from_record(
            "t1",
            "sub",
            1_700_000_000_000,
            TaskStatus::Running,
            None,
            None,
        );
        assert_eq!(task.get_task_status(), TaskStatus::Running);
        assert!(!task.is_completed());
    }
}
