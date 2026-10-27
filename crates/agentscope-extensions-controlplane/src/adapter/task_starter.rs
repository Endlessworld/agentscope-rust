//! Harness-side ASDP AgentTask execution, mirroring
//! `io.agentscope.extensions.controlplane.adapter.{AgentTaskStarter, HarnessAgentTaskStarter}`.
//!
//! The starter owns the physical AgentTask lifecycle: it reads the authoritative task context,
//! acknowledges inputs, ensures the task is running, runs the framework agent until the model
//! submits an explicit business outcome (or the continuation budget is exhausted), and reports
//! the outcome. The model never commits the lifecycle itself.
//!
//! The continuation loop is expressed against [`TurnRunner`] so it can be verified without a
//! control plane or a live model, mirroring the Java `HarnessAgentTaskOutcomeTest` suite.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope_core::agent::{RuntimeContext, StreamOptions};
use agentscope_core::event::{AgentEvent, ConfirmResult};
use agentscope_core::message::{GenerateReason, Msg, MsgRole, ToolUseBlock};
use agentscope_core::tool::Tool;
use agentscope_core::ConfirmHandler;
use agentscope_harness::agent::HarnessAgent;
use serde_json::Value;

use crate::adapter::agent_task::{
    AgentTaskOutcome, AgentTaskOutcomeState, AgentTaskOutcomeTool, AgentTaskToolContext,
    OUTCOME_BLOCKED, OUTCOME_SUCCEEDED, OUTCOME_WAITING, TOOL_SUBMIT_RESULT,
};
use crate::adapter::collaboration_tool::AgentTaskCollaborationTool;
use crate::adapter::support::AgentTaskStarter;
use crate::adapter::workspace_factory::HarnessWorkspaceAgentFactory;
use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::model::AgentTaskAssignment;
use crate::transport::{
    await_runtime_tool_approval, CollaborationClient, RuntimeApprovalSource,
    RUNTIME_APPROVAL_POLL_INTERVAL,
};

/// Credentials that must never reach the model context.
const REDACTED_KEYS: &[&str] = &["taskToken", "attemptToken", "headers", "apiKey", "apiToken"];

/// Continuation limits for one dispatch, mirroring the Java adapter's budget.
#[derive(Debug, Clone, Copy)]
pub struct StarterLimits {
    /// Maximum model turns per dispatch.
    pub max_turns: usize,
    /// Wall-clock budget for the whole dispatch.
    pub budget: Duration,
}

impl Default for StarterLimits {
    fn default() -> Self {
        Self {
            max_turns: 16,
            budget: Duration::from_secs(600),
        }
    }
}

/// A background task observed in the attempt's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTask {
    /// Task id.
    pub id: String,
    /// Whether the task reached a terminal state.
    pub terminal: bool,
    /// Result text once the task completed.
    pub result: Option<String>,
}

/// Framework operations the continuation loop needs, mirroring the Java adapter's use of
/// `HarnessAgent` + `TaskRepository`.
#[async_trait::async_trait]
pub trait TurnRunner: Send + Sync {
    /// Runs one turn and returns the reply message.
    async fn turn(&self, input: Msg) -> Result<Msg, String>;

    /// All tasks currently tracked in the attempt's session.
    async fn pending_tasks(&self) -> Result<Vec<PendingTask>, String>;

    /// One task by id, or `None` when it is not a task in this session.
    async fn get_task(&self, task_id: &str) -> Result<Option<PendingTask>, String>;

    /// The collected output of a terminal task.
    async fn task_output(&self, task_id: &str) -> Result<String, String>;

    /// Waits for the given tasks to converge, up to `timeout`.
    async fn wait_for(&self, task_ids: &[String], timeout: Duration) -> Result<(), String>;

    /// Cancels a task; `Ok(false)` when it was not found.
    async fn cancel_task(&self, task_id: &str) -> Result<bool, String>;

    /// Takes the outcome the model submitted during the last turn.
    async fn take_outcome(&self) -> Option<AgentTaskOutcome>;

    /// Whether a terminal lifecycle action was already committed for this attempt.
    fn is_terminal_committed(&self) -> bool;

    /// Whether this attempt is the Team coordinator.
    fn is_team_leader(&self) -> bool {
        false
    }
}

/// Turns plain text into a user message.
fn message(text: impl Into<String>) -> Msg {
    Msg::builder()
        .role(MsgRole::User)
        .text_content(text)
        .build()
}

/// Runs the continuation loop, mirroring `HarnessAgentTaskStarter.runToOutcome`.
///
/// Returns `None` when a terminal action was already committed (the adapter must not report
/// again), otherwise the business outcome to report.
pub async fn run_to_outcome(
    runner: &dyn TurnRunner,
    kickoff: Msg,
    limits: StarterLimits,
) -> Result<Option<AgentTaskOutcome>, String> {
    let deadline = Instant::now() + limits.budget;
    let mut next = kickoff;
    let mut corrections = 0usize;
    let mut partial = String::new();

    for _ in 0..limits.max_turns {
        if Instant::now() >= deadline {
            break;
        }
        let response = runner.turn(next).await?;
        if runner.is_terminal_committed() {
            return Ok(None);
        }
        let outcome = runner.take_outcome().await;
        let reason = response.generate_reason();
        if !matches!(
            reason,
            GenerateReason::ModelStop | GenerateReason::StructuredOutput
        ) {
            let result = match &outcome {
                Some(o) => o.result.clone(),
                None => partial.clone(),
            };
            return Ok(Some(blocked(
                result,
                format!("runtime_stopped: {}", reason_name(reason)),
            )?));
        }
        let Some(outcome) = outcome else {
            if corrections > 0 {
                return Ok(Some(blocked(
                    partial,
                    "missing_outcome: the agent returned text without submitting an explicit task \
                     outcome",
                )?));
            }
            corrections += 1;
            next = message(
                "The turn ended without a business outcome. Do the remaining work, or call \
                 task.submit_result with blocked and the concrete missing capability. Do not \
                 submit a plan or waiting promise as successful research.",
            );
            continue;
        };
        if !outcome.result.trim().is_empty() {
            partial = outcome.result.clone();
        }
        let tasks = runner.pending_tasks().await?;
        if outcome.outcome == OUTCOME_SUCCEEDED && tasks.iter().any(|t| !t.terminal) {
            if corrections > 0 {
                return Ok(Some(blocked(
                    partial,
                    "uncollected_dependencies: required background tasks are still running",
                )?));
            }
            corrections += 1;
            next = message(
                "There are unfinished background tasks. Collect their actual results or submit \
                 waiting with their real IDs. This AgentTask cannot succeed yet.",
            );
            continue;
        }
        if outcome.outcome != OUTCOME_WAITING {
            return Ok(Some(outcome));
        }
        // Control-plane delegation uses durable AgentTask IDs, not local session tasks. Its
        // waiting endpoint validates actual pending work before accepting the handoff.
        if runner.is_team_leader()
            && outcome
                .pending_task_ids
                .iter()
                .all(|id| !tasks.iter().any(|t| &t.id == id))
        {
            return Ok(Some(outcome));
        }
        let mut pending = Vec::new();
        for id in &outcome.pending_task_ids {
            match runner.get_task(id).await? {
                Some(task) => pending.push(task),
                None => {
                    return Ok(Some(blocked(
                        partial,
                        format!("dependency_not_found: {id} is not a task in this session"),
                    )?))
                }
            }
        }
        // Preserve execution ownership: no gateway turn and no new Issue is created here.
        while pending.iter().any(|t| !t.terminal) {
            if Instant::now() >= deadline {
                return Ok(Some(blocked(
                    partial,
                    "dependency_wait_timeout: background work did not converge within the \
                     execution budget",
                )?));
            }
            runner
                .wait_for(&outcome.pending_task_ids, Duration::from_secs(1))
                .await?;
            let mut refreshed = Vec::new();
            for id in &outcome.pending_task_ids {
                match runner.get_task(id).await? {
                    Some(task) => refreshed.push(task),
                    None => {
                        return Ok(Some(blocked(
                            partial,
                            "dependency_not_found: a waiting task disappeared",
                        )?))
                    }
                }
            }
            pending = refreshed;
        }
        let mut results = String::from(
            "The dependencies have reached terminal states. Validate their contents against the \
             original objective before submitting your outcome.\n",
        );
        for id in &outcome.pending_task_ids {
            results.push_str(&runner.task_output(id).await?);
            results.push('\n');
        }
        next = message(results);
    }

    Ok(Some(blocked(
        partial,
        "execution_budget_exceeded: no validated business outcome within the adapter \
         continuation budget",
    )?))
}

/// Builds a `blocked` outcome, falling back to a generic reason when it is empty.
fn blocked(result: String, reason: impl Into<String>) -> Result<AgentTaskOutcome, String> {
    let reason = reason.into();
    let reason = if reason.trim().is_empty() {
        "the adapter could not validate a business outcome".to_string()
    } else {
        reason
    };
    AgentTaskOutcome::new(OUTCOME_BLOCKED, result, reason, Vec::new())
}

/// Canonical name of a generate reason (Java `GenerateReason` enum names).
fn reason_name(reason: GenerateReason) -> &'static str {
    match reason {
        GenerateReason::ModelStop => "MODEL_STOP",
        GenerateReason::ToolCalls => "TOOL_CALLS",
        GenerateReason::StructuredOutput => "STRUCTURED_OUTPUT",
        GenerateReason::ToolSuspended => "TOOL_SUSPENDED",
        GenerateReason::ReasoningStopRequested => "REASONING_STOP_REQUESTED",
        GenerateReason::ActingStopRequested => "ACTING_STOP_REQUESTED",
        GenerateReason::PermissionAsking => "PERMISSION_ASKING",
        GenerateReason::MiddlewareStopRequested => "MIDDLEWARE_STOP_REQUESTED",
        GenerateReason::AllToolsDenied => "ALL_TOOLS_DENIED",
        GenerateReason::Interrupted => "INTERRUPTED",
        GenerateReason::MaxIterations => "MAX_ITERATIONS",
        GenerateReason::ToolReturnDirect => "TOOL_RETURN_DIRECT",
    }
}

/// Cancels every non-terminal task in the attempt session, mirroring the Java adapter's
/// `finally` block.
pub async fn cancel_unfinished(runner: &dyn TurnRunner) -> Result<(), String> {
    for task in runner.pending_tasks().await? {
        if !task.terminal {
            runner.cancel_task(&task.id).await?;
        }
    }
    Ok(())
}

/// Fails when a required delivery field is missing.
fn require(value: &str, name: &str) -> ControlPlaneResult<()> {
    if value.trim().is_empty() {
        return Err(ControlPlaneError::Unsupported(format!(
            "{name} is required"
        )));
    }
    Ok(())
}

/// Input ids acknowledged in `task.ack`, mirroring `HarnessAgentTaskStarter.inputIds`.
pub fn input_ids(envelope: &Value) -> Vec<String> {
    envelope
        .get("inputs")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("input")
                        .and_then(|i| i.get("id"))
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Collaboration actions the control plane advertises for this dispatch.
pub fn available_actions(envelope: &Value) -> HashSet<String> {
    envelope
        .get("availableActions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Removes credentials from a JSON tree in place, mirroring `stripCredentials`.
pub fn strip_credentials(node: &mut Value) {
    match node {
        Value::Object(map) => {
            for key in REDACTED_KEYS {
                map.remove(*key);
            }
            for child in map.values_mut() {
                strip_credentials(child);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                strip_credentials(item);
            }
        }
        _ => {}
    }
}

/// A credential-free copy of the task context for the model, mirroring `modelContext`.
pub fn model_context(node: &Value) -> Value {
    let mut copy = node.clone();
    strip_credentials(&mut copy);
    copy
}

/// Best-effort credential-free rendering of the dispatch payload, mirroring `safePayload`.
pub fn safe_payload(payload: &str) -> String {
    match serde_json::from_str::<Value>(payload) {
        Ok(value) => model_context(&value).to_string(),
        Err(_) => "[Non-JSON dispatch metadata omitted; use task context above.]".to_string(),
    }
}

/// Team-role instructions injected into the kickoff prompt, mirroring `roleInstructions`.
pub fn role_instructions(envelope: &Value, input_ids: &[String]) -> String {
    let task = envelope.get("task").cloned().unwrap_or(Value::Null);
    let text = |key: &str| {
        task.get(key)
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    if text("teamId").is_empty() {
        return " Complete the requested work and return the result.".to_string();
    }
    if !task
        .get("leaderTask")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return " You are a Team worker, not its coordinator. Do not create or accept child \
                Issues and do not call run.node.complete, run.node.fail, or run.replan. Complete \
                only the assigned work and submit its result using task.submit_result; the \
                adapter will complete this AgentTask."
            .to_string();
    }
    if input_ids.is_empty() {
        return " You are the Team leader's initial task. If you delegate child work, return \
                immediately after issue.child.create succeeds by calling task.submit_result with \
                waiting, a reason and the returned AgentTask IDs; do not wait through local \
                session/task tools and do not call run.node.complete yet. The control plane will \
                deliver a fresh leader follow-up when a worker result arrives. If no work is \
                delegated, call run.node.complete after your own work converges. Returning text \
                alone never completes a Team coordinator."
            .to_string();
    }
    " You are the Team leader resuming with new discussion inputs. Validate the new inputs \
     against the objective, collect real results, and only then call run.node.complete."
        .to_string()
}

/// Builds the kickoff prompt, mirroring `HarnessAgentTaskStarter.execute`.
pub fn kickoff_prompt(
    assignment: &AgentTaskAssignment,
    envelope: &Value,
    input_ids: &[String],
    payload: &str,
    available_tool_names: &[String],
) -> String {
    format!(
        "AgentTask {} is ready. The JSON below contains the authoritative Run input, Issue, \
         discussion inputs, Team role, and artifacts. Complete the requested work and return a \
         concise result; use CollaborationClient for fresh reads, progress comments, artifacts, \
         or child Issues. The available CollaborationClient actions are registered as tools with \
         the exact names shown in availableActions. The adapter owns task.complete and task.fail; \
         do not call them. Before returning, call task.submit_result with an explicit business \
         outcome and the actual deliverable. A promise to do work later is not completion. Check \
         the tool capabilities before delegating: spawning a subagent does not add missing web \
         access.{}contextUrl={}\n{}\nActual tool names available to this agent: {}{}",
        assignment.agent_task_id,
        role_instructions(envelope, input_ids),
        assignment.context_url,
        model_context(envelope),
        available_tool_names.join(", "),
        if payload.trim().is_empty() {
            String::new()
        } else {
            format!("\neventPayload={}", safe_payload(payload))
        }
    )
}

/// Builds the core HITL confirm handler backed by the fenced runtime-approval endpoint.
///
/// Java pauses the agent with `GenerateReason::PERMISSION_ASKING` and resumes it with
/// `Msg.METADATA_CONFIRM_RESULTS` in the next call's metadata. Rust core instead resolves
/// `PermissionBehavior::Ask` through an inline `ConfirmHandler`, so the Java
/// `callWithApprovals` loop maps onto that native seam: the control-plane decision drives
/// allow/deny. Unlike Java (which throws), an unavailable control plane denies the call so a
/// dispatch never aborts on a transient approval outage.
pub fn runtime_approval_confirm_handler(
    source: Arc<dyn RuntimeApprovalSource>,
    task_id: String,
    token: String,
) -> ConfirmHandler {
    Arc::new(move |call: ToolUseBlock| {
        let source = Arc::clone(&source);
        let task_id = task_id.clone();
        let token = token.clone();
        Box::pin(async move {
            let preview = Value::Object(call.input.clone());
            let decision = await_runtime_tool_approval(
                source.as_ref(),
                &task_id,
                &token,
                &call.id,
                &call.name,
                preview,
                RUNTIME_APPROVAL_POLL_INTERVAL,
            )
            .await;
            match decision {
                Ok(decision) if decision.allow => ConfirmResult::allowed(),
                Ok(decision) => ConfirmResult::denied(if decision.deny_message.trim().is_empty() {
                    "Runtime approval denied by the control plane".to_string()
                } else {
                    decision.deny_message
                }),
                Err(err) => ConfirmResult::denied(format!("Runtime approval unavailable: {err}")),
            }
        })
    })
}

/// Runs one `HarnessAgent` turn and returns the reply, extracting the adapter's `AgentResult`
/// reply message from the event stream.
pub async fn run_agent_turn(agent: &HarnessAgent, input: Msg) -> Result<Msg, String> {
    use futures::StreamExt;
    let mut stream = agent.stream_events(vec![input], StreamOptions::defaults());
    let mut reply: Option<Msg> = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(AgentEvent::AgentResult(data)) => {
                if let Some(value) = data.fields.get("reply") {
                    if let Ok(msg) = serde_json::from_value::<Msg>(value.clone()) {
                        reply = Some(msg);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => return Err(format!("agent runtime error: {e}")),
        }
    }
    reply.ok_or_else(|| "agent produced no reply".to_string())
}

/// Default executor for ASDP AgentTask deliveries, mirroring `HarnessAgentTaskStarter`.
pub struct HarnessAgentTaskStarter {
    agent: Arc<dyn Fn() -> HarnessAgent + Send + Sync>,
    collaboration: CollaborationClient,
    workspace_factory: Option<HarnessWorkspaceAgentFactory>,
    limits: StarterLimits,
    accepted_events: Mutex<HashSet<String>>,
}

impl HarnessAgentTaskStarter {
    /// Creates a starter that runs `agent()` for each accepted dispatch.
    pub fn new(
        agent: Arc<dyn Fn() -> HarnessAgent + Send + Sync>,
        collaboration: CollaborationClient,
    ) -> Self {
        Self {
            agent,
            collaboration,
            workspace_factory: None,
            limits: StarterLimits::default(),
            accepted_events: Mutex::new(HashSet::new()),
        }
    }

    /// Installs the workspace factory used to materialize published definitions per attempt.
    pub fn with_workspace_factory(mut self, factory: HarnessWorkspaceAgentFactory) -> Self {
        self.workspace_factory = Some(factory);
        self
    }

    /// Overrides the continuation limits.
    pub fn with_limits(mut self, limits: StarterLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Whether this starter consumes workspace-definition deliveries.
    pub fn consumes_workspace_definition(&self) -> bool {
        self.workspace_factory.is_some()
    }

    /// The version reported by a task envelope, keeping `previous` when absent.
    fn version_of(envelope: &Value, previous: i64) -> i64 {
        envelope
            .get("task")
            .and_then(|t| t.get("version"))
            .and_then(Value::as_i64)
            .unwrap_or(previous)
    }

    /// The `status` reported by a task envelope.
    fn status_of(envelope: &Value) -> Option<&str> {
        envelope
            .get("task")
            .and_then(|t| t.get("status"))
            .and_then(Value::as_str)
    }

    /// Reads the envelope, ensures the task runs, executes the agent and reports the outcome.
    async fn execute(&self, assignment: &AgentTaskAssignment) -> ControlPlaneResult<()> {
        let task_id = assignment.agent_task_id.as_str();
        let token = assignment.task_token.as_str();
        let mut envelope = self.collaboration.task_context(task_id, token).await?;
        let mut version = Self::version_of(&envelope, 0);
        let ids = input_ids(&envelope);
        if !ids.is_empty() {
            self.collaboration
                .acknowledge(task_id, token, ids.clone())
                .await?;
        }
        envelope = self.ensure_running(assignment, &envelope, version).await?;
        version = Self::version_of(&envelope, version);

        let owned_agent = match &self.workspace_factory {
            Some(factory) => {
                let definition = envelope
                    .get("task")
                    .and_then(|t| t.get("runtimeBinding"))
                    .and_then(|b| b.get("definition"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let digest = definition
                    .get("definitionDigest")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !definition.is_object() || digest.is_empty() {
                    return Err(ControlPlaneError::Unsupported(
                        "Dispatch has no immutable Workspace definition".to_string(),
                    ));
                }
                let agent = factory
                    .create(&definition, assignment)
                    .map_err(ControlPlaneError::Unsupported)?;
                self.collaboration
                    .workspace_applied(task_id, token, &digest)
                    .await?;
                Some(agent)
            }
            None => None,
        };
        let agent = match &owned_agent {
            Some(agent) => agent,
            None => &(self.agent)(),
        };
        // `PermissionBehavior::Ask` must resolve through a real control-plane decision; a failed
        // approval request denies instead of aborting the dispatch.
        agent.set_confirm_handler(Some(runtime_approval_confirm_handler(
            Arc::new(self.collaboration.clone()),
            task_id.to_string(),
            token.to_string(),
        )));
        let session_id = if assignment.session_id.trim().is_empty() {
            assignment.agent_task_id.clone()
        } else {
            assignment.session_id.clone()
        };
        let outcome_state = Arc::new(AgentTaskOutcomeState::new());
        register_collaboration_tools(
            agent,
            assignment,
            &available_actions(&envelope),
            &self.collaboration,
            &outcome_state,
        )
        .await;

        let prompt = kickoff_prompt(
            assignment,
            &envelope,
            &ids,
            &String::from_utf8_lossy(&assignment.payload),
            &agent.toolkit().tool_names(),
        );
        let runner = HarnessTurnRunner {
            agent,
            session_id,
            team_leader: envelope
                .get("task")
                .and_then(|t| t.get("leaderTask"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            state: Arc::clone(&outcome_state),
        };

        let result = run_to_outcome(&runner, message(prompt), self.limits).await;
        // Whatever happened, unfinished work must not outlive the dispatch.
        let _ = cancel_unfinished(&runner).await;
        let Some(outcome) = result.map_err(ControlPlaneError::Unsupported)? else {
            return Ok(());
        };
        let latest = self.collaboration.task_context(task_id, token).await?;
        version = Self::version_of(&latest, version);
        let result = if outcome.result.trim().is_empty() {
            Value::Null
        } else {
            Value::String(outcome.result.clone())
        };
        self.collaboration
            .finish(
                task_id,
                token,
                version,
                &outcome.outcome,
                &outcome.reason,
                result,
                ids,
                Vec::new(),
            )
            .await?;
        Ok(())
    }

    /// Issues `task.start` when the task is not running yet, mirroring `ensureRunning`.
    async fn ensure_running(
        &self,
        assignment: &AgentTaskAssignment,
        envelope: &Value,
        expected_version: i64,
    ) -> ControlPlaneResult<Value> {
        if Self::status_of(envelope) == Some("running") {
            return Ok(envelope.clone());
        }
        let task_id = assignment.agent_task_id.as_str();
        let token = assignment.task_token.as_str();
        match self
            .collaboration
            .start(task_id, token, expected_version)
            .await
        {
            Ok(_) => self.collaboration.task_context(task_id, token).await,
            Err(ControlPlaneError::Http(message)) if message.contains("409") => {
                // The ASDP attempt start and this fallback can race; a conflict is success only
                // when a fresh fenced read proves this same task is already running.
                let refreshed = self.collaboration.task_context(task_id, token).await?;
                if Self::status_of(&refreshed) == Some("running") {
                    Ok(refreshed)
                } else {
                    Err(ControlPlaneError::Http(message))
                }
            }
            Err(other) => Err(other),
        }
    }
}

#[async_trait::async_trait]
impl AgentTaskStarter for HarnessAgentTaskStarter {
    async fn start(&self, assignment: AgentTaskAssignment) -> ControlPlaneResult<()> {
        require(&assignment.agent_task_id, "agentTaskId")?;
        require(&assignment.task_token, "taskToken")?;
        let event_key = format!("{}:{}", assignment.attempt_id, assignment.generation);
        {
            let mut accepted = self
                .accepted_events
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !accepted.insert(event_key.clone()) {
                return Ok(());
            }
        }
        let task_id = assignment.agent_task_id.clone();
        let token = assignment.task_token.clone();
        let outcome = self.execute(&assignment).await;
        match outcome {
            Ok(()) => Ok(()),
            Err(error) => {
                let version = self
                    .collaboration
                    .task_context(&task_id, &token)
                    .await
                    .ok()
                    .map(|envelope| Self::version_of(&envelope, 0))
                    .unwrap_or(0);
                let _ = self
                    .collaboration
                    .fail(
                        &task_id,
                        &token,
                        version,
                        "agent_failed",
                        &error.to_string(),
                    )
                    .await;
                self.accepted_events
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&event_key);
                Err(error)
            }
        }
    }

    fn consumes_workspace_definition(&self) -> bool {
        self.consumes_workspace_definition()
    }
}

/// Registers `task.submit_result` and the advertised collaboration tools on the runtime agent,
/// mirroring `registerCollaborationTools`.
async fn register_collaboration_tools(
    agent: &HarnessAgent,
    assignment: &AgentTaskAssignment,
    actions: &HashSet<String>,
    collaboration: &CollaborationClient,
    state: &Arc<AgentTaskOutcomeState>,
) {
    let toolkit = agent.toolkit();
    if !toolkit.tool_names().iter().any(|n| n == TOOL_SUBMIT_RESULT) {
        toolkit.register_tool(Arc::new(AgentTaskOutcomeTool::new(Arc::clone(state))));
    }
    let Ok(body) = collaboration
        .tools(&assignment.agent_task_id, &assignment.task_token)
        .await
    else {
        return;
    };
    let definitions = match &body {
        Value::Array(items) => items.clone(),
        other => other
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    };
    let context = AgentTaskToolContext {
        task_id: assignment.agent_task_id.clone(),
        task_token: assignment.task_token.clone(),
    };
    for definition in definitions {
        let name = definition
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // The starter owns the physical lifecycle; exposing these would race fenced reporting.
        if name.is_empty()
            || crate::adapter::TERMINAL_TOOLS.contains(&name.as_str())
            || !actions.contains(&name)
            || toolkit.tool_names().iter().any(|n| n == &name)
        {
            continue;
        }
        let description = definition
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let parameters = definition
            .get("parameters")
            .or_else(|| definition.get("inputSchema"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
        let tool = AgentTaskCollaborationTool::new(
            collaboration.clone(),
            context.clone(),
            name,
            description,
            parameters,
        )
        .with_outcome_state(Arc::clone(state));
        toolkit.register_tool(Arc::new(tool) as Arc<dyn Tool>);
    }
}

/// [`TurnRunner`] backed by a live `HarnessAgent` and its task repository.
pub struct HarnessTurnRunner<'a> {
    agent: &'a HarnessAgent,
    session_id: String,
    team_leader: bool,
    state: Arc<AgentTaskOutcomeState>,
}

impl HarnessTurnRunner<'_> {
    /// The session this runner executes in.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The runtime context this runner executes with: the assigned session, no completion
    /// callback (the adapter collects results itself) and the Team role flag.
    fn context(&self) -> RuntimeContext {
        let mut ctx = RuntimeContext::new().with_session_id(self.session_id.clone());
        ctx.metadata
            .insert("agentTaskManaged".to_string(), Value::Bool(true));
        ctx.metadata.insert(
            AGENT_TASK_SUPPRESS_COMPLETION_CALLBACK.to_string(),
            Value::Bool(true),
        );
        ctx.metadata
            .insert("teamLeader".to_string(), Value::Bool(self.team_leader));
        ctx
    }
}

/// Harness task-repository metadata key suppressing per-call completion callbacks.
const AGENT_TASK_SUPPRESS_COMPLETION_CALLBACK: &str =
    agentscope_harness::agent::subagent::task::SUPPRESS_COMPLETION_CALLBACK;

#[async_trait::async_trait]
impl TurnRunner for HarnessTurnRunner<'_> {
    async fn turn(&self, input: Msg) -> Result<Msg, String> {
        run_agent_turn(self.agent, input).await
    }

    async fn pending_tasks(&self) -> Result<Vec<PendingTask>, String> {
        let repo = self.agent.task_repository();
        let ctx = self.context();
        Ok(repo
            .list_tasks(&ctx, &self.session_id, None)
            .await
            .into_iter()
            .map(|task| PendingTask {
                id: task.task_id().to_string(),
                terminal: task.get_task_status().is_terminal(),
                result: task.get_result(),
            })
            .collect())
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<PendingTask>, String> {
        let repo = self.agent.task_repository();
        let ctx = self.context();
        Ok(repo
            .get_task(&ctx, &self.session_id, task_id)
            .await
            .map(|task| PendingTask {
                id: task.task_id().to_string(),
                terminal: task.get_task_status().is_terminal(),
                result: task.get_result(),
            }))
    }

    async fn task_output(&self, task_id: &str) -> Result<String, String> {
        Ok(self
            .get_task(task_id)
            .await?
            .and_then(|task| task.result)
            .unwrap_or_default())
    }

    async fn wait_for(&self, task_ids: &[String], timeout: Duration) -> Result<(), String> {
        let repo = self.agent.task_repository();
        let ctx = self.context();
        let timeout_ms = timeout.as_millis() as u64;
        for id in task_ids {
            if let Some(task) = repo.get_task(&ctx, &self.session_id, id).await {
                let _ =
                    tokio::task::spawn_blocking(move || task.wait_for_completion(timeout_ms)).await;
            }
        }
        Ok(())
    }

    async fn cancel_task(&self, task_id: &str) -> Result<bool, String> {
        let repo = self.agent.task_repository();
        let ctx = self.context();
        Ok(repo.cancel_task(&ctx, &self.session_id, task_id).await)
    }

    async fn take_outcome(&self) -> Option<AgentTaskOutcome> {
        self.state.take()
    }

    fn is_terminal_committed(&self) -> bool {
        self.state.is_terminal_committed()
    }

    fn is_team_leader(&self) -> bool {
        self.team_leader
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Scripted [`TurnRunner`] mirroring the Java `HarnessAgentTaskOutcomeTest` doubles.
    struct FakeRunner {
        state: Arc<AgentTaskOutcomeState>,
        tasks: Mutex<Vec<PendingTask>>,
        cancels: Mutex<Vec<String>>,
        inputs: Mutex<Vec<String>>,
        turns: Mutex<usize>,
        team_leader: bool,
        on_turn: Box<dyn Fn(usize, &Msg, &FakeRunner) -> Msg + Send + Sync>,
    }

    impl FakeRunner {
        fn new(on_turn: impl Fn(usize, &Msg, &FakeRunner) -> Msg + Send + Sync + 'static) -> Self {
            Self {
                state: Arc::new(AgentTaskOutcomeState::new()),
                tasks: Mutex::new(Vec::new()),
                cancels: Mutex::new(Vec::new()),
                inputs: Mutex::new(Vec::new()),
                turns: Mutex::new(0),
                team_leader: false,
                on_turn: Box::new(on_turn),
            }
        }

        fn set_tasks(&self, tasks: Vec<PendingTask>) {
            *self.tasks.lock().unwrap_or_else(|p| p.into_inner()) = tasks;
        }

        fn turn_count(&self) -> usize {
            *self.turns.lock().unwrap_or_else(|p| p.into_inner())
        }

        fn cancels(&self) -> Vec<String> {
            self.cancels
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl TurnRunner for FakeRunner {
        async fn turn(&self, input: Msg) -> Result<Msg, String> {
            let index = {
                let mut turns = self.turns.lock().unwrap_or_else(|p| p.into_inner());
                let index = *turns;
                *turns += 1;
                index
            };
            self.inputs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(input.text_content());
            Ok((self.on_turn)(index, &input, self))
        }

        async fn pending_tasks(&self) -> Result<Vec<PendingTask>, String> {
            Ok(self.tasks.lock().unwrap_or_else(|p| p.into_inner()).clone())
        }

        async fn get_task(&self, task_id: &str) -> Result<Option<PendingTask>, String> {
            Ok(self
                .tasks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .find(|task| task.id == task_id)
                .cloned())
        }

        async fn task_output(&self, task_id: &str) -> Result<String, String> {
            Ok(self
                .get_task(task_id)
                .await?
                .and_then(|task| task.result)
                .unwrap_or_default())
        }

        async fn wait_for(&self, _task_ids: &[String], _timeout: Duration) -> Result<(), String> {
            Ok(())
        }

        async fn cancel_task(&self, task_id: &str) -> Result<bool, String> {
            self.cancels
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(task_id.to_string());
            Ok(true)
        }

        async fn take_outcome(&self) -> Option<AgentTaskOutcome> {
            self.state.take()
        }

        fn is_terminal_committed(&self) -> bool {
            self.state.is_terminal_committed()
        }

        fn is_team_leader(&self) -> bool {
            self.team_leader
        }
    }

    fn reply(text: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::Assistant)
            .text_content(text)
            .build()
    }

    fn outcome(outcome: &str, result: &str, reason: &str, pending: &[&str]) -> AgentTaskOutcome {
        AgentTaskOutcome::new(
            outcome,
            result,
            reason,
            pending.iter().map(|s| s.to_string()).collect(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn plain_waiting_promise_cannot_become_successful_completion() {
        let runner = FakeRunner::new(|_, _, _| reply("I will keep checking my subagent"));
        let result = run_to_outcome(&runner, message("kickoff"), StarterLimits::default())
            .await
            .unwrap()
            .expect("an outcome must be reported");
        assert_eq!(runner.turn_count(), 2);
        assert_eq!(result.outcome, OUTCOME_BLOCKED);
        assert!(
            result.reason.contains("missing_outcome"),
            "{}",
            result.reason
        );
        assert_eq!(result.result, "");
    }

    #[tokio::test]
    async fn waits_for_real_dependency_and_resumes_with_actual_result() {
        let runner = FakeRunner::new(|index, input, runner| {
            if index == 0 {
                runner.set_tasks(vec![PendingTask {
                    id: "child".to_string(),
                    terminal: true,
                    result: Some("EV research with evidence".to_string()),
                }]);
                runner.state.submit(outcome(
                    OUTCOME_WAITING,
                    "",
                    "collect EV evidence",
                    &["child"],
                ));
            } else {
                assert!(
                    input.text_content().contains("EV research with evidence"),
                    "the dependency result must be replayed into the continuation: {}",
                    input.text_content()
                );
                runner
                    .state
                    .submit(outcome(OUTCOME_SUCCEEDED, "verified EV report", "", &[]));
            }
            reply("done")
        });
        let result = run_to_outcome(&runner, message("kickoff"), StarterLimits::default())
            .await
            .unwrap()
            .expect("an outcome must be reported");
        assert_eq!(runner.turn_count(), 2);
        assert_eq!(result.outcome, OUTCOME_SUCCEEDED);
        assert_eq!(result.result, "verified EV report");
    }

    #[tokio::test]
    async fn nonexistent_dependency_blocks_instead_of_polling_forever() {
        let runner = FakeRunner::new(|_, _, runner| {
            runner.state.submit(outcome(
                OUTCOME_WAITING,
                "partial",
                "await child",
                &["missing"],
            ));
            reply("waiting")
        });
        let result = run_to_outcome(&runner, message("kickoff"), StarterLimits::default())
            .await
            .unwrap()
            .expect("an outcome must be reported");
        assert_eq!(runner.turn_count(), 1);
        assert_eq!(result.outcome, OUTCOME_BLOCKED);
        assert!(
            result.reason.contains("dependency_not_found"),
            "{}",
            result.reason
        );
        assert_eq!(result.result, "partial");
    }

    #[tokio::test]
    async fn premature_success_is_rejected_and_unfinished_dependency_is_cancelled() {
        let runner = FakeRunner::new(|_, _, runner| {
            runner
                .state
                .submit(outcome(OUTCOME_SUCCEEDED, "premature report", "", &[]));
            reply("done")
        });
        runner.set_tasks(vec![PendingTask {
            id: "child".to_string(),
            terminal: false,
            result: None,
        }]);
        let result = run_to_outcome(&runner, message("kickoff"), StarterLimits::default())
            .await
            .unwrap()
            .expect("an outcome must be reported");
        assert_eq!(runner.turn_count(), 2);
        assert_eq!(result.outcome, OUTCOME_BLOCKED);
        assert!(
            result.reason.contains("uncollected_dependencies"),
            "{}",
            result.reason
        );
        assert_eq!(result.result, "premature report");

        cancel_unfinished(&runner).await.unwrap();
        assert_eq!(runner.cancels(), vec!["child".to_string()]);
    }

    #[tokio::test]
    async fn abnormal_runtime_stop_cannot_commit_submitted_success() {
        let runner = FakeRunner::new(|_, _, runner| {
            runner
                .state
                .submit(outcome(OUTCOME_SUCCEEDED, "partial report", "", &[]));
            Msg::builder()
                .role(MsgRole::Assistant)
                .text_content("stopped")
                .generate_reason(GenerateReason::MaxIterations)
                .build()
        });
        let result = run_to_outcome(&runner, message("kickoff"), StarterLimits::default())
            .await
            .unwrap()
            .expect("an outcome must be reported");
        assert_eq!(runner.turn_count(), 1);
        assert_eq!(result.outcome, OUTCOME_BLOCKED);
        assert!(
            result.reason.contains("MAX_ITERATIONS"),
            "{}",
            result.reason
        );
        assert_eq!(result.result, "partial report");
    }

    #[tokio::test]
    async fn committed_terminal_action_is_not_reported_again() {
        let runner = FakeRunner::new(|_, _, runner| {
            runner.state.mark_terminal_committed();
            reply("committed")
        });
        assert!(
            run_to_outcome(&runner, message("kickoff"), StarterLimits::default())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn exhausted_turn_budget_reports_the_budget_reason() {
        let runner = FakeRunner::new(|_, _, _| reply("thinking"));
        let limits = StarterLimits {
            max_turns: 1,
            budget: Duration::from_secs(30),
        };
        let result = run_to_outcome(&runner, message("kickoff"), limits)
            .await
            .unwrap()
            .expect("an outcome must be reported");
        assert_eq!(result.outcome, OUTCOME_BLOCKED);
        assert!(
            result.reason.contains("execution_budget_exceeded"),
            "{}",
            result.reason
        );
    }

    #[test]
    fn credentials_never_reach_the_model_context() {
        let envelope = serde_json::json!({
            "task": {"id": "task", "taskToken": "secret-token"},
            "taskToken": "secret-token",
            "nested": {"apiKey": "k", "keep": "value"}
        });
        let clean = model_context(&envelope);
        let text = clean.to_string();
        assert!(!text.contains("secret-token"));
        assert!(!text.contains("apiKey"));
        assert!(text.contains("value"));
        assert!(safe_payload("not json").starts_with("[Non-JSON"));
        assert!(!safe_payload(r#"{"taskToken":"secret-token"}"#).contains("secret-token"));
    }

    #[test]
    fn role_instructions_follow_the_team_role() {
        let plain = serde_json::json!({"task": {"teamId": ""}});
        assert!(role_instructions(&plain, &[]).contains("Complete the requested work"));

        let worker = serde_json::json!({"task": {"teamId": "t", "leaderTask": false}});
        assert!(role_instructions(&worker, &[]).contains("Team worker"));

        let leader = serde_json::json!({"task": {"teamId": "t", "leaderTask": true}});
        assert!(role_instructions(&leader, &[]).contains("Team leader's initial task"));
        assert!(role_instructions(&leader, &["i1".to_string()]).contains("resuming with new"));
    }

    #[test]
    fn dispatches_are_extracted_from_the_envelope() {
        let envelope = serde_json::json!({
            "availableActions": ["issue.get", "", "task.get"],
            "inputs": [{"input": {"id": "i1"}}, {"input": {}}]
        });
        assert_eq!(input_ids(&envelope), vec!["i1".to_string()]);
        let actions = available_actions(&envelope);
        assert!(actions.contains("issue.get") && actions.contains("task.get"));
        assert_eq!(actions.len(), 2);
    }

    /// Scripted runtime-approval source, mirroring the Java approval test double.
    struct FakeApprovalSource {
        decisions: Mutex<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl RuntimeApprovalSource for FakeApprovalSource {
        async fn create_runtime_approval(
            &self,
            _task_id: &str,
            _token: &str,
            _request: Value,
        ) -> crate::error::ControlPlaneResult<Value> {
            Ok(serde_json::json!({ "approval": { "id": "appr-1" } }))
        }

        async fn poll_runtime_approval(
            &self,
            _task_id: &str,
            _token: &str,
            _approval_id: &str,
        ) -> crate::error::ControlPlaneResult<Value> {
            let mut decisions = self.decisions.lock().unwrap();
            if decisions.is_empty() {
                return Err(crate::error::ControlPlaneError::Http(
                    "runtime-approval.poll failed: HTTP 503".to_string(),
                ));
            }
            Ok(decisions.remove(0))
        }

        async fn ack_runtime_approval(
            &self,
            _task_id: &str,
            _token: &str,
            _approval_id: &str,
            _decision_version: i64,
        ) -> crate::error::ControlPlaneResult<Value> {
            Ok(serde_json::json!({}))
        }
    }

    fn tool_call(name: &str) -> ToolUseBlock {
        serde_json::from_value(serde_json::json!({
            "type": "tool_use",
            "id": "call-1",
            "name": name,
            "input": { "path": "a.txt" }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn approval_handler_allows_when_the_control_plane_approves() {
        let source = Arc::new(FakeApprovalSource {
            decisions: Mutex::new(vec![
                serde_json::json!({ "decisionVersion": 2, "allow": true }),
            ]),
        });
        let handler = runtime_approval_confirm_handler(source, "task-1".into(), "tok".into());
        let result = handler(tool_call("read_file")).await;
        assert!(result.is_confirmed());
    }

    #[tokio::test]
    async fn approval_handler_denies_with_the_control_plane_message() {
        let source = Arc::new(FakeApprovalSource {
            decisions: Mutex::new(vec![serde_json::json!({
                "decisionVersion": 1,
                "allow": false,
                "denyMessage": "no write access"
            })]),
        });
        let handler = runtime_approval_confirm_handler(source, "task-1".into(), "tok".into());
        let result = handler(tool_call("write_file")).await;
        assert!(!result.is_confirmed());
        assert_eq!(result.message, "no write access");
    }

    #[tokio::test]
    async fn approval_handler_denies_when_the_control_plane_is_unavailable() {
        let source = Arc::new(FakeApprovalSource {
            decisions: Mutex::new(Vec::new()),
        });
        let handler = runtime_approval_confirm_handler(source, "task-1".into(), "tok".into());
        let result = handler(tool_call("execute")).await;
        assert!(!result.is_confirmed());
        assert!(result.message.contains("Runtime approval unavailable"));
    }

    #[test]
    fn kickoff_prompt_carries_roles_context_and_tool_names() {
        let assignment = crate::model::AgentTaskAssignment {
            attempt_id: "attempt".to_string(),
            agent_task_id: "task".to_string(),
            run_id: "run".to_string(),
            node_id: "node".to_string(),
            generation: 1,
            command: "dispatch".to_string(),
            context_url: "http://control/task".to_string(),
            task_token: "secret-token".to_string(),
            attempt_token: "attempt-secret".to_string(),
            session_id: "assigned-session".to_string(),
            payload: Vec::new(),
            timestamp: 1,
        };
        let envelope = serde_json::json!({
            "task": {"status": "running", "version": 4, "teamId": "", "taskToken": "t"}
        });
        let prompt = kickoff_prompt(&assignment, &envelope, &[], "", &["read_file".to_string()]);
        assert!(prompt.contains("AgentTask task is ready"));
        assert!(prompt.contains("contextUrl=http://control/task"));
        assert!(prompt.contains("read_file"));
        assert!(!prompt.contains("secret-token"));
        assert!(!prompt.contains("eventPayload"));
    }
}
