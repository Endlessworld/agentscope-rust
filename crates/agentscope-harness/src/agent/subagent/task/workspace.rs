//! Workspace-backed `TaskRepository`, mirroring `WorkspaceTaskRepository`.
//!
//! Storage layout: `agents/<parentAgentId>/tasks/<sessionId>.json` — a JSON map of
//! `taskId → TaskRecord`, using the harness `Filesystem` trait as the authoritative truth
//! source, while maintaining in-memory `BackgroundTask` handles as a local overlay for tasks
//! running on the current node.
//!
//! A background heartbeat/sweeper pair (started lazily on the first `put_task`) keeps
//! `last_updated_at` fresh for live local tasks and marks dropped-node tasks as `FAILED`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;

use crate::agent::coordination::{LocalPeriodicGate, PeriodicGate};
use crate::agent::subagent::protocol::RemotePendingConfirm;
use crate::filesystem::Filesystem;

use super::background::BackgroundTask;
use super::delivery::TaskDelivery;
use super::now_millis;
use super::protocol_client::{
    AgentProtocolTransport, RemoteSubagentTransport, RemoteSubmitContext, RemoteTarget,
};
use super::record::TaskRecord;
use super::repository::{TaskCompletionCallback, TaskRepository, SUPPRESS_COMPLETION_CALLBACK};
use super::spec::TaskRunSpec;
use super::status::TaskStatus;

const TRANSPORT_AGENT_PROTOCOL: &str = "agent-protocol";

/// Short, fixed poll interval used while a remote task is awaiting user confirmation.
const AWAITING_CONFIRM_POLL_MS: u64 = 750;
/// Upper bound on the exponential backoff between remote status polls.
const REMOTE_POLL_MAX_MS: u64 = 5_000;
/// Timeout handed to `wait_for_result` once the remote reports terminal success.
const REMOTE_RESULT_TIMEOUT_SECONDS: u64 = 120;
/// How often the heartbeat refreshes `last_updated_at` for live local tasks. Must be well below
/// [`ORPHAN_TIMEOUT_MINUTES`] to avoid false orphan detection.
pub const HEARTBEAT_INTERVAL_SECONDS: u64 = 30;
/// How long (in minutes) a non-terminal local task may remain un-heartbeated before the sweeper
/// marks it `FAILED`. Should be several multiples of [`HEARTBEAT_INTERVAL_SECONDS`].
pub const ORPHAN_TIMEOUT_MINUTES: u64 = 10;
/// How often (in minutes) the orphan sweeper scans all workspace task records.
pub const SWEEP_INTERVAL_MINUTES: u64 = 5;

/// State shared with the background maintenance tasks, hence `Arc`. Mirrors the Java per-field
/// concurrent maps (`localTasks` / `localTaskSessionIds` / `localTaskContexts`).
struct RepoState {
    fs: Arc<dyn Filesystem>,
    parent_agent_id: String,
    /// In-memory local task handles, keyed by `"<sessionId>:<taskId>"`.
    local_tasks: std::sync::Mutex<BTreeMap<String, Arc<BackgroundTask>>>,
    /// `localKey → (sessionId, RuntimeContext)` captured at `put_task` time, so the heartbeat and
    /// sweeper can persist task state under the originating user's namespace.
    local_contexts: std::sync::Mutex<BTreeMap<String, (String, RuntimeContext)>>,
}

/// Workspace-backed `TaskRepository` that uses the `Filesystem` trait as the authoritative
/// truth source for task state, with in-memory `BackgroundTask` handles as a local overlay.
pub struct WorkspaceTaskRepository {
    state: Arc<RepoState>,
    completion_callback: tokio::sync::RwLock<Option<TaskCompletionCallback>>,
    /// Remote execution transport; `agent-protocol` by default, injectable for tests.
    transport: Arc<dyn RemoteSubagentTransport>,
    /// Throttles orphan sweeps (process-local by default, store-backed for cross-replica).
    sweep_gate: Arc<dyn PeriodicGate>,
    /// Heartbeat / sweeper handles, started lazily and aborted on drop.
    maintenance: std::sync::Mutex<Option<Vec<tokio::task::JoinHandle<()>>>>,
}

impl WorkspaceTaskRepository {
    pub fn new(fs: Arc<dyn Filesystem>, parent_agent_id: impl Into<String>) -> Self {
        Self::with_sweep_gate(fs, parent_agent_id, Arc::new(LocalPeriodicGate::new()))
    }

    /// Creates a repository with an explicit [`PeriodicGate`] for orphan-sweep throttling. Pass a
    /// store-backed gate when a shared store is available so only one replica sweeps per interval.
    pub fn with_sweep_gate(
        fs: Arc<dyn Filesystem>,
        parent_agent_id: impl Into<String>,
        sweep_gate: Arc<dyn PeriodicGate>,
    ) -> Self {
        Self {
            state: Arc::new(RepoState {
                fs,
                parent_agent_id: parent_agent_id.into(),
                local_tasks: std::sync::Mutex::new(BTreeMap::new()),
                local_contexts: std::sync::Mutex::new(BTreeMap::new()),
            }),
            completion_callback: tokio::sync::RwLock::new(None),
            transport: Arc::new(AgentProtocolTransport::new()),
            sweep_gate,
            maintenance: std::sync::Mutex::new(None),
        }
    }

    /// Injects a transport, bypassing real HTTP calls (remote task tests).
    pub fn with_transport(mut self, transport: Arc<dyn RemoteSubagentTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Registers a callback invoked when any task reaches a terminal state (COMPLETED or
    /// FAILED). Only one callback is supported; a second call replaces the previous one.
    pub async fn set_completion_callback(&self, callback: TaskCompletionCallback) {
        *self.completion_callback.write().await = Some(callback);
    }

    fn tasks_path(&self, session_id: &str) -> String {
        tasks_path(&self.state.parent_agent_id, session_id)
    }

    async fn persist_record(&self, ctx: &RuntimeContext, session_id: &str, record: &TaskRecord) {
        let path = self.tasks_path(session_id);
        let mut map = read_records(self.state.fs.as_ref(), ctx, &path).await;
        map.insert(record.task_id.clone(), record.clone());
        write_records(self.state.fs.as_ref(), ctx, &path, &map).await;
    }

    /// Refreshes `last_updated_at` in the workspace for every live local task that is still
    /// running, so the orphan sweeper can distinguish a genuinely running task from one whose
    /// originating node has disappeared. Scheduled every [`HEARTBEAT_INTERVAL_SECONDS`].
    pub async fn heartbeat(&self) {
        heartbeat_once(&self.state).await;
    }

    /// Scans persisted task records and marks non-terminal local tasks `FAILED` when their
    /// `last_updated_at` is older than [`ORPHAN_TIMEOUT_MINUTES`]. Throttled via the sweep gate;
    /// the sweep itself is idempotent, so a lost claim is harmless.
    pub async fn sweep_orphaned_tasks(&self) {
        let interval = Duration::from_secs(SWEEP_INTERVAL_MINUTES * 60);
        let key = format!("task-sweep:{}", self.state.parent_agent_id);
        if !self.sweep_gate.try_claim(&key, interval) {
            tracing::debug!("Skipping orphan sweep for {} — periodic gate denied claim", key);
            return;
        }
        self.sweep_orphaned_tasks_with(
            Duration::from_secs(ORPHAN_TIMEOUT_MINUTES * 60),
            now_millis(),
        )
        .await;
    }

    /// Sweeps with an explicit orphan timeout and sweep time (tests pin the clock).
    pub async fn sweep_orphaned_tasks_with(&self, orphan_timeout: Duration, sweep_time_ms: u64) {
        sweep_once(&self.state, orphan_timeout, sweep_time_ms).await;
    }

    /// Starts the heartbeat / orphan-sweeper pair on first use. Requires a Tokio runtime; it is
    /// skipped silently when none is available. Handles are aborted on drop.
    fn ensure_maintenance(&self) {
        let mut guard = self.maintenance.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let gate = Arc::clone(&self.sweep_gate);

        let heartbeat_state = Arc::clone(&state);
        let heartbeat = handle.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECONDS));
            ticker.tick().await; // the first tick completes immediately
            loop {
                ticker.tick().await;
                heartbeat_once(&heartbeat_state).await;
            }
        });

        let sweeper_state = Arc::clone(&state);
        let sweeper = handle.spawn(async move {
            let interval = Duration::from_secs(SWEEP_INTERVAL_MINUTES * 60);
            let orphan_timeout = Duration::from_secs(ORPHAN_TIMEOUT_MINUTES * 60);
            // Jitter the first sweep across the interval to spread load when several nodes
            // start simultaneously.
            tokio::time::sleep(Duration::from_millis(now_millis() % interval.as_millis() as u64))
                .await;
            loop {
                let key = format!("task-sweep:{}", sweeper_state.parent_agent_id);
                if gate.try_claim(&key, interval) {
                    sweep_once(&sweeper_state, orphan_timeout, now_millis()).await;
                }
                tokio::time::sleep(interval).await;
            }
        });

        *guard = Some(vec![heartbeat, sweeper]);
    }

    /// Aborts the background maintenance tasks, if running.
    fn abort_maintenance(&self) {
        if let Ok(mut guard) = self.maintenance.lock() {
            if let Some(handles) = guard.take() {
                for h in handles {
                    h.abort();
                }
            }
        }
    }

    fn synthetic_from_record(&self, rec: &TaskRecord) -> Arc<BackgroundTask> {
        let result = match rec.status {
            TaskStatus::Completed => Ok(rec.result.clone().unwrap_or_default()),
            _ => Err(rec.error_message.clone().unwrap_or_else(|| rec.status.as_str().to_string())),
        };
        BackgroundTask::from_result(rec.task_id.clone(), rec.sub_agent_id.clone(), result)
    }
}

#[async_trait]
impl TaskRepository for WorkspaceTaskRepository {
    async fn get_task(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
        task_id: &str,
    ) -> Option<Arc<BackgroundTask>> {
        let key = format!("{session_id}:{task_id}");
        if let Some(local) = self.state.local_tasks.lock().unwrap().get(&key).cloned() {
            return Some(local);
        }
        let path = self.tasks_path(session_id);
        let map = read_records(self.state.fs.as_ref(), ctx, &path).await;
        map.get(task_id).map(|rec| self.synthetic_from_record(rec))
    }

    async fn put_task(
        &self,
        ctx: &RuntimeContext,
        task_id: &str,
        sub_agent_id: &str,
        session_id: &str,
        spec: TaskRunSpec,
    ) -> Arc<BackgroundTask> {
        let mut record = TaskRecord::new(task_id, sub_agent_id, &self.state.parent_agent_id, session_id);
        record.status = TaskStatus::Pending;
        if let TaskRunSpec::Remote { base_url, headers, .. } = &spec {
            record.transport_type = Some(TRANSPORT_AGENT_PROTOCOL.to_string());
            record.remote_base_url = Some(base_url.clone());
            if !headers.is_empty() {
                record.remote_headers = Some(headers.clone());
            }
        }
        self.persist_record(ctx, session_id, &record).await;

        let local_key = format!("{session_id}:{task_id}");
        let bg = BackgroundTask::controlled(task_id, sub_agent_id);

        let state = Arc::clone(&self.state);
        let transport = Arc::clone(&self.transport);
        let task_ctx = ctx.clone();
        let task_id = task_id.to_string();
        let session_id = session_id.to_string();
        let session_for_task = session_id.clone();
        let bg2 = bg.clone();
        let cb = self.completion_callback.read().await.clone();

        let handle = tokio::task::spawn(async move {
            let session_id = session_for_task;
            update_record_status(
                state.fs.as_ref(), &task_ctx, &state.parent_agent_id, &session_id, &task_id,
                TaskStatus::Running, None, None,
            )
            .await;

            let outcome = match spec {
                TaskRunSpec::Local { execution } => {
                    #[allow(clippy::redundant_closure)]
                    match tokio::task::spawn_blocking(move || execution()).await {
                        Ok(Ok(r)) => Ok(r),
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(je) => Err(format!("task execution panicked: {je}")),
                    }
                }
                TaskRunSpec::Remote { base_url, headers, agent_id, input, context } => {
                    run_remote_task(
                        &state,
                        transport.as_ref(),
                        &task_ctx,
                        &session_id,
                        &task_id,
                        &base_url,
                        &headers,
                        &agent_id,
                        &input,
                        context,
                    )
                    .await
                }
                TaskRunSpec::Adopted { future } => match future.await {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(je) => Err(format!("task join error: {je}")),
                },
            };

            let terminal = match &outcome {
                Ok(r) => {
                    update_record_status(
                        state.fs.as_ref(), &task_ctx, &state.parent_agent_id, &session_id, &task_id,
                        TaskStatus::Completed, Some(r.clone()), None,
                    )
                    .await;
                    Some(Some(r.clone()))
                }
                // A cancelled task is already terminal in the workspace; there is nothing to
                // report back to the parent.
                Err(e) if e.as_str() == CANCELLED_SENTINEL => None,
                Err(e) => {
                    update_record_status(
                        state.fs.as_ref(), &task_ctx, &state.parent_agent_id, &session_id, &task_id,
                        TaskStatus::Failed, None, Some(e.clone()),
                    )
                    .await;
                    Some(None)
                }
            };
            if let Some(result) = terminal {
                fire_callback(&task_ctx, &cb, task_id.clone(), session_id.clone(), result);
            }
            bg2.set_done(outcome);
        });
        bg.attach_handle(handle);

        self.state.local_tasks.lock().unwrap().insert(local_key.clone(), bg.clone());
        self.state
            .local_contexts
            .lock()
            .unwrap()
            .insert(local_key, (session_id.to_string(), ctx.clone()));
        self.ensure_maintenance();
        bg
    }

    async fn remove_task(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) {
        let key = format!("{session_id}:{task_id}");
        self.state.local_tasks.lock().unwrap().remove(&key);
        self.state.local_contexts.lock().unwrap().remove(&key);
        let path = self.tasks_path(session_id);
        let mut map = read_records(self.state.fs.as_ref(), ctx, &path).await;
        map.remove(task_id);
        write_records(self.state.fs.as_ref(), ctx, &path, &map).await;
    }

    fn clear(&self) {
        self.state.local_tasks.lock().unwrap().clear();
        self.state.local_contexts.lock().unwrap().clear();
    }

    async fn list_tasks(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
        filter: Option<TaskStatus>,
    ) -> Vec<Arc<BackgroundTask>> {
        let path = self.tasks_path(session_id);
        let map = read_records(self.state.fs.as_ref(), ctx, &path).await;
        let mut result = Vec::new();
        for (task_id, ws_record) in map {
            let key = format!("{session_id}:{task_id}");
            let local = self.state.local_tasks.lock().unwrap().get(&key).cloned();
            let effective = if let Some(l) = local {
                if !ws_record.status.is_terminal() {
                    l
                } else {
                    self.synthetic_from_record(&ws_record)
                }
            } else {
                self.synthetic_from_record(&ws_record)
            };
            let keep = match filter {
                Some(f) => effective.get_task_status() == f,
                None => true,
            };
            if keep {
                result.push(effective);
            }
        }
        result
    }

    async fn cancel_task(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        let key = format!("{session_id}:{task_id}");
        let local = self.state.local_tasks.lock().unwrap().get(&key).cloned();
        let found_local = local.is_some();

        let path = self.tasks_path(session_id);
        let mut snapshot: Option<TaskRecord> = None;

        // Durable cancellation is written first so completion listeners always observe a
        // terminal record, never a still-running one. The write goes through an
        // optimistic-concurrency loop: a plain read-modify-write here would race the poller's
        // `update_record_status` and could resurrect a stale `Running` record (both sides used
        // to stamp the same next version, so neither detected the conflict).
        for _ in 0..RECORD_CAS_RETRIES {
            let mut map = read_records(self.state.fs.as_ref(), ctx, &path).await;
            let base_version = match map.get(task_id) {
                Some(rec) => rec.version,
                None => break,
            };
            {
                let rec = map.get_mut(task_id).expect("checked above");
                rec.cancel_requested = true;
                if !rec.status.is_terminal() {
                    rec.status = TaskStatus::Cancelled;
                }
                rec.touch();
                rec.version = base_version.wrapping_add(1);
                snapshot = Some(rec.clone());
            }
            let current = read_records(self.state.fs.as_ref(), ctx, &path).await;
            if current.get(task_id).map(|r| r.version) == Some(base_version) {
                write_records(self.state.fs.as_ref(), ctx, &path, &map).await;
                break;
            }
        }

        if let Some(local) = local {
            local.cancel();
        }

        // Best-effort remote cancellation for agent-protocol tasks.
        if let Some(rec) = snapshot {
            if rec.is_agent_protocol_transport() {
                if let Some(base_url) = rec.remote_base_url.clone() {
                    let target = RemoteTarget {
                        base_url,
                        headers: rec.remote_headers.clone().unwrap_or_default(),
                    };
                    if let Err(e) = self.transport.cancel(&target, task_id).await {
                        tracing::warn!("Remote cancel failed for task {task_id}: {e}");
                    }
                }
            }
            return true;
        }
        found_local
    }

    async fn find_pending_deliveries(&self, ctx: &RuntimeContext, session_id: &str) -> Vec<TaskDelivery> {
        let path = self.tasks_path(session_id);
        let map = read_records(self.state.fs.as_ref(), ctx, &path).await;
        let mut deliveries: Vec<TaskDelivery> = map
            .values()
            .filter(|r| r.status.is_terminal() && r.delivered_at.is_none())
            .map(|rec| TaskDelivery {
                task_id: rec.task_id.clone(),
                agent_id: Some(rec.sub_agent_id.clone()),
                status: rec.status,
                result: rec.result.clone(),
                error_message: rec.error_message.clone(),
                completed_at: rec.last_updated_at,
            })
            .collect();
        deliveries.sort_by_key(|d| d.completed_at);
        deliveries
    }

    async fn mark_delivered(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) {
        let path = self.tasks_path(session_id);
        let mut map = read_records(self.state.fs.as_ref(), ctx, &path).await;
        match map.get_mut(task_id) {
            // Idempotent: the first stamp wins, later calls bail out without touching storage.
            Some(rec) if rec.delivered_at.is_none() => {
                rec.delivered_at = Some(now_millis());
                rec.touch();
                rec.version = rec.version.wrapping_add(1);
            }
            _ => return,
        }
        write_records(self.state.fs.as_ref(), ctx, &path, &map).await;
    }

    async fn is_delivered(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        let path = self.tasks_path(session_id);
        let map = read_records(self.state.fs.as_ref(), ctx, &path).await;
        map.get(task_id).map(|r| r.is_delivered()).unwrap_or(false)
    }

    async fn set_completion_callback(&self, callback: TaskCompletionCallback) {
        *self.completion_callback.write().await = Some(callback);
    }

    async fn shutdown(&self) {
        self.abort_maintenance();
    }
}

// ---------------------------------------------------------------------
//  Persistence helpers
// ---------------------------------------------------------------------

fn tasks_path(parent_agent_id: &str, session_id: &str) -> String {
    format!("agents/{parent_agent_id}/tasks/{session_id}.json")
}

async fn read_records(
    fs: &dyn Filesystem,
    ctx: &RuntimeContext,
    path: &str,
) -> BTreeMap<String, TaskRecord> {
    if !fs.exists(ctx, path).await.unwrap_or(false) {
        return BTreeMap::new();
    }
    match fs.read(ctx, path, 0, 0).await {
        Ok(data) => serde_json::from_str(&data.content).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    }
}

async fn write_records(
    fs: &dyn Filesystem,
    ctx: &RuntimeContext,
    path: &str,
    map: &BTreeMap<String, TaskRecord>,
) {
    if let Ok(json) = serde_json::to_string(map) {
        // `Filesystem::write` refuses to overwrite an existing path, so writes go through a
        // sidecar file that is then `rename`d over the target. `rename` is atomic within a
        // volume, so a concurrent reader never observes a missing file (the old
        // delete-then-write sequence opened exactly that window, and a racing writer could
        // resurrect a map that had already dropped this record).
        let tmp = format!("{path}.tmp");
        if fs.exists(ctx, &tmp).await.unwrap_or(false) {
            let _ = fs.delete(ctx, &tmp).await;
        }
        match fs.write(ctx, &tmp, &json).await {
            Ok(()) => {
                if let Err(e) = fs.move_path(ctx, &tmp, path).await {
                    tracing::warn!("Failed to commit task records to {path}: {e}");
                    let _ = fs.delete(ctx, &tmp).await;
                }
            }
            Err(e) => tracing::warn!("Failed to stage task records for {path}: {e}"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn update_record_status(
    fs: &dyn Filesystem,
    ctx: &RuntimeContext,
    parent_agent_id: &str,
    session_id: &str,
    task_id: &str,
    status: TaskStatus,
    result: Option<String>,
    error: Option<String>,
) {
    let path = tasks_path(parent_agent_id, session_id);
    // Optimistic-concurrency loop: re-read, re-apply the guards/mutation onto the freshest
    // record, bump `version`, and only persist when the on-disk version is still the one we
    // based our mutation on. A concurrent writer bumps `version`, forcing a retry.
    for _ in 0..RECORD_CAS_RETRIES {
        let mut map = read_records(fs, ctx, &path).await;
        let base_version = match map.get(task_id) {
            Some(rec) => rec.version,
            None => {
                write_records(fs, ctx, &path, &map).await;
                return;
            }
        };
        {
            let rec = map.get_mut(task_id).expect("checked above");
            // Guard 1: terminal states are immutable — a late COMPLETED/FAILED write from a
            // still-running thread must not clobber a CANCELLED status set by `cancel_task`.
            if rec.status.is_terminal() {
                return;
            }
            // Guard 2: cancellation requested but not yet terminal (heartbeat races cancel) —
            // refuse non-terminal updates so the record cannot flip back to RUNNING.
            if !status.is_terminal() && rec.cancel_requested {
                return;
            }
            rec.status = status;
            // Null fields leave the existing value untouched (Java `updateStatus`).
            if result.is_some() {
                rec.result = result.clone();
            }
            if error.is_some() {
                rec.error_message = error.clone();
            }
            rec.touch();
            rec.version = base_version.wrapping_add(1);
        }
        let current = read_records(fs, ctx, &path).await;
        if current.get(task_id).map(|r| r.version) == Some(base_version) {
            write_records(fs, ctx, &path, &map).await;
            return;
        }
    }
    tracing::warn!("update_record_status: CAS retries exhausted for task {task_id}");
}
impl Drop for WorkspaceTaskRepository {
    fn drop(&mut self) {
        self.abort_maintenance();
    }
}

// ---------------------------------------------------------------------
//  Remote execution (agent-protocol transport)
// ---------------------------------------------------------------------

/// Outcome marker for a task cancelled mid-flight: the workspace record is already terminal, so
/// the caller must not write a terminal status or fire the completion callback.
const CANCELLED_SENTINEL: &str = "__agentscope_cancelled__";

/// Maximum optimistic-concurrency retries for a guarded task-record write.
const RECORD_CAS_RETRIES: u32 = 5;

/// Submits a remote task and polls it until it reaches a terminal state. Mirrors Java
/// `runRemoteTask` + `pollRemoteUntilDone`.
#[allow(clippy::too_many_arguments)]
async fn run_remote_task(
    state: &RepoState,
    transport: &dyn RemoteSubagentTransport,
    ctx: &RuntimeContext,
    session_id: &str,
    task_id: &str,
    base_url: &str,
    headers: &BTreeMap<String, String>,
    agent_id: &str,
    input: &str,
    context: Option<RemoteSubmitContext>,
) -> Result<String, String> {
    let target = RemoteTarget { base_url: base_url.to_string(), headers: headers.clone() };

    if task_cancelled(state, ctx, session_id, task_id).await {
        return Err(CANCELLED_SENTINEL.to_string());
    }

    let owned = context.unwrap_or_default();
    if let Err(e) = transport.submit(&target, task_id, agent_id, input, Some(&owned)).await {
        return Err(e.to_string());
    }
    update_record_status(
        state.fs.as_ref(),
        ctx,
        &state.parent_agent_id,
        session_id,
        task_id,
        TaskStatus::Running,
        None,
        None,
    )
    .await;

    poll_remote_until_done(state, transport, ctx, session_id, task_id, &target).await
}

/// Polls the remote task status with exponential backoff (200ms→5s), switching to a fixed
/// [`AWAITING_CONFIRM_POLL_MS`] interval while the remote agent is blocked on a user confirmation.
/// Returns the task result, or [`CANCELLED_SENTINEL`] when it was cancelled.
async fn poll_remote_until_done(
    state: &RepoState,
    transport: &dyn RemoteSubagentTransport,
    ctx: &RuntimeContext,
    session_id: &str,
    task_id: &str,
    target: &RemoteTarget,
) -> Result<String, String> {
    let mut attempt: u32 = 0;
    let mut was_awaiting_confirm = false;
    loop {
        if task_cancelled(state, ctx, session_id, task_id).await {
            if let Err(e) = transport.cancel(target, task_id).await {
                tracing::debug!("Remote cancel after local cancel flag failed: {e}");
            }
            return Err(CANCELLED_SENTINEL.to_string());
        }

        let status = transport.get_status(target, task_id).await.map_err(|e| e.to_string())?;

        if status.is_awaiting_confirm() {
            was_awaiting_confirm = true;
            update_awaiting_confirm(
                state,
                ctx,
                session_id,
                task_id,
                true,
                Some(status.pending_confirms.clone()),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(AWAITING_CONFIRM_POLL_MS)).await;
            continue;
        }
        if was_awaiting_confirm {
            update_awaiting_confirm(state, ctx, session_id, task_id, false, None).await;
            was_awaiting_confirm = false;
        }

        if status.is_terminal_success() {
            let result = transport
                .wait_for_result(target, task_id, REMOTE_RESULT_TIMEOUT_SECONDS)
                .await
                .map_err(|e| e.to_string())?;
            update_record_status(
                state.fs.as_ref(),
                ctx,
                &state.parent_agent_id,
                session_id,
                task_id,
                TaskStatus::Completed,
                Some(result.clone()),
                None,
            )
            .await;
            return Ok(result);
        }
        if status.is_terminal_failure() {
            let err = status.error.unwrap_or_else(|| "remote task error".to_string());
            update_record_status(
                state.fs.as_ref(),
                ctx,
                &state.parent_agent_id,
                session_id,
                task_id,
                TaskStatus::Failed,
                None,
                Some(err.clone()),
            )
            .await;
            return Err(err);
        }
        if status.is_cancelled() {
            update_record_status(
                state.fs.as_ref(),
                ctx,
                &state.parent_agent_id,
                session_id,
                task_id,
                TaskStatus::Cancelled,
                None,
                None,
            )
            .await;
            return Err(CANCELLED_SENTINEL.to_string());
        }

        // pending / running / empty: keep polling with exponential backoff.
        let sleep_ms = REMOTE_POLL_MAX_MS.min(200u64 << attempt.min(4));
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
    }
}

/// Whether the workspace record has been flagged for cancellation by `cancel_task`.
async fn task_cancelled(
    state: &RepoState,
    ctx: &RuntimeContext,
    session_id: &str,
    task_id: &str,
) -> bool {
    let path = tasks_path(&state.parent_agent_id, session_id);
    read_records(state.fs.as_ref(), ctx, &path)
        .await
        .get(task_id)
        .map(|r| r.cancel_requested)
        .unwrap_or(false)
}

/// Persists `awaiting_confirm` / `pending_confirms` without touching the task status — the task
/// stays `RUNNING` while paused for user confirmation. No-op once the record is terminal, and
/// no-op when nothing changed (avoids rewriting the workspace JSON on every poll).
async fn update_awaiting_confirm(
    state: &RepoState,
    ctx: &RuntimeContext,
    session_id: &str,
    task_id: &str,
    awaiting: bool,
    pending_confirms: Option<Vec<RemotePendingConfirm>>,
) {
    let path = tasks_path(&state.parent_agent_id, session_id);
    let mut map = read_records(state.fs.as_ref(), ctx, &path).await;
    let next_pending = if awaiting { pending_confirms } else { None };
    match map.get_mut(task_id) {
        Some(rec) if !rec.status.is_terminal() => {
            if rec.awaiting_confirm == awaiting
                && pending_confirms_unchanged(&rec.pending_confirms, &next_pending)
            {
                return;
            }
            rec.awaiting_confirm = awaiting;
            rec.pending_confirms = next_pending;
            rec.touch();
            rec.version = rec.version.wrapping_add(1);
        }
        _ => return,
    }
    write_records(state.fs.as_ref(), ctx, &path, &map).await;
}

fn pending_confirms_unchanged(
    current: &Option<Vec<RemotePendingConfirm>>,
    next: &Option<Vec<RemotePendingConfirm>>,
) -> bool {
    match (current, next) {
        (None, None) => true,
        (Some(a), Some(b)) => a.is_empty() == b.is_empty() && (a.is_empty() || a == b),
        _ => false,
    }
}

// ---------------------------------------------------------------------
//  Heartbeat / orphan sweeping
// ---------------------------------------------------------------------

/// Refreshes `last_updated_at` for every live local task that is still running.
async fn heartbeat_once(state: &RepoState) {
    let live: Vec<(String, RuntimeContext, String)> = {
        let tasks = state.local_tasks.lock().unwrap();
        let contexts = state.local_contexts.lock().unwrap();
        tasks
            .iter()
            .filter(|(_, t)| !t.is_completed())
            .filter_map(|(key, t)| {
                let (sid, rc) = contexts.get(key)?;
                Some((sid.clone(), rc.clone(), t.task_id().to_string()))
            })
            .collect()
    };
    for (sid, rc, tid) in live {
        update_record_status(
            state.fs.as_ref(),
            &rc,
            &state.parent_agent_id,
            &sid,
            &tid,
            TaskStatus::Running,
            None,
            None,
        )
        .await;
    }
}

/// Marks non-terminal, non-remote tasks whose `last_updated_at` predates `orphan_timeout` as
/// `FAILED`. Remote (`agent-protocol`) tasks are skipped: their liveness is governed by the
/// remote endpoint rather than by this node's heartbeat. Safe to run concurrently on several
/// nodes — the write is idempotent and last-writer-wins is acceptable for an already-orphaned task.
async fn sweep_once(state: &RepoState, orphan_timeout: Duration, sweep_time_ms: u64) {
    let ctx = RuntimeContext::default();
    let cutoff_ms = sweep_time_ms.saturating_sub(orphan_timeout.as_millis().min(u64::MAX as u128) as u64);
    let records = read_all_records(state, &ctx).await;
    for (task_id, rec) in records {
        if rec.status.is_terminal() || rec.is_agent_protocol_transport() {
            continue;
        }
        // A task is stale as soon as it reaches the timeout boundary.
        if rec.last_updated_at > cutoff_ms {
            continue;
        }
        let sid = rec.parent_session_id.clone();
        // Skip when this node still has an active local future — the heartbeat is live.
        let key = format!("{sid}:{task_id}");
        let live = {
            let tasks = state.local_tasks.lock().unwrap();
            tasks.get(&key).map(|t| !t.is_completed()).unwrap_or(false)
        };
        if live {
            continue;
        }
        let update_ctx = {
            let contexts = state.local_contexts.lock().unwrap();
            contexts.get(&key).map(|(_, rc)| rc.clone()).unwrap_or_else(RuntimeContext::default)
        };
        let stale_secs = sweep_time_ms.saturating_sub(rec.last_updated_at) / 1000;
        tracing::warn!(
            "Orphaned task {task_id} (session {sid}) last updated {stale_secs}s ago, marking FAILED"
        );
        update_record_status(
            state.fs.as_ref(),
            &update_ctx,
            &state.parent_agent_id,
            &sid,
            &task_id,
            TaskStatus::Failed,
            None,
            Some(format!(
                "executor lost: no heartbeat for {stale_secs} seconds (last updated: {})",
                rec.last_updated_at
            )),
        )
        .await;
    }
}

/// Reads every task record for this agent across all session files.
///
/// The Rust repository keeps one file per session with no cross-session index, so the sweeper
/// resolves the set via a glob over `agents/<parentAgentId>/tasks/*.json`.
async fn read_all_records(
    state: &RepoState,
    ctx: &RuntimeContext,
) -> BTreeMap<String, TaskRecord> {
    let dir = format!("agents/{}/tasks", state.parent_agent_id);
    let Ok(files) = state.fs.glob(ctx, "*.json", &dir).await else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for fi in files {
        if fi.is_directory {
            continue;
        }
        for (task_id, rec) in read_records(state.fs.as_ref(), ctx, &fi.path).await {
            out.insert(task_id, rec);
        }
    }
    out
}


/// Fires the completion callback, honouring the [`SUPPRESS_COMPLETION_CALLBACK`] runtime flag set
/// by callers that retain execution ownership and collect results themselves.
fn fire_callback(
    ctx: &RuntimeContext,
    cb: &Option<TaskCompletionCallback>,
    task_id: String,
    session_id: String,
    result: Option<String>,
) {
    if ctx.metadata.get(SUPPRESS_COMPLETION_CALLBACK).and_then(|v| v.as_bool()) == Some(true) {
        return;
    }
    if let Some(cb) = cb {
        cb(task_id, session_id, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::subagent::protocol::RemoteConfirmDecision;
    use crate::filesystem::LocalFilesystem;
    use super::super::protocol_client::RemoteTaskStatus;

    fn temp_fs(tag: &str) -> (std::path::PathBuf, Arc<dyn Filesystem>) {
        let dir = std::env::temp_dir().join(format!("agentscope_ws_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        (dir, fs)
    }

    async fn seed(
        fs: &Arc<dyn Filesystem>,
        ctx: &RuntimeContext,
        status: TaskStatus,
        cancel_requested: bool,
    ) -> String {
        let path = tasks_path("parent", "sess");
        let mut rec = TaskRecord::new("t1", "sub1", "parent", "sess");
        rec.status = status;
        rec.cancel_requested = cancel_requested;
        let mut map = BTreeMap::new();
        map.insert("t1".to_string(), rec);
        write_records(fs.as_ref(), ctx, &path, &map).await;
        path
    }

    #[tokio::test]
    async fn update_status_keeps_terminal_states_immutable() {
        let (dir, fs) = temp_fs("terminal");
        let ctx = RuntimeContext::default();
        let path = seed(&fs, &ctx, TaskStatus::Cancelled, true).await;

        update_record_status(
            fs.as_ref(), &ctx, "parent", "sess", "t1", TaskStatus::Failed, None,
            Some("late failure".to_string()),
        )
        .await;

        let after = read_records(fs.as_ref(), &ctx, &path).await;
        let rec = after.get("t1").expect("record kept");
        assert_eq!(rec.status, TaskStatus::Cancelled);
        assert!(rec.error_message.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn update_status_refuses_non_terminal_write_after_cancel_requested() {
        let (dir, fs) = temp_fs("cancel_race");
        let ctx = RuntimeContext::default();
        let path = seed(&fs, &ctx, TaskStatus::Pending, true).await;

        update_record_status(
            fs.as_ref(), &ctx, "parent", "sess", "t1", TaskStatus::Running, None, None,
        )
        .await;

        let after = read_records(fs.as_ref(), &ctx, &path).await;
        assert_eq!(after["t1"].status, TaskStatus::Pending);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mark_delivered_is_idempotent() {
        let (dir, fs) = temp_fs("delivered");
        let ctx = RuntimeContext::default();
        let path = seed(&fs, &ctx, TaskStatus::Completed, false).await;
        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent");

        repo.mark_delivered(&ctx, "sess", "t1").await;
        let first = read_records(fs.as_ref(), &ctx, &path).await["t1"].delivered_at;
        assert!(first.is_some());

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        repo.mark_delivered(&ctx, "sess", "t1").await;
        let second = read_records(fs.as_ref(), &ctx, &path).await["t1"].delivered_at;
        assert_eq!(first, second, "first stamp wins");

        // Unknown task ids are a no-op and must not create a record.
        repo.mark_delivered(&ctx, "sess", "nope").await;
        assert_eq!(read_records(fs.as_ref(), &ctx, &path).await.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- remote execution -------------------------------------------------

    struct FakeTransport {
        statuses: std::sync::Mutex<Vec<RemoteTaskStatus>>,
        submitted: std::sync::Mutex<Vec<String>>,
        cancelled: std::sync::Mutex<Vec<String>>,
        status_checks: std::sync::atomic::AtomicUsize,
    }

    impl FakeTransport {
        fn new(statuses: Vec<RemoteTaskStatus>) -> Arc<Self> {
            Arc::new(Self {
                statuses: std::sync::Mutex::new(statuses),
                submitted: std::sync::Mutex::new(Vec::new()),
                cancelled: std::sync::Mutex::new(Vec::new()),
                status_checks: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl RemoteSubagentTransport for FakeTransport {
        fn transport_type(&self) -> &str {
            "fake"
        }
        async fn submit(
            &self,
            _target: &RemoteTarget,
            task_id: &str,
            _agent_id: &str,
            _input: &str,
            _context: Option<&RemoteSubmitContext>,
        ) -> agentscope_core::error::Result<()> {
            self.submitted.lock().unwrap().push(task_id.to_string());
            Ok(())
        }
        async fn get_status(
            &self,
            _target: &RemoteTarget,
            _task_id: &str,
        ) -> agentscope_core::error::Result<RemoteTaskStatus> {
            self.status_checks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut st = self.statuses.lock().unwrap();
            // Keep serving the last status once the script is exhausted.
            let s = if st.len() > 1 { st.remove(0) } else { st[0].clone() };
            Ok(s)
        }
        async fn wait_for_result(
            &self,
            _target: &RemoteTarget,
            _task_id: &str,
            _timeout_seconds: u64,
        ) -> agentscope_core::error::Result<String> {
            Ok("remote-done".to_string())
        }
        async fn cancel(
            &self,
            _target: &RemoteTarget,
            task_id: &str,
        ) -> agentscope_core::error::Result<()> {
            self.cancelled.lock().unwrap().push(task_id.to_string());
            Ok(())
        }
        async fn resume(
            &self,
            _target: &RemoteTarget,
            _task_id: &str,
            _decisions: &[RemoteConfirmDecision],
        ) -> agentscope_core::error::Result<()> {
            Ok(())
        }
    }

    fn remote_spec() -> TaskRunSpec {
        TaskRunSpec::remote("http://localhost:1", BTreeMap::new(), "coder", "hello")
    }

    /// Async wait helper: `BackgroundTask::wait_for_completion` blocks the calling thread on a
    /// condvar, which would starve the single-threaded test runtime.
    async fn wait_done(bg: &Arc<BackgroundTask>, timeout_ms: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if bg.is_completed() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        bg.is_completed()
    }

    /// Waits until `transport` has served at least `count` status checks, so tests can
    /// synchronise on the poller's progress instead of guessing a sleep duration.
    async fn wait_for_status_checks(
        transport: &Arc<FakeTransport>,
        count: usize,
        timeout_ms: u64,
    ) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if transport
                .status_checks
                .load(std::sync::atomic::Ordering::SeqCst)
                >= count
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    #[tokio::test]
    async fn remote_task_success_completes_and_fires_callback() {
        let (dir, fs) = temp_fs("remote_ok");
        let ctx = RuntimeContext::default();
        // Exercises the backoff path once (`running`) before reporting terminal success.
        let transport = FakeTransport::new(vec![
            RemoteTaskStatus::new("running", None),
            RemoteTaskStatus::new("success", None),
        ]);
        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent")
            .with_transport(transport.clone());

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        repo.set_completion_callback(Arc::new(move |tid, sid, result| {
            seen2.lock().unwrap().push((tid, sid, result));
        }))
        .await;

        let bg = repo.put_task(&ctx, "t-remote", "sub1", "sess", remote_spec()).await;
        assert!(wait_done(&bg, 5_000).await);

        let rec = read_records(fs.as_ref(), &ctx, &tasks_path("parent", "sess")).await;
        let rec = rec.get("t-remote").expect("record persisted");
        assert_eq!(rec.status, TaskStatus::Completed);
        assert_eq!(rec.result.as_deref(), Some("remote-done"));
        assert_eq!(rec.transport_type.as_deref(), Some("agent-protocol"));
        assert_eq!(transport.submitted.lock().unwrap().as_slice(), ["t-remote"]);
        assert_eq!(seen.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn awaiting_confirm_is_persisted_then_cleared() {
        let (dir, fs) = temp_fs("remote_confirm");
        let ctx = RuntimeContext::default();
        let mut awaiting = RemoteTaskStatus::new("awaiting_confirm", None);
        awaiting.pending_confirms = vec![RemotePendingConfirm::new("c1", "read_file", None)];
        let transport =
            FakeTransport::new(vec![awaiting, RemoteTaskStatus::new("success", None)]);
        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent")
            .with_transport(transport.clone());

        let bg = repo.put_task(&ctx, "t-ac", "sub1", "sess", remote_spec()).await;
        assert!(wait_done(&bg, 10_000).await);

        let rec = read_records(fs.as_ref(), &ctx, &tasks_path("parent", "sess")).await;
        let rec = rec.get("t-ac").expect("record persisted");
        // Cleared again once the remote left the awaiting state, and terminal.
        assert!(!rec.awaiting_confirm);
        assert!(rec.pending_confirms.is_none());
        assert_eq!(rec.status, TaskStatus::Completed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn record_version_bumps_and_terminal_update_is_refused() {
        let (dir, fs) = temp_fs("cas_version");
        let ctx = RuntimeContext::default();
        let path = tasks_path("parent", "sess");
        let mut map = BTreeMap::new();
        map.insert("t1".to_string(), TaskRecord::new("t1", "sub1", "parent", "sess"));
        write_records(fs.as_ref(), &ctx, &path, &map).await;

        // Pending -> Running bumps the version once.
        update_record_status(
            fs.as_ref(), &ctx, "parent", "sess", "t1", TaskStatus::Running, None, None,
        )
        .await;
        let map = read_records(fs.as_ref(), &ctx, &path).await;
        assert_eq!(map.get("t1").unwrap().version, 1);

        // Running -> Completed bumps again.
        update_record_status(
            fs.as_ref(), &ctx, "parent", "sess", "t1", TaskStatus::Completed,
            Some("ok".into()), None,
        )
        .await;
        let map = read_records(fs.as_ref(), &ctx, &path).await;
        let rec = map.get("t1").unwrap();
        assert_eq!(rec.version, 2);
        assert_eq!(rec.status, TaskStatus::Completed);

        // Terminal record refuses further status writes; version stays put.
        update_record_status(
            fs.as_ref(), &ctx, "parent", "sess", "t1", TaskStatus::Failed,
            None, Some("late".into()),
        )
        .await;
        let map = read_records(fs.as_ref(), &ctx, &path).await;
        let rec = map.get("t1").unwrap();
        assert_eq!(rec.version, 2);
        assert_eq!(rec.status, TaskStatus::Completed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn remote_failure_marks_task_failed() {
        let (dir, fs) = temp_fs("remote_fail");
        let ctx = RuntimeContext::default();
        let transport = FakeTransport::new(vec![RemoteTaskStatus::new(
            "error",
            Some("boom".to_string()),
        )]);
        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent")
            .with_transport(transport.clone());

        let bg = repo.put_task(&ctx, "t-bad", "sub1", "sess", remote_spec()).await;
        assert!(wait_done(&bg, 5_000).await);

        let rec = read_records(fs.as_ref(), &ctx, &tasks_path("parent", "sess")).await;
        let rec = rec.get("t-bad").expect("record persisted");
        assert_eq!(rec.status, TaskStatus::Failed);
        assert_eq!(rec.error_message.as_deref(), Some("boom"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn suppress_completion_callback_flag_skips_the_callback() {
        let (dir, fs) = temp_fs("suppress");
        let mut ctx = RuntimeContext::default();
        ctx.metadata.insert(SUPPRESS_COMPLETION_CALLBACK.into(), serde_json::json!(true));
        let transport = FakeTransport::new(vec![RemoteTaskStatus::new("success", None)]);
        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent")
            .with_transport(transport.clone());

        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        repo.set_completion_callback(Arc::new(move |_, _, _| {
            hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }))
        .await;

        let bg = repo.put_task(&ctx, "t-sup", "sub1", "sess", remote_spec()).await;
        assert!(wait_done(&bg, 5_000).await);

        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        let rec = read_records(fs.as_ref(), &ctx, &tasks_path("parent", "sess")).await;
        assert_eq!(rec["t-sup"].status, TaskStatus::Completed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- heartbeat / orphan sweeping --------------------------------------

    #[tokio::test]
    async fn heartbeat_refreshes_last_updated_at_for_live_local_tasks() {
        let (dir, fs) = temp_fs("heartbeat");
        let ctx = RuntimeContext::default();
        let path = tasks_path("parent", "sess");
        let mut rec = TaskRecord::new("t1", "sub1", "parent", "sess");
        rec.status = TaskStatus::Running;
        rec.last_updated_at = 1; // very stale
        let mut map = BTreeMap::new();
        map.insert("t1".to_string(), rec);
        write_records(fs.as_ref(), &ctx, &path, &map).await;

        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent");
        // No local handle → nothing is live, so the heartbeat must not touch the record.
        repo.heartbeat().await;
        assert_eq!(read_records(fs.as_ref(), &ctx, &path).await["t1"].last_updated_at, 1);

        // Register a live local handle + captured context, then heartbeat again.
        let bg = BackgroundTask::controlled("t1", "sub1");
        repo.state.local_tasks.lock().unwrap().insert("sess:t1".into(), bg);
        repo.state
            .local_contexts
            .lock()
            .unwrap()
            .insert("sess:t1".into(), ("sess".into(), ctx.clone()));
        repo.heartbeat().await;
        assert!(read_records(fs.as_ref(), &ctx, &path).await["t1"].last_updated_at > 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sweep_marks_stale_local_tasks_failed_and_spares_remote_and_live_ones() {
        let (dir, fs) = temp_fs("sweep");
        let ctx = RuntimeContext::default();
        let path = tasks_path("parent", "sess");
        let mut map = BTreeMap::new();
        // orphan: non-terminal, not heartbeat for a long time, no local handle
        let mut orphan = TaskRecord::new("orphan", "sub1", "parent", "sess");
        orphan.status = TaskStatus::Running;
        orphan.last_updated_at = 1_000;
        map.insert("orphan".to_string(), orphan);
        // remote: liveness is the remote endpoint's business
        let mut remote = TaskRecord::new("remote", "sub1", "parent", "sess");
        remote.status = TaskStatus::Running;
        remote.transport_type = Some("agent-protocol".into());
        remote.last_updated_at = 1_000;
        map.insert("remote".to_string(), remote);
        // fresh: updated recently, inside the timeout window
        let mut fresh = TaskRecord::new("fresh", "sub1", "parent", "sess");
        fresh.status = TaskStatus::Running;
        fresh.last_updated_at = 600_000;
        map.insert("fresh".to_string(), fresh);
        // live: stale on disk but this node still holds a running handle
        let mut live = TaskRecord::new("live", "sub1", "parent", "sess");
        live.status = TaskStatus::Running;
        live.last_updated_at = 1_000;
        map.insert("live".to_string(), live);
        write_records(fs.as_ref(), &ctx, &path, &map).await;

        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent");
        repo.state
            .local_tasks
            .lock()
            .unwrap()
            .insert("sess:live".into(), BackgroundTask::controlled("live", "sub1"));

        // Sweep at t=600s with a 300s orphan timeout window.
        repo.sweep_orphaned_tasks_with(Duration::from_secs(300), 600_000).await;

        let after = read_records(fs.as_ref(), &ctx, &path).await;
        assert_eq!(after["orphan"].status, TaskStatus::Failed);
        assert!(after["orphan"]
            .error_message
            .as_deref()
            .unwrap_or_default()
            .starts_with("executor lost"));
        assert_eq!(after["remote"].status, TaskStatus::Running);
        assert_eq!(after["fresh"].status, TaskStatus::Running);
        assert_eq!(after["live"].status, TaskStatus::Running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sweep_is_idempotent_and_skips_terminal_tasks() {
        let (dir, fs) = temp_fs("sweep_idem");
        let ctx = RuntimeContext::default();
        let path = tasks_path("parent", "sess");
        let mut rec = TaskRecord::new("done", "sub1", "parent", "sess");
        rec.status = TaskStatus::Completed;
        rec.last_updated_at = 1;
        let mut map = BTreeMap::new();
        map.insert("done".to_string(), rec);
        write_records(fs.as_ref(), &ctx, &path, &map).await;

        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent");
        repo.sweep_orphaned_tasks_with(Duration::from_secs(1), 600_000).await;
        repo.sweep_orphaned_tasks_with(Duration::from_secs(1), 600_000).await;

        let after = read_records(fs.as_ref(), &ctx, &path).await;
        assert_eq!(after["done"].status, TaskStatus::Completed);
        assert!(after["done"].error_message.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sweep_gate_throttles_repeated_sweeps() {
        let (dir, fs) = temp_fs("sweep_gate");
        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent");
        // The first claim wins; the second falls inside the 5-minute window.
        repo.sweep_orphaned_tasks().await;
        let key = "task-sweep:parent";
        assert!(!repo
            .sweep_gate
            .try_claim(key, Duration::from_secs(SWEEP_INTERVAL_MINUTES * 60)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancel_task_cancels_remote_tasks_over_the_transport() {
        let (dir, fs) = temp_fs("remote_cancel");
        let ctx = RuntimeContext::default();
        // Always `running` so the poller keeps looping until cancelled.
        let transport = FakeTransport::new(vec![RemoteTaskStatus::new("running", None)]);
        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent")
            .with_transport(transport.clone());

        let bg = repo.put_task(&ctx, "t-cancel", "sub1", "sess", remote_spec()).await;
        // Wait deterministically until the poller has actually observed the remote status
        // (a fixed sleep raced the submit → first-poll handshake and made this test flaky).
        assert!(
            wait_for_status_checks(&transport, 1, 10_000).await,
            "poller never checked the remote status"
        );
        assert!(repo.cancel_task(&ctx, "sess", "t-cancel").await);
        assert!(wait_done(&bg, 10_000).await);

        let rec = read_records(fs.as_ref(), &ctx, &tasks_path("parent", "sess")).await;
        let rec = &rec["t-cancel"];
        assert_eq!(rec.status, TaskStatus::Cancelled);
        assert!(rec.cancel_requested);
        assert_eq!(transport.cancelled.lock().unwrap().as_slice(), ["t-cancel"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
