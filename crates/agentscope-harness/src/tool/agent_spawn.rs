//! `AgentSpawnTool` — spawns a registered subagent and returns its reply,
//! mirroring `io.agentscope.harness.agent.tool.AgentSpawnTool` (thin but real
//! slice: foreground/background spawn, permission scoping, plan-mode
//! propagation, cancel/end; streaming events and remote targets land later).
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use crate::agent::subagent::manager::DefaultAgentManager;
use crate::agent::subagent::task::background::BackgroundTask;
use crate::agent::subagent::task::repository::TaskRepository;
use crate::agent::subagent::task::spec::TaskRunSpec;

/// Spawns registered subagents as tool invocations.
pub struct AgentSpawnTool {
    manager: Arc<DefaultAgentManager>,
    tasks: Option<Arc<dyn TaskRepository>>,
    /// Empty set = every non-PRIMARY agent may be spawned.
    allowed_agents: HashSet<String>,
    propagate_plan_mode: bool,
    counter: AtomicU64,
    /// Locally spawned background tasks keyed by `session:task_id`, for cancel/end.
    active_local: Mutex<HashMap<String, Arc<BackgroundTask>>>,
}

impl AgentSpawnTool {
    pub fn new(manager: Arc<DefaultAgentManager>) -> Self {
        Self {
            manager,
            tasks: None,
            allowed_agents: HashSet::new(),
            propagate_plan_mode: true,
            counter: AtomicU64::new(0),
            active_local: Mutex::new(HashMap::new()),
        }
    }

    /// Attaches a task repository so background spawns are tracked/cancellable.
    pub fn with_task_repository(mut self, repo: Arc<dyn TaskRepository>) -> Self {
        self.tasks = Some(repo);
        self
    }

    /// Restricts spawnable agent ids (permission scoping). Empty = allow all.
    pub fn with_allowed_agents(mut self, allowed: impl IntoIterator<Item = String>) -> Self {
        self.allowed_agents = allowed.into_iter().collect();
        self
    }

    /// Whether the parent's plan mode is propagated into the child prompt.
    pub fn with_plan_propagation(mut self, enabled: bool) -> Self {
        self.propagate_plan_mode = enabled;
        self
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, tool: Arc<Self>) {
        toolkit.register_tool(tool);
    }

    fn permitted(&self, agent_id: &str) -> bool {
        self.allowed_agents.is_empty() || self.allowed_agents.contains(agent_id)
    }

    /// Cancels a locally spawned background task (`end` semantics). Returns
    /// whether a running task was found and cancelled.
    pub fn end_spawn(&self, session_id: &str, task_id: &str) -> bool {
        let key = format!("{session_id}:{task_id}");
        let task = self.active_local.lock().unwrap().get(&key).cloned();
        match task {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    fn build_prompt(&self, prompt: &str, plan_mode_active: bool) -> String {
        if self.propagate_plan_mode && plan_mode_active {
            format!(
                "<plan-mode-note>The parent agent is currently in PLAN MODE. Research and \
                 draft a plan; do not implement.</plan-mode-note>\n\n{prompt}"
            )
        } else {
            prompt.to_string()
        }
    }
}

fn opt_str<'a>(args: &'a JsonMap, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

#[async_trait]
impl Tool for AgentSpawnTool {
    fn name(&self) -> &str {
        "agent_spawn"
    }

    fn description(&self) -> &str {
        "Spawn a named subagent with a prompt and wait for (foreground) or track \
         (background=true) its result."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string", "description": "Subagent name to spawn"},
                "prompt": {"type": "string", "description": "Task prompt for the subagent"},
                "background": {"type": "boolean", "description": "Run in background and return a task id"},
                "task_id": {"type": "string", "description": "Optional explicit task id for background runs"}
            },
            "required": ["agent", "prompt"]
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(agent_id) = opt_str(&args, "agent").map(str::to_string) else {
            return Ok(ToolResultBlock::error("Missing string argument: agent"));
        };
        let Some(prompt) = opt_str(&args, "prompt").map(str::to_string) else {
            return Ok(ToolResultBlock::error("Missing string argument: prompt"));
        };
        let background = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);

        // Permission scoping + registry presence (PRIMARY-only agents rejected).
        if !self.permitted(&agent_id) {
            return Ok(ToolResultBlock::error(format!(
                "Agent '{agent_id}' is not in the list of spawnable agents."
            )));
        }
        let Some(agent) = self.manager.create_agent_if_present(&agent_id, &ctx_runtime(&ctx)) else {
            return Ok(ToolResultBlock::error(format!(
                "No spawnable subagent named '{agent_id}' is registered."
            )));
        };

        let plan_active = ctx
            .runtime
            .get("plan_mode_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let effective_prompt = self.build_prompt(&prompt, plan_active);
        let session_id = ctx
            .runtime
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();

        // Foreground: run inline and return the reply text.
        if !background || self.tasks.is_none() {
            return match self.manager.invoke_agent(&agent, &effective_prompt).await {
                Ok(msg) => Ok(ToolResultBlock::text(msg.text_content())),
                Err(e) => Ok(ToolResultBlock::error(format!("Subagent '{agent_id}' failed: {e}"))),
            };
        }

        // Background: submit through the task repository (Local run spec).
        let repo = self.tasks.as_ref().unwrap().clone();
        let task_id = opt_str(&args, "task_id")
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!("spawn-{}", self.counter.fetch_add(1, Ordering::Relaxed))
            });

        let exec_agent = agent.clone();
        let execution_prompt = effective_prompt.clone();
        let spec = TaskRunSpec::Local {
            execution: Box::new(move || {
                let handle = tokio::runtime::Handle::current();
                let msg = handle.block_on(
                    exec_agent.call(vec![agentscope_core::message::Msg::user(
                        &execution_prompt,
                    )]),
                )?;
                Ok(msg.text_content())
            }),
        };

        let bg = repo.put_task(&ctx_runtime(&ctx), &task_id, &agent_id, &session_id, spec).await;
        self.active_local
            .lock()
            .unwrap()
            .insert(format!("{session_id}:{task_id}"), bg.clone());

        Ok(ToolResultBlock::text(format!(
            "Subagent '{agent_id}' started in background as task '{task_id}'. Use \
             task_output('{task_id}') / wait_async_results to collect the result."
        )))
    }
}

/// Rebuilds a lightweight RuntimeContext carrying only what tools may see.
fn ctx_runtime(ctx: &ToolExecutionContext) -> agentscope_core::agent::RuntimeContext {
    let mut rt = agentscope_core::agent::RuntimeContext::default();
    for (k, v) in &ctx.runtime {
        rt.metadata.insert(k.clone(), v.clone());
    }
    rt
}
