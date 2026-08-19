//! Workspace-backed `TaskRepository`, mirroring `WorkspaceTaskRepository`.
//!
//! Storage layout: `agents/<parentAgentId>/tasks/<sessionId>.json` — a JSON map of
//! `taskId → TaskRecord`, using the harness `Filesystem` trait as the authoritative truth
//! source, while maintaining in-memory `BackgroundTask` handles as a local overlay for tasks
//! running on the current node.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;

use crate::filesystem::Filesystem;

use super::background::BackgroundTask;
use super::delivery::TaskDelivery;
use super::now_millis;
use super::record::TaskRecord;
use super::repository::{TaskCompletionCallback, TaskRepository};
use super::spec::TaskRunSpec;
use super::status::TaskStatus;

const TRANSPORT_AGENT_PROTOCOL: &str = "agent-protocol";

/// Workspace-backed `TaskRepository` that uses the `Filesystem` trait as the authoritative
/// truth source for task state, with in-memory `BackgroundTask` handles as a local overlay.
pub struct WorkspaceTaskRepository {
    fs: Arc<dyn Filesystem>,
    parent_agent_id: String,
    /// In-memory local task handles, keyed by `"<sessionId>:<taskId>"`.
    local_tasks: std::sync::Mutex<BTreeMap<String, Arc<BackgroundTask>>>,
    completion_callback: tokio::sync::RwLock<Option<TaskCompletionCallback>>,
}

impl WorkspaceTaskRepository {
    pub fn new(fs: Arc<dyn Filesystem>, parent_agent_id: impl Into<String>) -> Self {
        Self {
            fs,
            parent_agent_id: parent_agent_id.into(),
            local_tasks: std::sync::Mutex::new(BTreeMap::new()),
            completion_callback: tokio::sync::RwLock::new(None),
        }
    }

    /// Registers a callback invoked when any task reaches a terminal state (COMPLETED or
    /// FAILED). Only one callback is supported; a second call replaces the previous one.
    pub async fn set_completion_callback(&self, callback: TaskCompletionCallback) {
        *self.completion_callback.write().await = Some(callback);
    }

    fn tasks_path(&self, session_id: &str) -> String {
        format!("agents/{}/tasks/{}.json", self.parent_agent_id, session_id)
    }

    async fn persist_record(&self, ctx: &RuntimeContext, session_id: &str, record: &TaskRecord) {
        let path = self.tasks_path(session_id);
        let mut map = read_records(self.fs.as_ref(), ctx, &path).await;
        map.insert(record.task_id.clone(), record.clone());
        write_records(self.fs.as_ref(), ctx, &path, &map).await;
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
        if let Some(local) = self.local_tasks.lock().unwrap().get(&key).cloned() {
            return Some(local);
        }
        let path = self.tasks_path(session_id);
        let map = read_records(self.fs.as_ref(), ctx, &path).await;
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
        let mut record = TaskRecord::new(task_id, sub_agent_id, &self.parent_agent_id, session_id);
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

        let fs = self.fs.clone();
        let parent_agent_id = self.parent_agent_id.clone();
        let ctx = ctx.clone();
        let task_id = task_id.to_string();
        let session_id = session_id.to_string();
        let bg2 = bg.clone();
        let cb = self.completion_callback.read().await.clone();

        let handle = tokio::task::spawn(async move {
            update_record_status(
                fs.as_ref(), &ctx, &parent_agent_id, &session_id, &task_id,
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
                TaskRunSpec::Remote { .. } => {
                    // Remote agent-protocol execution is not wired to a real client yet; the
                    // task stays tracked via the workspace record as RUNNING.
                    return;
                }
                TaskRunSpec::Adopted { future } => match future.await {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(je) => Err(format!("task join error: {je}")),
                },
            };

            match &outcome {
                Ok(r) => {
                    update_record_status(
                        fs.as_ref(), &ctx, &parent_agent_id, &session_id, &task_id,
                        TaskStatus::Completed, Some(r.clone()), None,
                    )
                    .await;
                    fire_callback(&cb, task_id.clone(), session_id.clone(), Some(r.clone()));
                }
                Err(e) => {
                    update_record_status(
                        fs.as_ref(), &ctx, &parent_agent_id, &session_id, &task_id,
                        TaskStatus::Failed, None, Some(e.clone()),
                    )
                    .await;
                    fire_callback(&cb, task_id.clone(), session_id.clone(), None);
                }
            }
            bg2.set_done(outcome);
        });
        bg.attach_handle(handle);

        self.local_tasks.lock().unwrap().insert(local_key, bg.clone());
        bg
    }

    async fn remove_task(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) {
        let key = format!("{session_id}:{task_id}");
        self.local_tasks.lock().unwrap().remove(&key);
        let path = self.tasks_path(session_id);
        let mut map = read_records(self.fs.as_ref(), ctx, &path).await;
        map.remove(task_id);
        write_records(self.fs.as_ref(), ctx, &path, &map).await;
    }

    fn clear(&self) {
        self.local_tasks.lock().unwrap().clear();
    }

    async fn list_tasks(
        &self,
        ctx: &RuntimeContext,
        session_id: &str,
        filter: Option<TaskStatus>,
    ) -> Vec<Arc<BackgroundTask>> {
        let path = self.tasks_path(session_id);
        let map = read_records(self.fs.as_ref(), ctx, &path).await;
        let mut result = Vec::new();
        for (task_id, ws_record) in map {
            let key = format!("{session_id}:{task_id}");
            let local = self.local_tasks.lock().unwrap().get(&key).cloned();
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
        let found_local = {
            let map = self.local_tasks.lock().unwrap();
            if let Some(local) = map.get(&key) {
                local.cancel();
                true
            } else {
                false
            }
        };

        let path = self.tasks_path(session_id);
        let mut map = read_records(self.fs.as_ref(), ctx, &path).await;
        if let Some(rec) = map.get_mut(task_id) {
            rec.cancel_requested = true;
            if !rec.status.is_terminal() {
                rec.status = TaskStatus::Cancelled;
            }
            rec.touch();
            write_records(self.fs.as_ref(), ctx, &path, &map).await;
            true
        } else {
            found_local
        }
    }

    async fn find_pending_deliveries(&self, ctx: &RuntimeContext, session_id: &str) -> Vec<TaskDelivery> {
        let path = self.tasks_path(session_id);
        let map = read_records(self.fs.as_ref(), ctx, &path).await;
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
        let mut map = read_records(self.fs.as_ref(), ctx, &path).await;
        if let Some(rec) = map.get_mut(task_id) {
            rec.delivered_at = Some(now_millis());
            rec.touch();
        }
        write_records(self.fs.as_ref(), ctx, &path, &map).await;
    }

    async fn is_delivered(&self, ctx: &RuntimeContext, session_id: &str, task_id: &str) -> bool {
        let path = self.tasks_path(session_id);
        let map = read_records(self.fs.as_ref(), ctx, &path).await;
        map.get(task_id).map(|r| r.is_delivered()).unwrap_or(false)
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
        // `Filesystem::write` refuses to overwrite an existing path, so delete-then-write to
        // implement an idempotent full-map upsert.
        if fs.exists(ctx, path).await.unwrap_or(false) {
            let _ = fs.delete(ctx, path).await;
        }
        let _ = fs.write(ctx, path, &json).await;
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
    let mut map = read_records(fs, ctx, &path).await;
    if let Some(rec) = map.get_mut(task_id) {
        rec.status = status;
        rec.result = result;
        rec.error_message = error;
        rec.touch();
    }
    write_records(fs, ctx, &path, &map).await;
}

fn fire_callback(
    cb: &Option<TaskCompletionCallback>,
    task_id: String,
    session_id: String,
    result: Option<String>,
) {
    if let Some(cb) = cb {
        cb(task_id, session_id, result);
    }
}
