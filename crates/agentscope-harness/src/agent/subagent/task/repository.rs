//! Repository for managing background subagent tasks, scoped by session, mirroring
//! `TaskRepository`.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use async_trait::async_trait;

use super::background::BackgroundTask;
use super::delivery::TaskDelivery;
use super::spec::TaskRunSpec;
use super::status::TaskStatus;

/// Callback invoked when a task reaches a terminal state (COMPLETED or FAILED).
/// Arguments: task id, parent session id, result (`None` for failed tasks).
pub type TaskCompletionCallback = Arc<dyn Fn(String, String, Option<String>) + Send + Sync>;

/// Prefix of the completion payload a cancelled task reports.
///
/// The callback carries no status field, so a cancellation travels as its own result text
/// instead of being folded into "failed" (which made a cancelled subagent read like a broken
/// one) or into a success (which would claim work that never finished).
pub const CANCELLED_RESULT_PREFIX: &str = "[cancelled] ";

/// What a cancellation attempt actually achieved.
///
/// `Requested` is deliberately distinct from `Terminated`: a local task runs on the blocking
/// pool, which cannot be aborted, so the honest answer is that cancellation was requested and
/// the run may still be unwinding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// No task with that id is tracked for the session.
    NotFound,
    /// The task already reached a terminal state; nothing to cancel.
    AlreadyTerminal(TaskStatus),
    /// Cancellation was delivered; the run may still be finishing.
    Requested,
    /// The task is terminal now (it had already stopped when the cancel landed).
    Terminated,
    /// Cancellation could not be delivered (e.g. the remote endpoint refused it).
    Failed(String),
}

impl CancelOutcome {
    /// Whether a caller should treat this as "found and cancellation attempted".
    pub fn accepted(&self) -> bool {
        matches!(
            self,
            CancelOutcome::Requested
                | CancelOutcome::Terminated
                | CancelOutcome::AlreadyTerminal(_)
        )
    }
}

/// [`RuntimeContext`](agentscope_core::agent::RuntimeContext) `metadata` key for callers that
/// retain execution ownership and collect results themselves; when set to `true` no completion
/// callback is fired for that call. Mirrors Java `TaskRepository#SUPPRESS_COMPLETION_CALLBACK`.
pub const SUPPRESS_COMPLETION_CALLBACK: &str = "harness.suppressTaskCompletionCallback";

/// Repository for managing background subagent tasks, scoped by session. All operations are
/// scoped to a `session_id` so that tasks from different parent sessions are isolated.
#[async_trait]
pub trait TaskRepository: Send + Sync {
    /// Retrieve a background task by session and task id, or `None` if not found.
    async fn get_task(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
        task_id: &str,
    ) -> Option<Arc<BackgroundTask>>;

    /// Submit a new background task according to `TaskRunSpec`.
    async fn put_task(
        &self,
        ctx: &RuntimeContext,
        task_id: &str,
        sub_agent_id: &str,
        session_id: &str,
        spec: TaskRunSpec,
    ) -> Arc<BackgroundTask>;

    /// Remove a task from the repository.
    async fn remove_task(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str);

    /// Clear all tasks across all sessions.
    fn clear(&self);

    /// List all tracked tasks for the given session, optionally filtered by status.
    async fn list_tasks(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
        filter: Option<TaskStatus>,
    ) -> Vec<Arc<BackgroundTask>>;

    /// Cancel a running task. Returns true if the task was found and cancellation attempted.
    ///
    /// Kept for callers that only need the boolean; [`Self::request_cancel`] reports what the
    /// attempt actually achieved (a failed remote cancel used to answer `true` here).
    async fn cancel_task(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        self.request_cancel(ctx, session_id, task_id)
            .await
            .accepted()
    }

    /// Cancel every task that belongs to `session_id` and is still live.
    ///
    /// A cancelled parent used to stop alone: its children kept running (and billing) with
    /// nobody to receive their result. Returns how many tasks were signalled.
    async fn cancel_session_tasks(&self, ctx: &RuntimeContext, session_id: &str) -> usize {
        let mut signalled = 0;
        for task in self.list_tasks(ctx, session_id, None).await {
            if task.get_task_status().is_terminal() {
                continue;
            }
            if self
                .request_cancel(ctx, session_id, task.task_id())
                .await
                .accepted()
            {
                signalled += 1;
            }
        }
        signalled
    }

    /// Cancel a running task and report the exact outcome.
    ///
    /// The default implementation preserves the pull-only behaviour of repositories that only
    /// implement [`Self::cancel_task`].
    async fn request_cancel(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
        task_id: &str,
    ) -> CancelOutcome {
        match self.get_task(ctx, session_id, task_id).await {
            None => CancelOutcome::NotFound,
            Some(task) if task.get_task_status().is_terminal() => {
                CancelOutcome::AlreadyTerminal(task.get_task_status())
            }
            Some(task) => {
                task.cancel();
                if task.is_execution_finished() {
                    CancelOutcome::Terminated
                } else {
                    CancelOutcome::Requested
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // Phase B-3 — push delivery API. Defaults preserve pull-only semantics for
    // in-memory repositories.
    // ---------------------------------------------------------------------

    /// Returns terminal-state tasks whose completions have not yet been pushed back.
    async fn find_pending_deliveries(
        &self,
        ctx: &RuntimeContext,
        _session_id: &str,
    ) -> Vec<TaskDelivery> {
        let _ = ctx;
        Vec::new()
    }

    /// Stamps the given task as delivered so it does not get pushed again.
    async fn mark_delivered(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) {
        let _ = (ctx, session_id, task_id);
    }

    /// Whether the given task has already been delivered to the parent agent.
    async fn is_delivered(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        let _ = (ctx, session_id, task_id);
        false
    }

    // ---------------------------------------------------------------------
    // Lifecycle. Persistent repositories override both; the defaults preserve the
    // behaviour of in-memory repositories that have nothing to release.
    // ---------------------------------------------------------------------

    /// Registers a callback invoked when any task reaches a terminal state (COMPLETED or
    /// FAILED). Only one callback is supported; a second call replaces the previous one.
    ///
    /// Synchronous like Java's `setCompletionCallback`: `HarnessAgent::build` wires it while
    /// assembling the agent, which is not an async context.
    fn set_completion_callback(&self, _callback: TaskCompletionCallback) {}

    /// Releases background maintenance resources (heartbeat / orphan sweeper).
    async fn shutdown(&self) {}

    /// Persists live in-memory task handles before the process goes away.
    ///
    /// A background task runs in this process even when its record write failed; flushing it at
    /// shutdown is the last chance to keep its result discoverable after a restart. The default
    /// is a no-op for repositories without a local overlay.
    async fn flush_live(&self) {}
}
