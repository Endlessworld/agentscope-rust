//! `AgentSpawnTool` — spawns a registered subagent as a background task and returns its
//! addressing handles, plus the `agent_send` / `agent_list` companions for follow-up turns,
//! mirroring `io.agentscope.harness.agent.tool.AgentSpawnTool` (permission scoping,
//! plan-mode propagation, cancel/end; streaming events and remote targets land later).
//!
//! **Neither `agent_spawn` nor `agent_send` blocks the parent turn.** Both submit the run
//! through the [`TaskRepository`] and return a `task_id` immediately; the child's result is
//! delivered back to the parent as a `<system-reminder>` by the subagent middleware, and
//! `task_output` is the explicit way to wait for it.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::CallableAgent;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, Msg, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

use crate::agent::subagent::manager::DefaultAgentManager;
use crate::agent::subagent::task::background::BackgroundTask;
use crate::agent::subagent::task::repository::TaskRepository;
use crate::agent::subagent::task::spec::TaskRunSpec;
use crate::agent::subagent::task::{task_session_key_from_runtime, RemoteSubmitContext};
use crate::agent::subagent::SubagentDeclaration;

/// A subagent instance spawned by [`AgentSpawnTool`], addressable via its
/// `agent_key` (and optionally its `label`) for follow-up `agent_send` turns.
pub struct SpawnedAgent {
    pub agent_id: String,
    pub session_id: String,
    pub label: Option<String>,
    pub agent: Arc<dyn CallableAgent>,
}

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
    /// Live subagents keyed by `agent_key` (`agent:<agent_id>:<uuid>`).
    live: Mutex<HashMap<String, SpawnedAgent>>,
    /// Canonical (lowercased) label → `agent_key`.
    labels: Mutex<HashMap<String, String>>,
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
            live: Mutex::new(HashMap::new()),
            labels: Mutex::new(HashMap::new()),
        }
    }

    /// Attaches a task repository so spawns and sends are tracked/cancellable.
    ///
    /// Required: both tools run their work as background tasks, so without a
    /// repository they can neither start a run nor deliver its result back.
    pub fn with_task_repository(mut self, repo: Arc<dyn TaskRepository>) -> Self {
        self.tasks = Some(repo);
        self
    }

    /// The attached task repository, when one was configured.
    pub fn task_repository(&self) -> Option<Arc<dyn TaskRepository>> {
        self.tasks.clone()
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

    /// Registers a freshly created subagent under a new `agent_key` (and label),
    /// returning `(agent_key, child_session_id)`.
    fn register_live(
        &self,
        agent_id: &str,
        label: Option<&str>,
        agent: Arc<dyn CallableAgent>,
    ) -> (String, String) {
        let uuid = uuid::Uuid::new_v4();
        let key = format!("agent:{agent_id}:{uuid}");
        let session_id = format!("sub-{uuid}");
        if let Some(l) = label {
            self.labels
                .lock()
                .unwrap()
                .insert(l.to_lowercase(), key.clone());
        }
        self.live.lock().unwrap().insert(
            key.clone(),
            SpawnedAgent {
                agent_id: agent_id.to_string(),
                session_id: session_id.clone(),
                label: label.map(str::to_string),
                agent,
            },
        );
        (key, session_id)
    }

    /// Resolves an `agent_key` from either the explicit key or a `label`.
    fn resolve_key(
        &self,
        agent_key: Option<&str>,
        label: Option<&str>,
    ) -> std::result::Result<String, String> {
        match (agent_key, label) {
            (Some(_), Some(_)) => Err("Error: Provide either agent_key or label, not both.".into()),
            (None, None) => Err("Error: Either agent_key or label is required.".into()),
            (Some(k), None) => Ok(k.trim().to_string()),
            (None, Some(l)) => self
                .labels
                .lock()
                .unwrap()
                .get(&l.trim().to_lowercase())
                .cloned()
                .ok_or_else(|| format!("Error: Unknown label: {l}")),
        }
    }

    /// Snapshot of the live subagents as `(agent_key, agent_id, session_id, label)`.
    pub fn live_agents(&self) -> Vec<(String, String, String, Option<String>)> {
        let mut out: Vec<_> = self
            .live
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.agent_id.clone(),
                    v.session_id.clone(),
                    v.label.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn live_agent(&self, key: &str) -> Option<(String, Arc<dyn CallableAgent>)> {
        self.live
            .lock()
            .unwrap()
            .get(key)
            .map(|s| (s.agent_id.clone(), Arc::clone(&s.agent)))
    }

    /// Next background task id for `kind` (`spawn` / `send`).
    fn next_task_id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.counter.fetch_add(1, Ordering::Relaxed))
    }

    /// A label must be unique across the subagents this tool owns.
    fn label_in_use(&self, label: &str) -> bool {
        self.labels
            .lock()
            .unwrap()
            .contains_key(&label.to_lowercase())
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

    /// A spawn never blocks: the run is submitted as a background task and its result is
    /// delivered back to the parent, so the parent turn continues immediately.
    fn description(&self) -> &str {
        "Spawn a registered subagent with a prompt. The run starts in the background and its \
         reply arrives later as a <system-reminder>; the response carries agent_key / agent_id / \
         session_id / task_id. Pass agent_key verbatim to agent_send for follow-up turns."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string", "description": "Subagent name to spawn"},
                "prompt": {"type": "string", "description": "Task prompt for the subagent"},
                "label": {"type": "string", "description": "Optional human-readable name for addressing this subagent via agent_send"},
                "task_id": {"type": "string", "description": "Optional explicit task id for the background run"}
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
        let label = opt_str(&args, "label")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // Permission scoping + registry presence (PRIMARY-only agents rejected).
        if !self.permitted(&agent_id) {
            return Ok(ToolResultBlock::error(format!(
                "Agent '{agent_id}' is not in the list of spawnable agents."
            )));
        }
        if let Some(l) = &label {
            if self.label_in_use(l) {
                return Ok(ToolResultBlock::error(format!(
                    "Error: Label already in use: {l}"
                )));
            }
        }

        // A declaration carrying a remote URL routes to the agent-protocol transport
        // (Java `runRemoteTask`) instead of an in-process local run.
        let declaration = self.manager.get_declaration(&agent_id);
        let remote = declaration.as_ref().filter(|d| d.is_remote());

        let plan_active = ctx
            .runtime
            .get("plan_mode_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let effective_prompt = self.build_prompt(&prompt, plan_active);
        // `sessionId` is the key every harness tool shares (`TaskTool` reads the
        // same one), so the run lands in the session that queries and delivers it.
        let session_id = task_session_key_from_runtime(&ctx.runtime);

        // Local targets are instantiated eagerly so the spawned instance stays
        // addressable via `agent_key`/`label` for later `agent_send` turns.
        let local_agent = if remote.is_none() {
            match self
                .manager
                .create_agent_if_present(&agent_id, &ctx_runtime(&ctx))
            {
                Some(a) => Some(a),
                None => {
                    return Ok(ToolResultBlock::error(format!(
                        "No spawnable subagent named '{agent_id}' is registered."
                    )))
                }
            }
        } else {
            None
        };

        // The run is always a background task: the repository owns execution and the
        // subagent middleware delivers the result back through a `<system-reminder>`.
        let Some(repo) = self.tasks.clone() else {
            return Ok(ToolResultBlock::error(
                "Error: agent_spawn needs a task repository to run the subagent in the \
                 background; the host did not attach one.",
            ));
        };
        let rt = ctx_runtime(&ctx);
        let task_id = opt_str(&args, "task_id")
            .map(str::to_string)
            .unwrap_or_else(|| self.next_task_id("spawn"));

        let mut header = String::new();
        let spec = if let Some(decl) = remote {
            remote_spec(decl, &agent_id, &effective_prompt)
        } else {
            let exec_agent = local_agent.expect("local agent");
            let (key, child_session) =
                self.register_live(&agent_id, label.as_deref(), Arc::clone(&exec_agent));
            header = format!("{}\n", spawn_header(&key, &agent_id, &child_session));
            TaskRunSpec::Adopted {
                future: tokio::spawn(run_child_turn(exec_agent, effective_prompt)),
            }
        };

        let bg = repo
            .put_task(&rt, &task_id, &agent_id, &session_id, spec)
            .await;

        // Delivery confirmation: a submission the repository never registered cannot run,
        // so reporting it as "started" would silently lose the subagent's work.
        if repo.get_task(&rt, &session_id, &task_id).await.is_none() {
            return Ok(ToolResultBlock::error(format!(
                "Error: subagent '{agent_id}' was not started: task '{task_id}' was not \
                 registered by the task repository."
            )));
        }

        self.active_local
            .lock()
            .unwrap()
            .insert(format!("{session_id}:{task_id}"), bg);

        Ok(ToolResultBlock::text(format!(
            "{header}\ntask_id: {task_id}\nSubagent '{agent_id}' started in the background. Its \
             result is delivered back to you as a <system-reminder>; call \
             task_output('{task_id}') only if you need to wait for it now."
        )))
    }
}

/// Drives one child turn to completion inside its background task.
async fn run_child_turn(agent: Arc<dyn CallableAgent>, prompt: String) -> Result<String> {
    let msg = agent.call(vec![Msg::user(prompt)]).await?;
    Ok(msg.text_content())
}

/// The identity header every spawn response starts with, mirroring the Java
/// `formatSpawnInfo` block (the model copies `agent_key` verbatim into `agent_send`).
fn spawn_header(key: &str, agent_id: &str, session_id: &str) -> String {
    format!("agent_key: {key}\nagent_id: {agent_id}\nsession_id: {session_id}\nstatus: accepted")
}

/// Builds the remote agent-protocol run spec for a remote subagent declaration, mirroring
/// Java `runRemoteTask`'s submission context (streaming/detail/attributes).
fn remote_spec(decl: &SubagentDeclaration, agent_id: &str, input: &str) -> TaskRunSpec {
    let mut builder = RemoteSubmitContext::builder()
        .stream(decl.is_remote_streaming())
        .detail(decl.remote_stream_detail_or_default().wire_value());
    if let Some(attrs) = &decl.remote_context_attributes {
        builder = builder.attributes(attrs.clone());
    }
    TaskRunSpec::Remote {
        base_url: decl.url.clone().unwrap_or_default(),
        headers: decl.headers.clone(),
        agent_id: agent_id.to_string(),
        input: input.to_string(),
        context: Some(builder.build()),
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

/// Sends a follow-up message to a subagent previously spawned by [`AgentSpawnTool`].
pub struct AgentSendTool {
    spawn: Arc<AgentSpawnTool>,
}

impl AgentSendTool {
    pub fn new(spawn: Arc<AgentSpawnTool>) -> Self {
        Self { spawn }
    }
}

#[async_trait]
impl Tool for AgentSendTool {
    fn name(&self) -> &str {
        "agent_send"
    }

    /// A send never blocks: the message is handed to the subagent as a background task
    /// and the reply is delivered back to the parent afterwards.
    fn description(&self) -> &str {
        "Send a follow-up message to a previously spawned subagent, addressed by the \
         agent_key copied verbatim from agent_spawn's output (or by its label). The message \
         is delivered immediately as a background task and the reply arrives later as a \
         <system-reminder>."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_key": {"type": "string", "description": "Full agent_key value from agent_spawn's output (starts with 'agent:')"},
                "label": {"type": "string", "description": "Label set at spawn time (mutually exclusive with agent_key)"},
                "message": {"type": "string", "description": "Follow-up message for the subagent"}
            },
            "required": ["message"]
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(message) = opt_str(&args, "message").map(str::to_string) else {
            return Ok(ToolResultBlock::error("Missing string argument: message"));
        };
        if message.trim().is_empty() {
            return Ok(ToolResultBlock::error(
                "Error: message must not be empty; there is nothing to deliver.",
            ));
        }
        let key = match self
            .spawn
            .resolve_key(opt_str(&args, "agent_key"), opt_str(&args, "label"))
        {
            Ok(k) => k,
            Err(e) => return Ok(ToolResultBlock::error(e)),
        };
        let Some((agent_id, agent)) = self.spawn.live_agent(&key) else {
            return Ok(ToolResultBlock::error(format!(
                "Error: Unknown or expired agent_key: {key}. Use agent_list to see active subagents."
            )));
        };
        let Some(repo) = self.spawn.task_repository() else {
            return Ok(ToolResultBlock::error(
                "Error: agent_send needs a task repository to run the subagent turn in the \
                 background; the host did not attach one.",
            ));
        };

        let rt = ctx_runtime(&ctx);
        let session_id = task_session_key_from_runtime(&ctx.runtime);
        let task_id = self.spawn.next_task_id("send");
        repo.put_task(
            &rt,
            &task_id,
            &agent_id,
            &session_id,
            TaskRunSpec::Adopted {
                future: tokio::spawn(run_child_turn(agent, message)),
            },
        )
        .await;

        // Delivery confirmation: the message counts as delivered only once the repository
        // registered the run. Otherwise the caller would believe the subagent got it while
        // nothing is scheduled and nothing will ever be delivered back.
        if repo.get_task(&rt, &session_id, &task_id).await.is_none() {
            return Ok(ToolResultBlock::error(format!(
                "Error: the message was not delivered to '{key}': task '{task_id}' was not \
                 registered by the task repository."
            )));
        }

        Ok(ToolResultBlock::text(format!(
            "agent_key: {key}\nagent_id: {agent_id}\nstatus: delivered\ntask_id: {task_id}\n\
             The subagent is handling the message in the background; its reply is delivered \
             back to you as a <system-reminder>. Call task_output('{task_id}') only if you \
             need to wait for it now."
        )))
    }
}

/// Lists the subagents spawned in this process, with their addressing handles.
pub struct AgentListTool {
    spawn: Arc<AgentSpawnTool>,
}

impl AgentListTool {
    pub fn new(spawn: Arc<AgentSpawnTool>) -> Self {
        Self { spawn }
    }
}

#[async_trait]
impl Tool for AgentListTool {
    fn name(&self) -> &str {
        "agent_list"
    }

    fn description(&self) -> &str {
        "List the subagents currently spawned by this agent, with the agent_key/label \
         needed to address them via agent_send."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}, "required": []})
    }

    async fn call(&self, _args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let agents = self.spawn.live_agents();
        if agents.is_empty() {
            return Ok(ToolResultBlock::text("No subagents spawned yet."));
        }
        let mut sb = format!("{} spawned subagent(s):\n", agents.len());
        for (key, agent_id, session_id, label) in agents {
            sb.push_str(&format!(
                "- agent_key: {key}  agent_id: {agent_id}  session_id: {session_id}"
            ));
            if let Some(l) = label {
                sb.push_str(&format!("  label: {l}"));
            }
            sb.push('\n');
        }
        Ok(ToolResultBlock::text(sb.trim_end().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_spec_maps_declaration_fields() {
        let mut decl = SubagentDeclaration::new("coder", "desc", "prompt");
        decl.url = Some("http://remote:8080".into());
        decl.headers.insert("X-Auth".into(), "tok".into());
        decl.remote_streaming = Some(false);
        decl.remote_context_attributes = Some(
            [("workspace".to_string(), serde_json::json!("proj"))]
                .into_iter()
                .collect(),
        );

        let spec = remote_spec(&decl, "coder", "do it");
        match spec {
            TaskRunSpec::Remote {
                base_url,
                headers,
                agent_id,
                input,
                context,
            } => {
                assert_eq!(base_url, "http://remote:8080");
                assert_eq!(headers.get("X-Auth").map(String::as_str), Some("tok"));
                assert_eq!(agent_id, "coder");
                assert_eq!(input, "do it");
                let ctx = context.expect("context");
                assert!(!ctx.stream);
                assert_eq!(ctx.detail, "full");
                assert_eq!(
                    ctx.attributes.get("workspace"),
                    Some(&serde_json::json!("proj"))
                );
            }
            _ => panic!("expected remote spec"),
        }
    }

    #[test]
    fn remote_spec_defaults_streaming_and_detail() {
        let mut decl = SubagentDeclaration::new("coder", "desc", "prompt");
        decl.url = Some("http://remote:8080".into());
        let spec = remote_spec(&decl, "coder", "do it");
        match spec {
            TaskRunSpec::Remote { context, .. } => {
                let ctx = context.expect("context");
                assert!(ctx.stream);
                assert_eq!(ctx.detail, "full");
            }
            _ => panic!("expected remote spec"),
        }
    }
}
