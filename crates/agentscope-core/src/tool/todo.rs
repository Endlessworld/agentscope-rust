//! Built-in `todo_write` tool, mirroring `io.agentscope.core.tool.builtin.TodoTools`.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::message::{ContentBlock, JsonMap, TextBlock, ToolResultBlock, ToolResultState};
use crate::state::context::{Task, TaskContextState, TaskState};

use super::tool::{Tool, ToolExecutionContext};

/// A single todo item as supplied by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub priority: Option<String>,
}

const DESCRIPTION: &str = "Create and maintain a structured task list for the current session. \
Tracks progress, organizes multi-step work, and surfaces status to the user. Pass the COMPLETE \
updated list every call - this tool replaces the whole list (it does not merge).";

/// Metadata key naming the state bag a tool receipt belongs to.
pub const METADATA_STATE_KEY: &str = "context.state_key";

/// Metadata key carrying the task-context revision the receipt was produced at.
pub const METADATA_STATE_REVISION: &str = "context.state_revision";

/// Metadata key distinguishing the complete current state from a superseded receipt.
pub const METADATA_REPRESENTATION: &str = "context.representation";

/// [`METADATA_STATE_KEY`] value of the session task context.
pub const STATE_KEY_TASKS_CONTEXT: &str = "tasksContext";

/// [`METADATA_REPRESENTATION`] value for the complete, current state.
pub const REPRESENTATION_FULL: &str = "full";

/// [`METADATA_REPRESENTATION`] value for a superseded receipt (written by the projection).
pub const REPRESENTATION_RECEIPT: &str = "receipt";

/// A tool that maintains the agent's structured todo list using full-list-replace semantics.
pub struct TodoWriteTool {
    tasks: Arc<Mutex<TaskContextState>>,
}

impl TodoWriteTool {
    /// Creates a new tool bound to the shared task bag.
    pub fn new(tasks: Arc<Mutex<TaskContextState>>) -> Self {
        Self { tasks }
    }
}


#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Brief, specific, actionable description of the task."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                            "priority": {"type": "string", "enum": ["high", "medium", "low"]}
                        },
                        "required": ["content"]
                    },
                    "description": "The COMPLETE updated todo list. Replaces the existing list entirely."
                }
            },
            "required": ["todos"]
        })
    }
    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let items: Vec<TodoItem> = args
            .get("todos")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // Validate the single-in_progress invariant before mutating anything.
        let mut in_progress = 0usize;
        for item in &items {
            let parsed = parse_state(&item.status);
            if parsed.is_none() {
                return Ok(ToolResultBlock::text(format!(
                    "Error: invalid status '{}' for todo '{}'. Allowed: pending, in_progress, completed.",
                    item.status, item.content
                )));
            }
            if parsed == Some(TaskState::InProgress) {
                in_progress += 1;
            }
            if item.content.trim().is_empty() {
                return Ok(ToolResultBlock::text(
                    "Error: every todo must have non-blank content.".to_string(),
                ));
            }
        }
        if in_progress > 1 {
            return Ok(ToolResultBlock::text(format!(
                "Error: at most one task may be in_progress at a time, but {} were provided.",
                in_progress
            )));
        }

        let mut ctx = self.tasks.lock().unwrap();
        let snapshot = ctx.snapshot();
        let mut by_content: HashMap<String, Task> = HashMap::new();
        for existing in snapshot.tasks() {
            by_content.entry(existing.subject.clone()).or_insert_with(|| existing.clone());
        }

        let mut rebuilt = Vec::with_capacity(items.len());
        for item in &items {
            let parsed = parse_state(&item.status).unwrap_or(TaskState::Pending);
            let prior = by_content.get(&item.content).cloned();
            let mut metadata = JsonMap::new();
            if let Some(p) = &prior {
                metadata = p.metadata.clone();
            }
            if let Some(priority) = normalize_priority(item.priority.as_deref()) {
                metadata.insert("priority".to_string(), serde_json::json!(priority));
            }
            let mut b = Task::builder()
                .subject(&item.content)
                .description(&item.content)
                .state(parsed)
                .metadata(metadata);
            if let Some(p) = prior {
                b = b.id(p.id).created_at(p.created_at);
            }
            rebuilt.push(b.build());
        }
        // Replace through the revision-checked API: a concurrent writer that already moved the
        // task context past the snapshot must not be clobbered, and the receipt below has to
        // cite the revision it actually produced.
        if let Err(error) = ctx.replace_tasks(rebuilt.clone(), snapshot.revision()) {
            return Ok(ToolResultBlock::error(error.to_string()));
        }
        let revision = ctx.revision();
        drop(ctx);

        let mut metadata = JsonMap::new();
        metadata.insert(
            METADATA_STATE_KEY.to_string(),
            serde_json::json!(STATE_KEY_TASKS_CONTEXT),
        );
        metadata.insert(
            METADATA_STATE_REVISION.to_string(),
            serde_json::json!(revision),
        );
        metadata.insert(
            METADATA_REPRESENTATION.to_string(),
            serde_json::json!(REPRESENTATION_FULL),
        );
        Ok(ToolResultBlock::builder()
            .output(ContentBlock::Text(TextBlock::new(render(&rebuilt))))
            .metadata(metadata)
            .state(ToolResultState::Success)
            .build())
    }
}

fn parse_state(wire: &str) -> Option<TaskState> {
    match wire.trim() {
        "" | "pending" => Some(TaskState::Pending),
        "in_progress" => Some(TaskState::InProgress),
        "completed" => Some(TaskState::Completed),
        _ => None,
    }
}

fn normalize_priority(priority: Option<&str>) -> Option<String> {
    match priority.map(|p| p.trim().to_ascii_lowercase()).as_deref() {
        Some("high") | Some("medium") | Some("low") => {
            Some(priority.unwrap().trim().to_ascii_lowercase())
        }
        _ => None,
    }
}

fn marker(state: TaskState) -> &'static str {
    match state {
        TaskState::Completed => "- [x]",
        TaskState::InProgress => "- [~]",
        TaskState::Pending => "- [ ]",
    }
}

/// Renders the task list for a tool/middleware response.
pub fn render(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "Todo list cleared (0 items).".to_string();
    }
    let open = tasks.iter().filter(|t| t.state != TaskState::Completed).count();
    let mut s = format!("{} open todo(s):\n", open);
    for t in tasks {
        let priority = t.metadata.get("priority").cloned();
        let suffix = match priority {
            Some(serde_json::Value::String(p)) => format!(" (priority: {})", p),
            _ => String::new(),
        };
        s.push_str(&format!("{}{}{}\n", marker(t.state), t.subject, suffix));
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(todos: serde_json::Value) -> JsonMap {
        let mut args = JsonMap::new();
        args.insert("todos".to_string(), todos);
        args
    }

    fn metadata_str<'a>(result: &'a ToolResultBlock, key: &str) -> Option<&'a str> {
        result.metadata.get(key).and_then(|value| value.as_str())
    }

    #[tokio::test]
    async fn write_returns_a_full_receipt_citing_the_new_revision() {
        let tasks = Arc::new(Mutex::new(TaskContextState::new()));
        let tool = TodoWriteTool::new(Arc::clone(&tasks));
        let result = tool
            .call(
                args(serde_json::json!([
                    {"content": "step one", "status": "in_progress"},
                    {"content": "step two", "status": "pending", "priority": "high"}
                ])),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(result.state, ToolResultState::Success);
        assert_eq!(metadata_str(&result, METADATA_STATE_KEY), Some(STATE_KEY_TASKS_CONTEXT));
        assert_eq!(
            result.metadata.get(METADATA_STATE_REVISION).and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            metadata_str(&result, METADATA_REPRESENTATION),
            Some(REPRESENTATION_FULL)
        );
        assert_eq!(result.output.len(), 1);
        assert_eq!(result.id, None);
        assert_eq!(result.name, None);

        let rendered = render(tasks.lock().unwrap().tasks());
        assert_eq!(result.text_content(), rendered);
        assert_eq!(tasks.lock().unwrap().revision(), 1);
    }

    #[tokio::test]
    async fn a_second_write_advances_the_revision_and_keeps_task_ids() {
        let tasks = Arc::new(Mutex::new(TaskContextState::new()));
        let tool = TodoWriteTool::new(Arc::clone(&tasks));

        let first = tool
            .call(
                args(serde_json::json!([{"content": "step one", "status": "pending"}])),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            first.metadata.get(METADATA_STATE_REVISION).and_then(|v| v.as_u64()),
            Some(1)
        );

        let second = tool
            .call(
                args(serde_json::json!([{"content": "step one", "status": "completed"}])),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            second.metadata.get(METADATA_STATE_REVISION).and_then(|v| v.as_u64()),
            Some(2)
        );

        let state = tasks.lock().unwrap();
        assert_eq!(state.revision(), 2);
        assert_eq!(state.tasks().len(), 1);
        assert_eq!(state.tasks()[0].state, TaskState::Completed);
    }

    #[tokio::test]
    async fn validation_errors_keep_their_text_and_do_not_touch_the_state() {
        let tasks = Arc::new(Mutex::new(TaskContextState::new()));
        let tool = TodoWriteTool::new(Arc::clone(&tasks));
        let result = tool
            .call(
                args(serde_json::json!([
                    {"content": "a", "status": "in_progress"},
                    {"content": "b", "status": "in_progress"}
                ])),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.text_content(),
            "Error: at most one task may be in_progress at a time, but 2 were provided."
        );
        assert!(result.metadata.is_empty());
        assert_eq!(tasks.lock().unwrap().revision(), 0);
        assert!(tasks.lock().unwrap().tasks().is_empty());
    }
}
