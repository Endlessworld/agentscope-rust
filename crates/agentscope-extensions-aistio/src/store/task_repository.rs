//! Control-plane hosted [`TaskRepository`] over `/api/v1/dp/tasks/*`, mirroring
//! `io.agentscope.extensions.aistio.store.ControlPlaneTaskRepository`.
//!
//! In-memory [`BackgroundTask`] handles track tasks running on this node; the control
//! plane is the authoritative persistence layer. Live local tasks are heartbeated via
//! `POST /tasks/heartbeat`; there is no client-side orphan sweeper.
//!
//! **Adaptation:** the Java version rethrows when a control-plane call fails, which makes
//! the repository unusable while the control plane is down. This port degrades to
//! best-effort persistence (log + continue) so the agent's own task path never breaks —
//! consistent with the bypass principle applied everywhere else in this crate. Logic is
//! then fully exercisable offline (tracked in the module gap tracker).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use agentscope_harness::agent::subagent::task::{
    AgentProtocolTaskClient, BackgroundTask, RemoteSubmitContext, SUPPRESS_COMPLETION_CALLBACK,
    TaskCompletionCallback, TaskDelivery, TaskRecord, TaskRepository, TaskRunSpec, TaskStatus,
};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::error::AistioResult;
use crate::store::control_plane_stores::tenant_query;
use crate::transport::{encode_path_segment, encode_query, ControlPlaneHttpClient};

/// `transportType` marking a task driven by a remote agent-protocol client.
pub const TRANSPORT_AGENT_PROTOCOL: &str = "agent-protocol";

/// Heartbeat interval for live local tasks (seconds).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Interval between remote-status polls is capped at this value.
const REMOTE_POLL_MAX_MS: u64 = 5_000;

/// Shared, cloneable internals of the repository (spawned watchers hold their own `Arc`).
struct Shared {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
    parent_agent_id: String,
    protocol_client: AgentProtocolTaskClient,
    local_tasks: Mutex<HashMap<String, Arc<BackgroundTask>>>,
    local_task_session_ids: Mutex<HashMap<String, String>>,
    completion_callback: RwLock<Option<TaskCompletionCallback>>,
}

/// Control-plane hosted [`TaskRepository`].
pub struct ControlPlaneTaskRepository {
    shared: Arc<Shared>,
    maintenance: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ControlPlaneTaskRepository {
    /// Creates a repository bound to the given client. Maintenance (heartbeat) is not
    /// started; call [`start_maintenance`](Self::start_maintenance) from a tokio runtime.
    pub fn new(
        http: Arc<ControlPlaneHttpClient>,
        agent_name: impl Into<String>,
        namespace: impl Into<String>,
        parent_agent_id: impl Into<String>,
    ) -> Self {
        let agent_name = agent_name.into();
        let parent_agent_id = parent_agent_id.into();
        let parent_agent_id = if parent_agent_id.trim().is_empty() {
            agent_name.clone()
        } else {
            parent_agent_id
        };
        Self {
            shared: Arc::new(Shared {
                http,
                agent_name,
                namespace: namespace.into(),
                parent_agent_id,
                protocol_client: AgentProtocolTaskClient::new(),
                local_tasks: Mutex::new(HashMap::new()),
                local_task_session_ids: Mutex::new(HashMap::new()),
                completion_callback: RwLock::new(None),
            }),
            maintenance: Mutex::new(None),
        }
    }

    /// Starts the periodic heartbeat loop (no-op when no tokio runtime is present).
    pub async fn start_maintenance(self: &Arc<Self>, interval: Duration) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let this = Arc::clone(self);
        let interval = interval.max(Duration::from_millis(1));
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                this.heartbeat_once().await;
            }
        });
        *self.maintenance.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
    }

    fn tenant_query(&self) -> String {
        tenant_query(&self.shared.agent_name, &self.shared.namespace)
    }

    fn tenant_body(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("agentName".into(), self.shared.agent_name.clone().into());
        m.insert("namespace".into(), self.shared.namespace.clone().into());
        m
    }

    async fn post_json(&self, path: &str, body: Value, op: &str) -> AistioResult<u16> {
        let resp = self.shared.http.send(Method::POST, path, Some(body)).await?;
        if (200..300).contains(&resp.status) {
            Ok(resp.status)
        } else {
            Err(crate::error::AistioError::ControlPlane {
                status: resp.status,
                message: format!("{op}: {}", resp.body),
            })
        }
    }

    /// Builds the upsert body for a task record (pure).
    fn upsert_body(
        tenant: Map<String, Value>,
        session_id: &str,
        record: &TaskRecord,
        user_id: Option<&str>,
    ) -> Value {
        let mut body = tenant;
        body.insert("parentSessionId".into(), session_id.into());
        body.insert("subAgentId".into(), record.sub_agent_id.clone().into());
        body.insert(
            "subSessionId".into(),
            record
                .sub_session_id
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        body.insert("status".into(), record.status.as_str().to_uppercase().into());
        body.insert(
            "result".into(),
            record.result.clone().map(Value::String).unwrap_or(Value::Null),
        );
        body.insert(
            "errorMessage".into(),
            record.error_message.clone().map(Value::String).unwrap_or(Value::Null),
        );
        body.insert("cancelRequested".into(), record.cancel_requested.into());
        body.insert(
            "transportType".into(),
            record
                .transport_type
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        body.insert(
            "remoteBaseUrl".into(),
            record.remote_base_url.clone().map(Value::String).unwrap_or(Value::Null),
        );
        if let Some(headers) = &record.remote_headers {
            body.insert("remoteHeaders".into(), json!(headers));
        }
        if let Some(uid) = user_id.filter(|u| !u.trim().is_empty()) {
            body.insert("userId".into(), uid.into());
        }
        body.insert(
            "lastCheckedAt".into(),
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                record.last_checked_at as i64,
            )
            .map(|d| d.to_rfc3339())
            .unwrap_or_default()
            .into(),
        );
        Value::Object(body)
    }

    /// Parses a control-plane task document into a [`TaskRecord`] (pure).
    fn parse_record(value: &Value) -> TaskRecord {
        let mut record = TaskRecord::default();
        record.task_id = text(value, "taskId").unwrap_or_default();
        record.sub_agent_id = text(value, "subAgentId").unwrap_or_default();
        record.parent_agent_id = text(value, "parentAgentId").unwrap_or_default();
        record.parent_session_id = text(value, "parentSessionId").unwrap_or_default();
        record.sub_session_id = text(value, "subSessionId");
        if let Some(status) = text(value, "status") {
            record.status = parse_status(&status);
        }
        record.result = text(value, "result");
        record.error_message = text(value, "errorMessage");
        record.cancel_requested = value
            .get("cancelRequested")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        record.transport_type = text(value, "transportType");
        record.remote_base_url = text(value, "remoteBaseUrl");
        record.remote_headers = value.get("remoteHeaders").and_then(|v| {
            let map = v.as_object()?;
            let mut out = BTreeMap::new();
            for (k, val) in map {
                if let Some(s) = val.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
            Some(out)
        });
        record.created_at = parse_millis(value, "createdAt").unwrap_or(record.created_at);
        record.last_checked_at = parse_millis(value, "lastCheckedAt").unwrap_or(0);
        record.last_updated_at = parse_millis(value, "lastUpdatedAt").unwrap_or(0);
        record.delivered_at = parse_millis(value, "deliveredAt");
        record
    }

    async fn read_record(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Option<TaskRecord> {
        let path = format!(
            "/api/v1/dp/tasks/{}?{}&parentSessionId={}",
            encode_path_segment(task_id),
            self.tenant_query(),
            encode_query(session_id)
        );
        let resp = self.shared.http.send(Method::GET, &path, None).await.ok()?;
        if resp.status == 404 {
            return None;
        }
        if !(200..300).contains(&resp.status) {
            tracing::warn!(status = resp.status, "aistio task get failed");
            return None;
        }
        let value: Value = serde_json::from_str(&resp.body).ok()?;
        Some(Self::parse_record(&value))
    }

    async fn list_records(
        &self,
        session_id: &str,
        filter: Option<TaskStatus>,
    ) -> Vec<TaskRecord> {
        let mut path = format!(
            "/api/v1/dp/tasks?{}&parentSessionId={}",
            self.tenant_query(),
            encode_query(session_id)
        );
        if let Some(f) = filter {
            path.push_str(&format!("&status={}", encode_query(&f.as_str().to_uppercase())));
        }
        let resp = match self.shared.http.send(Method::GET, &path, None).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "aistio task list failed");
                return Vec::new();
            }
        };
        if !(200..300).contains(&resp.status) {
            tracing::warn!(status = resp.status, "aistio task list failed");
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(&resp.body) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        value
            .get("tasks")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().map(Self::parse_record).collect())
            .unwrap_or_default()
    }

    async fn persist_record(&self, session_id: &str, record: &TaskRecord, user_id: Option<&str>) {
        let body = Self::upsert_body(self.tenant_body(), session_id, record, user_id);
        let path = format!("/api/v1/dp/tasks/{}", encode_path_segment(&record.task_id));
        if let Err(e) = self
            .shared
            .http
            .send(Method::PUT, &path, Some(body))
            .await
            .map(|r| r.status)
        {
            tracing::warn!(error = %e, task_id = %record.task_id, "aistio task persist failed");
            return;
        }
    }

    async fn update_status(
        &self,
        session_id: &str,
        task_id: &str,
        sub_agent_id: &str,
        status: TaskStatus,
        result: Option<String>,
        error: Option<String>,
    ) {
        let existing = self.read_record(session_id, task_id).await;
        if let Some(rec) = &existing {
            if rec.status.is_terminal() {
                return;
            }
            if !status.is_terminal() && rec.cancel_requested {
                return;
            }
        }
        let mut record = existing.unwrap_or_else(|| {
            TaskRecord::new(task_id, sub_agent_id, &self.shared.parent_agent_id, session_id)
        });
        record.status = status;
        if let Some(r) = result {
            record.result = Some(r);
        }
        if let Some(e) = error {
            record.error_message = Some(e);
        }
        record.touch();
        self.persist_record(session_id, &record, None).await;
    }

    fn completion_callback(&self) -> Option<TaskCompletionCallback> {
        self.shared
            .completion_callback
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn fire_completion_callback(
        &self,
        ctx: &RuntimeContext,
        task_id: &str,
        session_id: &str,
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
        if let Some(cb) = self.completion_callback() {
            cb(task_id.to_string(), session_id.to_string(), result);
        }
    }

    fn local_key(session_id: &str, task_id: &str) -> String {
        format!("{}:{}", if session_id.is_empty() { "_" } else { session_id }, task_id)
    }

    fn store_local(&self, session_id: &str, task: Arc<BackgroundTask>) {
        let key = Self::local_key(session_id, task.task_id());
        self.shared
            .local_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone(), task);
        self.shared
            .local_task_session_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, session_id.to_string());
    }

    /// Builds a synthetic handle from a persisted record (pure; no execution).
    fn synthetic_task(record: &TaskRecord) -> Arc<BackgroundTask> {
        match record.status {
            TaskStatus::Completed => BackgroundTask::from_result(
                record.task_id.clone(),
                record.sub_agent_id.clone(),
                Ok(record.result.clone().unwrap_or_default()),
            ),
            TaskStatus::Failed => BackgroundTask::from_result(
                record.task_id.clone(),
                record.sub_agent_id.clone(),
                Err(record
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Task failed".to_string())),
            ),
            TaskStatus::Cancelled => {
                let task = BackgroundTask::controlled(
                    record.task_id.clone(),
                    record.sub_agent_id.clone(),
                );
                task.cancel();
                task
            }
            // Pending / Running without a local handle: a not-yet-completed placeholder.
            _ => BackgroundTask::controlled(record.task_id.clone(), record.sub_agent_id.clone()),
        }
    }

    /// Persists a status transition off the calling task's critical path.
    ///
    /// The control plane is a best-effort reporting sink: an unreachable control plane
    /// must never block or delay the local task lifecycle. The transition therefore runs
    /// on a detached tokio task. Passing the handle returned by an earlier call as `after`
    /// keeps successive transitions ordered (RUNNING before COMPLETED/FAILED) without
    /// awaiting them on the hot path.
    fn persist_status_detached(
        self: &Arc<Self>,
        after: Option<tokio::task::JoinHandle<()>>,
        session_id: &str,
        task_id: &str,
        sub_agent_id: &str,
        status: TaskStatus,
        result: Option<String>,
        error: Option<String>,
    ) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let session_id = session_id.to_string();
        let task_id = task_id.to_string();
        let sub_agent_id = sub_agent_id.to_string();
        tokio::spawn(async move {
            if let Some(prior) = after {
                let _ = prior.await;
            }
            this.update_status(&session_id, &task_id, &sub_agent_id, status, result, error)
                .await;
        })
    }

    /// Spawns the local execution watcher, persisting lifecycle transitions.
    fn spawn_local(
        self: &Arc<Self>,
        ctx: &RuntimeContext,
        session_id: &str,
        task_id: &str,
        sub_agent_id: &str,
        execution: Box<dyn Fn() -> agentscope_core::error::Result<String> + Send + Sync>,
    ) -> Arc<BackgroundTask> {
        let task = BackgroundTask::controlled(task_id.to_string(), sub_agent_id.to_string());
        let this = Arc::clone(self);
        let session = session_id.to_string();
        let task_id_owned = task_id.to_string();
        let sub_agent_owned = sub_agent_id.to_string();
        let ctx = ctx.clone();
        let task_ref = Arc::clone(&task);
        let handle = tokio::spawn(async move {
            // Persist RUNNING best-effort on a detached task: the control plane may be
            // unreachable and must not delay local execution/completion (module docs).
            let running = this.persist_status_detached(
                None,
                &session,
                &task_id_owned,
                &sub_agent_owned,
                TaskStatus::Running,
                None,
                None,
            );
            let outcome = tokio::task::spawn_blocking(move || execution()).await;
            let result = match outcome {
                Ok(Ok(r)) => Ok(r),
                Ok(Err(e)) => Err(e.to_string()),
                Err(join) => Err(format!("task execution panicked: {join}")),
            };
            task_ref.set_done(result.clone());
            match result {
                Ok(r) => {
                    // The completion callback is a local side effect: fire it immediately.
                    this.fire_completion_callback(&ctx, &task_id_owned, &session, Some(r.clone()));
                    // Terminal status is persisted off the hot path, ordered after RUNNING.
                    this.persist_status_detached(
                        Some(running),
                        &session,
                        &task_id_owned,
                        &sub_agent_owned,
                        TaskStatus::Completed,
                        Some(r),
                        None,
                    );
                }
                Err(e) => {
                    this.fire_completion_callback(&ctx, &task_id_owned, &session, None);
                    this.persist_status_detached(
                        Some(running),
                        &session,
                        &task_id_owned,
                        &sub_agent_owned,
                        TaskStatus::Failed,
                        None,
                        Some(e),
                    );
                }
            }
        });
        task.attach_handle(handle);
        task
    }

    /// Spawns the remote (agent-protocol) execution watcher: submit, then poll to done.
    fn spawn_remote(
        self: &Arc<Self>,
        ctx: &RuntimeContext,
        session_id: &str,
        task_id: &str,
        sub_agent_id: &str,
        base_url: String,
        headers: BTreeMap<String, String>,
        input: String,
        context: Option<RemoteSubmitContext>,
    ) -> Arc<BackgroundTask> {
        let task = BackgroundTask::controlled(task_id.to_string(), sub_agent_id.to_string());
        let this = Arc::clone(self);
        let session = session_id.to_string();
        let task_id_owned = task_id.to_string();
        let sub_agent_owned = sub_agent_id.to_string();
        let ctx = ctx.clone();
        let task_ref = Arc::clone(&task);
        let handle = tokio::spawn(async move {
            let client = AgentProtocolTaskClient::new();
            if let Err(e) = client
                .submit_task(
                    &base_url,
                    &headers,
                    &task_id_owned,
                    &sub_agent_owned,
                    &input,
                    context.as_ref(),
                )
                .await
            {
                task_ref.set_done(Err(e.to_string()));
                this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Failed, None, Some(e.to_string()))
                    .await;
                this.fire_completion_callback(&ctx, &task_id_owned, &session, None);
                return;
            }
            this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Running, None, None)
                .await;
            let mut attempt: u32 = 0;
            loop {
                // Respect a local cancel before each poll.
                if let Some(rec) = this.read_record(&session, &task_id_owned).await {
                    if rec.cancel_requested {
                        let _ = client.cancel_task(&base_url, &headers, &task_id_owned).await;
                        task_ref.cancel();
                        this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Cancelled, None, None)
                            .await;
                        return;
                    }
                }
                match client.get_status(&base_url, &headers, &task_id_owned).await {
                    Ok(status) => {
                        if status.is_terminal_success() {
                            let result = client
                                .wait_for_result(&base_url, &headers, &task_id_owned, 120)
                                .await
                                .unwrap_or_default();
                            task_ref.set_done(Ok(result.clone()));
                            this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Completed, Some(result.clone()), None)
                                .await;
                            this.fire_completion_callback(&ctx, &task_id_owned, &session, Some(result));
                            return;
                        }
                        if status.is_terminal_failure() {
                            let err = status.error.unwrap_or_else(|| "remote task error".into());
                            task_ref.set_done(Err(err.clone()));
                            this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Failed, None, Some(err))
                                .await;
                            this.fire_completion_callback(&ctx, &task_id_owned, &session, None);
                            return;
                        }
                        if status.is_cancelled() {
                            task_ref.cancel();
                            this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Cancelled, None, None)
                                .await;
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "remote task status poll failed");
                    }
                }
                let sleep_ms = REMOTE_POLL_MAX_MS.min(200u64 << attempt.min(4));
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            }
        });
        task.attach_handle(handle);
        task
    }

    /// Sends a heartbeat for every live local task (mirrors Java `heartbeat`).
    pub async fn heartbeat_once(&self) {
        let mut refs: Vec<Value> = Vec::new();
        let tasks: Vec<(String, Arc<BackgroundTask>)> = self
            .shared
            .local_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        for (key, task) in tasks {
            if task.is_completed() {
                continue;
            }
            let session_id = self
                .shared
                .local_task_session_ids
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&key)
                .cloned()
                .unwrap_or_default();
            refs.push(json!({
                "parentSessionId": session_id,
                "taskId": task.task_id(),
            }));
        }
        if refs.is_empty() {
            return;
        }
        let mut body = self.tenant_body();
        body.insert("tasks".into(), Value::Array(refs));
        if let Err(e) = self
            .post_json("/api/v1/dp/tasks/heartbeat", Value::Object(body), "heartbeat")
            .await
        {
            tracing::debug!(error = %e, "aistio task heartbeat failed");
        }
    }
}

#[async_trait]
impl TaskRepository for ControlPlaneTaskRepository {
    async fn get_task(
        &self,
        _ctx: &RuntimeContext,
        session_id: &str,
        task_id: &str,
    ) -> Option<Arc<BackgroundTask>> {
        let key = Self::local_key(session_id, task_id);
        if let Some(local) = self
            .shared
            .local_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned()
        {
            return Some(local);
        }
        self.read_record(session_id, task_id)
            .await
            .map(|r| Self::synthetic_task(&r))
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
            &self.shared.parent_agent_id,
            session_id,
        );
        if let TaskRunSpec::Remote { base_url, headers, .. } = &spec {
            record.transport_type = Some(TRANSPORT_AGENT_PROTOCOL.to_string());
            record.remote_base_url = Some(base_url.clone());
            record.remote_headers = Some(headers.clone());
        }
        self.persist_record(session_id, &record, ctx.user_id()).await;

        // `self` is `&ControlPlaneTaskRepository` here, but the watchers need an `Arc`.
        // The repository is only ever driven through an `Arc` by callers; recover it via
        // a shallow clone of the shared internals when possible.
        let task = match spec {
            TaskRunSpec::Local { execution } => {
                let shared = Arc::clone(&self.shared);
                let repo = Arc::new(ControlPlaneTaskRepository {
                    shared,
                    maintenance: Mutex::new(None),
                });
                repo.spawn_local(ctx, session_id, task_id, sub_agent_id, execution)
            }
            TaskRunSpec::Remote {
                base_url,
                headers,
                input,
                context,
                ..
            } => {
                let shared = Arc::clone(&self.shared);
                let repo = Arc::new(ControlPlaneTaskRepository {
                    shared,
                    maintenance: Mutex::new(None),
                });
                repo.spawn_remote(
                    ctx, session_id, task_id, sub_agent_id, base_url, headers, input, context,
                )
            }
            TaskRunSpec::Adopted { future } => {
                let task = BackgroundTask::adopt(task_id.to_string(), sub_agent_id.to_string(), future);
                let this = Arc::new(ControlPlaneTaskRepository {
                    shared: Arc::clone(&self.shared),
                    maintenance: Mutex::new(None),
                });
                let session = session_id.to_string();
                let task_id_owned = task_id.to_string();
                let sub_agent_owned = sub_agent_id.to_string();
                let ctx = ctx.clone();
                let watcher = Arc::clone(&task);
                tokio::spawn(async move {
                    this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Running, None, None)
                        .await;
                    loop {
                        if watcher.is_completed() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    match watcher.get_task_status() {
                        TaskStatus::Completed => {
                            let result = watcher.get_result();
                            this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Completed, result.clone(), None)
                                .await;
                            this.fire_completion_callback(&ctx, &task_id_owned, &session, result);
                        }
                        TaskStatus::Cancelled => {
                            this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Cancelled, None, None)
                                .await;
                        }
                        _ => {
                            let err = watcher.get_error();
                            this.update_status(&session, &task_id_owned, &sub_agent_owned, TaskStatus::Failed, None, err)
                                .await;
                            this.fire_completion_callback(&ctx, &task_id_owned, &session, None);
                        }
                    }
                });
                task
            }
        };
        self.store_local(session_id, Arc::clone(&task));
        task
    }

    async fn remove_task(&self, _ctx: &RuntimeContext, session_id: &str, task_id: &str) {
        let key = Self::local_key(session_id, task_id);
        if let Some(task) = self
            .shared
            .local_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key)
        {
            task.cancel();
        }
        self.shared
            .local_task_session_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
    }

    fn clear(&self) {
        let tasks: Vec<Arc<BackgroundTask>> = self
            .shared
            .local_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .map(|(_, v)| v)
            .collect();
        self.shared
            .local_task_session_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        for task in tasks {
            task.cancel();
        }
    }

    async fn list_tasks(
        &self,
        _ctx: &RuntimeContext,
        session_id: &str,
        filter: Option<TaskStatus>,
    ) -> Vec<Arc<BackgroundTask>> {
        let records = self.list_records(session_id, filter).await;
        let mut out = Vec::with_capacity(records.len());
        for stored in records {
            let key = Self::local_key(session_id, &stored.task_id);
            let local = self
                .shared
                .local_tasks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&key)
                .cloned();
            let effective = match local {
                Some(local) if !stored.status.is_terminal() => local,
                _ => Self::synthetic_task(&stored),
            };
            if filter.is_none() || Some(effective.get_task_status()) == filter {
                out.push(effective);
            }
        }
        out
    }

    async fn cancel_task(&self, _ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        let key = Self::local_key(session_id, task_id);
        let mut found = false;
        if let Some(local) = self
            .shared
            .local_tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned()
        {
            local.cancel();
            found = true;
        }
        if let Some(record) = self.read_record(session_id, task_id).await {
            let mut body = self.tenant_body();
            body.insert("parentSessionId".into(), session_id.into());
            let path = format!(
                "/api/v1/dp/tasks/{}/cancel",
                encode_path_segment(task_id)
            );
            match self
                .post_json(&path, Value::Object(body), "cancel")
                .await
            {
                Ok(_) => {}
                Err(crate::error::AistioError::ControlPlane { status: 404, .. }) => return found,
                Err(e) => tracing::warn!(error = %e, "aistio task cancel failed"),
            }
            if record.transport_type.as_deref() == Some(TRANSPORT_AGENT_PROTOCOL) {
                if let Some(base_url) = &record.remote_base_url {
                    let headers = record.remote_headers.clone().unwrap_or_default();
                    if let Err(e) = self
                        .shared
                        .protocol_client
                        .cancel_task(base_url, &headers, task_id)
                        .await
                    {
                        tracing::warn!(error = %e, "remote task cancel failed");
                    }
                }
            }
            return true;
        }
        found
    }

    async fn find_pending_deliveries(
        &self,
        _ctx: &RuntimeContext,
        session_id: &str,
    ) -> Vec<TaskDelivery> {
        let path = format!(
            "/api/v1/dp/tasks/pending-deliveries?{}&parentSessionId={}",
            self.tenant_query(),
            encode_query(session_id)
        );
        let resp = match self.shared.http.send(Method::GET, &path, None).await {
            Ok(r) if (200..300).contains(&r.status) => r,
            Ok(r) => {
                tracing::warn!(status = r.status, "aistio pending-deliveries failed");
                return Vec::new();
            }
            Err(e) => {
                tracing::warn!(error = %e, "aistio pending-deliveries failed");
                return Vec::new();
            }
        };
        let value: Value = match serde_json::from_str(&resp.body) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut records: Vec<TaskRecord> = value
            .get("tasks")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().map(Self::parse_record).collect())
            .unwrap_or_default();
        records.sort_by_key(|r| r.last_updated_at);
        records
            .into_iter()
            .map(|r| TaskDelivery {
                task_id: r.task_id,
                agent_id: Some(r.sub_agent_id),
                status: r.status,
                result: r.result,
                error_message: r.error_message,
                completed_at: r.last_updated_at,
            })
            .collect()
    }

    async fn mark_delivered(&self, _ctx: &RuntimeContext, session_id: &str, task_id: &str) {
        if self.read_record(session_id, task_id).await.is_none() {
            return;
        }
        let mut body = self.tenant_body();
        body.insert("parentSessionId".into(), session_id.into());
        let path = format!("/api/v1/dp/tasks/{}/delivered", encode_path_segment(task_id));
        match self.post_json(&path, Value::Object(body), "delivered").await {
            Ok(_) => {}
            Err(crate::error::AistioError::ControlPlane { status: 404, .. }) => {}
            Err(e) => tracing::warn!(error = %e, "aistio task markDelivered failed"),
        }
    }

    async fn is_delivered(&self, _ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        self.read_record(session_id, task_id)
            .await
            .map(|r| r.delivered_at.is_some())
            .unwrap_or(false)
    }

    async fn set_completion_callback(&self, callback: TaskCompletionCallback) {
        *self
            .shared
            .completion_callback
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(callback);
    }

    async fn shutdown(&self) {
        if let Some(handle) = self
            .maintenance
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            handle.abort();
        }
    }
}

// ─── pure helpers ───

fn text(value: &Value, field: &str) -> Option<String> {
    let v = value.get(field)?;
    if v.is_null() {
        return None;
    }
    let s = v.as_str().unwrap_or_default();
    if s.trim().is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn parse_status(status: &str) -> TaskStatus {
    match status.to_ascii_uppercase().as_str() {
        "RUNNING" => TaskStatus::Running,
        "COMPLETED" => TaskStatus::Completed,
        "FAILED" => TaskStatus::Failed,
        "CANCELLED" | "CANCELED" => TaskStatus::Cancelled,
        _ => TaskStatus::Pending,
    }
}

fn parse_millis(value: &Value, field: &str) -> Option<u64> {
    let v = value.get(field)?;
    if v.is_null() {
        return None;
    }
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis().max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> Arc<ControlPlaneTaskRepository> {
        Arc::new(ControlPlaneTaskRepository::new(
            Arc::new(ControlPlaneHttpClient::new("http://127.0.0.1:1", "tok").unwrap()),
            "friday",
            "default",
            "parent-1",
        ))
    }

    #[test]
    fn local_key_handles_blank_session() {
        assert_eq!(ControlPlaneTaskRepository::local_key("s1", "t1"), "s1:t1");
        assert_eq!(ControlPlaneTaskRepository::local_key("", "t1"), "_:t1");
    }

    #[test]
    fn parses_task_document() {
        let doc = json!({
            "taskId": "t1",
            "subAgentId": "researcher",
            "parentSessionId": "s1",
            "status": "RUNNING",
            "cancelRequested": true,
            "transportType": "agent-protocol",
            "remoteBaseUrl": "http://remote",
            "remoteHeaders": {"Authorization": "Bearer x"},
            "createdAt": "2026-09-18T10:00:00Z",
            "lastUpdatedAt": "2026-09-18T10:00:05Z"
        });
        let record = ControlPlaneTaskRepository::parse_record(&doc);
        assert_eq!(record.task_id, "t1");
        assert_eq!(record.sub_agent_id, "researcher");
        assert_eq!(record.status, TaskStatus::Running);
        assert!(record.cancel_requested);
        assert_eq!(record.transport_type.as_deref(), Some(TRANSPORT_AGENT_PROTOCOL));
        assert_eq!(record.remote_base_url.as_deref(), Some("http://remote"));
        assert_eq!(record.remote_headers.unwrap().get("Authorization").unwrap(), "Bearer x");
        assert!(record.created_at > 0);
        assert!(record.last_updated_at > record.created_at);
    }

    #[test]
    fn upsert_body_carries_status_and_transport() {
        let repo = repository();
        let mut record = TaskRecord::new("t1", "researcher", "parent-1", "s1");
        record.status = TaskStatus::Failed;
        record.error_message = Some("boom".into());
        record.transport_type = Some(TRANSPORT_AGENT_PROTOCOL.into());
        record.remote_headers = Some(BTreeMap::from([("X".to_string(), "1".to_string())]));
        let body = ControlPlaneTaskRepository::upsert_body(
            repo.tenant_body(),
            "s1",
            &record,
            Some("user-9"),
        );
        assert_eq!(body["status"], "FAILED");
        assert_eq!(body["errorMessage"], "boom");
        assert_eq!(body["transportType"], TRANSPORT_AGENT_PROTOCOL);
        assert_eq!(body["remoteHeaders"]["X"], "1");
        assert_eq!(body["userId"], "user-9");
        assert_eq!(body["agentName"], "friday");
    }

    #[test]
    fn synthetic_task_reflects_terminal_state() {
        let mut record = TaskRecord::new("t1", "researcher", "parent-1", "s1");
        record.status = TaskStatus::Completed;
        record.result = Some("done".into());
        let task = ControlPlaneTaskRepository::synthetic_task(&record);
        assert_eq!(task.get_task_status(), TaskStatus::Completed);
        assert_eq!(task.get_result().as_deref(), Some("done"));

        let mut failed = TaskRecord::new("t2", "researcher", "parent-1", "s1");
        failed.status = TaskStatus::Failed;
        failed.error_message = Some("nope".into());
        let task = ControlPlaneTaskRepository::synthetic_task(&failed);
        assert_eq!(task.get_task_status(), TaskStatus::Failed);
        assert_eq!(task.get_error().as_deref(), Some("nope"));

        let mut cancelled = TaskRecord::new("t3", "researcher", "parent-1", "s1");
        cancelled.status = TaskStatus::Cancelled;
        let task = ControlPlaneTaskRepository::synthetic_task(&cancelled);
        assert_eq!(task.get_task_status(), TaskStatus::Cancelled);
    }

    #[test]
    fn status_parsing_is_lenient() {
        assert_eq!(parse_status("running"), TaskStatus::Running);
        assert_eq!(parse_status("COMPLETED"), TaskStatus::Completed);
        assert_eq!(parse_status("canceled"), TaskStatus::Cancelled);
        assert_eq!(parse_status("weird"), TaskStatus::Pending);
    }

    #[tokio::test]
    async fn local_task_reaches_completed_without_control_plane() {
        // Persistence is best-effort: the control plane is unreachable, but the local
        // task lifecycle still completes.
        let repo = repository();
        let ctx = RuntimeContext::new();
        let task = repo
            .put_task(
                &ctx,
                "t1",
                "researcher",
                "s1",
                TaskRunSpec::local(|| Ok("value-1".to_string())),
            )
            .await;
        for _ in 0..100 {
            if task.is_completed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(task.get_task_status(), TaskStatus::Completed);
        assert_eq!(task.get_result().as_deref(), Some("value-1"));
        let fetched = repo.get_task(&ctx, "s1", "t1").await.unwrap();
        assert!(Arc::ptr_eq(&fetched, &task));
        repo.clear();
        assert!(repo.get_task(&ctx, "s1", "t1").await.is_none());
    }

    #[tokio::test]
    async fn completion_callback_fires_and_respects_suppression() {
        let repo = repository();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&seen);
        repo.set_completion_callback(Arc::new(move |task_id, _session, _result| {
            sink.lock().unwrap().push(task_id);
        }))
        .await;

        let ctx = RuntimeContext::new();
        let task = repo
            .put_task(&ctx, "t2", "researcher", "s1", TaskRunSpec::local(|| Ok("ok".into())))
            .await;
        for _ in 0..100 {
            if task.is_completed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(seen.lock().unwrap().as_slice(), ["t2"]);

        let mut suppressed = RuntimeContext::new();
        suppressed.metadata.insert(
            SUPPRESS_COMPLETION_CALLBACK.to_string(),
            Value::Bool(true),
        );
        let task = repo
            .put_task(&suppressed, "t3", "researcher", "s1", TaskRunSpec::local(|| Ok("ok".into())))
            .await;
        for _ in 0..100 {
            if task.is_completed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(seen.lock().unwrap().as_slice(), ["t2"]);
    }
}
