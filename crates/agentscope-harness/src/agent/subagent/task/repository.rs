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
    async fn cancel_task(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool;

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
    async fn set_completion_callback(&self, _callback: TaskCompletionCallback) {}

    /// Releases background maintenance resources (heartbeat / orphan sweeper).
    async fn shutdown(&self) {}
}
