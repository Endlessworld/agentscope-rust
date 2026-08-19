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
    state: Arc<TaskState>,
}

impl BackgroundTask {
    fn new(task_id: String, agent_id: String, handle: Option<tokio::task::JoinHandle<()>>) -> Arc<Self> {
        let now = now_millis();
        Arc::new(Self {
            task_id,
            agent_id,
            created_at: now,
            last_checked_at: AtomicU64::new(now),
            cancelled: AtomicBool::new(false),
            handle: Mutex::new(handle),
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

    /// Spawns a blocking local execution and tracks it (self-contained).
    pub fn spawn_local<F>(task_id: impl Into<String>, agent_id: impl Into<String>, execution: F) -> Arc<Self>
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
            state,
        })
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
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

    pub fn is_completed(&self) -> bool {
        matches!(*self.state.status.lock().unwrap(), TaskInner::Done(_))
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
        match &*self.state.status.lock().unwrap() {
            TaskInner::Done(Err(e)) => Some(e.clone()),
            _ => None,
        }
    }

    /// Marks the task cancelled and aborts the backing execution, if any.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.lock().unwrap().take() {
            h.abort();
        }
        {
            let mut st = self.state.status.lock().unwrap();
            if matches!(*st, TaskInner::Running) {
                *st = TaskInner::Done(Err("task cancelled".to_string()));
            }
        }
        self.state.condvar.notify_all();
    }

    /// Blocks until the task completes or the timeout elapses.
    pub fn wait_for_completion(&self, timeout_ms: u64) -> bool {
        let timeout = Duration::from_millis(timeout_ms);
        let mut st = self.state.status.lock().unwrap();
        let start = std::time::Instant::now();
        while matches!(*st, TaskInner::Running) {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return false;
            }
            let (guard, _) = self.state.condvar.wait_timeout(st, timeout - elapsed).unwrap();
            st = guard;
        }
        true
    }
}
