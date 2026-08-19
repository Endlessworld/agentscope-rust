//! Managed subagent mechanism, mirroring `SubagentsMiddleware`.
//!
//! On every reasoning step it prepends rich subagent usage guidance and the current
//! async task summary to the leading SYSTEM message, and pushes newly-terminal
//! background-task results back to the agent as a `<system-reminder>` delivery.
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::event::AgentEvent;
use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock};
use agentscope_core::middleware::{Middleware, Next, ReasoningInput};
use agentscope_core::tool::Tool;
use futures::StreamExt;

use crate::agent::subagent::task::{TaskDelivery, TaskRepository, TaskStatus};
use crate::agent::subagent::SubagentEntry;

/// Cap on individual task results aggregated into one delivery message.
const MAX_DELIVERIES_PER_REMINDER: usize = 10;
/// Cap on in-flight tasks listed in the system summary.
const MAX_TASK_SUMMARY_ENTRIES: usize = 10;

const SUBAGENT_SECTION_TEMPLATE: &str = r#"
## Subagents

You have access to subagent tools for spawning and coordinating isolated subagents.
Subagents are ephemeral — they live only for the duration of the task and return a single result.

### Agent Tools

**`%s`** — Spawn an isolated subagent
- `agent_id` (required): which subagent to instantiate
- `task` (optional): initial prompt; omit to create a persistent session
- `label` (optional): human-readable name for referencing via send
- `timeout_seconds`: wait time; 0=fire-and-forget (returns task_id), default=30, max=600
- Response always includes `agent_key:` (opaque handle) — save it for follow-up sends

**`%s`** — Send a follow-up message to an existing subagent
- `agent_key`: copy the **full value** after `agent_key:` from spawn output (starts with `agent:`). This is NOT `agent_id`, NOT `session_id`, and NOT `task_id`
- Or use `label` if you set one at spawn (mutually exclusive with agent_key)
- `message` (required): content to send

**`%s`** — List active subagents

### Task Tools (for async/background operations)

**`task_output`** — Retrieve the result of a background task by task_id.
- **You rarely need this.** Completed tasks are pushed back to you automatically as a `<system-reminder>` block before your next reasoning step.
- Use `task_output(block=false)` only when you need the full result or to inspect a specific task on demand.

**`task_cancel`** — Cancel a running background task by task_id. No effect on already-completed tasks.

**`task_list`** — List all in-flight background tasks.

### Background task flow
1. Spawn with `timeout_seconds=0` to fire-and-forget; the response gives you a task_id.
2. **Do not poll.** Continue with other work; when the task finishes you'll see a `<system-reminder>` containing its result.
3. If the agent has nothing useful to do, hand control back to the user — the next reasoning round will surface any completions.

### Timeout promotion
When a sync spawn/send exceeds its timeout, the task is **not lost** — it is automatically promoted to a background task. You receive `status: timeout_promoted` with a `task_id`. Do NOT retry the same task.

### Available agent ids
%s

### When to use subagents
- When a task is complex and multi-step, and can be fully delegated in isolation
- When a task is independent of other tasks and can run in parallel
- When a task requires focused reasoning or heavy context usage that would bloat the main thread
- When you only care about the output, not the intermediate steps

### Subagent lifecycle
1. **Spawn** → Provide clear role, instructions, and expected output format
2. **Run** → The subagent completes the task autonomously
3. **Return** → The subagent provides a single structured result
4. **Reconcile** → Incorporate or synthesize the result into the main thread

### Usage patterns
- **Parallel execution**: Launch multiple subagents concurrently with timeout_seconds=0, then collect results with task_output(block=false) after a delay
- **Sync delegation**: Use default timeout for simple one-shot delegation
- **Persistent session**: Spawn without a task, then use send for multi-turn interaction
- **Cancel stale work**: Use task_cancel to stop background tasks that are no longer needed
- Subagent results are NOT visible to the user — always summarize them in your response
"#;

/// Middleware that provides the managed subagent mechanism: subagent usage guidance
/// and task-delivery push to the reasoning loop.
pub struct SubagentsMiddleware {
    entries: Mutex<Vec<SubagentEntry>>,
    task_repository: Arc<dyn TaskRepository>,
    /// Additional agent tools (e.g. `agent_spawn`/`agent_send`/`agent_list`) to register.
    extra_tools: Vec<Arc<dyn Tool>>,
}

impl SubagentsMiddleware {
    pub fn new(
        entries: Vec<SubagentEntry>,
        task_repository: Arc<dyn TaskRepository>,
    ) -> Self {
        Self {
            entries: Mutex::new(entries),
            task_repository,
            extra_tools: Vec::new(),
        }
    }

    /// Adds extra agent tools (spawn/send/list) contributed by this middleware.
    pub fn with_extra_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.extra_tools = tools;
        self
    }

    /// Replaces the registered subagent set (used for dynamic reload).
    pub fn replace_entries(&self, entries: Vec<SubagentEntry>) {
        *self.entries.lock().unwrap() = entries;
    }

    /// Returns the tools this middleware contributes to the agent toolkit.
    pub fn get_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut out = Vec::new();
        out.extend(self.extra_tools.iter().cloned());
        out
    }

    /// The task repository used by this middleware.
    pub fn task_repository(&self) -> &Arc<dyn TaskRepository> {
        &self.task_repository
    }
}

impl Middleware for SubagentsMiddleware {
    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> agentscope_core::middleware::EventStream {
        let entries = self.entries.lock().unwrap().clone();
        if entries.is_empty() {
            return next(input);
        }
        let session = ctx.session_id().unwrap_or("").to_string();
        let repo = Arc::clone(&self.task_repository);
        let rc = ctx.clone();
        let sid = session.clone();
        let fut = async move {
            let pending = repo.find_pending_deliveries(&rc, &sid).await;
            let delivery_msg = if pending.is_empty() {
                None
            } else {
                Some(build_delivery_reminder(&pending))
            };

            let mut addition = String::new();
            addition.push_str(&render_subagent_section(&entries, false));
            if let Some(summary) = build_task_summary(&*repo, &rc).await {
                addition.push_str(&summary);
            }

            let mut rebuilt = prepend_to_system_message(input.messages, &addition);
            if let Some(dm) = delivery_msg.clone() {
                rebuilt.push(dm);
            }

            let downstream =
                next(ReasoningInput::new(rebuilt, input.tools, input.options));
            if pending.is_empty() {
                return downstream;
            }

            // Mark delivered ONLY after the inner reasoning completes successfully.
            let repo2 = Arc::clone(&repo);
            let sid2 = sid.clone();
            let rc2 = rc.clone();
            let tail = async move {
                let mut stream = downstream;
                while let Some(ev) = stream.next().await {
                    let _ = ev;
                }
                for d in &pending {
                    repo2.mark_delivered(&rc2, &sid2, &d.task_id).await;
                }
                Vec::<Result<AgentEvent>>::new()
            };
            Box::pin(
                futures::stream::once(tail)
                    .map(futures::stream::iter)
                    .flatten(),
            )
        };
        Box::pin(futures::stream::once(fut).flatten())
    }
}

/// Builds the single aggregated `<system-reminder>` message surfacing newly-terminal
/// background tasks to the LLM.
pub fn build_delivery_reminder(deliveries: &[TaskDelivery]) -> Msg {
    let total = deliveries.len();
    let shown = total.min(MAX_DELIVERIES_PER_REMINDER);
    let mut sb = String::new();
    sb.push_str("<system-reminder>\n");
    sb.push_str(&format!("{total} background subagent task"));
    sb.push_str(if total == 1 { " has" } else { "s have" });
    sb.push_str(" completed since your last turn. These results are now part of your conversation history.\n\n");
    for d in deliveries.iter().take(shown) {
        let state = state_literal(d.status);
        sb.push_str(&format!("<task id=\"{}\" state=\"{}\"", d.task_id, state));
        if let Some(agent) = &d.agent_id {
            sb.push_str(&format!(" agent=\"{agent}\""));
        }
        sb.push_str(">\n");
        match d.status {
            TaskStatus::Completed => {
                sb.push_str("<task_result>\n");
                sb.push_str(d.result.as_deref().unwrap_or(""));
                sb.push_str("\n</task_result>\n");
            }
            TaskStatus::Failed => {
                sb.push_str("<task_error>\n");
                sb.push_str(d.error_message.as_deref().unwrap_or("(no error message)"));
                sb.push_str("\n</task_error>\n");
            }
            TaskStatus::Cancelled => {
                sb.push_str("Task was cancelled before producing a result.\n");
            }
            other => sb.push_str(&format!("Task ended in non-terminal state {other:?}\n")),
        }
        sb.push_str("</task>\n");
    }
    if total > shown {
        sb.push_str(&format!("\n... and {} more — call task_list() to inspect.\n", total - shown));
    }
    sb.push_str("\nIf you need a single task's full output, call task_output(task_id=..., block=false).\n");
    sb.push_str("</system-reminder>");
    let mut metadata = serde_json::Map::new();
    metadata.insert("synthetic".to_string(), serde_json::Value::Bool(true));
    metadata.insert(
        "reminder_kind".to_string(),
        serde_json::Value::String("task_delivery".to_string()),
    );
    Msg::builder()
        .role(MsgRole::User)
        .name("system")
        .content(vec![ContentBlock::Text(TextBlock::new(sb))])
        .metadata(metadata)
        .build()
}

fn state_literal(status: TaskStatus) -> String {
    match status {
        TaskStatus::Completed => "completed".to_string(),
        TaskStatus::Failed => "error".to_string(),
        TaskStatus::Cancelled => "cancelled".to_string(),
        other => other.as_str().to_lowercase(),
    }
}

/// Appends the given extra content to the leading SYSTEM message of `messages`.
/// If no SYSTEM message is present, a new one is inserted at index 0.
pub fn prepend_to_system_message(messages: Vec<Msg>, extra: &str) -> Vec<Msg> {
    if extra.is_empty() {
        return messages;
    }
    let mut out = Vec::with_capacity(messages.len() + 1);
    let is_first_system =
        messages.first().map(|m| m.role == MsgRole::System).unwrap_or(false);
    if !is_first_system {
        out.push(
            Msg::builder()
                .role(MsgRole::System)
                .name("system")
                .content(vec![ContentBlock::Text(TextBlock::new(extra.to_string()))])
                .build(),
        );
        out.extend(messages);
        return out;
    }
    let sys = messages[0].clone();
    let existing = sys.text_content();
    let merged = if existing.is_empty() {
        extra.to_string()
    } else {
        format!("{existing}\n{extra}")
    };
    out.push(
        Msg::builder()
            .id(sys.id.clone())
            .name(sys.name.clone().unwrap_or_default())
            .role(MsgRole::System)
            .content(vec![ContentBlock::Text(TextBlock::new(merged))])
            .metadata(sys.metadata.clone())
            .timestamp(sys.timestamp)
            .build(),
    );
    out.extend_from_slice(&messages[1..]);
    out
}

/// Renders the `## Subagents` system-prompt section for the supplied entries.
/// Filters out hidden and PRIMARY-only declarations.
pub fn render_subagent_section(entries: &[SubagentEntry], is_session_mode: bool) -> String {
    let agent_list = entries
        .iter()
        .filter(|e| e.is_visible_to_llm())
        .map(|e| format!("- `{}`: {}", e.name, e.description))
        .collect::<Vec<_>>()
        .join("\n");
    let spawn = if is_session_mode { "sessions_spawn" } else { "agent_spawn" };
    let send = if is_session_mode { "sessions_send" } else { "agent_send" };
    let list = if is_session_mode { "sessions_list" } else { "agent_list" };
    SUBAGENT_SECTION_TEMPLATE
        .replacen("%s", spawn, 1)
        .replacen("%s", send, 1)
        .replacen("%s", list, 1)
        .replacen("%s", &agent_list, 1)
}

/// Builds a concise summary of in-flight (undelivered) tasks for the current session,
/// or `None` if none.
pub async fn build_task_summary(repo: &dyn TaskRepository, ctx: &RuntimeContext) -> Option<String> {
    let session = ctx.session_id().unwrap_or("").to_string();
    let tasks = repo.list_tasks(ctx, &session, None).await;
    let mut visible = Vec::new();
    for t in &tasks {
        if repo.is_delivered(ctx, &session, t.task_id()).await {
            continue;
        }
        visible.push(t);
    }
    if visible.is_empty() {
        return None;
    }
    let mut sb = String::from("\n### Async tasks (current session)\n");
    for (count, task) in visible.iter().enumerate() {
        if count >= MAX_TASK_SUMMARY_ENTRIES {
            sb.push_str(&format!(
                "- ... ({} more — use task_list() to see all)\n",
                visible.len() - MAX_TASK_SUMMARY_ENTRIES
            ));
            break;
        }
        sb.push_str(&format!("- task_id: {}", task.task_id()));
        if !task.agent_id().is_empty() {
            sb.push_str(&format!("  agent: {}", task.agent_id()));
        }
        sb.push_str(&format!(
            "  status: {}\n",
            task.get_task_status().as_str().to_lowercase()
        ));
    }
    sb.push_str(
        "(Completed/failed/cancelled tasks are pushed back to you as <system-reminder> blocks, not listed here.)\n",
    );
    Some(sb)
}
