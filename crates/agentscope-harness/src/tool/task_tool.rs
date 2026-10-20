//! Background task lifecycle tools, mirroring `TaskTool`: `task_output`,
//! `task_cancel` and `task_list`. All operations are scoped to the parent session
//! id carried in `ToolExecutionContext.runtime` under the `"sessionId"` key.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};

use crate::agent::subagent::task::{BackgroundTask, TaskRepository, TaskStatus};

/// Shared core for the three task tools.
pub struct TaskTool {
    repo: Arc<dyn TaskRepository>,
}

impl TaskTool {
    pub fn new(repo: Arc<dyn TaskRepository>) -> Arc<Self> {
        Arc::new(Self { repo })
    }

    pub fn output_tool(self: &Arc<Self>) -> Arc<TaskOutputTool> {
        Arc::new(TaskOutputTool { tool: self.clone() })
    }
    pub fn cancel_tool(self: &Arc<Self>) -> Arc<TaskCancelTool> {
        Arc::new(TaskCancelTool { tool: self.clone() })
    }
    pub fn list_tool(self: &Arc<Self>) -> Arc<TaskListTool> {
        Arc::new(TaskListTool { tool: self.clone() })
    }

    pub fn register_all(self: &Arc<Self>, toolkit: &Toolkit) {
        toolkit.register_tool(self.output_tool());
        toolkit.register_tool(self.cancel_tool());
        toolkit.register_tool(self.list_tool());
    }
}

/// Reads the parent session id out of the tool execution context, if present.
fn session_from_ctx(ctx: &ToolExecutionContext) -> String {
    ctx.runtime
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn runtime_for(session: &str) -> RuntimeContext {
    if session.is_empty() {
        RuntimeContext::default()
    } else {
        RuntimeContext::default().with_session_id(session)
    }
}

fn required_str(args: &JsonMap, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| agentscope_core::Error::Tool(format!("Missing string argument: {key}")))
}

fn format_iso(millis: u64) -> String {
    let secs = (millis / 1000) as i64;
    let nsecs = ((millis % 1000) * 1_000_000) as u32;
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nsecs);
    dt.map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_task_detail(task: &BackgroundTask) -> String {
    let mut sb = String::new();
    sb.push_str(&format!("task_id: {}\n", task.task_id()));
    if !task.agent_id().is_empty() {
        sb.push_str(&format!("agent_id: {}\n", task.agent_id()));
    }
    sb.push_str(&format!("status: {}\n", task.get_status_string()));
    sb.push_str(&format!("created_at: {}\n", format_iso(task.created_at())));
    if let Some(r) = task.get_result() {
        sb.push_str(&format!("\nResult:\n{r}"));
    } else if let Some(e) = task.get_error() {
        sb.push_str(&format!("\nError:\n{e}"));
    } else if !task.is_completed() {
        sb.push_str("\nTask still running...");
    } else {
        sb.push_str("\nTask completed with no result.");
    }
    sb
}

fn parse_status_filter(filter: Option<&str>) -> Option<TaskStatus> {
    let f = filter?;
    let f = f.trim();
    if f.is_empty() || f.eq_ignore_ascii_case("all") {
        return None;
    }
    match f.to_uppercase().as_str() {
        "PENDING" => Some(TaskStatus::Pending),
        "RUNNING" => Some(TaskStatus::Running),
        "COMPLETED" => Some(TaskStatus::Completed),
        "FAILED" => Some(TaskStatus::Failed),
        "CANCELLED" => Some(TaskStatus::Cancelled),
        _ => None,
    }
}

pub struct TaskOutputTool {
    tool: Arc<TaskTool>,
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }
    fn description(&self) -> &str {
        "Retrieve the output of a background subagent task by task_id. Prefer block=false to check status without waiting."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The task_id returned by agent_spawn or agent_send" },
                "block": { "type": "boolean", "description": "Whether to wait for completion (default: true)", "default": true },
                "timeout": { "type": "integer", "description": "Max wait in ms (default 30000, max 600000)", "default": 30000 }
            },
            "required": ["task_id"]
        })
    }
    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let task_id = match required_str(&args, "task_id") {
            Ok(t) => t,
            Err(_) => return Ok(ToolResultBlock::error("Error: task_id is required")),
        };
        let session = session_from_ctx(&ctx);
        let rc = runtime_for(&session);
        let Some(bg) = self.tool.repo.get_task(&rc, &session, &task_id).await else {
            return Ok(ToolResultBlock::error(format!(
                "Error: No background task found with ID: {task_id}. Use task_list() to see all known tasks."
            )));
        };
        bg.update_last_checked_at();
        let should_block = args.get("block").and_then(|v| v.as_bool()).unwrap_or(true);
        let timeout = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .map(|t| t.min(600_000))
            .unwrap_or(30_000);
        if should_block && !bg.is_completed() {
            let st = bg.get_task_status();
            if st == TaskStatus::Pending || st == TaskStatus::Running {
                // `wait_for_completion` parks the calling thread on a condvar; run it on the
                // blocking pool so this async tool does not starve a runtime worker (and, on a
                // current-thread runtime, does not deadlock against the task it is waiting for).
                let waiter = Arc::clone(&bg);
                let _ = tokio::task::spawn_blocking(move || waiter.wait_for_completion(timeout))
                    .await;
            }
        }
        if bg.is_completed() && bg.get_task_status().is_terminal() {
            self.tool.repo.mark_delivered(&rc, &session, &task_id).await;
        }
        Ok(ToolResultBlock::text(format_task_detail(&bg)))
    }
}

pub struct TaskCancelTool {
    tool: Arc<TaskTool>,
}

#[async_trait]
impl Tool for TaskCancelTool {
    fn name(&self) -> &str {
        "task_cancel"
    }
    fn description(&self) -> &str {
        "Cancel a running background task. No effect on already-completed tasks."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "task_id": { "type": "string", "description": "The task_id to cancel" } },
            "required": ["task_id"]
        })
    }
    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let task_id = match required_str(&args, "task_id") {
            Ok(t) => t,
            Err(_) => return Ok(ToolResultBlock::error("Error: task_id is required")),
        };
        let session = session_from_ctx(&ctx);
        let rc = runtime_for(&session);
        let Some(bg) = self.tool.repo.get_task(&rc, &session, &task_id).await else {
            return Ok(ToolResultBlock::error(format!("Error: No background task found with ID: {task_id}")));
        };
        let current = bg.get_task_status();
        if current.is_terminal() {
            return Ok(ToolResultBlock::text(format!(
                "task_id: {task_id}\nstatus: {}\nnote: Task already in terminal state, cannot cancel.",
                current.as_str().to_lowercase()
            )));
        }
        self.tool.repo.cancel_task(&rc, &session, &task_id).await;
        Ok(ToolResultBlock::text(format!(
            "task_id: {task_id}\nstatus: cancelled\nCancellation requested successfully."
        )))
    }
}

pub struct TaskListTool {
    tool: Arc<TaskTool>,
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }
    fn description(&self) -> &str {
        "List all background tasks for the current session with their statuses. Optionally filter by status."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status_filter": { "type": "string", "description": "Filter: running, completed, failed, cancelled (optional)", "default": null }
            },
            "required": []
        })
    }
    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let filter = args.get("status_filter").and_then(|v| v.as_str());
        let filter = parse_status_filter(filter);
        let session = session_from_ctx(&ctx);
        let rc = runtime_for(&session);
        let tasks = self.tool.repo.list_tasks(&rc, &session, filter).await;
        if tasks.is_empty() {
            let filter_desc = filter
                .map(|f| format!(" with status '{}'", f.as_str().to_lowercase()))
                .unwrap_or_default();
            return Ok(ToolResultBlock::text(format!("No background tasks tracked{filter_desc}.")));
        }
        let mut sb = format!("{} tracked task(s):\n", tasks.len());
        for t in &tasks {
            sb.push_str(&format!("- task_id: {}", t.task_id()));
            if !t.agent_id().is_empty() {
                sb.push_str(&format!("  agent: {}", t.agent_id()));
            }
            sb.push_str(&format!("  status: {}", t.get_task_status().as_str().to_lowercase()));
            sb.push_str(&format!("  created: {}\n", format_iso(t.created_at())));
        }
        Ok(ToolResultBlock::text(sb.trim().to_string()))
    }
}
