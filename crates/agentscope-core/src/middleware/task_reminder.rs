//! TaskReminderMiddleware: keeps the agent's todo list in front of the model each step.
//! Mirrors `io.agentscope.core.middleware.TaskReminderMiddleware`.
use std::sync::{Arc, Mutex};

use crate::agent::RuntimeContext;
use crate::message::{JsonMap, Msg, MsgRole};
use crate::middleware::input::{EventStream, ReasoningInput};
use crate::middleware::{Middleware, Next};
use crate::state::context::TaskContextState;
use crate::tool::todo::render;

const GROUNDING: &str = "\n\n## Task List\nYou have a `todo_write` tool that maintains a structured task list for this session. \
Use it for multi-step work: capture the plan as todos, keep exactly one task `in_progress`, \
and update the whole list as you make progress. Your current list (if any) is shown to you \
before each step inside a `<system-reminder>` block.";

/// Appends a fresh `<system-reminder>` rendering the current task list before every
/// reasoning step, plus a one-time grounding explanation in the system prompt.
pub struct TaskReminderMiddleware {
    tasks: Arc<Mutex<TaskContextState>>,
}

impl TaskReminderMiddleware {
    /// Creates a middleware bound to the same shared task bag as the `todo_write` tool.
    pub fn new(tasks: Arc<Mutex<TaskContextState>>) -> Self {
        Self { tasks }
    }
}

#[async_trait::async_trait]
impl Middleware for TaskReminderMiddleware {
    async fn on_system_prompt(&self, _ctx: &RuntimeContext, current_prompt: String) -> String {
        format!("{}{}", current_prompt, GROUNDING)
    }

    fn on_reasoning(
        &self,
        _ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        let tasks = self.tasks.lock().unwrap().clone();
        if tasks.tasks.is_empty() {
            return next(input);
        }
        let mut metadata = JsonMap::new();
        metadata.insert(Msg::METADATA_SYNTHETIC.to_string(), serde_json::json!(true));
        metadata.insert(
            Msg::METADATA_REMINDER_KIND.to_string(),
            serde_json::json!("todo_state"),
        );
        let reminder = Msg::builder()
            .role(MsgRole::User)
            .name("system")
            .text_content(format!(
                "<system-reminder>\nYour current todo list is shown below. This is the \
                 source of truth - do not assume earlier statuses still hold. Keep exactly \
                 one task in_progress. Update the whole list with todo_write as you progress.\n\n{}",
                render(&tasks.tasks)
            ))
            .metadata(metadata)
            .build();
        let mut messages = input.messages;
        messages.push(reminder);
        next(ReasoningInput::new(messages, input.tools, input.options))
    }
}
