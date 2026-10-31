//! Workspace-backed `TaskRepository`, mirroring `WorkspaceTaskRepository`.
//!
//! Storage layout: `agents/<parentAgentId>/tasks/<sessionId>.json` — a JSON map of
//! `taskId → TaskRecord`, using the harness `Filesystem` trait as the authoritative truth
//! source, while maintaining in-memory `BackgroundTask` handles as a local overlay for tasks
//! running on the current node.
//!
//! A background heartbeat/sweeper pair (started lazily on the first `put_task`) keeps
//! `last_updated_at` fresh for live local tasks and marks dropped-node tasks as `FAILED`.
//!
//! Isolation domains that write task records are registered durably under
//! `harness/task-scopes/<parentAgentId>/<scopeKey>.json` (mirroring Java
//! `RemoteFilesystem#registerTaskScope`), so the sweeper can revisit every domain — and write its
//! repair back into that domain's own namespace — even after a restart.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::coordination::{LocalPeriodicGate, PeriodicGate};
use crate::agent::isolation_scope::IsolationScope;
use crate::agent::subagent::protocol::RemotePendingConfirm;
use crate::filesystem::{
    Filesystem, FilesystemKind, NamespaceFactory, OverlayFilesystem, SandboxBackedFilesystem,
};

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
    ///
    /// Only a fast path for local heartbeats: the durable registry (`register_task_scope`) is the
    /// authority the sweeper uses, because this map is empty after a process restart.
    local_contexts: std::sync::Mutex<BTreeMap<String, (String, RuntimeContext)>>,
    /// Namespace factory used to derive a context's isolation domain. `None` means
    /// [`IsolationScope::User`], the Java default (`IsolationScope.USER.toNamespaceFactory()`).
    namespace_factory: Option<Arc<dyn NamespaceFactory>>,
    /// Scope keys already written to the registry by *this* process, so repeated task reads do not
    /// re-hit the store. Purely an optimization: an empty set after a restart only costs one
    /// registry read per domain, never correctness.
    registered_scopes: std::sync::Mutex<BTreeSet<String>>,
}

/// Workspace-backed `TaskRepository` that uses the `Filesystem` trait as the authoritative
/// truth source for task state, with in-memory `BackgroundTask` handles as a local overlay.
pub struct WorkspaceTaskRepository {
    state: Arc<RepoState>,
    completion_callback: std::sync::RwLock<Option<TaskCompletionCallback>>,
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
                namespace_factory: None,
                registered_scopes: std::sync::Mutex::new(BTreeSet::new()),
            }),
            completion_callback: std::sync::RwLock::new(None),
            transport: Arc::new(AgentProtocolTransport::new()),
            sweep_gate,
            maintenance: std::sync::Mutex::new(None),
        }
    }

    /// Overrides how a [`RuntimeContext`] is mapped to an isolation namespace.
    ///
    /// Mirrors handing the Java `RemoteFilesystem` a `NamespaceFactory`; without this the
    /// [`IsolationScope::User`] default is used. Task-scope registration and the orphan sweeper
    /// both go through the same factory, so registration and lookup can never disagree.
    pub fn with_namespace_factory(mut self, factory: Arc<dyn NamespaceFactory>) -> Self {
        match Arc::get_mut(&mut self.state) {
            Some(state) => state.namespace_factory = Some(factory),
            // Only reachable if a clone escaped before the builder finished; the default factory
            // is a correct fallback, so degrade loudly instead of panicking in a builder method.
            None => tracing::warn!(
                "with_namespace_factory called on a shared repository; keeping the default \
                 IsolationScope::User namespace factory"
            ),
        }
        self
    }

    /// Injects a transport, bypassing real HTTP calls (remote task tests).
    pub fn with_transport(mut self, transport: Arc<dyn RemoteSubagentTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Registers a callback invoked when any task reaches a terminal state (COMPLETED or
    /// FAILED). Only one callback is supported; a second call replaces the previous one.
    pub fn set_completion_callback(&self, callback: TaskCompletionCallback) {
        *self
            .completion_callback
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(callback);
    }

    fn tasks_path(&self, session_id: &str) -> String {
        tasks_path(&self.state.parent_agent_id, session_id)
    }

    async fn persist_record(&self, ctx: &RuntimeContext, session_id: &str, record: &TaskRecord) {
        // Mirror Java `WorkspaceManager#writeTaskRecord`: register the caller's isolation domain
        // before writing, so a sweep in any later process can find and repair this record.
        register_task_scope(&self.state, ctx).await;
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
            tracing::debug!(
                "Skipping orphan sweep for {} — periodic gate denied claim",
                key
            );
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
            tokio::time::sleep(Duration::from_millis(
                now_millis() % interval.as_millis() as u64,
            ))
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
        BackgroundTask::from_record(
            rec.task_id.clone(),
            rec.sub_agent_id.clone(),
            rec.created_at,
            rec.status,
            rec.result.clone(),
            rec.error_message.clone(),
        )
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
        // Java registers the scope from reads too (`WorkspaceManager#readTaskRecord`), which keeps
        // recovery possible for a domain that only ever read an existing record.
        register_task_scope(&self.state, ctx).await;
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
        let mut record = TaskRecord::new(
            task_id,
            sub_agent_id,
            &self.state.parent_agent_id,
            session_id,
        );
        record.status = TaskStatus::Pending;
        if let TaskRunSpec::Remote {
            base_url, headers, ..
        } = &spec
        {
            record.transport_type = Some(TRANSPORT_AGENT_PROTOCOL.to_string());
            record.remote_base_url = Some(base_url.clone());
            if !headers.is_empty() {
                record.remote_headers = Some(headers.clone());
            }
        }
        self.persist_record(ctx, session_id, &record).await;

        let local_key = format!("{session_id}:{task_id}");
        let bg = BackgroundTask::controlled(task_id, sub_agent_id);

        // `cancel()` aborts the wrapper task. Dropping a `JoinHandle` *detaches* its task, so a
        // cancel that lands before the wrapper reaches the `Adopted` arm would leave the adopted
        // run executing (dropping the spec drops the handle without aborting it). Capture the
        // abort handle here and abort from the wrapper's own drop, which runs on abort too.
        let adopted_abort = match &spec {
            TaskRunSpec::Adopted { future } => Some(future.abort_handle()),
            _ => None,
        };

        let state = Arc::clone(&self.state);
        let transport = Arc::clone(&self.transport);
        let task_ctx = ctx.clone();
        let task_id = task_id.to_string();
        let session_id = session_id.to_string();
        let session_for_task = session_id.clone();
        let bg2 = bg.clone();
        let cb = self
            .completion_callback
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        let handle = tokio::task::spawn(async move {
            /// Aborts the adopted run whenever the wrapper goes away — completion **or** an
            /// external `abort()` while the wrapper is still waiting on its own bookkeeping.
            struct AbortAdoptedOnDrop(Option<tokio::task::AbortHandle>);
            impl Drop for AbortAdoptedOnDrop {
                fn drop(&mut self) {
                    if let Some(handle) = &self.0 {
                        handle.abort();
                    }
                }
            }
            let _adopted = AbortAdoptedOnDrop(adopted_abort);
            let session_id = session_for_task;
            update_record_status(
                state.fs.as_ref(),
                &task_ctx,
                &state.parent_agent_id,
                &session_id,
                &task_id,
                TaskStatus::Running,
                None,
                None,
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
                TaskRunSpec::Remote {
                    base_url,
                    headers,
                    agent_id,
                    input,
                    context,
                } => {
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
                TaskRunSpec::Adopted { future } => {
                    // `cancel_task` aborts *this* wrapper task. Propagate that abort to the
                    // adopted run, otherwise a cancelled subagent turn keeps executing
                    // detached (and keeps burning tokens after the user cancelled it).
                    struct AbortOnDrop(tokio::task::AbortHandle);
                    impl Drop for AbortOnDrop {
                        fn drop(&mut self) {
                            self.0.abort();
                        }
                    }
                    let _abort_on_drop = AbortOnDrop(future.abort_handle());
                    match future.await {
                        Ok(Ok(r)) => Ok(r),
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(je) => Err(format!("task join error: {je}")),
                    }
                }
            };

            let terminal = match &outcome {
                Ok(r) => {
                    update_record_status(
                        state.fs.as_ref(),
                        &task_ctx,
                        &state.parent_agent_id,
                        &session_id,
                        &task_id,
                        TaskStatus::Completed,
                        Some(r.clone()),
                        None,
                    )
                    .await;
                    Some(Some(r.clone()))
                }
                // A cancelled task is already terminal in the workspace; there is nothing to
                // report back to the parent.
                Err(e) if e.as_str() == CANCELLED_SENTINEL => None,
                Err(e) => {
                    update_record_status(
                        state.fs.as_ref(),
                        &task_ctx,
                        &state.parent_agent_id,
                        &session_id,
                        &task_id,
                        TaskStatus::Failed,
                        None,
                        Some(e.clone()),
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

        self.state
            .local_tasks
            .lock()
            .unwrap()
            .insert(local_key.clone(), bg.clone());
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
        // Java registers the scope from `listTaskRecords` as well (WorkspaceManager:624).
        register_task_scope(&self.state, ctx).await;
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

    async fn find_pending_deliveries(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
    ) -> Vec<TaskDelivery> {
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

    fn set_completion_callback(&self, callback: TaskCompletionCallback) {
        WorkspaceTaskRepository::set_completion_callback(self, callback);
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

// ---------------------------------------------------------------------
//  Task-scope registry (durable isolation domains)
// ---------------------------------------------------------------------

/// Durable record of one isolation domain that has written task records.
///
/// Mirrors the Java `RemoteFilesystem#registerTaskScope` entry (`namespace` / `userId` /
/// `sessionId`). The registry lives outside `agents/<agentId>/…` so it is never visible to agent
/// filesystem tools, and each domain gets its own file so concurrent registrations cannot
/// overwrite a shared list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct TaskScopeEntry {
    namespace: Vec<String>,
    user_id: String,
    session_id: String,
}

fn task_scopes_dir(parent_agent_id: &str) -> String {
    format!("harness/task-scopes/{parent_agent_id}")
}

fn task_scope_path(parent_agent_id: &str, key: &str) -> String {
    format!("{}/{key}.json", task_scopes_dir(parent_agent_id))
}

/// Synthetic identity used for registry I/O.
///
/// The Java registry lives in its own `["harness","task-scopes",…]` store namespace, which is
/// always non-empty; the `Filesystem` trait cannot pick a namespace directly, and a store-backed
/// backend rejects an empty one. A reserved id therefore stands in: every id-based namespace
/// factory maps it to a stable domain that no real user or session can occupy, identical across
/// processes.
const REGISTRY_SCOPE_ID: &str = "__agentscope_task_scopes__";

fn registry_ctx() -> RuntimeContext {
    RuntimeContext::default()
        .with_user_id(REGISTRY_SCOPE_ID)
        .with_session_id(REGISTRY_SCOPE_ID)
}

/// Longest scope key kept verbatim; longer ones are hashed (filesystem component limit).
const SCOPE_KEY_MAX_LEN: usize = 120;

/// Java `RemoteFilesystem#scopeKey`: base64url (unpadded) of each segment, joined with `.`.
///
/// The result is a stable, filename-safe function of the namespace tuple: two contexts that
/// resolve to the same domain produce the same key, and distinct domains cannot collide (the join
/// is unambiguous because base64url never emits `.`). Over-long keys fall back to a SHA-256 hex
/// digest, which is just as deterministic but stays inside a path-component budget.
fn scope_key(namespace: &[String]) -> String {
    use base64::Engine as _;
    let joined = namespace
        .iter()
        .map(|segment| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(segment.as_bytes()))
        .collect::<Vec<_>>()
        .join(".");
    if !joined.is_empty() && joined.len() <= SCOPE_KEY_MAX_LEN {
        return joined;
    }
    // Empty (all segments empty — never produced by a real factory) or over-long: hash it, so the
    // key is always a usable, unambiguous path component.
    let digest = Sha256::digest(joined.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Whether `fs` applies a namespace at all.
///
/// Mirrors Java `WorkspaceManager#taskRemoteFilesystem`, which unwraps layered backends and only
/// registers a scope once it reaches a store-backed (namespaced) filesystem. A plain local backend
/// ignores the context entirely, so every context is the default domain there and registration
/// would only produce noise.
fn namespace_is_applied(fs: &dyn Filesystem) -> bool {
    match fs.kind() {
        FilesystemKind::Local => false,
        // An overlay writes through to its upper layer, so that layer decides.
        FilesystemKind::Overlay => match fs.as_any().downcast_ref::<OverlayFilesystem>() {
            Some(overlay) => namespace_is_applied(overlay.upper()),
            None => true,
        },
        // A sandbox backend delegates every file operation to its inner filesystem.
        FilesystemKind::Sandbox => match fs.as_any().downcast_ref::<SandboxBackedFilesystem>() {
            Some(sandbox) => namespace_is_applied(sandbox.inner()),
            None => true,
        },
        // Remote and composite backends route to store-backed layers that this trait cannot
        // unwrap (`CompositeFilesystem` exposes no backend accessor); assume namespaced.
        _ => true,
    }
}

/// Resolves the isolation domain of `ctx`.
///
/// Uses the injected [`NamespaceFactory`] when there is one, otherwise the Java default
/// (`IsolationScope.USER.toNamespaceFactory()`): `[userId]`, falling back to `[sessionId]`, and
/// empty for a context with neither (the default domain).
///
/// A non-namespacing backend (see [`namespace_is_applied`]) maps every context to the default
/// domain, so no registry entry is ever written for it.
fn namespace_for(state: &RepoState, ctx: &RuntimeContext) -> Vec<String> {
    if !namespace_is_applied(state.fs.as_ref()) {
        return Vec::new();
    }
    match state.namespace_factory.as_deref() {
        Some(factory) => factory.get_namespace(ctx),
        None => IsolationScope::User.namespace(ctx.user_id(), ctx.session_id()),
    }
}

/// Persists one registry entry, replacing any previous content for the same key.
///
/// Returns whether the entry is now durably stored.
async fn write_scope_entry(fs: &dyn Filesystem, path: &str, json: &str) -> bool {
    let ctx = registry_ctx();
    let tmp = format!("{path}.tmp");
    if fs.exists(&ctx, &tmp).await.unwrap_or(false) {
        let _ = fs.delete(&ctx, &tmp).await;
    }
    match fs.write(&ctx, &tmp, json).await {
        Ok(()) => match fs.move_path(&ctx, &tmp, path).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("Failed to commit task-scope registry entry to {path}: {e}");
                let _ = fs.delete(&ctx, &tmp).await;
                false
            }
        },
        Err(e) => {
            tracing::warn!("Failed to stage task-scope registry entry for {path}: {e}");
            false
        }
    }
}

/// Registers `ctx`'s isolation domain so a later sweep — possibly in another process — can route
/// maintenance writes back to the original namespace. Best-effort: a failure only costs orphan
/// recovery for that domain, so it must never fail the task write itself.
///
/// Mirrors `RemoteFilesystem#registerTaskScope`: the default domain is not registered, because the
/// sweeper always visits it implicitly.
async fn register_task_scope(state: &RepoState, ctx: &RuntimeContext) {
    let namespace = namespace_for(state, ctx);
    if namespace.is_empty() || namespace == namespace_for(state, &RuntimeContext::default()) {
        return;
    }
    let entry = TaskScopeEntry {
        namespace,
        user_id: ctx.user_id().unwrap_or_default().to_string(),
        session_id: ctx.session_id().unwrap_or_default().to_string(),
    };
    let Ok(json) = serde_json::to_string(&entry) else {
        return;
    };
    let path = task_scope_path(&state.parent_agent_id, &scope_key(&entry.namespace));
    // Registration runs on every task read *and* write (as in Java), so skip it when this process
    // already wrote this exact entry — otherwise every read would re-hit the store. The lock is
    // released before any await below (never hold it across a suspension point).
    let already_registered = state.registered_scopes.lock().unwrap().contains(&path);
    if already_registered {
        return;
    }
    let registry = registry_ctx();
    let unchanged = state
        .fs
        .read(&registry, &path, 0, 0)
        .await
        .is_ok_and(|existing| existing.content == json);
    if unchanged {
        state.registered_scopes.lock().unwrap().insert(path);
        return;
    }
    // Only remember success: a failed write must be retried on the next task write/read.
    if write_scope_entry(state.fs.as_ref(), &path, &json).await {
        state.registered_scopes.lock().unwrap().insert(path);
    }
}

/// Every isolation domain whose task records must be swept, default domain first.
///
/// Mirrors `RemoteFilesystem#taskMaintenanceContexts`: entries whose stored namespace no longer
/// matches what the current namespace factory derives (incompatible configuration) are ignored,
/// and domains are de-duplicated by scope key.
async fn maintenance_contexts(state: &RepoState) -> Vec<RuntimeContext> {
    let default_ctx = RuntimeContext::default();
    // A backend that ignores namespaces keeps every record in the default domain, so there is
    // nothing to enumerate (and any leftover registry entry would be stale configuration).
    if !namespace_is_applied(state.fs.as_ref()) {
        return vec![default_ctx];
    }
    let registry = registry_ctx();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    seen.insert(scope_key(&namespace_for(state, &default_ctx)));
    let mut contexts = vec![default_ctx];

    let dir = task_scopes_dir(&state.parent_agent_id);
    let files = match state.fs.glob(&registry, "*.json", &dir).await {
        Ok(files) => files,
        Err(e) => {
            tracing::debug!("Task-scope registry unavailable at {dir}: {e}");
            return contexts;
        }
    };
    for fi in files {
        if fi.is_directory {
            continue;
        }
        let Ok(data) = state.fs.read(&registry, &fi.path, 0, 0).await else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<TaskScopeEntry>(&data.content) else {
            continue;
        };
        // Java rebuilds the context from the stored ids and re-derives the namespace, dropping
        // entries written under a different factory/configuration.
        let ctx = RuntimeContext::default()
            .with_user_id(entry.user_id.clone())
            .with_session_id(entry.session_id.clone());
        if namespace_for(state, &ctx) != entry.namespace {
            continue;
        }
        if !seen.insert(scope_key(&entry.namespace)) {
            continue;
        }
        contexts.push(ctx);
    }
    contexts
}

/// Picks the context used to write back a record found in `domain`.
///
/// The in-memory capture from `put_task` is preferred when it belongs to the same domain (it may
/// carry extra metadata), but it is never allowed to redirect a write into another namespace.
fn write_back_context(
    state: &RepoState,
    domain: &RuntimeContext,
    local_key: &str,
) -> RuntimeContext {
    let cached = {
        let contexts = state.local_contexts.lock().unwrap();
        contexts.get(local_key).map(|(_, rc)| rc.clone())
    };
    match cached {
        Some(rc) if namespace_for(state, &rc) == namespace_for(state, domain) => rc,
        _ => domain.clone(),
    }
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
    let target = RemoteTarget {
        base_url: base_url.to_string(),
        headers: headers.clone(),
    };

    if task_cancelled(state, ctx, session_id, task_id).await {
        return Err(CANCELLED_SENTINEL.to_string());
    }

    let owned = context.unwrap_or_default();
    if let Err(e) = transport
        .submit(&target, task_id, agent_id, input, Some(&owned))
        .await
    {
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

        let status = transport
            .get_status(target, task_id)
            .await
            .map_err(|e| e.to_string())?;

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
            let err = status
                .error
                .unwrap_or_else(|| "remote task error".to_string());
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
    let timeout_ms = orphan_timeout.as_millis().min(u64::MAX as u128) as u64;
    let cutoff_ms = sweep_time_ms.saturating_sub(timeout_ms);
    // Sweep every registered isolation domain (plus the default one), each with its own context,
    // so a record written under a user's namespace is read — and written back — in that same
    // namespace. A single default-domain pass silently missed every isolated record.
    for domain in maintenance_contexts(state).await {
        let records = read_all_records(state, &domain).await;
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
            // The write-back context must belong to the domain the record was found in; the
            // in-memory capture is only a fast path, never a redirect into another namespace.
            let update_ctx = write_back_context(state, &domain, &key);
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
}

/// Reads every task record for this agent across all session files.
///
/// The Rust repository keeps one file per session with no cross-session index, so the sweeper
/// resolves the set via a glob over `agents/<parentAgentId>/tasks/*.json`.
async fn read_all_records(state: &RepoState, ctx: &RuntimeContext) -> BTreeMap<String, TaskRecord> {
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
    if ctx
        .metadata
        .get(SUPPRESS_COMPLETION_CALLBACK)
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        return;
    }
    if let Some(cb) = cb {
        cb(task_id, session_id, result);
    }
}

#[cfg(test)]
mod tests {
    use super::super::protocol_client::RemoteTaskStatus;
    use super::*;
    use crate::agent::subagent::protocol::RemoteConfirmDecision;
    use crate::filesystem::{FileData, FileInfo, GrepMatch, LocalFilesystem};

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
            fs.as_ref(),
            &ctx,
            "parent",
            "sess",
            "t1",
            TaskStatus::Failed,
            None,
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
            fs.as_ref(),
            &ctx,
            "parent",
            "sess",
            "t1",
            TaskStatus::Running,
            None,
            None,
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
            let s = if st.len() > 1 {
                st.remove(0)
            } else {
                st[0].clone()
            };
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
        let repo =
            WorkspaceTaskRepository::new(fs.clone(), "parent").with_transport(transport.clone());

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        repo.set_completion_callback(Arc::new(move |tid, sid, result| {
            seen2.lock().unwrap().push((tid, sid, result));
        }));

        let bg = repo
            .put_task(&ctx, "t-remote", "sub1", "sess", remote_spec())
            .await;
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
        let transport = FakeTransport::new(vec![awaiting, RemoteTaskStatus::new("success", None)]);
        let repo =
            WorkspaceTaskRepository::new(fs.clone(), "parent").with_transport(transport.clone());

        let bg = repo
            .put_task(&ctx, "t-ac", "sub1", "sess", remote_spec())
            .await;
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
        map.insert(
            "t1".to_string(),
            TaskRecord::new("t1", "sub1", "parent", "sess"),
        );
        write_records(fs.as_ref(), &ctx, &path, &map).await;

        // Pending -> Running bumps the version once.
        update_record_status(
            fs.as_ref(),
            &ctx,
            "parent",
            "sess",
            "t1",
            TaskStatus::Running,
            None,
            None,
        )
        .await;
        let map = read_records(fs.as_ref(), &ctx, &path).await;
        assert_eq!(map.get("t1").unwrap().version, 1);

        // Running -> Completed bumps again.
        update_record_status(
            fs.as_ref(),
            &ctx,
            "parent",
            "sess",
            "t1",
            TaskStatus::Completed,
            Some("ok".into()),
            None,
        )
        .await;
        let map = read_records(fs.as_ref(), &ctx, &path).await;
        let rec = map.get("t1").unwrap();
        assert_eq!(rec.version, 2);
        assert_eq!(rec.status, TaskStatus::Completed);

        // Terminal record refuses further status writes; version stays put.
        update_record_status(
            fs.as_ref(),
            &ctx,
            "parent",
            "sess",
            "t1",
            TaskStatus::Failed,
            None,
            Some("late".into()),
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
        let repo =
            WorkspaceTaskRepository::new(fs.clone(), "parent").with_transport(transport.clone());

        let bg = repo
            .put_task(&ctx, "t-bad", "sub1", "sess", remote_spec())
            .await;
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
        ctx.metadata
            .insert(SUPPRESS_COMPLETION_CALLBACK.into(), serde_json::json!(true));
        let transport = FakeTransport::new(vec![RemoteTaskStatus::new("success", None)]);
        let repo =
            WorkspaceTaskRepository::new(fs.clone(), "parent").with_transport(transport.clone());

        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        repo.set_completion_callback(Arc::new(move |_, _, _| {
            hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        let bg = repo
            .put_task(&ctx, "t-sup", "sub1", "sess", remote_spec())
            .await;
        assert!(wait_done(&bg, 5_000).await);

        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        let rec = read_records(fs.as_ref(), &ctx, &tasks_path("parent", "sess")).await;
        assert_eq!(rec["t-sup"].status, TaskStatus::Completed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_tasks_reports_the_recorded_creation_time_and_cancel_status() {
        // Regression: tasks rebuilt for a listing restarted their clock at "now" and folded a
        // cancelled record into a failure, so `task_list` and `task_output` disagreed.
        let (dir, fs) = temp_fs("list_fidelity");
        let ctx = RuntimeContext::default();
        let path = tasks_path("parent", "sess");
        let mut map = BTreeMap::new();

        let mut cancelled = TaskRecord::new("cancelled", "sub1", "parent", "sess");
        cancelled.status = TaskStatus::Cancelled;
        cancelled.cancel_requested = true;
        cancelled.created_at = 1_600_000_000_000;
        map.insert("cancelled".to_string(), cancelled);

        let mut done = TaskRecord::new("done", "sub1", "parent", "sess");
        done.status = TaskStatus::Completed;
        done.result = Some("ok".into());
        done.created_at = 1_600_000_123_456;
        map.insert("done".to_string(), done);

        write_records(fs.as_ref(), &ctx, &path, &map).await;

        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent");
        let tasks = repo.list_tasks(&ctx, "sess", None).await;
        let find = |id: &str| {
            tasks
                .iter()
                .find(|t| t.task_id() == id)
                .unwrap_or_else(|| panic!("missing {id}"))
        };
        assert_eq!(find("cancelled").created_at(), 1_600_000_000_000);
        assert_eq!(
            find("cancelled").get_task_status(),
            TaskStatus::Cancelled,
            "a cancelled task must not be reported as failed"
        );
        assert_eq!(find("done").created_at(), 1_600_000_123_456);
        assert_eq!(find("done").get_task_status(), TaskStatus::Completed);
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
        assert_eq!(
            read_records(fs.as_ref(), &ctx, &path).await["t1"].last_updated_at,
            1
        );

        // Register a live local handle + captured context, then heartbeat again.
        let bg = BackgroundTask::controlled("t1", "sub1");
        repo.state
            .local_tasks
            .lock()
            .unwrap()
            .insert("sess:t1".into(), bg);
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
        repo.state.local_tasks.lock().unwrap().insert(
            "sess:live".into(),
            BackgroundTask::controlled("live", "sub1"),
        );

        // Sweep at t=600s with a 300s orphan timeout window.
        repo.sweep_orphaned_tasks_with(Duration::from_secs(300), 600_000)
            .await;

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
        repo.sweep_orphaned_tasks_with(Duration::from_secs(1), 600_000)
            .await;
        repo.sweep_orphaned_tasks_with(Duration::from_secs(1), 600_000)
            .await;

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
        let repo =
            WorkspaceTaskRepository::new(fs.clone(), "parent").with_transport(transport.clone());

        let bg = repo
            .put_task(&ctx, "t-cancel", "sub1", "sess", remote_spec())
            .await;
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

    // ---- task-scope registry (durable isolation domains) ------------------

    /// `LocalFilesystem` wrapper that prefixes every path with the isolation namespace derived
    /// from the context, mirroring what `RemoteFilesystem` does with a `NamespaceFactory`.
    /// Without it the local backend ignores the context, so a write-back into the wrong domain
    /// would land in the same file and the test could not tell the difference.
    ///
    /// Paths in and out are namespace-relative, like the store-backed backend's.
    struct NamespacedLocalFs {
        inner: LocalFilesystem,
    }

    impl NamespacedLocalFs {
        fn new(root: impl Into<std::path::PathBuf>) -> Self {
            Self {
                inner: LocalFilesystem::sandboxed(root),
            }
        }

        fn prefix(ctx: &RuntimeContext) -> String {
            let ns = IsolationScope::User.namespace(ctx.user_id(), ctx.session_id());
            if ns.is_empty() {
                String::new()
            } else {
                format!("{}/", ns.join("/"))
            }
        }

        fn scoped(ctx: &RuntimeContext, path: &str) -> String {
            format!("{}{}", Self::prefix(ctx), path.trim_start_matches('/'))
        }

        fn unscoped(ctx: &RuntimeContext, path: &str) -> String {
            let ns = Self::prefix(ctx);
            let normalized = if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            };
            if ns.is_empty() {
                return normalized;
            }
            let prefix = format!("/{}", ns.trim_end_matches('/'));
            normalized
                .strip_prefix(&prefix)
                .map(str::to_string)
                .unwrap_or(normalized)
        }

        fn unscope_all(ctx: &RuntimeContext, mut infos: Vec<FileInfo>) -> Vec<FileInfo> {
            for info in &mut infos {
                info.path = Self::unscoped(ctx, &info.path);
            }
            infos
        }
    }

    #[async_trait]
    impl Filesystem for NamespacedLocalFs {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        /// Reported as `Remote` so the repository treats this backend as namespace-aware, the way
        /// a real `RemoteFilesystem` is. A `Local` kind deliberately maps every context to the
        /// default domain (local disk applies no namespace).
        fn kind(&self) -> FilesystemKind {
            FilesystemKind::Remote
        }

        async fn ls(
            &self,
            ctx: &RuntimeContext,
            path: &str,
        ) -> agentscope_core::error::Result<Vec<FileInfo>> {
            let infos = self.inner.ls(ctx, &Self::scoped(ctx, path)).await?;
            Ok(Self::unscope_all(ctx, infos))
        }

        async fn read(
            &self,
            ctx: &RuntimeContext,
            path: &str,
            offset: usize,
            limit: usize,
        ) -> agentscope_core::error::Result<FileData> {
            self.inner
                .read(ctx, &Self::scoped(ctx, path), offset, limit)
                .await
        }

        async fn write(
            &self,
            ctx: &RuntimeContext,
            path: &str,
            content: &str,
        ) -> agentscope_core::error::Result<()> {
            self.inner
                .write(ctx, &Self::scoped(ctx, path), content)
                .await
        }

        async fn edit(
            &self,
            ctx: &RuntimeContext,
            path: &str,
            old: &str,
            new: &str,
            replace_all: bool,
        ) -> agentscope_core::error::Result<usize> {
            self.inner
                .edit(ctx, &Self::scoped(ctx, path), old, new, replace_all)
                .await
        }

        async fn grep(
            &self,
            ctx: &RuntimeContext,
            pattern: &str,
            path: Option<&str>,
            glob: Option<&str>,
        ) -> agentscope_core::error::Result<Vec<GrepMatch>> {
            let scoped = path.map(|p| Self::scoped(ctx, p));
            self.inner.grep(ctx, pattern, scoped.as_deref(), glob).await
        }

        async fn glob(
            &self,
            ctx: &RuntimeContext,
            pattern: &str,
            path: &str,
        ) -> agentscope_core::error::Result<Vec<FileInfo>> {
            let infos = self
                .inner
                .glob(ctx, pattern, &Self::scoped(ctx, path))
                .await?;
            Ok(Self::unscope_all(ctx, infos))
        }

        async fn delete(
            &self,
            ctx: &RuntimeContext,
            path: &str,
        ) -> agentscope_core::error::Result<()> {
            self.inner.delete(ctx, &Self::scoped(ctx, path)).await
        }

        async fn move_path(
            &self,
            ctx: &RuntimeContext,
            from: &str,
            to: &str,
        ) -> agentscope_core::error::Result<()> {
            self.inner
                .move_path(ctx, &Self::scoped(ctx, from), &Self::scoped(ctx, to))
                .await
        }

        async fn exists(
            &self,
            ctx: &RuntimeContext,
            path: &str,
        ) -> agentscope_core::error::Result<bool> {
            self.inner.exists(ctx, &Self::scoped(ctx, path)).await
        }
    }

    fn namespaced_fs(tag: &str) -> (std::path::PathBuf, Arc<dyn Filesystem>) {
        let dir = std::env::temp_dir().join(format!("agentscope_ws_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn Filesystem> = Arc::new(NamespacedLocalFs::new(&dir));
        (dir, fs)
    }

    fn user_ctx(user: &str, session: &str) -> RuntimeContext {
        RuntimeContext::default()
            .with_user_id(user)
            .with_session_id(session)
    }

    /// Writes one stale `Running` record through the repository, which is what registers the
    /// scope — the same path `put_task` uses.
    async fn persist_stale(repo: &WorkspaceTaskRepository, ctx: &RuntimeContext, task_id: &str) {
        let mut rec = TaskRecord::new(task_id, "sub1", "parent", "sess");
        rec.status = TaskStatus::Running;
        rec.last_updated_at = 1_000;
        repo.persist_record(ctx, "sess", &rec).await;
    }

    async fn registry_entries(fs: &Arc<dyn Filesystem>) -> Vec<FileInfo> {
        fs.glob(&registry_ctx(), "*.json", &task_scopes_dir("parent"))
            .await
            .unwrap()
    }

    #[test]
    fn scope_key_is_stable_filename_safe_and_namespace_distinct() {
        // Java `scopeKey`: base64url without padding, one segment per namespace element.
        assert_eq!(scope_key(&["alice".to_string()]), "YWxpY2U");
        assert_eq!(
            scope_key(&["users".to_string(), "alice".to_string()]),
            "dXNlcnM.YWxpY2U"
        );
        // Deterministic across calls, and distinct namespaces never share a key.
        assert_eq!(
            scope_key(&["alice".to_string()]),
            scope_key(&["alice".to_string()])
        );
        assert_ne!(
            scope_key(&["alice".to_string()]),
            scope_key(&["bob".to_string()])
        );
        // No path separators can leak out of a hostile segment.
        for segment in ["a/b", "a\\b", "..", "x.y", "with space"] {
            let key = scope_key(&[segment.to_string()]);
            assert!(!key.contains('/'), "separator leaked from {segment:?}");
            assert!(!key.contains('\\'), "separator leaked from {segment:?}");
        }
    }

    #[tokio::test]
    async fn scope_registry_skips_the_default_domain() {
        let (dir, fs) = namespaced_fs("scope_default");
        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent");
        persist_stale(&repo, &RuntimeContext::default(), "t1").await;

        assert!(
            registry_entries(&fs).await.is_empty(),
            "default domain must not be registered"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sweep_writes_back_to_the_original_user_domain_after_restart() {
        let (dir, fs) = namespaced_fs("scope_restart");
        let alice = user_ctx("alice", "sess");
        let path = tasks_path("parent", "sess");

        let first = WorkspaceTaskRepository::new(fs.clone(), "parent");
        persist_stale(&first, &alice, "t1").await;
        assert_eq!(
            read_records(fs.as_ref(), &alice, &path).await["t1"].status,
            TaskStatus::Running
        );
        // Registration is durable: one entry, in the agent's own registry directory.
        let entries = registry_entries(&fs).await;
        assert_eq!(
            entries.len(),
            1,
            "expected one registered scope: {entries:?}"
        );
        drop(first);

        // Simulated restart: a fresh repository, so `local_contexts` is empty and the durable
        // registry is the only way to find the record's domain.
        let second = WorkspaceTaskRepository::new(fs.clone(), "parent");
        assert!(second.state.local_contexts.lock().unwrap().is_empty());
        second
            .sweep_orphaned_tasks_with(Duration::from_secs(300), 600_000)
            .await;

        let after = read_records(fs.as_ref(), &alice, &path).await;
        assert_eq!(after["t1"].status, TaskStatus::Failed);
        assert!(after["t1"]
            .error_message
            .as_deref()
            .unwrap_or_default()
            .starts_with("executor lost"));
        // The default domain was never touched: no stray record was created there.
        assert!(
            read_records(fs.as_ref(), &RuntimeContext::default(), &path)
                .await
                .is_empty(),
            "sweep must not write into the default domain"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sweep_still_covers_the_default_domain_without_registration() {
        let (dir, fs) = namespaced_fs("scope_default_sweep");
        let ctx = RuntimeContext::default();
        let path = tasks_path("parent", "sess");
        let first = WorkspaceTaskRepository::new(fs.clone(), "parent");
        persist_stale(&first, &ctx, "t1").await;
        drop(first);

        let second = WorkspaceTaskRepository::new(fs.clone(), "parent");
        second
            .sweep_orphaned_tasks_with(Duration::from_secs(300), 600_000)
            .await;

        assert_eq!(
            read_records(fs.as_ref(), &ctx, &path).await["t1"].status,
            TaskStatus::Failed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_domains_are_swept_independently() {
        let (dir, fs) = namespaced_fs("scope_multi");
        let alice = user_ctx("alice", "sess");
        let bob = user_ctx("bob", "sess");
        let path = tasks_path("parent", "sess");

        let first = WorkspaceTaskRepository::new(fs.clone(), "parent");
        persist_stale(&first, &alice, "t-alice").await;
        persist_stale(&first, &bob, "t-bob").await;
        drop(first);

        let entries = registry_entries(&fs).await;
        assert_eq!(entries.len(), 2, "both domains registered: {entries:?}");

        let second = WorkspaceTaskRepository::new(fs.clone(), "parent");
        second
            .sweep_orphaned_tasks_with(Duration::from_secs(300), 600_000)
            .await;

        let alice_records = read_records(fs.as_ref(), &alice, &path).await;
        let bob_records = read_records(fs.as_ref(), &bob, &path).await;
        assert_eq!(alice_records["t-alice"].status, TaskStatus::Failed);
        assert_eq!(bob_records["t-bob"].status, TaskStatus::Failed);
        // Each domain only ever sees (and repairs) its own records.
        assert!(
            !alice_records.contains_key("t-bob"),
            "alice saw bob's record"
        );
        assert!(
            !bob_records.contains_key("t-alice"),
            "bob saw alice's record"
        );
        assert!(read_records(fs.as_ref(), &RuntimeContext::default(), &path)
            .await
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registry_entries_from_another_namespace_factory_are_ignored() {
        let (dir, fs) = namespaced_fs("scope_stale_entry");
        // A registry entry written under a different namespace configuration.
        let stale = TaskScopeEntry {
            namespace: vec!["some-other-domain".to_string()],
            user_id: "alice".to_string(),
            session_id: "sess".to_string(),
        };
        let path = task_scope_path("parent", &scope_key(&stale.namespace));
        let json = serde_json::to_string(&stale).unwrap();
        assert!(write_scope_entry(fs.as_ref(), &path, &json).await);

        let repo = WorkspaceTaskRepository::new(fs.clone(), "parent");
        let contexts = maintenance_contexts(&repo.state).await;
        // Only the default domain survives: the stored namespace no longer matches the factory.
        assert_eq!(contexts.len(), 1, "unexpected contexts: {contexts:?}");
        assert_eq!(
            namespace_for(&repo.state, &contexts[0]),
            namespace_for(&repo.state, &RuntimeContext::default())
        );
        // The entry itself is left in place (harmless, and a config rollback revives it).
        assert!(fs.exists(&registry_ctx(), &path).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
