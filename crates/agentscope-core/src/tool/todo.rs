//! Built-in `todo_write` tool, mirroring `io.agentscope.core.tool.builtin.TodoTools`.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::message::{JsonMap, ToolResultBlock};
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
        let mut by_content: HashMap<String, Task> = HashMap::new();
        for existing in &ctx.tasks {
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
        ctx.tasks = rebuilt;
        drop(ctx);

        Ok(ToolResultBlock::text(render(&self.tasks.lock().unwrap().tasks)))
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
