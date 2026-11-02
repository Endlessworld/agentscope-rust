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
use crate::agent::subagent::{SubagentDeclaration, SubagentSessionRepair};

/// A subagent instance spawned by [`AgentSpawnTool`], addressable via its
/// `agent_key` (and optionally its `label`) for follow-up `agent_send` turns.
pub struct SpawnedAgent {
    pub agent_id: String,
    pub session_id: String,
    pub label: Option<String>,
    pub agent: Arc<dyn CallableAgent>,
    /// Settles a session left unusable by a cancelled turn, when the factory supplies one.
    pub repair: Option<Arc<dyn SubagentSessionRepair>>,
}

/// One subagent exposed as a user-addressable entry point, mirroring Java
/// `SubagentExposedEvent`.
///
/// `subagent_id` is the addressable handle (`agent_key`), which is exactly what a caller passes
/// to a follow-up `agent_send`; `session_id` is the child's own session, not the parent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentExposure {
    pub subagent_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub label: Option<String>,
}

impl SubagentExposure {
    /// The in-band `SubagentExposed` event carrying these fields (Java property names).
    pub fn into_event(self) -> agentscope_core::event::AgentEvent {
        let mut fields = JsonMap::new();
        fields.insert(
            "subagentId".to_string(),
            serde_json::json!(self.subagent_id),
        );
        fields.insert("agentId".to_string(), serde_json::json!(self.agent_id));
        fields.insert("sessionId".to_string(), serde_json::json!(self.session_id));
        if let Some(label) = self.label {
            fields.insert("label".to_string(), serde_json::json!(label));
        }
        agentscope_core::event::AgentEvent::with_fields(
            agentscope_core::event::AgentEventType::SubagentExposed,
            fields,
        )
    }
}

/// Collects the exposures recorded by [`AgentSpawnTool`].
///
/// The tool runs inside a tool call and has no handle on the agent's event stream, so it records
/// the exposure here and the subagent middleware announces it at the next reasoning step — the
/// same point where a background result becomes a `<system-reminder>`.
#[derive(Clone, Default)]
pub struct SubagentExposureSink {
    pending: Arc<Mutex<Vec<SubagentExposure>>>,
}

impl SubagentExposureSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one exposure for the next step.
    pub fn record(&self, exposure: SubagentExposure) {
        self.pending.lock().unwrap().push(exposure);
    }

    /// Takes everything recorded since the last drain, in spawn order.
    pub fn drain(&self) -> Vec<SubagentExposure> {
        std::mem::take(&mut *self.pending.lock().unwrap())
    }
}

/// Spawns registered subagents as tool invocations.
pub struct AgentSpawnTool {
    manager: Arc<DefaultAgentManager>,
    tasks: Option<Arc<dyn TaskRepository>>,
    /// Where freshly exposed subagents are recorded for the in-band `SubagentExposed` event.
    exposure_sink: Option<SubagentExposureSink>,
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
            exposure_sink: None,
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

    /// Records every spawned subagent into `sink`, so the subagent middleware can emit the
    /// in-band `SubagentExposed` event.
    pub fn with_exposure_sink(mut self, sink: SubagentExposureSink) -> Self {
        self.exposure_sink = Some(sink);
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

    /// Registers a freshly created subagent under a new `agent_key` (and label),
    /// returning `(agent_key, child_session_id)`.
    fn register_live(
        &self,
        agent_id: &str,
        label: Option<&str>,
        agent: Arc<dyn CallableAgent>,
        repair: Option<Arc<dyn SubagentSessionRepair>>,
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
                repair,
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

    fn live_agent(
        &self,
        key: &str,
    ) -> Option<(
        String,
        Arc<dyn CallableAgent>,
        Option<Arc<dyn SubagentSessionRepair>>,
    )> {
        self.live
            .lock()
            .unwrap()
            .get(key)
            .map(|s| (s.agent_id.clone(), Arc::clone(&s.agent), s.repair.clone()))
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
                "prompt": {"type": "string", "description": "Task prompt for the subagent. For coding tasks, also include the `<编码指南>` section from your own system prompt (the coding guidelines, verbatim) so the subagent follows the same rules"},
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
                .create_agent_with_repair_if_present(&agent_id, &ctx_runtime(&ctx))
            {
                Some(pair) => Some(pair),
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
            let (exec_agent, repair) = local_agent.expect("local agent");
            let (key, child_session) = self.register_live(
                &agent_id,
                label.as_deref(),
                Arc::clone(&exec_agent),
                repair.clone(),
            );
            // The subagent is now user-addressable, so announce it in-band (Java
            // `AgentSpawnTool#withSubagentExposedEvent`).
            if let Some(sink) = &self.exposure_sink {
                sink.record(SubagentExposure {
                    subagent_id: key.clone(),
                    agent_id: agent_id.clone(),
                    session_id: child_session.clone(),
                    label: label.clone(),
                });
            }
            header = format!("{}\n", spawn_header(&key, &agent_id, &child_session));
            TaskRunSpec::Adopted {
                future: tokio::spawn(run_child_turn(exec_agent, repair, effective_prompt)),
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
async fn run_child_turn(
    agent: Arc<dyn CallableAgent>,
    repair: Option<Arc<dyn SubagentSessionRepair>>,
    prompt: String,
) -> Result<String> {
    match agent.call(vec![Msg::user(prompt.clone())]).await {
        Ok(msg) => Ok(msg.text_content()),
        // A turn cancelled mid-tool leaves an unknown outcome in the journal, which makes every
        // later open of that session fail. Settle it (recording `interrupted`, never success)
        // and replay this prompt once instead of leaving the `agent_key` permanently unusable.
        Err(err) => match repair {
            Some(repair) if is_unreconciled(&err) => {
                repair.repair().await?;
                let msg = agent.call(vec![Msg::user(prompt)]).await?;
                Ok(msg.text_content())
            }
            _ => Err(err),
        },
    }
}

/// Whether a call was refused because the journal still holds an unknown tool outcome.
fn is_unreconciled(error: &agentscope_core::Error) -> bool {
    matches!(
        error,
        agentscope_core::Error::Session(message) if message.contains("reconciliation required")
    )
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
        let Some((agent_id, agent, repair)) = self.spawn.live_agent(&key) else {
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
                future: tokio::spawn(run_child_turn(agent, repair, message)),
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
    use std::sync::atomic::AtomicUsize;

    use agentscope_core::agent::Agent;
    use agentscope_core::formatter::JsonSchema;
    use agentscope_core::state::AgentState;
    use agentscope_core::tool::Toolkit;

    /// A child whose first turn is refused because the journal still holds an unknown tool
    /// outcome (what an aborted turn leaves behind).
    struct UnreconciledOnce {
        calls: AtomicUsize,
        toolkit: Toolkit,
    }

    #[async_trait]
    impl Agent for UnreconciledOnce {
        fn agent_id(&self) -> &str {
            "child"
        }
        fn name(&self) -> &str {
            "child"
        }
        fn description(&self) -> &str {
            "child"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> AgentState {
            AgentState::default()
        }
        fn toolkit(&self) -> &Toolkit {
            &self.toolkit
        }
    }

    #[async_trait]
    impl CallableAgent for UnreconciledOnce {
        async fn call(&self, _messages: Vec<Msg>) -> Result<Msg> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(agentscope_core::Error::Session(
                    "session log: Explicit reconciliation required for unknown tool outcomes: \
                     {\"call-1\"}"
                        .into(),
                ))
            } else {
                Ok(Msg::assistant("recovered"))
            }
        }
        async fn call_with_schema(&self, messages: Vec<Msg>, _schema: JsonSchema) -> Result<Msg> {
            self.call(messages).await
        }
    }

    /// Counts repair invocations so the test can see the hook actually ran.
    struct CountingRepair(Arc<AtomicUsize>);

    impl SubagentSessionRepair for CountingRepair {
        fn repair(&self) -> futures::future::BoxFuture<'_, Result<bool>> {
            let count = Arc::clone(&self.0);
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
        }
    }

    #[tokio::test]
    async fn a_cancelled_session_is_repaired_and_the_prompt_replayed() {
        // Regression: a cancelled subagent turn leaves an unknown tool outcome in its journal.
        // Every later `agent_send` to that `agent_key` then failed with "reconciliation
        // required", so the subagent was permanently unusable.
        let repairs = Arc::new(AtomicUsize::new(0));
        let agent: Arc<dyn CallableAgent> = Arc::new(UnreconciledOnce {
            calls: AtomicUsize::new(0),
            toolkit: Toolkit::new(),
        });

        let out = run_child_turn(
            agent,
            Some(Arc::new(CountingRepair(Arc::clone(&repairs)))),
            "go".to_string(),
        )
        .await
        .expect("the replayed turn must succeed");

        assert_eq!(out, "recovered");
        assert_eq!(repairs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unrelated_failure_is_not_papered_over_by_a_repair() {
        struct AlwaysFails(Toolkit);

        #[async_trait]
        impl Agent for AlwaysFails {
            fn agent_id(&self) -> &str {
                "child"
            }
            fn name(&self) -> &str {
                "child"
            }
            fn description(&self) -> &str {
                "child"
            }
            fn interrupt(&self) {}
            fn agent_state(&self) -> AgentState {
                AgentState::default()
            }
            fn toolkit(&self) -> &Toolkit {
                &self.0
            }
        }

        #[async_trait]
        impl CallableAgent for AlwaysFails {
            async fn call(&self, _messages: Vec<Msg>) -> Result<Msg> {
                Err(agentscope_core::Error::Agent("boom".into()))
            }
            async fn call_with_schema(
                &self,
                messages: Vec<Msg>,
                _schema: JsonSchema,
            ) -> Result<Msg> {
                self.call(messages).await
            }
        }

        let repairs = Arc::new(AtomicUsize::new(0));
        let agent: Arc<dyn CallableAgent> = Arc::new(AlwaysFails(Toolkit::new()));
        let err = run_child_turn(
            agent,
            Some(Arc::new(CountingRepair(Arc::clone(&repairs)))),
            "go".to_string(),
        )
        .await
        .expect_err("a non-journal failure must propagate");

        assert!(err.to_string().contains("boom"), "{err}");
        assert_eq!(repairs.load(Ordering::SeqCst), 0);
    }

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

    /// Child stub for the exposure test: the spawn handshake only needs a callable agent.
    struct EchoChild(Toolkit);

    #[async_trait]
    impl Agent for EchoChild {
        fn agent_id(&self) -> &str {
            "researcher"
        }
        fn name(&self) -> &str {
            "researcher"
        }
        fn description(&self) -> &str {
            "researcher"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> AgentState {
            AgentState::default()
        }
        fn toolkit(&self) -> &Toolkit {
            &self.0
        }
    }

    #[async_trait]
    impl CallableAgent for EchoChild {
        async fn call(&self, _messages: Vec<Msg>) -> Result<Msg> {
            Ok(Msg::assistant("child reply"))
        }
        async fn call_with_schema(&self, messages: Vec<Msg>, _schema: JsonSchema) -> Result<Msg> {
            self.call(messages).await
        }
    }

    struct EchoFactory;

    impl crate::agent::subagent::SubagentFactory for EchoFactory {
        fn create(&self, _rc: &agentscope_core::agent::RuntimeContext) -> Arc<dyn CallableAgent> {
            Arc::new(EchoChild(Toolkit::new()))
        }
    }

    fn echo_entry() -> crate::agent::subagent::SubagentEntry {
        use crate::agent::subagent::{SubagentEntry, SubagentMode};
        let mut entry = SubagentEntry::new("researcher", "sub", Arc::new(EchoFactory));
        entry = entry.with_declaration(SubagentDeclaration {
            name: "researcher".to_string(),
            mode: SubagentMode::Subagent,
            ..SubagentDeclaration::new("researcher", "sub", "")
        });
        entry
    }

    #[tokio::test]
    async fn a_spawned_subagent_is_announced_as_a_subagent_exposed_event() {
        // Regression: Java announces a newly exposed subagent in-band
        // (`AgentSpawnTool#withSubagentExposedEvent`); without it a UI never learns the
        // `agent_key` needed for follow-up `agent_send` turns.
        let dir = std::env::temp_dir().join(format!("as_exposed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs: Arc<dyn crate::filesystem::Filesystem> =
            Arc::new(crate::filesystem::LocalFilesystem::sandboxed(&dir));
        let repo: Arc<dyn TaskRepository> = Arc::new(
            crate::agent::subagent::task::WorkspaceTaskRepository::new(fs, "parent"),
        );

        let sink = SubagentExposureSink::new();
        let tool = AgentSpawnTool::new(Arc::new(DefaultAgentManager::new(&[echo_entry()])))
            .with_task_repository(repo)
            .with_exposure_sink(sink.clone());

        let mut args = JsonMap::new();
        args.insert("agent".to_string(), serde_json::json!("researcher"));
        args.insert("prompt".to_string(), serde_json::json!("hi"));
        args.insert("label".to_string(), serde_json::json!("alpha"));
        let out = tool
            .call(args, ToolExecutionContext::default())
            .await
            .expect("call ok")
            .text_content();
        assert!(out.contains("task_id: "), "spawn failed: {out}");

        // The middleware drains the sink at the next reasoning step.
        let events =
            crate::agent::middleware::subagents::take_subagent_exposed_events(&Some(sink.clone()));
        assert_eq!(
            events.len(),
            1,
            "one spawn must announce exactly one exposure"
        );
        let event = events.into_iter().next().expect("event").expect("ok event");
        assert_eq!(
            event.event_type(),
            agentscope_core::event::AgentEventType::SubagentExposed
        );
        let fields = &event.data().fields;
        assert_eq!(
            fields.get("agentId").and_then(|v| v.as_str()),
            Some("researcher")
        );
        assert_eq!(fields.get("label").and_then(|v| v.as_str()), Some("alpha"));
        let subagent_id = fields
            .get("subagentId")
            .and_then(|v| v.as_str())
            .expect("subagentId");
        assert!(
            subagent_id.starts_with("agent:researcher:"),
            "{subagent_id}"
        );
        assert_eq!(
            fields
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(str::is_empty),
            Some(false)
        );

        // One-shot: the same announcement must not be replayed on the next step.
        assert!(
            crate::agent::middleware::subagents::take_subagent_exposed_events(&Some(sink))
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
