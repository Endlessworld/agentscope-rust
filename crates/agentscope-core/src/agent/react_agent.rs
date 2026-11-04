//! ReAct agent execution loop, mirroring `io.agentscope.core.agent.ReActAgent`.
//!
//! The ReAct agent runs an iterative reasoning → acting loop: it streams a model
//! call, accumulates the reasoning result, and if the model requested tool calls
//! it executes them and feeds the results back, repeating until the model stops
//! or the maximum number of iterations is reached.
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::StreamExt;

use super::agent_traits::{Agent, CallableAgent, ObservableAgent};
use super::lifecycle::{CallGate, ExecutionGuard};
use super::stream_options::StreamOptions;
use crate::agent::run::{
    AgentRun, CancelSignal, RunControl, RunStatus, CANCEL_SIGNAL_CONTEXT_KEY,
    RUN_CONTROL_CONTEXT_KEY,
};
use crate::error::{Error, Result};
use crate::event::{AgentEvent, AgentEventType, ConfirmResult, EventData};
use crate::formatter::{JsonSchema, ResponseFormat};
use crate::interruption::InterruptControl;
use crate::message::{
    ChatUsage, ContentBlock, GenerateReason, JsonMap, Msg, MsgRole, TextBlock, ThinkingBlock,
    ToolResultBlock, ToolResultState, ToolUseBlock,
};
use crate::middleware::{
    ActingInput, ActingResultsHandle, AgentInput, EventStream, ExtensionPoint, MiddlewareChain,
    ModelCallInput, Next, ReasoningInput, TaskReminderMiddleware,
};
use crate::model::{ExecutionConfig, GenerateOptions, Model, ModelUtils};
use crate::permission::{
    PermissionBehavior, PermissionContextState, PermissionDecision, PermissionEngine,
    PermissionRule,
};
use crate::session::{
    outcome, SessionExecution, SessionExportSinkHandle, SessionHistoryMode, SessionKey, SessionLog,
    SessionLogStore, SessionModels, SessionRecorder, RECORDER_CONTEXT_KEY,
    SESSION_EXPORT_SINK_CONTEXT_KEY, TURN_ID_CONTEXT_KEY,
};
use crate::state::context::TaskContextState;
use crate::state::AgentState;
use crate::tool::{RequirementProposeTool, TodoWriteTool, ToolExecutionContext, Toolkit};

/// A boxed event sink registered via [`ObservableAgent::subscribe`].
type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Confirmation handler invoked when a tool call requires user approval
/// (`PermissionBehavior::Ask`).
///
/// The handler receives the tool call that needs confirmation and returns a
/// future resolving to a [`ConfirmResult`]. Typical implementations emit a
/// `RequireUserConfirm` event, wait for the user's response (e.g. via ACP
/// `RequestPermissionRequest`), and return the decision.
///
/// If no handler is set, `Ask` results in an error (backward-compatible default).
pub type ConfirmHandler =
    Arc<dyn Fn(ToolUseBlock) -> Pin<Box<dyn Future<Output = ConfirmResult> + Send>> + Send + Sync>;

/// Default maximum number of reasoning→acting iterations per call.
pub const DEFAULT_MAX_ITERS: i32 = 10;

/// Tool name used for the per-call structured-output `generate_response` tool.
pub const STRUCTURED_OUTPUT_TOOL_NAME: &str = "generate_response";

/// Maximum number of forced retries when the model returns without calling the
/// `generate_response` tool on the structured-output fallback path (Java
/// `soForceToolChoiceCount`).
pub const MAX_STRUCTURED_OUTPUT_FORCE_RETRIES: u32 = 3;

/// Immutable core of a [`ReActAgent`], shared across clones and calls.
struct AgentCore {
    /// The model bound to this agent. Guarded by a mutex so a session can swap
    /// the model at runtime (dynamic model switching) without rebuilding.
    model: Mutex<Arc<dyn Model>>,
    /// Optional fallback model invoked once the primary exhausts its retry budget.
    /// Mirrors Java `ModelConfig.fallbackModel`; `None` leaves the behaviour unchanged.
    fallback_model: Option<Arc<dyn Model>>,
    /// Observer notified when the fallback takes over. Mirrors Java
    /// `ModelConfig.failoverListener`.
    failover_listener: Option<Arc<dyn crate::agent::FailoverListener>>,
    /// Per-call generation defaults (reasoning effort, retry policy, ...)
    /// merged into every model request built by this agent.
    generate_options_defaults: Mutex<GenerateOptions>,
    toolkit: Toolkit,
    sys_prompt: String,
    max_iters: i32,
    /// Whether a permission rejection of any tool call ends the turn instead of being fed
    /// back to the model. Mirrors Java `ReactConfig.stopOnReject` (default `false`).
    stop_on_reject: bool,
    /// Whether the built-in `task_requirement_propose` tool was registered at build time.
    /// Mirrors Java `ReactConfig.taskRequirementsEnabled` (default `false`).
    task_requirements_enabled: bool,
    state: Arc<Mutex<AgentState>>,
    #[allow(dead_code)]
    enable_pending_tool_recovery: bool,
    /// The permission engine evaluating every tool request. Guarded by a mutex so
    /// the session can swap in a new permission context between requests (mode
    /// switches take effect on the next tool call) without rebuilding the agent.
    permission_engine: Mutex<PermissionEngine>,
    interrupt: Arc<InterruptControl>,
    /// The run currently in flight, when one started through [`ReActAgent::call`].
    /// Its own [`InterruptControl`] takes precedence over the agent-level flag so
    /// interrupts are scoped to one execution (mirrors Java's per-`AgentRun` control).
    active_run: Mutex<Option<Arc<RunControl>>>,
    middleware_chain: Arc<MiddlewareChain>,
    call_gate: Arc<CallGate>,
    subscriber: Arc<Mutex<Option<EventSink>>>,
    tasks: Arc<Mutex<TaskContextState>>,
    /// Optional confirmation handler for tool calls that require user approval.
    /// When `Some`, `acting_phase` invokes it on `PermissionBehavior::Ask` instead
    /// of returning an error. The handler should emit a `RequireUserConfirm` event,
    /// wait for the user's decision, and return a `ConfirmResult`.
    ///
    /// Guarded by a mutex so an ACP session can install/swap the handler per
    /// prompt (e.g. bound to the current client connection) at runtime.
    confirm_handler: Mutex<Option<ConfirmHandler>>,
    /// Partial assistant message produced by the most recent model call, recorded
    /// when that call was interrupted mid-stream. `reasoning_phase` appends it to
    /// the context so an interrupted turn keeps what the model had already
    /// produced and the caller's reconciliation can answer its dangling tool_use
    /// blocks. Mirrors Java's `InterruptedException` branch, which adds
    /// `buildFinalMessage()` to the context.
    partial_assistant: Mutex<Option<Msg>>,
    /// Execution-scoped services copied into every call context (session recorder,
    /// caller-supplied action observer, …). Mirrors the Java `RuntimeContext`
    /// attribute map being seeded by the host before an execution.
    extensions: crate::agent::Extensions,
    /// Whether native structured-output responses are validated against the requested
    /// JSON Schema before being accepted (default `true`). Mirrors Java
    /// `validateStructuredOutput`.
    validate_structured_output: bool,
    /// Native session journal backend. When set together with
    /// [`SessionHistoryMode::EventLog`], every call owns a [`SessionRecorder`] and
    /// history is reconstructed from committed events instead of a state store.
    /// Mirrors Java `ReActAgent.sessionLogStore`.
    session_log_store: Option<Arc<dyn SessionLogStore>>,
    /// Agent id used inside the native journal key. Mirrors Java
    /// `sessionLogAgentId` (defaults to the agent name/id).
    session_log_agent_id: String,
    /// Exactly one history authority: state store (legacy) or native event log.
    /// Mirrors Java `ReActAgent.sessionHistoryMode` (default `EVENT_LOG`).
    session_history_mode: SessionHistoryMode,
    /// What a recorder this agent opens itself journals per model call.
    recording_policy: crate::session::RecordingPolicy,
    /// Tool-result metadata keys that exist only for the event stream.
    ///
    /// A transport may attach render-only payload to a result (for example the ACP file
    /// diffs, which carry the whole old/new file content). Such keys are delivered on the
    /// `ToolResultEnd` event and then dropped, so the archived message — and therefore the
    /// journal, every checkpoint and every later request — does not keep a second copy of
    /// data the client already rendered.
    event_only_tool_metadata: Vec<String>,
}

/// RAII guard that finalizes a call's run when the call's future goes away.
///
/// A call that returns normally finalizes its own run; a call whose future is **dropped**
/// (a timeout, an abort, a lost lease) never reaches that code. Without this guard such a
/// call would leave the run stuck in `Running`, the agent-level slot pointing at a dead run,
/// and the journal's writer lease held — blocking the next turn on the same session.
///
/// The guard mirrors the release semantics of Java's `Mono.usingWhen(..., cancelled ->
/// release)`: the release happens on the disposal path, exactly once.
struct RunGuard {
    core: Arc<AgentCore>,
    run: Arc<RunControl>,
    /// The journal this call owns, if any. Released (not finished) on the disposal path:
    /// the terminal facts of an aborted run are the recovery path's business, and writing
    /// them twice would duplicate the `run/end` event.
    journal: Arc<Mutex<Option<Arc<SessionRecorder>>>>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        // The run never reached its own terminal transition: the call was dropped.
        if !self.run.status().is_terminal() {
            self.run.cancel();
        }
        // Clear the agent-level slot only when it still points at *this* run: a concurrent
        // call may have installed its own run in the meantime.
        {
            let mut slot = self.core.active_run.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.run))
            {
                *slot = None;
            }
        }
        // Release the journal's writer lease so the next turn on this session can write.
        // `close` is idempotent, and the run stays unterminated in the journal, which the
        // recovery path reports as an interrupted run.
        let recorder = self.journal.lock().unwrap().take();
        if let Some(recorder) = recorder {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    recorder.close().await;
                });
            }
            // Without a runtime the process is shutting down: the lease is released with
            // the process, so there is nothing left to do.
        }
    }
}

/// Accumulates streamed model chunks into a single assistant message.
#[derive(Debug, Default)]
struct ReasoningContext {
    id: String,
    text: String,
    /// The accumulated thinking block: its text plus the provider metadata
    /// (`encryptedContent` / `thinkingSignature` / `reasoningDetails`). Streaming must not
    /// drop the metadata, or replaying a thinking block after a streamed turn loses it.
    thinking: Option<ThinkingBlock>,
    tool_calls: Vec<ToolUseBlock>,
    tool_results: Vec<ToolResultBlock>,
    usage: Option<ChatUsage>,
    /// Last non-empty finish reason seen on a chunk. Mirrors the Java
    /// `ReasoningContext.finishReason`, which decides whether an empty tool call
    /// start was truncated by the output-token limit.
    finish_reason: Option<String>,
}

impl ReasoningContext {
    /// Merges a streamed chunk into this accumulator.
    fn accumulate(&mut self, chunk: crate::model::ChatResponse) {
        if self.id.is_empty() {
            self.id = chunk.id.clone();
        }
        if chunk.finish_reason.is_some() {
            self.finish_reason = chunk.finish_reason.clone();
        }
        for block in chunk.content {
            match block {
                ContentBlock::Text(t) => self.text.push_str(&t.text),
                ContentBlock::Thinking(t) => self.accumulate_thinking(t),
                ContentBlock::ToolUse(t) => self.tool_calls.push(t),
                ContentBlock::ToolResult(t) => self.tool_results.push(t),
                _ => {}
            }
        }
        if chunk.usage.total_tokens() > 0 {
            self.usage = Some(chunk.usage);
        }
    }

    /// Merges one thinking chunk: text is concatenated, metadata is merged key by key.
    ///
    /// A later chunk wins on a conflicting key, and a chunk without metadata (empty map)
    /// never clears a key an earlier chunk supplied — providers send the payload
    /// (`encryptedContent`, `thinkingSignature`, `reasoningDetails`) on the last chunk only.
    fn accumulate_thinking(&mut self, chunk: ThinkingBlock) {
        if let Some(accumulated) = self.thinking.as_mut() {
            accumulated.thinking.push_str(&chunk.thinking);
            if let Some(metadata) = chunk.metadata.filter(|metadata| !metadata.is_empty()) {
                accumulated
                    .metadata
                    .get_or_insert_with(JsonMap::new)
                    .extend(metadata);
            }
        } else {
            self.thinking = Some(chunk);
        }
    }

    /// Whether the response stopped at its output-token limit.
    ///
    /// OpenAI Chat Completions and DashScope report `length`; Anthropic and Gemini
    /// report `max_tokens` / `MAX_TOKENS`. Mirrors Java `ReasoningContext#isTruncated`.
    fn output_limit_reached(&self) -> bool {
        self.finish_reason.as_deref().is_some_and(|reason| {
            reason.eq_ignore_ascii_case("length") || reason.eq_ignore_ascii_case("max_tokens")
        })
    }

    /// Builds the final assistant message from the accumulated content.
    fn build_final_message(self) -> Msg {
        let output_limit_reached = self.output_limit_reached();
        let mut blocks = Vec::new();
        if let Some(thinking) = self.thinking.filter(|block| {
            !block.thinking.is_empty()
                || block
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| !metadata.is_empty())
        }) {
            blocks.push(ContentBlock::Thinking(thinking));
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text(TextBlock::new(self.text)));
        }
        for mut tc in self.tool_calls {
            finalize_tool_call_arguments(&mut tc, output_limit_reached);
            blocks.push(ContentBlock::ToolUse(tc));
        }
        // Inline provider-side tool results (server tools) travel in the same
        // assistant message, so the turn can be recognized as already finished.
        for tr in self.tool_results {
            blocks.push(ContentBlock::ToolResult(tr));
        }
        let mut b = Msg::builder().role(MsgRole::Assistant).content(blocks);
        if !self.id.is_empty() {
            b = b.id(self.id);
        }
        if let Some(u) = self.usage {
            b = b.usage(u);
        }
        // Persisted on the assistant message so a later acting phase (and a resumed
        // session) can tell "truncated at the output limit" from "corrupt arguments".
        if output_limit_reached {
            b = b.metadata_entry(
                crate::message::metadata_keys::OUTPUT_LIMIT_REACHED,
                serde_json::json!(true),
            );
        }
        b.build()
    }
}

/// Validates the accumulated raw JSON of one tool call and marks it malformed when it
/// never became a JSON object.
///
/// The accumulated `content` is the source of truth: streamed argument fragments may
/// look complete early on, and provider parsers commonly leave `input` empty when the
/// raw text did not parse. Non-empty but invalid content (truncated / corrupt) is
/// preserved in `content` and flagged via [`ToolUseBlock::METADATA_MALFORMED_ARGUMENTS`]
/// so the acting phase refuses to execute it with `{}`.
///
/// Mirrors Java `ToolCallsAccumulator.ToolCallBuilder#build(boolean)`.
fn finalize_tool_call_arguments(tool: &mut ToolUseBlock, output_limit_reached: bool) {
    let raw = tool.content.as_deref().unwrap_or("");
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // Without a completed block or any parsed arguments, an output-limit stop
        // cannot establish that an empty tool-call start was a complete no-arg call.
        if output_limit_reached && tool.input.is_empty() {
            tool.metadata.insert(
                ToolUseBlock::METADATA_MALFORMED_ARGUMENTS.to_string(),
                serde_json::json!(true),
            );
        }
        return;
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Object(parsed)) => {
            // Fill only missing keys: an incremental parser may already have resolved
            // some arguments, and its values win (Java merges the same way).
            for (key, value) in parsed {
                if !tool.input.contains_key(&key) {
                    tool.input.insert(key, value);
                }
            }
        }
        _ => {
            tool.input.clear();
            tool.metadata.insert(
                ToolUseBlock::METADATA_MALFORMED_ARGUMENTS.to_string(),
                serde_json::json!(true),
            );
        }
    }
}

/// A ReAct (Reasoning + Acting) agent.
///
/// Each call runs the reasoning→acting loop against the bound model and toolkit,
/// honouring the middleware chain, permission engine and per-call interrupt.
#[derive(Clone)]
pub struct ReActAgent {
    id: String,
    name: String,
    description: String,
    core: Arc<AgentCore>,
}

impl std::fmt::Debug for ReActAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReActAgent")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish()
    }
}

impl ReActAgent {
    /// Returns the stable unique identifier.
    pub fn agent_id(&self) -> &str {
        &self.id
    }

    /// Returns the display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the system prompt bound to this agent.
    pub fn sys_prompt(&self) -> &str {
        &self.core.sys_prompt
    }

    /// Returns the maximum number of reasoning→acting iterations per call.
    pub fn max_iters(&self) -> i32 {
        self.core.max_iters
    }

    /// Whether a permission rejection of any tool call ends the turn (mirrors Java
    /// `ReActAgent`'s `ReactConfig.stopOnReject`).
    pub fn stop_on_reject(&self) -> bool {
        self.core.stop_on_reject
    }

    /// Whether the built-in `task_requirement_propose` tool is registered on this agent
    /// (mirrors Java `ReactConfig.taskRequirementsEnabled`, default `false`).
    pub fn task_requirements_enabled(&self) -> bool {
        self.core.task_requirements_enabled
    }

    /// Whether orphaned pending tool calls are auto-patched with synthetic error
    /// results before a turn runs (mirrors Java `isPendingToolRecoveryEnabled`).
    pub fn enable_pending_tool_recovery(&self) -> bool {
        self.core.enable_pending_tool_recovery
    }

    /// Whether native structured-output responses are validated against the requested
    /// JSON Schema (mirrors Java `validateStructuredOutput`).
    pub fn validate_structured_output(&self) -> bool {
        self.core.validate_structured_output
    }

    /// Whether native committed history is authoritative for this agent.
    ///
    /// Mirrors Java `ReActAgent.sessionLogEnabled()`: a journal backend must be installed
    /// and the mode must be [`SessionHistoryMode::EventLog`].
    pub fn session_log_enabled(&self) -> bool {
        self.core.session_log_store.is_some()
            && self.core.session_history_mode == SessionHistoryMode::EventLog
    }

    /// The logical journal identity for `context`, or `None` when no journal is installed.
    /// Mirrors Java `ReActAgent.sessionKey(RuntimeContext)`.
    ///
    /// The context wins for each component independently; a component it omits falls back to
    /// the agent state. Both [`ReActAgent::session_key`] and the recorder created during a call
    /// resolve through this one function, so a caller reading the journal can never address a
    /// different session than the one the call wrote.
    pub fn session_key(&self, context: &crate::agent::RuntimeContext) -> Option<SessionKey> {
        self.core.session_log_store.as_ref()?;
        let state = self.core.state.lock().unwrap();
        let session_id = context.session_id().map(str::to_string).or_else(|| {
            let sid = state.session_id().to_string();
            if sid.is_empty() {
                None
            } else {
                Some(sid)
            }
        })?;
        let user_id = context
            .user_id()
            .map(str::to_string)
            .or_else(|| state.user_id().map(str::to_string));
        SessionKey::new(user_id, self.core.session_log_agent_id.clone(), session_id).ok()
    }

    /// Opens the journal for `context`. Mirrors Java `ReActAgent.sessionLog(RuntimeContext)`.
    pub async fn session_log(
        &self,
        context: &crate::agent::RuntimeContext,
    ) -> Result<Arc<dyn SessionLog>> {
        let store = self
            .core
            .session_log_store
            .as_ref()
            .ok_or_else(|| Error::Session("session log is not configured".into()))?;
        let key = self.session_key(context).ok_or_else(|| {
            Error::Session("session identity is incomplete (missing session id)".into())
        })?;
        store.open(&key, context).await
    }

    /// Returns the model bound to this agent.
    pub fn model(&self) -> Arc<dyn Model> {
        self.core.model.lock().unwrap().clone()
    }

    /// Returns the shared, call-scoped agent-state handle.
    ///
    /// Exposes the same `Arc<Mutex<AgentState>>` the runner uses, so host-provided
    /// tools (e.g. plan-mode control tools) can read/update persisted session state
    /// such as `plan_mode_context` across turns. Mirrors Java tools reaching state via
    /// `RuntimeContext.resolveAgentState(...)`.
    pub fn state_handle(&self) -> Arc<Mutex<AgentState>> {
        Arc::clone(&self.core.state)
    }

    /// Replaces the model bound to this agent. The next reasoning call uses the
    /// new model while preserving accumulated conversation state.
    pub fn set_model(&self, model: Arc<dyn Model>) {
        *self.core.model.lock().unwrap() = model;
    }

    /// Sets per-call generation defaults (e.g. reasoning effort, retry policy)
    /// merged into every model request built by this agent.
    pub fn set_generate_options_defaults(&self, options: GenerateOptions) {
        *self.core.generate_options_defaults.lock().unwrap() = options;
    }

    /// Returns the current per-call generation defaults.
    pub fn generate_options_defaults(&self) -> GenerateOptions {
        self.core.generate_options_defaults.lock().unwrap().clone()
    }

    /// Replaces the permission engine with one built from `ctx`.
    ///
    /// The engine is consulted on every tool call, so after a mode switch the
    /// *next* tool call is already evaluated against the new rules. State such as
    /// accumulated conversation history is untouched.
    pub fn set_permission_context(&self, ctx: PermissionContextState) {
        *self.core.permission_engine.lock().unwrap() = PermissionEngine::new(ctx);
    }

    /// Installs (or removes, with `None`) the confirmation handler used when a
    /// tool call resolves to `PermissionBehavior::Ask`.
    pub fn set_confirm_handler(&self, handler: Option<ConfirmHandler>) {
        *self.core.confirm_handler.lock().unwrap() = handler;
    }

    /// Adds a single permission rule to the *current* engine.
    ///
    /// Used e.g. when the user picks "allow always / reject always" in an ACP
    /// permission prompt, so later invocations of the same tool are decided
    /// without asking again. The rule is lost when [`set_permission_context`]
    /// (Self::set_permission_context) swaps in a fresh context on mode changes.
    pub fn add_permission_rule(&self, rule: PermissionRule) {
        self.core.permission_engine.lock().unwrap().add_rule(rule);
    }

    /// Clears the current session's model-visible conversation context without
    /// creating a new session, mirroring `ReActAgent#clearContext`. The session id
    /// and persisted state metadata are preserved; only the message history is reset.
    pub fn clear_context(&self) {
        self.core.state.lock().unwrap().context_mut().clear();
    }

    /// Clears the model-visible context for the session identified by `session_id`.
    /// No-op when the current state belongs to a different session.
    pub fn clear_context_for(&self, session_id: &str) -> bool {
        let mut state = self.core.state.lock().unwrap();
        if state.session_id() != session_id {
            return false;
        }
        state.context_mut().clear();
        true
    }

    /// Interrupts the current session's execution, mirroring `ReActAgent#interrupt`.
    pub fn interrupt(&self) {
        // Prefer the in-flight run's own control so the signal is scoped to one
        // execution; fall back to the agent-level flag for ad-hoc usage.
        if let Some(run) = self.core.active_run.lock().unwrap().as_ref() {
            run.interrupt(None, None);
            return;
        }
        self.core.interrupt.trigger(None, None);
    }

    /// Interrupts `run_id` only, and reports whether that run was the one in flight.
    ///
    /// A plain [`interrupt`](Self::interrupt) targets whatever run is active *at the moment it
    /// is called*. A caller that wants to stop run A but reaches this point after A already
    /// finished (and B started) would stop B instead — the cross-turn interrupt leak that made
    /// a fresh prompt fail with `interrupted`. Passing the run id makes the signal
    /// self-identifying: a stale request is dropped instead of landing on the next turn.
    pub fn interrupt_run(&self, run_id: &str) -> bool {
        let mut active = self.core.active_run.lock().unwrap();
        match active.as_mut() {
            Some(run) if run.run_id() == run_id => {
                run.interrupt(None, None);
                true
            }
            _ => false,
        }
    }

    /// The run currently in flight, if any.
    pub fn active_run(&self) -> Option<AgentRun> {
        self.core
            .active_run
            .lock()
            .unwrap()
            .as_ref()
            .map(|run| AgentRun::from_control(Arc::clone(run)))
    }

    /// Creates a single-use execution handle without starting it, mirroring
    /// `ReActAgent#prepareRun`.
    pub fn prepare_run(&self) -> AgentRun {
        AgentRun::new(self.id.clone())
    }

    /// Interrupts execution only when the current session matches `session_id`,
    /// mirroring the per-session `HarnessAgent#interrupt(userId, sessionId)` overload.
    pub fn interrupt_session(&self, session_id: &str) -> bool {
        let matches = self.core.state.lock().unwrap().session_id() == session_id;
        if matches {
            self.interrupt();
        }
        matches
    }

    /// Creates a new builder.
    pub fn builder() -> ReActAgentBuilder {
        ReActAgentBuilder::default()
    }

    /// Runs the agent against the given input messages and returns the reply.
    pub async fn call(&self, messages: Vec<Msg>) -> Result<Msg> {
        self.call_with_format(messages, None).await
    }

    /// Runs the agent with a caller-held cancellation signal.
    ///
    /// The signal lets the caller stop this execution from another task: the agent observes
    /// it at its next checkpoint and ends the run as cancelled (the reply is never produced).
    /// Mirrors Java's `RunControl` being handed to the call through the runtime context.
    pub async fn call_with_cancel(&self, messages: Vec<Msg>, signal: CancelSignal) -> Result<Msg> {
        let mut extensions = crate::agent::Extensions::new();
        extensions.insert(CANCEL_SIGNAL_CONTEXT_KEY, signal);
        self.call_with_format_and_extensions(messages, None, extensions)
            .await
    }

    /// Internal: runs the full lifecycle (graceful-shutdown guard + per-session
    /// serialization) and returns the reply message.
    async fn call_with_format(
        &self,
        messages: Vec<Msg>,
        response_format: Option<ResponseFormat>,
    ) -> Result<Msg> {
        self.call_with_format_and_extensions(messages, response_format, Default::default())
            .await
    }

    /// Runs one call, merging caller-supplied per-run services into the call context.
    ///
    /// When native committed history is enabled and the caller did **not** already attach a
    /// [`SessionRecorder`], this call owns one for its lifetime: it is opened inside the
    /// per-session gate, attached to the runtime context (so `observe`, tools and middlewares
    /// see it), and finished when the call ends. This is the Rust equivalent of Java
    /// `ReActAgent.acquireCallExecution`, which is why a bare `HarnessAgent::call` journals
    /// without any host-side `AgentSession`.
    async fn call_with_format_and_extensions(
        &self,
        messages: Vec<Msg>,
        response_format: Option<ResponseFormat>,
        extensions: crate::agent::Extensions,
    ) -> Result<Msg> {
        let _guard = ExecutionGuard::acquire()?;
        // Every call runs under its own `RunControl`: fresh interrupt flag, own
        // status machine and own `run_id`. This replaces the agent-level singleton
        // flag (Java models the same thing as a per-`AgentRun` control), so an
        // earlier interrupt cannot wedge the agent and concurrent runs no longer
        // share one signal.
        let run = Arc::new(RunControl::new(self.id.clone(), None));
        run.queue();
        run.start();
        *self.core.active_run.lock().unwrap() = Some(Arc::clone(&run));
        // The run travels in this call's own context, so the interrupt check reads the run that
        // belongs to *this* call instead of the agent-level slot a concurrent call may have
        // replaced.
        let mut extensions = extensions;
        extensions.insert_arc(RUN_CONTROL_CONTEXT_KEY, Arc::clone(&run));
        // The journal handle is shared with the drop guard: when the call's future is dropped
        // (timeout, abort, lost lease) the guard releases the writer lease instead of leaving
        // the session blocked.
        let journal_slot: Arc<Mutex<Option<Arc<SessionRecorder>>>> = Arc::new(Mutex::new(None));
        let _run_guard = RunGuard {
            core: Arc::clone(&self.core),
            run: Arc::clone(&run),
            journal: Arc::clone(&journal_slot),
        };

        // Seed the shared task bag from the persisted state, then restore it after the run
        // so any `todo_write` mutations survive.
        let seed = self.core.state.lock().unwrap().tasks_context.clone();
        *self.core.tasks.lock().unwrap() = seed;
        let key = self.core.state.lock().unwrap().session_id().to_string();
        let gate = Arc::clone(&self.core.call_gate);
        let this = self.clone();
        let run_status = Arc::clone(&run);
        // The journal admits a single execution writer per session. Opening the recorder
        // *inside* the per-session gate keeps concurrent calls on one session from racing for
        // that lease; opening it earlier would make the loser fail instead of queue.
        let result = gate
            .run(&key, async move {
                let mut extensions = extensions;
                // Native history: own a recorder unless the caller already supplied one.
                let owned_recorder = if extensions.contains(RECORDER_CONTEXT_KEY) {
                    None
                } else {
                    this.open_owned_recorder(&mut extensions).await?
                };
                // Record the turn's start facts against the state the call will actually use.
                // The loop appends the input itself, so the pre-call snapshot is the right
                // baseline (Java `acquireCallExecution` records the same facts).
                if let Some(recorder) = &owned_recorder {
                    // Publish the recorder to the drop guard: a call whose future is dropped
                    // must still release the journal's writer lease.
                    *journal_slot.lock().unwrap() = Some(Arc::clone(recorder));
                    let state = this.core.state.lock().unwrap().clone();
                    recorder.start(&messages, &state).await?;
                }

                let (stream, slot) =
                    this.run_stream_with_extensions(messages, response_format, extensions);
                // Forward each event to the subscriber as it is produced: a caller that renders
                // progress (e.g. a background subagent streamed to a client) must not receive the
                // whole run only after the final reply has been produced.
                let mut stream = stream;
                while let Some(item) = stream.next().await {
                    if let Ok(ev) = item {
                        if let Some(sink) = this.core.subscriber.lock().unwrap().as_ref() {
                            sink(ev);
                        }
                    }
                }
                let result = slot.lock().unwrap().take().unwrap_or_else(|| {
                    Err(Error::Agent("agent produced no result message".into()))
                });
                // Persist task-list mutations made by the `todo_write` tool back into state.
                let tasks = this.core.tasks.lock().unwrap().clone();
                this.core.state.lock().unwrap().tasks_context = tasks;

                // Close the turn before releasing the gate so the journal's terminal facts
                // match what the caller observes.
                if let Some(recorder) = owned_recorder {
                    if let Ok(reply) = &result {
                        let _ = recorder.result(Some(reply));
                    }
                    let status = if run_status.is_cancelled() {
                        "cancelled"
                    } else if matches!(result, Err(Error::Cancelled(_))) {
                        "cancelled"
                    } else if matches!(result, Err(Error::Interrupted(_))) {
                        "interrupted"
                    } else if result.is_ok() {
                        "completed"
                    } else {
                        "failed"
                    };
                    // Persist the failure detail with the terminal facts so a host reading
                    // the run receipt reports the real cause, not just "run failed".
                    let detail = result.as_ref().err().map(|error| error.to_string());
                    let state = this.core.state.lock().unwrap().clone();
                    if let Err(error) = recorder
                        .finish_with_error(&state, status, detail.as_deref())
                        .await
                    {
                        // Persistence failures must be visible, never silently downgraded.
                        tracing::warn!("failed to close session journal: {error}");
                    }
                    // The journal is closed; the drop guard must not close it a second time.
                    *journal_slot.lock().unwrap() = None;
                }
                result
            })
            .await;

        // A cancellation must win over the run's own outcome so the terminal status
        // is not reported as `Completed` after `cancel()` was observed.
        let outcome = if run.is_cancelled() {
            RunStatus::Cancelled
        } else if result.is_ok() {
            RunStatus::Completed
        } else {
            RunStatus::Failed
        };
        run.finish(outcome);
        result
    }

    /// Opens a recorder this call owns, attaching it to `extensions`.
    ///
    /// Returns `None` when native history is disabled or the agent has no session identity
    /// yet (a caller that never pinned a session id keeps the stateless behaviour).
    async fn open_owned_recorder(
        &self,
        extensions: &mut crate::agent::Extensions,
    ) -> Result<Option<Arc<SessionRecorder>>> {
        if !self.session_log_enabled() {
            return Ok(None);
        }
        // The turn id may be supplied by a host that wants to address this exact turn
        // (Java `RuntimeContext.put(TURN_ID_KEY, ...)`); otherwise the recorder allocates one.
        let turn_id = extensions
            .get::<String>(TURN_ID_CONTEXT_KEY)
            .map(|id| id.as_str().to_string());
        let Some(key) = self.state_session_key() else {
            return Ok(None);
        };
        let store = Arc::clone(self.core.session_log_store.as_ref().unwrap());
        let context = crate::agent::RuntimeContext::new()
            .with_session_id(key.session_id.clone())
            .with_agent_id(key.agent_id.clone());
        let context = match key.user_id.clone() {
            Some(user_id) => context.with_user_id(user_id),
            None => context,
        };
        let log = store.open(&key, &context).await?;
        // A host opts into committed-event export by installing a sink in the run's
        // extensions (Java `SessionExportSink.CONTEXT_KEY`). Without one this is byte-for-byte
        // the previous path: no exporter, no extra work.
        let sink = extensions
            .get::<SessionExportSinkHandle>(SESSION_EXPORT_SINK_CONTEXT_KEY)
            .or_else(|| {
                self.core
                    .extensions
                    .get::<SessionExportSinkHandle>(SESSION_EXPORT_SINK_CONTEXT_KEY)
            });
        let exporter = sink.map(|handle| {
            Arc::new(handle.exporter_for_session(Arc::clone(&log), key.session_id.clone()))
        });
        let recorder = Arc::new(
            SessionRecorder::open_with_exporter_and_policy(
                log,
                turn_id.as_deref(),
                exporter,
                self.core.recording_policy,
            )
            .await?,
        );
        extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));
        Ok(Some(recorder))
    }

    /// The journal identity derived from the agent's pinned state.
    ///
    /// Delegates to [`ReActAgent::session_key`] with an empty context so the call path and any
    /// reader share one resolution rule.
    fn state_session_key(&self) -> Option<SessionKey> {
        self.session_key(&crate::agent::RuntimeContext::new())
    }

    /// Builds the (middleware-wrapped) event stream for a single agent invocation,
    /// together with a slot the core loop writes its final result into.
    fn run_stream(
        &self,
        messages: Vec<Msg>,
        response_format: Option<ResponseFormat>,
    ) -> (EventStream, Arc<Mutex<Option<Result<Msg>>>>) {
        self.run_stream_with_extensions(messages, response_format, Default::default())
    }

    /// Runs one invocation, merging `extensions` into the call-scoped context.
    ///
    /// This is the seam a session scheduler uses to attach per-run services (session
    /// recorder, inbox command) without leaking them into later runs: the base context is
    /// rebuilt each call and the previous run's services are never inherited.
    pub fn run_stream_with_extensions(
        &self,
        messages: Vec<Msg>,
        response_format: Option<ResponseFormat>,
        extensions: crate::agent::Extensions,
    ) -> (EventStream, Arc<Mutex<Option<Result<Msg>>>>) {
        // Register the execution with the graceful-shutdown manager for *this* path too. The
        // ACP host drives sessions through `stream_events_with_extensions`, not `call`, so
        // without this the shutdown drain sees zero active requests, exits immediately, and
        // leaves a writer lease and an open turn behind.
        let shutdown_guard = ExecutionGuard::acquire().ok();
        let input = AgentInput::new(messages);
        let mut ctx = self.new_context(&input);
        // Layer the per-run services on top of the build-time ones: assigning would silently
        // drop everything the agent was configured with (recorders, observers).
        ctx.extensions.extend(&extensions);
        // Every streamed run needs its own interrupt control, exactly like the `call` path
        // does. Without one, `check_interrupted` falls back to the agent-level flag, which
        // `interrupt()` sets when no run is registered and which nothing ever clears: the
        // first cancel would make *every* later run abort immediately with `Interrupted`.
        // That is the "output stops, then nothing runs again" symptom.
        //
        // When a recorder is attached, its run id is the identity the journal (and therefore
        // the host's task receipt) publishes, so the control must adopt it: otherwise a host
        // that asks to stop "run X" compares against an id nobody else has seen and the
        // signal is silently dropped.
        let recorded_run_id = extensions
            .get::<SessionRecorder>(RECORDER_CONTEXT_KEY)
            .map(|recorder| recorder.run_id().to_string());
        let run = match extensions.get::<RunControl>(RUN_CONTROL_CONTEXT_KEY) {
            Some(run) => run,
            None => {
                let run = Arc::new(RunControl::new(self.id.clone(), recorded_run_id));
                run.queue();
                run.start();
                ctx.extensions
                    .insert_arc(RUN_CONTROL_CONTEXT_KEY, Arc::clone(&run));
                run
            }
        };
        // Drop an interrupt that arrived while nothing was running: the agent-level flag is
        // sticky, so keeping it would abort this run on its first check. Java gives every call
        // a fresh `InterruptControl`, and so does this path.
        if self.core.interrupt.is_interrupted() {
            self.core.interrupt.reset();
        }
        *self.core.active_run.lock().unwrap() = Some(Arc::clone(&run));
        // The run id belongs to *this* call's run, not to whatever run the agent-level slot
        // currently holds (a concurrent call may have replaced it).
        ctx.run_id = Some(run.run_id().to_string());
        ctx.response_format = response_format;
        let slot: Arc<Mutex<Option<Result<Msg>>>> = Arc::new(Mutex::new(None));
        // Shared channel: the model-call loop forwards fine-grained delta events
        // (text/thinking) through `delta_tx`, and the agent-level stream merges them
        // in so callers can render incrementally (边生成边打印).
        let (delta_tx, delta_rx) = futures::channel::mpsc::unbounded::<Result<AgentEvent>>();
        ctx.delta_tx = Some(delta_tx);
        let core = Arc::clone(&self.core);
        let ctx_core = ctx.clone();
        let slot_core = Arc::clone(&slot);
        let run_core = Arc::clone(&run);
        let shutdown_guard_core = shutdown_guard;
        let agent_core: Box<dyn FnOnce(AgentInput) -> EventStream + Send> =
            Box::new(move |input: AgentInput| {
                let fut = async move {
                    // Kept alive for the whole run so the shutdown manager sees this execution.
                    let _shutdown_guard = shutdown_guard_core;
                    let mut events = Vec::new();
                    events.push(Ok(AgentEvent::AgentStart(EventData::default())));
                    let res = run_agent_loop(&core, &ctx_core, input).await;
                    match &res {
                        Ok(msg) => {
                            let mut data = EventData::default();
                            data.fields.insert(
                                "reply".to_string(),
                                serde_json::to_value(msg).unwrap_or_default(),
                            );
                            events.push(Ok(AgentEvent::AgentResult(data)));
                        }
                        Err(e) => {
                            // Surface the failure into the stream so consumers
                            // (e.g. ACP `stream_events`) don't silently swallow it
                            // and report `end_turn` with no events.
                            //
                            // Keep the classification of the terminal states a scheduler must
                            // tell apart. The session runtime maps `Interrupted` to
                            // `run/end{status:"interrupted"}` (a resumable turn) and anything
                            // else to `failed`; flattening every error into `Agent` made an
                            // interruption look like a provider failure. Other kinds stay
                            // `Agent` because only their message is consumed downstream.
                            events.push(Err(match e {
                                Error::Interrupted(reason) => Error::Interrupted(reason.clone()),
                                Error::Cancelled(reason) => Error::Cancelled(reason.clone()),
                                other => Error::Agent(other.to_string()),
                            }));
                        }
                    }
                    *slot_core.lock().unwrap() = Some(res);
                    events.push(Ok(AgentEvent::AgentEnd(EventData::default())));
                    // The run is over: clear the agent-level slot so a later `interrupt()`
                    // cannot land on this finished run (a no-op) instead of the next one.
                    // Only clear it while it still points at *this* run.
                    {
                        let mut active = core.active_run.lock().unwrap();
                        if active
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &run_core))
                        {
                            *active = None;
                        }
                    }
                    futures::stream::iter(events)
                };
                Box::pin(futures::stream::once(fut).flatten())
            });
        let stream = self
            .core
            .middleware_chain
            .wrap_agent(ctx, input, agent_core);
        // Merge the forwarded delta events into the agent-level stream.
        let stream = Box::pin(futures::stream::select(stream, delta_rx));
        (stream, slot)
    }

    /// Streams fine-grained agent events, filtered by the given [`StreamOptions`].
    pub fn stream_events(&self, messages: Vec<Msg>, options: StreamOptions) -> EventStream {
        self.stream_events_with_extensions(messages, options, Default::default())
    }

    /// Streams one invocation with per-run context extensions.
    ///
    /// Used by session schedulers to attach a run-scoped recorder; ordinary callers should
    /// use [`ReActAgent::stream_events`].
    pub fn stream_events_with_extensions(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
        extensions: crate::agent::Extensions,
    ) -> EventStream {
        let (stream, _slot) = self.run_stream_with_extensions(messages, None, extensions);
        Box::pin(stream.filter_map(move |res| {
            let opts = options.clone();
            async move {
                match res {
                    Ok(ev) if opts.should_stream(&ev) => Some(Ok(ev)),
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                }
            }
        }))
    }

    /// Structured-output call: runs the agent with the given JSON schema and stores
    /// the parsed JSON reply in the returned message's structured-output metadata.
    /// Uses the native `response_format` path when the model supports it, otherwise
    /// falls back to the synthetic `generate_response` tool.
    pub async fn call_with_schema(&self, messages: Vec<Msg>, schema: JsonSchema) -> Result<Msg> {
        let has_tools = !self.core.toolkit.tool_schemas().is_empty();
        let model = self.core.model.lock().unwrap().clone();
        let use_native = if has_tools {
            model.supports_native_structured_output_with_tools()
        } else {
            model.supports_native_structured_output()
        };
        if use_native {
            match self
                .call_native_structured(messages.clone(), schema.clone())
                .await
            {
                Ok(reply) => Ok(reply),
                Err(e) => {
                    // A rejected native response (transport error, empty/invalid JSON or
                    // schema-invalid) falls back to the synthetic tool path; the rejected
                    // native reply never entered the context (Java `onErrorResume`).
                    tracing::warn!(
                        "native structured output rejected, falling back to tool path: {e}"
                    );
                    self.call_fallback_structured(messages, schema).await
                }
            }
        } else {
            self.call_fallback_structured(messages, schema).await
        }
    }

    /// Native structured-output path: passes the schema via `response_format` and parses
    /// the model's JSON text reply into structured-output metadata.
    ///
    /// The parsed payload is validated against the schema when
    /// [`ReActAgent::validate_structured_output`] is enabled; empty text, invalid JSON or a
    /// schema mismatch returns an error so the caller can fall back to the tool path.
    async fn call_native_structured(&self, messages: Vec<Msg>, schema: JsonSchema) -> Result<Msg> {
        let reply = self
            .call_with_format(messages, Some(ResponseFormat::json_schema(schema.clone())))
            .await?;
        let text = reply.text_content();
        if text.trim().is_empty() {
            return Err(Error::Agent("Native structured output is empty".into()));
        }
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            Error::Agent(format!("Native structured output is not valid JSON: {e}"))
        })?;
        if self.core.validate_structured_output {
            if let Some(err) =
                crate::tool::ToolValidator::validate_json_output(&parsed, &schema.schema)
            {
                return Err(Error::Agent(format!(
                    "Native structured output failed schema validation: {err}"
                )));
            }
        }
        Ok(reply.with_structured_data(parsed))
    }

    /// Structured-output fallback path: injects a synthetic `generate_response` tool so the
    /// model produces the response via a tool call. Used when the model does not support
    /// native `response_format`. Mirrors the Java `doFallbackStructuredCall`.
    async fn call_fallback_structured(
        &self,
        messages: Vec<Msg>,
        schema: JsonSchema,
    ) -> Result<Msg> {
        let _guard = ExecutionGuard::acquire()?;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));

        // Per-call toolkit that additionally exposes the `generate_response` tool.
        let toolkit = self.core.toolkit.clone();
        let captured_tool = Arc::clone(&captured);
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            STRUCTURED_OUTPUT_TOOL_NAME,
            "Generate the final structured response. Call this function when you have all the information needed to provide a complete answer.",
            generate_response_parameters(&schema),
            Box::new(move |args: JsonMap, _ctx: crate::tool::ToolExecutionContext| {
                let resp = args.get("response").cloned();
                *captured_tool.lock().unwrap() = resp.clone();
                let text = match &resp {
                    Some(v) => serde_json::to_string(v).unwrap_or_default(),
                    None => String::new(),
                };
                Ok(ToolResultBlock::text(text))
            }),
        )));

        {
            let mut live = self.core.state.lock().unwrap();
            for m in messages {
                live.context_mut().push(m);
            }
        }
        // The reminder is request-local: it steers this retry request only and never
        // enters `state.context`, so hooks, middleware and session persistence observe
        // only the caller's real messages (Java `appendTransientSoToolReminder`).
        let so_reminder = structured_output_reminder(&schema);
        // Request preparation happens inside the middleware chain, so it needs the
        // call-scoped identity a normal run would carry. The live agent state stays the
        // source of truth: preparation may rewrite it (overflow compaction), so this path
        // appends its own messages instead of writing back a stale snapshot.
        let history = self.core.state.lock().unwrap().context().to_vec();
        let ctx = self.new_context(&AgentInput::new(history));

        let tools = toolkit.tool_schemas();
        let mut last_text = String::new();
        // The assistant message that produced the structured output. Its identity and
        // token accounting are carried over to the rebuilt reply below.
        let mut so_reply: Option<Msg> = None;
        // Number of retries spent steering the model toward `generate_response` without a
        // tool call. Capped at 3 (Java `soForceToolChoiceCount`) to avoid an endless loop.
        let mut force_retries = 0u32;
        for _ in 0..self.core.max_iters {
            let mut msgs = self.core.state.lock().unwrap().context().to_vec();
            // Append the transient reminder to the request-local copy only.
            msgs.push(so_reminder.clone());
            let model = self.core.model.lock().unwrap().clone();
            let mut options = GenerateOptions::builder()
                .model_name(model.model_name())
                .build();
            // From the first retry on, force the tool when the provider supports it.
            if force_retries > 0 && model.supports_tool_choice_specific() {
                options.tool_choice = Some(crate::model::ToolChoice::specific(
                    STRUCTURED_OUTPUT_TOOL_NAME,
                ));
            }
            let input = ModelCallInput::new(msgs, tools.clone(), options, Arc::clone(&model));
            // The reminder and the `generate_response` schema travel *into* preparation,
            // so the prepared request is the one the provider actually receives
            // (Java 51a74e2ef: append reminder/schema -> prepare -> model).
            let msg = model_call_through_chain(&self.core, &ctx, input).await?;
            self.core
                .state
                .lock()
                .unwrap()
                .context_mut()
                .push(msg.clone());

            let mut gen_called = false;
            for tc in msg.tool_use_blocks() {
                if tc.name == STRUCTURED_OUTPUT_TOOL_NAME {
                    // No delta channel here: the structured-output retry path is not
                    // streamed, so the emitter (still carrying the call id) discards.
                    let tctx = tool_execution_context(tc, Default::default(), &None, None, "");
                    if let Ok(rb) = toolkit.call_tool(tc, tctx).await {
                        self.core.state.lock().unwrap().context_mut().push(
                            Msg::builder()
                                .role(MsgRole::Tool)
                                .content(vec![ContentBlock::ToolResult(rb)])
                                .build(),
                        );
                    }
                    gen_called = true;
                }
            }
            if gen_called {
                so_reply = Some(msg.clone());
                break;
            }
            // The model finished without calling `generate_response`.
            // Keep the first non-empty text seen so a lenient final parse is still possible
            // (existing behaviour), then force a retry (up to 3) with a forced tool choice.
            if last_text.is_empty() {
                last_text = msg.text_content();
            }
            if msg.tool_use_blocks().next().is_none() {
                force_retries += 1;
                if force_retries > MAX_STRUCTURED_OUTPUT_FORCE_RETRIES {
                    tracing::warn!(
                        "structured output: model did not call `generate_response` after {} forced retries; returning unstructured reply",
                        MAX_STRUCTURED_OUTPUT_FORCE_RETRIES
                    );
                    break;
                }
            }
            // Other (non-structured) tool calls: keep looping, bounded by `max_iters`.
        }

        let structured = captured.lock().unwrap().clone().or_else(|| {
            let t = last_text.trim();
            if t.is_empty() {
                None
            } else {
                serde_json::from_str(t).ok()
            }
        });
        let text = match &structured {
            Some(v) => serde_json::to_string(v).unwrap_or_default(),
            None => last_text,
        };
        let mut builder = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Text(TextBlock::new(text))]);
        // Rebuilding the reply must not mint a fresh id/timestamp nor drop the token
        // usage: those belong to the original reply (Java `extractResponseData`, commit
        // c2ff087ca). Only reached once the `generate_response` tool actually ran
        // (Java's `soCompleted` guard).
        if let Some(src) = &so_reply {
            builder = builder.id(src.id()).timestamp(src.timestamp());
            if let Some(usage) = src.usage() {
                builder = builder.usage(*usage);
            }
        }
        let mut reply = builder.build();
        if let Some(s) = structured {
            reply = reply.with_structured_data(s);
        }
        Ok(reply)
    }

    fn new_context(&self, input: &AgentInput) -> crate::agent::RuntimeContext {
        // Call-scoped identity is taken from the agent state so that harness
        // middlewares (inbox, async tools, subagent tasks) and the tools they
        // register can scope their work to the current session/user.
        let (session_id, user_id) = {
            let st = self.core.state.lock().unwrap();
            let sid = st.session_id().to_string();
            (
                if sid.is_empty() { None } else { Some(sid) },
                st.user_id().map(str::to_string),
            )
        };
        crate::agent::RuntimeContext {
            agent_name: Some(self.name.clone()),
            user_input: input.messages.last().map(|m| m.text_content()),
            memory: input.messages.clone(),
            toolkit: Some(self.core.toolkit.clone()),
            metadata: Default::default(),
            cwd: None,
            response_format: None,
            session_id,
            user_id,
            agent_id: Some(self.id.clone()),
            // Identifies this execution; set by `call`, absent for ad-hoc contexts.
            run_id: self
                .core
                .active_run
                .lock()
                .unwrap()
                .as_ref()
                .map(|run| run.run_id().to_string()),
            delta_tx: None,
            // Expose the shared state so context-managing middlewares (e.g.
            // conversation compaction) can read/rewrite the accumulated history.
            state: Some(Arc::clone(&self.core.state)),
            extensions: self.core.extensions.clone(),
        }
    }
}

impl Agent for ReActAgent {
    fn agent_id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn interrupt(&self) {
        ReActAgent::interrupt(self);
    }
    fn agent_state(&self) -> AgentState {
        self.core.state.lock().unwrap().clone()
    }
    fn toolkit(&self) -> &Toolkit {
        &self.core.toolkit
    }
}

#[async_trait]
impl CallableAgent for ReActAgent {
    async fn call(&self, messages: Vec<Msg>) -> Result<Msg> {
        ReActAgent::call(self, messages).await
    }

    async fn call_with_cancel(&self, messages: Vec<Msg>, signal: CancelSignal) -> Result<Msg> {
        ReActAgent::call_with_cancel(self, messages, signal).await
    }

    async fn call_with_schema(&self, messages: Vec<Msg>, schema: JsonSchema) -> Result<Msg> {
        ReActAgent::call_with_schema(self, messages, schema).await
    }
}

impl ObservableAgent for ReActAgent {
    fn subscribe(&self, sink: Box<dyn Fn(AgentEvent) + Send + Sync>) {
        *self.core.subscriber.lock().unwrap() = Some(Arc::new(sink));
    }

    fn unsubscribe(&self) {
        *self.core.subscriber.lock().unwrap() = None;
    }
}

/// Builder for [`ReActAgent`].
pub struct ReActAgentBuilder {
    id: String,
    name: String,
    description: String,
    model: Option<Arc<dyn Model>>,
    fallback_model: Option<Arc<dyn Model>>,
    failover_listener: Option<Arc<dyn crate::agent::FailoverListener>>,
    toolkit: Option<Toolkit>,
    sys_prompt: String,
    max_iters: i32,
    stop_on_reject: bool,
    task_requirements_enabled: bool,
    middleware_chain: Option<MiddlewareChain>,
    state: Option<AgentState>,
    enable_pending_tool_recovery: bool,
    permission_context: Option<PermissionContextState>,
    enable_todo_tool: bool,
    confirm_handler: Option<ConfirmHandler>,
    validate_structured_output: bool,
    extensions: crate::agent::Extensions,
    session_log_store: Option<Arc<dyn SessionLogStore>>,
    session_log_agent_id: Option<String>,
    session_history_mode: SessionHistoryMode,
    recording_policy: crate::session::RecordingPolicy,
    event_only_tool_metadata: Vec<String>,
}

impl Default for ReActAgentBuilder {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            model: None,
            fallback_model: None,
            failover_listener: None,
            toolkit: None,
            sys_prompt: String::new(),
            max_iters: 0,
            stop_on_reject: false,
            task_requirements_enabled: false,
            middleware_chain: None,
            state: None,
            enable_pending_tool_recovery: false,
            permission_context: None,
            enable_todo_tool: true,
            confirm_handler: None,
            validate_structured_output: true,
            extensions: crate::agent::Extensions::new(),
            session_log_store: None,
            session_log_agent_id: None,
            session_history_mode: SessionHistoryMode::EventLog,
            recording_policy: crate::session::RecordingPolicy::default(),
            event_only_tool_metadata: Vec::new(),
        }
    }
}

impl ReActAgentBuilder {
    /// Creates a new builder with a randomly generated agent id.
    pub fn new() -> Self {
        Self::default().id(uuid::Uuid::new_v4().to_string().replace('-', ""))
    }

    /// Sets the stable agent id.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }
    /// Sets the display name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
    /// Sets the description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }
    /// Sets the backing model (required).
    pub fn model(mut self, model: Arc<dyn Model>) -> Self {
        self.model = Some(model);
        self
    }
    /// Sets the fallback model, invoked once the primary exhausts its retry budget.
    ///
    /// The fallback gets the same retry budget of its own. Mirrors Java
    /// `ReActAgent.Builder.fallbackModel(Model)`.
    pub fn fallback_model(mut self, model: Arc<dyn Model>) -> Self {
        self.fallback_model = Some(model);
        self
    }
    /// Sets the observer notified when the fallback takes over. Mirrors Java
    /// `ReActAgent.Builder.failoverListener`.
    pub fn failover_listener(mut self, listener: Arc<dyn crate::agent::FailoverListener>) -> Self {
        self.failover_listener = Some(listener);
        self
    }
    /// Sets the toolkit used for tool execution.
    pub fn toolkit(mut self, toolkit: Toolkit) -> Self {
        self.toolkit = Some(toolkit);
        self
    }
    /// Attaches an execution-scoped service to every call context, e.g. a session
    /// recorder or a caller-supplied action observer.
    pub fn extension<T: std::any::Any + Send + Sync>(
        mut self,
        key: &'static str,
        value: T,
    ) -> Self {
        self.extensions.insert(key, value);
        self
    }
    /// Attaches an already-shared execution-scoped service.
    pub fn extension_arc<T: std::any::Any + Send + Sync>(
        mut self,
        key: &'static str,
        value: Arc<T>,
    ) -> Self {
        self.extensions.insert_arc(key, value);
        self
    }
    /// Sets the system prompt prepended to every reasoning call.
    pub fn sys_prompt(mut self, sys_prompt: impl Into<String>) -> Self {
        self.sys_prompt = sys_prompt.into();
        self
    }
    /// Sets the maximum reasoning→acting iterations (default 10).
    pub fn max_iters(mut self, max_iters: i32) -> Self {
        self.max_iters = max_iters;
        self
    }
    /// Ends the turn when a tool call is rejected by the permission engine, instead of
    /// feeding the rejection back to the model for another iteration (default `false`).
    /// Mirrors Java `ReActAgent.Builder.stopOnReject` / `ReactConfig.stopOnReject`.
    pub fn stop_on_reject(mut self, stop_on_reject: bool) -> Self {
        self.stop_on_reject = stop_on_reject;
        self
    }
    /// Enables candidate requirement proposals independently from todo tracking
    /// (default `false`). Mirrors Java `ReActAgent.Builder.enableTaskRequirements`.
    ///
    /// When enabled, `build()` registers the built-in `task_requirement_propose` tool over
    /// this agent's own state handle; when disabled the tool is absent, so the model never
    /// sees it.
    pub fn task_requirements_enabled(mut self, enabled: bool) -> Self {
        self.task_requirements_enabled = enabled;
        self
    }
    /// Sets the middleware chain.
    pub fn middleware_chain(mut self, chain: MiddlewareChain) -> Self {
        self.middleware_chain = Some(chain);
        self
    }
    /// Sets the initial agent state.
    pub fn state(mut self, state: AgentState) -> Self {
        self.state = Some(state);
        self
    }
    /// Enables or disables pending-tool recovery across calls.
    pub fn enable_pending_tool_recovery(mut self, v: bool) -> Self {
        self.enable_pending_tool_recovery = v;
        self
    }
    /// Sets the permission context used to evaluate tool requests.
    pub fn permission_context(mut self, ctx: PermissionContextState) -> Self {
        self.permission_context = Some(ctx);
        self
    }
    /// Enables or disables the built-in `todo_write` tool + `TaskReminderMiddleware` (default on).
    pub fn todo_tool(mut self, v: bool) -> Self {
        self.enable_todo_tool = v;
        self
    }

    /// Sets the confirmation handler invoked when a tool call requires user approval
    /// (`PermissionBehavior::Ask`). When set, `acting_phase` delegates to this handler
    /// instead of returning an error.
    pub fn confirm_handler(mut self, handler: ConfirmHandler) -> Self {
        self.confirm_handler = Some(handler);
        self
    }

    /// Controls whether native structured-output responses are validated against the
    /// requested JSON Schema after the model returns (default `true`). When disabled the
    /// reply must still be valid JSON, but schema conformance is trusted to the provider.
    pub fn validate_structured_output(mut self, v: bool) -> Self {
        self.validate_structured_output = v;
        self
    }

    /// Installs the native session journal backend. Mirrors Java
    /// `ReActAgent.Builder.sessionLogStore`.
    ///
    /// Setting a store enables journal-backed history (the default mode). Without one the
    /// agent keeps no journal and history is whatever the caller persists.
    pub fn session_log_store(mut self, store: Arc<dyn SessionLogStore>) -> Self {
        self.session_log_store = Some(store);
        self
    }

    /// Overrides the agent id stored in the journal key. Mirrors Java
    /// `sessionLogAgentId`; defaults to the agent's own id.
    pub fn session_log_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.session_log_agent_id = Some(agent_id.into());
        self
    }

    /// Selects the single history authority. Mirrors Java `sessionHistoryMode`,
    /// defaulting to [`SessionHistoryMode::EventLog`].
    pub fn session_history_mode(mut self, mode: SessionHistoryMode) -> Self {
        self.session_history_mode = mode;
        self
    }

    /// Sets what a recorder this agent opens itself journals per model call.
    ///
    /// Hosts that attach a recorder through the runtime context (the session scheduler) set
    /// this on their own recorder instead; the policy must match on both paths so the switch
    /// means one thing.
    pub fn recording_policy(mut self, policy: crate::session::RecordingPolicy) -> Self {
        self.recording_policy = policy;
        self
    }

    /// Declares tool-result metadata keys that are delivered on the event stream only.
    ///
    /// The transport reads them from `ToolResultEnd`; archiving them would keep a second
    /// copy of what the client already rendered (a file diff can be hundreds of kilobytes
    /// per call) in the context, the journal and every checkpoint.
    pub fn event_only_tool_metadata(mut self, keys: Vec<String>) -> Self {
        self.event_only_tool_metadata = keys;
        self
    }

    /// Builds the [`ReActAgent`].
    pub fn build(self) -> Result<ReActAgent> {
        let model = self
            .model
            .ok_or_else(|| Error::Agent("ReActAgent requires a model".into()))?;
        let id = if self.id.is_empty() {
            uuid::Uuid::new_v4().to_string().replace('-', "")
        } else {
            self.id
        };
        let name = if self.name.is_empty() {
            id.clone()
        } else {
            self.name
        };
        let max_iters = if self.max_iters <= 0 {
            DEFAULT_MAX_ITERS
        } else {
            self.max_iters
        };
        let initial_state = self.state.unwrap_or_default();
        let tasks: Arc<Mutex<TaskContextState>> =
            Arc::new(Mutex::new(initial_state.tasks_context.clone()));

        let toolkit = self.toolkit.unwrap_or_default();
        let mut chain = self.middleware_chain.unwrap_or_default();
        if self.enable_todo_tool {
            toolkit.register_tool(Arc::new(TodoWriteTool::new(Arc::clone(&tasks))));
            chain.add(Box::new(TaskReminderMiddleware::new(Arc::clone(&tasks))));
        }
        // Java `configureTodoTools`: requirement proposals are an independent opt-in. The tool
        // gets the shared task bag (Java `stateInjected` mutates `AgentState.tasksContext`) so a
        // proposal is visible in the same object the loop reads and persists.
        if self.task_requirements_enabled {
            toolkit.register_tool(Arc::new(RequirementProposeTool::new(Some(Arc::clone(
                &tasks,
            )))));
        }

        let core = Arc::new(AgentCore {
            model: Mutex::new(model),
            fallback_model: self.fallback_model,
            failover_listener: self.failover_listener,
            generate_options_defaults: Mutex::new(GenerateOptions::default()),
            toolkit,
            sys_prompt: self.sys_prompt,
            max_iters,
            stop_on_reject: self.stop_on_reject,
            task_requirements_enabled: self.task_requirements_enabled,
            state: Arc::new(Mutex::new(initial_state)),
            enable_pending_tool_recovery: self.enable_pending_tool_recovery,
            permission_engine: Mutex::new(PermissionEngine::new(
                self.permission_context.unwrap_or_default(),
            )),
            interrupt: Arc::new(InterruptControl::new()),
            active_run: Mutex::new(None),
            middleware_chain: Arc::new(chain),
            call_gate: Arc::new(CallGate::new()),
            subscriber: Arc::new(Mutex::new(None)),
            tasks,
            confirm_handler: Mutex::new(self.confirm_handler),
            partial_assistant: Mutex::new(None),
            validate_structured_output: self.validate_structured_output,
            extensions: self.extensions,
            session_log_store: self.session_log_store,
            // Java: `builder.sessionLogAgentId == null ? getName() : builder.sessionLogAgentId`.
            session_log_agent_id: self.session_log_agent_id.unwrap_or_else(|| name.clone()),
            session_history_mode: self.session_history_mode,
            recording_policy: self.recording_policy,
            event_only_tool_metadata: self.event_only_tool_metadata,
        });
        Ok(ReActAgent {
            id,
            name,
            description: self.description,
            core,
        })
    }
}

// ---------------------------------------------------------------------------
// Execution loop
// ---------------------------------------------------------------------------

/// Entry point of the reasoning→acting loop. Copies the shared state out, folds
/// request-scoped system material into a frozen system message, appends the input
/// messages, runs the iteration loop and writes the state back.
async fn run_agent_loop(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    input: AgentInput,
) -> Result<Msg> {
    let AgentInput {
        mut messages,
        mut system_prompt,
    } = input;

    // Legacy compatibility: middlewares used to attach a SYSTEM message directly to
    // `AgentInput.messages`. Fold that material into the request-scoped system
    // prompt and remove it from the conversation input. Java forbids this shape
    // outright (`AgentBase.notifyPreCall` guard); Rust accepts it but never persists
    // it, so a request-scoped prompt cannot accumulate across turns.
    let legacy_system: Vec<Msg> = messages
        .iter()
        .filter(|message| message.role == MsgRole::System)
        .cloned()
        .collect();
    for message in legacy_system {
        crate::middleware::input::append_system_prompt_text(
            &mut system_prompt,
            &message.text_content(),
        );
    }
    messages.retain(|message| message.role != MsgRole::System);

    let mut state = core.state.lock().unwrap().clone();
    // Java's `AgentState.context` never stores the system message: it is frozen
    // per call in `CallExecution.systemMsg`. Drop any SYSTEM material persisted by
    // older builds before this request enters the loop.
    state
        .context_mut()
        .retain(|message| message.role != MsgRole::System);

    // The call-scoped state is now bound: notify the middlewares that participate in
    // `OnAgentStateReady` before the input enters the pipeline. This is the only seam
    // that exposes the state a call will actually use *before* that input is
    // persisted (session bootstrapping, state-driven decisions, per-session audit).
    // Mirrors Java `ReActAgent.beforeAgentExecution`.
    core.middleware_chain
        .notify_agent_state_ready(ctx, &mut state, &messages);

    // Mirrors Java `doCallInner`: when the previous turn left unresolved tool calls,
    // decide how to proceed *before* appending the new input. Two of Java's four
    // branches are ported — recovery (patch orphans) and the fail-fast guard below.
    // Java's "resume" branch (act on the pending tools without another reasoning
    // step) is not migrated, and Java's ASKING branch is unreachable here because
    // this loop never persists `ToolCallState::Asking` (HITL is resolved inline by
    // the confirm handler during the acting phase).
    let mut pending = extract_pending_tool_calls(&state);
    if !pending.is_empty() {
        if core.enable_pending_tool_recovery {
            patch_pending_tool_calls(&mut state, &messages);
            pending = extract_pending_tool_calls(&state);
        }
        // Java throws here — and so do we. Without this the dangling tool_use is sent
        // to the provider verbatim (rejected with HTTP 400) or silently dropped by the
        // harness sanitizer, leaving no hint about the real cause. The caller can still
        // resolve the orphan itself by supplying tool results in `messages`.
        let caller_supplied_results = messages
            .iter()
            .any(|m| m.tool_result_blocks().next().is_some());
        if !pending.is_empty() && !messages.is_empty() && !caller_supplied_results {
            let ids: Vec<&str> = pending.iter().map(|c| c.id.as_str()).collect();
            return Err(Error::Agent(format!(
                "Pending tool calls exist without results. Enable \
                 enable_pending_tool_recovery or provide tool results. Pending IDs: {ids:?}"
            )));
        }
    }

    // Freeze the system prompt once per call. Mirrors Java `seedSystemMsg` +
    // `applySystemPromptMiddlewares`: request-scoped workspace/memory material and
    // the agent-level prompt are merged first, then `on_system_prompt` transformers
    // run once. Every reasoning iteration receives a fresh copy of this frozen
    // message, never a persisted conversation entry.
    let mut frozen_prompt = if core.sys_prompt.trim().is_empty() {
        None
    } else {
        Some(core.sys_prompt.trim().to_string())
    };
    if let Some(extra) = system_prompt.as_deref() {
        crate::middleware::input::append_system_prompt_text(&mut frozen_prompt, extra);
    }
    let mut frozen_prompt = core
        .middleware_chain
        .apply_system_prompt(ctx, frozen_prompt.unwrap_or_default())
        .await;
    // Java `PreCallEvent`: the hooks see the restored history snapshot, the call
    // input, and may append request-scoped system material before it is frozen.
    core.middleware_chain
        .notify_pre_call(ctx, &mut state, &messages, &mut frozen_prompt)
        .await;
    let system_msg = (!frozen_prompt.trim().is_empty()).then(|| Msg::system(frozen_prompt));

    for message in messages {
        state.context_mut().push(message);
    }
    state.set_cur_iter(0);
    // Snapshot plan mode so a plan-control tool's direct write to the shared state can
    // be detected and carried over (the loop otherwise runs on a private clone).
    let plan_mode_at_start = state.plan_mode_context.clone();
    let mut result = match execute_iteration(core, ctx, &mut state, 0, system_msg.as_ref()).await {
        // An interrupt is not a failure: reconcile the dangling tool_use left by
        // the interrupted turn and append a recovery message, so the caller sees
        // a normal reply and the next turn continues from a consistent context.
        // Mirrors Java `handleInterrupt`.
        Err(Error::Interrupted(_)) => Ok(reconcile_interrupted(&mut state)),
        other => other,
    };
    // Session settle loop (Java `settleSession`): a completed result does not end the turn
    // when guidance accepted during the run has not been consumed yet. Continuing keeps the
    // inbox turn open so the accepted steering is acted on instead of being dropped by the
    // close that follows.
    while session_continues(ctx, &result).await? {
        let next = state.cur_iter() + 1;
        result = match execute_iteration(core, ctx, &mut state, next, system_msg.as_ref()).await {
            Err(Error::Interrupted(_)) => Ok(reconcile_interrupted(&mut state)),
            other => other,
        };
    }
    // Commit the accumulated state even when the run failed or was interrupted:
    // the messages that completed before the error (user input, finished
    // assistant turns, tool results) must survive, so a follow-up "continue"
    // resumes instead of replaying from the previous turn boundary. Mirrors
    // Java, where `scope.state` is mutated in place and
    // `saveStateAfterCallFailure` persists it on the error path.
    {
        let mut shared = core.state.lock().unwrap();
        // Carry over plan-mode updates a plan-control tool made directly on the shared
        // state (the loop ran on a private clone; only rebind when it actually changed).
        if shared.plan_mode_context != plan_mode_at_start {
            state.plan_mode_context = shared.plan_mode_context.clone();
        }
        *shared = state;
    }
    result
}

/// Whether the session settle loop must run one more iteration.
///
/// Mirrors Java `settleSession`: only a *completed* outcome can continue, and only when the
/// execution still holds unconsumed guidance for this turn. Everything else (suspended,
/// failed, interrupted, no execution attached) ends the turn as before. A persistence failure
/// propagates instead of being read as "nothing pending" — silently dropping accepted guidance
/// is exactly what the settle loop exists to prevent.
async fn session_continues(
    ctx: &crate::agent::RuntimeContext,
    result: &Result<Msg>,
) -> Result<bool> {
    let Ok(message) = result else {
        return Ok(false);
    };
    if outcome(message.generate_reason()) != "completed" {
        return Ok(false);
    }
    let (Some(execution), Some(recorder)) = (
        SessionExecution::from_context(ctx),
        SessionRecorder::from_context(ctx),
    ) else {
        return Ok(false);
    };
    execution.continue_or_close(&recorder).await
}

/// Whether a reasoning message still has tool calls that need local action.
///
/// Server-tool calls are executed by the provider and their results arrive inline in
/// the same assistant message, so a message whose tool calls are all server tools with
/// results present is already finished. Mirrors the Java `isFinished` tail.
fn has_actionable_tool_calls(msg: &Msg) -> bool {
    let calls: Vec<&ToolUseBlock> = msg.tool_use_blocks().collect();
    if calls.is_empty() {
        return false;
    }
    let inline = msg.inline_server_tool_result_ids();
    !calls
        .iter()
        .all(|c| c.is_server_tool() && inline.contains(&c.id))
}

async fn execute_iteration(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    state: &mut AgentState,
    mut iter: i32,
    system_msg: Option<&Msg>,
) -> Result<Msg> {
    loop {
        let msg = reasoning_phase(core, ctx, state, iter, system_msg).await?;
        let reason = msg.generate_reason();
        let has_tool_calls = has_actionable_tool_calls(&msg);
        // The calls this round asked for, captured before the acting phase so the
        // all-denied check below does not have to re-read the assistant message.
        let round_calls: Vec<ToolUseBlock> = if has_tool_calls {
            msg.tool_use_blocks().cloned().collect()
        } else {
            Vec::new()
        };
        let outcome = match reason {
            GenerateReason::ModelStop | GenerateReason::ToolCalls => {
                if has_tool_calls {
                    let outcome = acting_phase(core, ctx, state).await?;
                    iter += 1;
                    outcome
                } else {
                    return Ok(msg);
                }
            }
            // A stop requested by a hook/middleware ends the turn here: acting on the
            // message would keep running exactly the work the stop asked to abandon.
            GenerateReason::ReasoningStopRequested
            | GenerateReason::ActingStopRequested
            | GenerateReason::MiddlewareStopRequested
            | GenerateReason::MaxIterations => {
                return Ok(msg);
            }
            _ => {
                if has_tool_calls {
                    let outcome = acting_phase(core, ctx, state).await?;
                    iter += 1;
                    outcome
                } else {
                    return Ok(msg);
                }
            }
        };
        if let Some(stopped) = outcome.stopped {
            // The stop message is the turn's result; the acting phase already persisted the
            // results that ran before the stop, so the context stays well formed.
            return Ok(stopped);
        }
        // Every tool call of this round was denied (permission rules, plan mode or HITL):
        // emit `AllToolsDenied` through the acting middlewares so one of them can stop the
        // turn. Without a stop the loop reasons again, exactly as it did before this signal
        // existed. Mirrors Java `emitAllToolsDeniedThroughMiddleware`.
        if all_tool_calls_denied(state, &round_calls) {
            if let Some(reason) = emit_all_tools_denied(core, ctx, &round_calls).await {
                return Ok(last_assistant_message_with_reason(state, reason));
            }
        }
        // `stop_on_reject`: a rejection ends the turn instead of feeding it back to the
        // model for another iteration. Mirrors Java `ReactConfig.stopOnReject`; the default
        // (`false`) keeps the pre-existing loop, which retries after the denial.
        if core.stop_on_reject {
            if let Some(reason) = rejected_turn_reason(state, &round_calls) {
                return Ok(last_assistant_message_with_reason(state, reason));
            }
        }
        if outcome.return_direct {
            // returnDirect short-circuit: the successful tool result *is* the turn's
            // final answer, so no further reasoning iteration runs. The closing
            // assistant message is synthesized from the executed results and returned
            // with `GenerateReason::ToolReturnDirect`. Mirrors Java
            // `finalizeReturnDirect`, which is shared by the in-framework and the
            // external-resume path.
            let closing = build_return_direct_result(ctx, &outcome.results);
            state.context_mut().push(closing.clone());
            publish_return_direct_text_events(ctx, &closing);
            return Ok(closing);
        }
        if outcome.suspended {
            return Ok(msg.with_generate_reason(GenerateReason::ToolSuspended));
        }
    }
}

/// The result of one acting phase.
#[derive(Default)]
struct ActingOutcome {
    /// At least one tool call suspended itself and awaits external execution.
    suspended: bool,
    /// Every executed tool declared `returnDirect` and succeeded, so the batch's
    /// results are the turn's final answer (no follow-up model call).
    return_direct: bool,
    /// The executed results, in tool-call order (used to build the closing message).
    results: Vec<ToolResultBlock>,
    /// A middleware stopped the turn during acting; this message *is* the turn's result.
    stopped: Option<Msg>,
}

/// Synthesizes the closing assistant message of a `returnDirect` turn, mirroring
/// Java `buildReturnDirectResultMsg`.
///
/// The output blocks are concatenated in execution order (= the model's tool_calls
/// order) without inlining tool names or separators: like a model-generated summary,
/// the message carries no per-block provenance. A tool whose result has zero content
/// blocks contributes nothing; only when the whole batch yields zero blocks is a
/// defensive `"(no output)"` placeholder inserted, keeping the message non-empty for
/// providers that reject empty assistant content.
fn build_return_direct_result(
    ctx: &crate::agent::RuntimeContext,
    results: &[ToolResultBlock],
) -> Msg {
    let mut content: Vec<ContentBlock> = Vec::new();
    for rb in results {
        content.extend(rb.output.iter().cloned());
    }
    if content.is_empty() {
        content.push(ContentBlock::Text(TextBlock::new("(no output)")));
    }
    let mut metadata = JsonMap::new();
    metadata.insert(
        crate::message::metadata_keys::TOOL_RETURN_DIRECT.to_string(),
        serde_json::Value::Bool(true),
    );
    let mut builder = Msg::builder()
        .role(MsgRole::Assistant)
        .content(content)
        .metadata(metadata)
        .generate_reason(GenerateReason::ToolReturnDirect);
    if let Some(name) = ctx.agent_name.as_deref() {
        builder = builder.name(name);
    }
    builder.build()
}

/// Emits the standard text-block lifecycle (`TextBlockStart` → `TextBlockDelta` →
/// `TextBlockEnd`) for a `returnDirect` closing message, so event-stream consumers
/// (AG-UI, A2A, ...) see the same projection as for a model-generated final answer.
///
/// The delta carries the `generate_reason` metadata key set to `TOOL_RETURN_DIRECT`,
/// letting observers distinguish "the model said this" from "a tool result was
/// substituted for the model's final answer". Non-text blocks emit nothing and still
/// travel via the message-level result event.
fn publish_return_direct_text_events(ctx: &crate::agent::RuntimeContext, closing: &Msg) {
    let reply_id = closing.id().to_string();
    for block in closing.content() {
        let ContentBlock::Text(t) = block else {
            continue;
        };
        let block_id = uuid::Uuid::new_v4().to_string().replace('-', "");
        let mut start = JsonMap::new();
        start.insert(
            "reply_id".to_string(),
            serde_json::Value::String(reply_id.clone()),
        );
        start.insert(
            "block_id".to_string(),
            serde_json::Value::String(block_id.clone()),
        );
        start.insert(
            "block_type".to_string(),
            serde_json::Value::String("text".to_string()),
        );
        emit_delta(
            &ctx.delta_tx,
            AgentEvent::with_fields(AgentEventType::TextBlockStart, start),
        );

        let mut delta = JsonMap::new();
        delta.insert(
            "reply_id".to_string(),
            serde_json::Value::String(reply_id.clone()),
        );
        delta.insert(
            "block_id".to_string(),
            serde_json::Value::String(block_id.clone()),
        );
        delta.insert(
            "block_type".to_string(),
            serde_json::Value::String("text".to_string()),
        );
        delta.insert(
            "delta".to_string(),
            serde_json::Value::String(t.text.clone()),
        );
        emit_delta(
            &ctx.delta_tx,
            AgentEvent::with_fields(AgentEventType::TextBlockDelta, delta).with_metadata(
                crate::message::metadata_keys::EVENT_GENERATE_REASON,
                serde_json::Value::String("TOOL_RETURN_DIRECT".to_string()),
            ),
        );

        let mut end = JsonMap::new();
        end.insert(
            "reply_id".to_string(),
            serde_json::Value::String(reply_id.clone()),
        );
        end.insert("block_id".to_string(), serde_json::Value::String(block_id));
        end.insert(
            "block_type".to_string(),
            serde_json::Value::String("text".to_string()),
        );
        emit_delta(
            &ctx.delta_tx,
            AgentEvent::with_fields(AgentEventType::TextBlockEnd, end),
        );
    }
}

/// Whether a tool result may participate in the `returnDirect` short-circuit: the
/// tool declared `returnDirect` and its result actually succeeded. `Denied` /
/// `Error` / `Interrupted` / `Suspended` results must never be presented as the
/// final answer. Mirrors Java `isReturnDirectToolCall`.
fn is_return_direct_result(core: &Arc<AgentCore>, rb: &ToolResultBlock) -> bool {
    if rb.final_state() != ToolResultState::Success {
        return false;
    }
    rb.name
        .as_deref()
        .and_then(|name| core.toolkit.get_tool(name))
        .map(|t| t.is_return_direct())
        .unwrap_or(false)
}

/// The placeholder-shaped [`ToolResultBlock`] persisted in place of a `returnDirect`
/// tool's real result: same id/name/state, output swapped for the placeholder
/// sentence. Mirrors Java `placeholderResultBlock`.
fn placeholder_result_block(rb: &ToolResultBlock) -> ToolResultBlock {
    ToolResultBlock {
        id: rb.id.clone(),
        name: rb.name.clone(),
        output: vec![ContentBlock::Text(TextBlock::new(
            RETURN_DIRECT_PLACEHOLDER,
        ))],
        metadata: rb.metadata.clone(),
        state: rb.state,
    }
}

/// Placeholder sentence written to the `tool_result` of a `returnDirect` tool.
///
/// The real result is lifted into the closing assistant message, so the context
/// keeps `[tool_use, tool_result(placeholder), assistant(full result)]` and the
/// model never sees the payload duplicated.
const RETURN_DIRECT_PLACEHOLDER: &str = "Tool call completed. The result has been presented to the user as the final output of this turn.";

/// Runs one reasoning phase: check interrupt, possibly summarize if over budget,
/// build the model input and run the (middleware-wrapped) model call. Appends the
/// resulting assistant message to the context and returns it for the loop to decide
/// whether to act or terminate.
async fn reasoning_phase(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    state: &mut AgentState,
    iter: i32,
    system_msg: Option<&Msg>,
) -> Result<Msg> {
    check_interrupted(core, Some(ctx))?;
    state.set_cur_iter(iter);
    // `todo_write` mutates the shared task bag, which only lands in the caller's state at the
    // end of the call. Folding it in here keeps every published snapshot (per-iteration
    // checkpoint, context compilation, journal) on the live task state, as Java does by
    // mutating `AgentState` in place.
    state.tasks_context = core.tasks.lock().unwrap().clone();
    // Publish the state accumulated so far (this turn's input plus the previous
    // iteration's assistant/tool messages) into the shared state before the
    // reasoning chain runs. Without this, `core.state` would stay frozen at the
    // previous turn boundary for the whole turn, so a per-iteration checkpoint
    // middleware would have nothing to persist and an abrupt process death
    // (kill -9 / OOM / power loss) would lose the entire turn.
    {
        let mut shared = core.state.lock().unwrap();
        // Plan mode is owned by the tools, which hold this same handle: `plan_enter` /
        // `plan_exit` issued in the previous acting phase wrote the shared state, not this
        // clone. Carry the tool-written value over instead of republishing the stale one,
        // otherwise the flip is undone before the next request (no PLAN banner, and core's
        // acting-phase interception reverts to BUILD).
        state.plan_mode_context = shared.plan_mode_context.clone();
        *shared = state.clone();
    }
    // The summarizing path is not a step: Java checks `maxIters` before `step(iter)`, so an
    // over-budget iteration never opens a step boundary nor folds in guidance.
    if iter >= core.max_iters {
        return summarizing(core, ctx, state).await;
    }
    // Step boundary plus mid-run guidance (Java `reasoning`): the recorder marks the step and
    // the execution folds in accepted `steer`/`inject` input *before* the model call, so
    // guidance accepted while the previous step was running reaches this one instead of
    // waiting for the turn to end. A persistence failure propagates: a step that never reached
    // the journal must not look like one that did.
    if let Some(recorder) = SessionRecorder::from_context(ctx) {
        recorder.step(iter.max(0) as usize)?;
        if let Some(execution) = SessionExecution::from_context(ctx) {
            execution.before_step(&recorder, state).await?;
        }
    }

    // `before_step` folds accepted `inject`/`steer` guidance into the *local* context after
    // the turn snapshot published above. Hand the context the request is actually built from
    // to the shared state before the model call: context middlewares read it there, and
    // without this handoff an overflow compaction would summarize — and then durably
    // replace — a history that is missing this step's guidance. Only `context` is handed
    // over; the rest of the shared state already carries the loop's snapshot.
    publish_context(core, state);

    // Java prepends a fresh copy of the frozen `CallExecution.systemMsg` to every
    // reasoning request. The conversation state itself remains system-free.
    let mut messages = Vec::new();
    if let Some(system_msg) = system_msg {
        messages.push(system_msg.clone());
    }
    messages.extend(state.context().to_vec());

    let tool_request_config = ctx.extension::<crate::tool::ToolRequestConfig>(
        crate::tool::tool_request_config::TOOL_REQUEST_CONFIG_CONTEXT_KEY,
    );
    let tools = core
        .toolkit
        .tool_schemas_with(tool_request_config.as_deref());
    let model = core.model.lock().unwrap().clone();
    // `OnFormat` seam: middlewares may rewrite the structured-output request before it is
    // built. It runs for every reasoning step (Java's `callFormat` wraps each formatter
    // invocation), so a rewrite always lands in the provider call it belongs to.
    let mut response_format = ctx.response_format.clone();
    core.middleware_chain
        .apply_format(ctx, &mut response_format)
        .await;
    let mut ob = GenerateOptions::builder().model_name(model.model_name());
    if let Some(rf) = &response_format {
        ob = ob.response_format(rf.clone());
    }
    // Merge per-agent generation defaults (reasoning effort, retry policy, ...)
    // so runtime config (thought level, model retry) reaches every model call.
    let defaults = core.generate_options_defaults.lock().unwrap().clone();
    let options = GenerateOptions::merge_options(&ob.build(), &defaults);
    let mut options = options;
    // Model-level retry: apply the standard model retry policy (429/5xx/IO with
    // exponential backoff) when no explicit execution config was supplied.
    ModelUtils::ensure_default_execution_config(&mut options);
    let input = ReasoningInput::new(messages, tools, options);

    // The context compiler runs inside the middleware chain and holds no agent state, so the
    // committed task snapshot travels with this call (Java resolves the live state from its
    // runtime context instead). The clone is per-call and read-only for the compiler.
    let mut call_ctx = ctx.clone();
    call_ctx.set_extension(
        crate::middleware::TASK_CONTEXT_STATE_CONTEXT_KEY,
        state.tasks_context.clone(),
    );
    let final_msg = match run_reasoning(core, &call_ctx, input).await {
        Ok(msg) => msg,
        Err(e) => {
            // A budget-overflow compaction may have rewritten the shared history before the
            // model call failed; keep that reduction instead of restoring the oversized clone.
            adopt_published_context(core, state);
            // Keep the assistant message the model had already produced when the
            // call was interrupted, so the interrupted turn can reconcile its
            // dangling tool_use blocks instead of losing the response entirely
            // (Java parity).
            if matches!(e, Error::Interrupted(_)) {
                let partial = core.partial_assistant.lock().unwrap().take();
                if let Some(partial) = partial {
                    state.context_mut().push(partial);
                }
            }
            return Err(e);
        }
    };
    // Context-management middleware (budget-overflow compaction) can rewrite the shared
    // history *during* the model call. This loop runs on a private state clone, so take the
    // middleware's rewrite back before appending the assistant message; otherwise the next
    // iteration would publish the old oversized history back and compaction would be undone.
    adopt_published_context(core, state);
    let mut final_msg = final_msg;
    // Tag the usage with the model that reported it. Token accounting anchors on this
    // number, and a model switch changes the tokenizer, so a mismatch must invalidate it.
    if final_msg.usage.is_some() {
        let model_name = core.model.lock().unwrap().model_name().to_string();
        final_msg.metadata.insert(
            Msg::METADATA_USAGE_MODEL.to_string(),
            serde_json::Value::String(model_name),
        );
    }
    // A stop that never produced a message must not add an empty assistant turn to the
    // history (Java returns that message without persisting it).
    let synthesized_stop = final_msg.content().is_empty()
        && matches!(
            final_msg.generate_reason(),
            GenerateReason::ReasoningStopRequested
                | GenerateReason::ActingStopRequested
                | GenerateReason::MiddlewareStopRequested
        );
    if !synthesized_stop {
        state.context_mut().push(final_msg.clone());
    }
    Ok(final_msg)
}

/// Hands the loop's conversation to the shared state.
///
/// The shared state is the handle `RuntimeContext` exposes to middlewares. The loop runs
/// on a private clone, so without this handoff a middleware that reads `ctx.state.context()`
/// (overflow compaction) would see a history missing anything appended after the turn
/// snapshot — accepted guidance, for instance. Only the conversation is handed over: task,
/// plan-mode and permission state belong to the tools and are not overwritten here.
fn publish_context(core: &Arc<AgentCore>, state: &AgentState) {
    core.state.lock().unwrap().context = state.context().to_vec();
}

/// Takes back a conversation rewrite a middleware made during the model call.
///
/// The loop publishes its context right before the call, so any difference between the
/// shared state and the loop's own (untouched during the call) copy is a middleware rewrite
/// (overflow compaction, proactive compaction). Adopting it keeps the next iteration and the
/// next turn on the compacted history instead of republishing the oversized one. Comparing
/// against the loop's copy avoids holding a third copy of the whole conversation per call;
/// an unchanged context leaves the loop's copy untouched.
fn adopt_published_context(core: &Arc<AgentCore>, state: &mut AgentState) {
    let shared = core.state.lock().unwrap();
    if shared.context() != state.context() {
        state.context = shared.context().to_vec();
        if !shared.summary().is_empty() {
            state.set_summary(shared.summary().to_string());
        }
    }
}

/// Reads the `GenerateReason` an event payload carries, if any.
///
/// Java puts it on `RequestStopEvent` as a typed field; Rust keeps it in the event payload
/// under the same `generate_reason` metadata key the text projections use. Both the metadata
/// map and the flattened `fields` are checked so a producer that attaches it either way is
/// understood.
fn event_generate_reason(data: &EventData) -> Option<GenerateReason> {
    for key in [
        crate::message::metadata_keys::EVENT_GENERATE_REASON,
        "generateReason",
    ] {
        let Some(raw) = data.metadata.get(key).or_else(|| data.fields.get(key)) else {
            continue;
        };
        if let Ok(reason) = serde_json::from_value::<GenerateReason>(raw.clone()) {
            return Some(reason);
        }
        // Tolerate a lower-case wire spelling from a hand-built payload.
        if let Some(name) = raw.as_str() {
            if let Ok(reason) = serde_json::from_value::<GenerateReason>(serde_json::Value::String(
                name.to_ascii_uppercase(),
            )) {
                return Some(reason);
            }
        }
    }
    None
}

/// The reason a `RequestStop` ends the turn with: the one it carries, or the phase default.
fn stop_reason(data: &EventData, phase_default: GenerateReason) -> GenerateReason {
    event_generate_reason(data).unwrap_or(phase_default)
}

/// Builds the assistant message that ends a turn a middleware stopped.
///
/// `results` are the tool results produced before the stop; each is paired with its
/// `tool_use` block so the persisted context holds no dangling call and a follow-up turn can
/// resume from it (Java `buildStopMsg`).
fn stop_message(
    ctx: &crate::agent::RuntimeContext,
    calls: &[ToolUseBlock],
    results: &[ToolResultBlock],
    reason: GenerateReason,
) -> Msg {
    let mut content: Vec<ContentBlock> = Vec::new();
    for result in results {
        if let Some(call) = result
            .id
            .as_deref()
            .and_then(|id| calls.iter().find(|call| call.id == id))
        {
            content.push(ContentBlock::ToolUse(call.clone()));
        }
        content.push(ContentBlock::ToolResult(result.clone()));
    }
    let mut builder = Msg::builder()
        .role(MsgRole::Assistant)
        .content(content)
        .generate_reason(reason);
    if let Some(name) = ctx.agent_name.as_deref() {
        builder = builder.name(name);
    }
    builder.build()
}

/// Dispatches one prepared model call through the middleware chain.
///
/// The structured-output fallback path builds its request by hand (the transient
/// reminder plus the synthetic `generate_response` schema), so it must not call the
/// provider directly: the chain's `on_model_call` middleware is the request
/// preparation boundary (harness `ContextMiddleware` mirrors Java
/// `ModelRequestPreparer`), and it has to see the *complete* request — including the
/// reminder and the extra tool — before any budget check runs (Java 51a74e2ef).
async fn model_call_through_chain(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    input: ModelCallInput,
) -> Result<Msg> {
    let slot: Arc<Mutex<Option<Result<Msg>>>> = Arc::new(Mutex::new(None));
    let chain = Arc::clone(&core.middleware_chain);
    let model_core: Next<ModelCallInput> = {
        let slot = Arc::clone(&slot);
        let core = Arc::clone(core);
        let ctx = ctx.clone();
        Box::new(move |mc_input: ModelCallInput| -> EventStream {
            let slot = Arc::clone(&slot);
            let core = Arc::clone(&core);
            let ctx = ctx.clone();
            let fut = async move {
                // This path is not streamed, so nothing is emitted to a consumer and
                // the retries stay allowed.
                let msg = model_call_impl(&core, Some(&ctx), mc_input, false, |_| {}).await;
                *slot.lock().unwrap() = Some(msg);
                Ok(AgentEvent::ModelCallEnd(EventData::default()))
            };
            Box::pin(futures::stream::once(fut))
        })
    };
    let mut stream = chain.wrap_model_call(ctx.clone(), input, model_core);
    // A middleware can reject the request before the model core runs (over-budget
    // context, for instance). That failure only exists as a stream item, so it has to
    // be carried out explicitly instead of turning into "model produced no message".
    let mut first_error: Option<Error> = None;
    while let Some(item) = stream.next().await {
        if let Err(error) = item {
            first_error = Some(error);
            break;
        }
    }
    drop(stream);
    let produced = slot.lock().unwrap().take();
    match produced {
        Some(result) => result,
        None => {
            Err(first_error.unwrap_or_else(|| Error::Agent("model produced no message".into())))
        }
    }
}

/// Runs the reasoning phase through the middleware chain, wrapping the raw model
/// call, and returns the accumulated assistant message.
async fn run_reasoning(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    input: ReasoningInput,
) -> Result<Msg> {
    let slot: Arc<Mutex<Option<Result<Msg>>>> = Arc::new(Mutex::new(None));
    let chain = Arc::clone(&core.middleware_chain);
    let model = core.model.lock().unwrap().clone();

    let delta_tx = ctx.delta_tx.clone();
    let model_core: Next<ModelCallInput> = {
        let slot = Arc::clone(&slot);
        let core = Arc::clone(core);
        let delta_tx = delta_tx.clone();
        // The call context is cloned in: the model call runs on a `'static` future, so it
        // cannot borrow the caller's context.
        let ctx = ctx.clone();
        Box::new(move |mc_input: ModelCallInput| -> EventStream {
            let slot = Arc::clone(&slot);
            let core = Arc::clone(&core);
            let delta_tx = delta_tx.clone();
            let fut = async move {
                // Deltas only reach a consumer when the agent-level stream has a
                // channel; without one nothing is visible, so retries stay allowed.
                let msg = model_call_impl(&core, Some(&ctx), mc_input, delta_tx.is_some(), |ev| {
                    if let Some(tx) = &delta_tx {
                        let _ = tx.unbounded_send(ev);
                    }
                })
                .await;
                *slot.lock().unwrap() = Some(msg);
                Ok(AgentEvent::ModelCallEnd(EventData::default()))
            };
            Box::pin(futures::stream::once(fut))
        })
    };

    // The chain may rewrite the reasoning input (dangling tool-call pruning,
    // tool-result eviction, inbox/task reminders, dynamic tools), so the model
    // call MUST be built from the input the chain hands over — not from the
    // original one. Otherwise those rewrites never reach the provider.
    let reasoning_core: Next<ReasoningInput> = {
        let chain = Arc::clone(&chain);
        let ctx = ctx.clone();
        let model = Arc::clone(&model);
        Box::new(move |r: ReasoningInput| -> EventStream {
            // Journal the final adapter boundary (Java `SessionModels.wrap`): with a recorder
            // attached to `ctx`, each dispatch records
            // request/prepared → model/dispatch → model/chunk… → model/end.
            let model = SessionModels::wrap(Arc::clone(&model), &ctx, "REASONING");
            // Java keeps exactly one system message at the head of every request:
            // `PreReasoningEvent` hooks may append to it but cannot inject additional
            // SYSTEM entries into `inputMessages`. Normalise here so a third-party
            // `on_reasoning` middleware cannot leave two system messages behind.
            let messages = normalize_system_messages(r.messages);
            let mc_input = ModelCallInput::new(messages, r.tools, r.options, model);
            chain.wrap_model_call(ctx.clone(), mc_input, model_core)
        })
    };

    let mut stream = chain.wrap_reasoning(ctx.clone(), input, reasoning_core);
    // A middleware can ask the agent to stop by emitting `RequestStop` (Java
    // `RequestStopEvent`). The stream is abandoned at that point instead of being drained, so
    // a middleware that stops *before* delegating prevents the model call entirely, and the
    // rest of the round — acting on this message and any further iteration — is discarded.
    let mut events: Vec<crate::error::Result<AgentEvent>> = Vec::new();
    let mut stopped = None;
    while let Some(item) = stream.next().await {
        if let Ok(AgentEvent::RequestStop(data)) = &item {
            stopped = Some(stop_reason(data, GenerateReason::ReasoningStopRequested));
            events.push(item);
            break;
        }
        events.push(item);
    }
    drop(stream);
    // A middleware can fail before the model core runs (for example the context
    // compiler rejecting an over-budget request). That failure only exists as a
    // stream item; discarding it here would turn every such failure into the
    // indistinguishable "model produced no message" error.
    let stream_error = events.into_iter().find_map(|event| event.err());
    let produced = slot.lock().unwrap().take();
    // A stop that never reached the model has no reasoning message to report; the synthesized
    // one is the turn's closing result, not something the model produced.
    let synthesized_stop = produced.is_none() && stopped.is_some();
    let result = match (produced, stopped) {
        // Whatever the model already produced is kept (its text may have been streamed to
        // the caller); only the reason changes, so no tool call is acted on.
        (Some(Ok(message)), Some(reason)) => Ok(message.with_generate_reason(reason)),
        (Some(result), _) => result,
        // A stop without a message is not a failure: the middleware answered the round
        // itself (Java `Mono.justOrEmpty(finalMsg)` on the same branch).
        (None, Some(reason)) => Ok(stop_message(ctx, &[], &[], reason)),
        (None, None) => match stream_error {
            Some(error) => Err(error),
            None => Err(Error::Agent("model produced no message".into())),
        },
    };
    // Java `PostReasoningEvent`: tell the chain what this reasoning step produced so a
    // middleware can observe (and, e.g., log) each intermediate assistant message. A
    // notification only — the message is borrowed and cannot be rewritten, and a failed
    // step does not fire it (Java does not publish the event either).
    match (&result, synthesized_stop) {
        (Ok(message), false) => chain.notify_reasoning_result(ctx, message).await,
        _ => {}
    }
    result
}
/// Helper: emit an agent event through the delta_tx channel if available.
fn emit_delta(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    ev: AgentEvent,
) {
    if let Some(tx) = delta_tx {
        let _ = tx.unbounded_send(Ok(ev));
    }
}

/// Builds an agent event carrying tool-call metadata fields.
fn tool_event(event_type: AgentEventType, tool_call_id: &str, tool_name: &str) -> AgentEvent {
    let mut fields = JsonMap::new();
    fields.insert(
        "tool_call_id".to_string(),
        serde_json::Value::String(tool_call_id.to_string()),
    );
    fields.insert(
        "tool_name".to_string(),
        serde_json::Value::String(tool_name.to_string()),
    );
    AgentEvent::with_fields(event_type, fields)
}

/// Emits a ToolResultStart event through the delta_tx channel.
fn emit_tool_result_start(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    tool_call_id: &str,
    tool_name: &str,
) {
    let mut fields = JsonMap::new();
    fields.insert(
        "tool_call_id".to_string(),
        serde_json::Value::String(tool_call_id.to_string()),
    );
    fields.insert(
        "tool_name".to_string(),
        serde_json::Value::String(tool_name.to_string()),
    );
    emit_delta(
        delta_tx,
        AgentEvent::with_fields(AgentEventType::ToolResultStart, fields),
    );
}

/// Emits a ToolResultTextDelta event through the delta_tx channel.
fn emit_tool_result_text_delta(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    tool_call_id: &str,
    tool_name: &str,
    delta: &str,
) {
    let mut fields = JsonMap::new();
    fields.insert(
        "tool_call_id".to_string(),
        serde_json::Value::String(tool_call_id.to_string()),
    );
    fields.insert(
        "tool_name".to_string(),
        serde_json::Value::String(tool_name.to_string()),
    );
    fields.insert(
        "delta".to_string(),
        serde_json::Value::String(delta.to_string()),
    );
    emit_delta(
        delta_tx,
        AgentEvent::with_fields(AgentEventType::ToolResultTextDelta, fields),
    );
}

/// The wire name of a tool-result state, as carried by `TOOL_RESULT_END`'s `state`
/// field.
///
/// One name per variant, spelled like the other `ToolResultState` projections
/// (`observation::ActionStatus::as_str`). `Interrupted` must stay distinct from
/// `Error`: a user cancellation is not a tool failure, and consumers (AG-UI,
/// Studio) surface the two differently.
fn tool_result_state_str(state: ToolResultState) -> &'static str {
    match state {
        ToolResultState::Success => "SUCCESS",
        ToolResultState::Error => "ERROR",
        ToolResultState::Denied => "DENIED",
        ToolResultState::Running => "RUNNING",
        ToolResultState::Interrupted => "INTERRUPTED",
        ToolResultState::Suspended => "SUSPENDED",
    }
}

/// Emits a ToolResultEnd event through the delta_tx channel.
///
/// `metadata` is attached verbatim under the `metadata` field so transport layers
/// (e.g. the ACP mapper) can surface structured side data such as file diffs.
fn emit_tool_result_end(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    tool_call_id: &str,
    tool_name: &str,
    state: ToolResultState,
    metadata: &JsonMap,
) {
    let mut fields = JsonMap::new();
    fields.insert(
        "tool_call_id".to_string(),
        serde_json::Value::String(tool_call_id.to_string()),
    );
    fields.insert(
        "tool_name".to_string(),
        serde_json::Value::String(tool_name.to_string()),
    );
    fields.insert(
        "state".to_string(),
        serde_json::Value::String(tool_result_state_str(state).to_string()),
    );
    if !metadata.is_empty() {
        fields.insert(
            "metadata".to_string(),
            serde_json::to_value(metadata).unwrap_or(serde_json::Value::Null),
        );
    }
    emit_delta(
        delta_tx,
        AgentEvent::with_fields(AgentEventType::ToolResultEnd, fields),
    );
}

/// Emits a `RequireExternalExecution` event through the delta_tx channel for a tool
/// call that suspended itself pending out-of-process execution.
///
/// Java #3435: this is the **single** signal for a suspended call — no
/// ToolResultTextDelta/ToolResultEnd is emitted for it, because the framework must
/// not fabricate a result it never produced. The event carries the suspended call's
/// id/name (flat fields, matching the other tool events) plus the suspend reason and,
/// mirroring Java `RequireExternalExecutionEvent.getToolCalls()`, the call itself under
/// `tool_calls` in the same shape the `REQUIRE_USER_CONFIRM` payload uses.
fn emit_require_external_execution(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    reply_id: &str,
    call: &ToolUseBlock,
    reason: Option<&str>,
) {
    let mut fields = JsonMap::new();
    fields.insert(
        "reply_id".to_string(),
        serde_json::Value::String(reply_id.to_string()),
    );
    fields.insert(
        "tool_call_id".to_string(),
        serde_json::Value::String(call.id.clone()),
    );
    fields.insert(
        "tool_name".to_string(),
        serde_json::Value::String(call.name.clone()),
    );
    if let Some(reason) = reason.filter(|r| !r.is_empty()) {
        fields.insert(
            "reason".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
    }
    fields.insert(
        "tool_calls".to_string(),
        serde_json::to_value(std::slice::from_ref(call))
            .unwrap_or(serde_json::Value::Array(Vec::new())),
    );
    emit_delta(
        delta_tx,
        AgentEvent::with_fields(AgentEventType::RequireExternalExecution, fields),
    );
}

/// The id of the current turn's reply: the last assistant message in the context.
///
/// Acting runs after the reasoning phase appended that message, so this is the reply
/// whose tool calls are being executed (Java correlates the event through
/// `RequireExternalExecutionEvent.getReplyId()`).
fn current_reply_id(state: &AgentState) -> String {
    state
        .context()
        .iter()
        .rev()
        .find(|m| m.role() == MsgRole::Assistant)
        .map(|m| m.id().to_string())
        .unwrap_or_default()
}

/// Builds a text/thinking delta event carrying the given incremental text.
fn delta_event(event_type: AgentEventType, delta: &str) -> AgentEvent {
    let mut fields = JsonMap::new();
    fields.insert(
        "delta".to_string(),
        serde_json::Value::String(delta.to_string()),
    );
    AgentEvent::with_fields(event_type, fields)
}

/// State machine tracking the start/end lifecycle of text, thinking, and tool-call
/// blocks during model streaming. Mirrors the Java `ModelCallBlockLifecycle`.
///
/// Each block type follows a Start → Delta(s) → End cadence. When a new block
/// type starts, the previous one is automatically flushed (end event emitted).
struct BlockLifecycle {
    reply_id: String,
    text_started: bool,
    thinking_started: bool,
    started_tool_calls: std::collections::HashMap<String, String>,
}

impl BlockLifecycle {
    fn new(reply_id: String) -> Self {
        Self {
            reply_id,
            text_started: false,
            thinking_started: false,
            started_tool_calls: std::collections::HashMap::new(),
        }
    }

    fn block_field(block_type: &str) -> serde_json::Value {
        serde_json::Value::String(block_type.to_string())
    }

    fn start_text(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        self.flush_thinking(emit);
        if !self.text_started {
            self.text_started = true;
            let mut fields = JsonMap::new();
            fields.insert(
                "reply_id".to_string(),
                serde_json::Value::String(self.reply_id.clone()),
            );
            fields.insert("block_type".to_string(), Self::block_field("text"));
            emit(Ok(AgentEvent::with_fields(
                AgentEventType::TextBlockStart,
                fields,
            )));
        }
    }

    fn start_thinking(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        if !self.thinking_started {
            self.thinking_started = true;
            let mut fields = JsonMap::new();
            fields.insert(
                "reply_id".to_string(),
                serde_json::Value::String(self.reply_id.clone()),
            );
            fields.insert("block_type".to_string(), Self::block_field("thinking"));
            emit(Ok(AgentEvent::with_fields(
                AgentEventType::ThinkingBlockStart,
                fields,
            )));
        }
    }

    fn start_tool_call(
        &mut self,
        tool_id: &str,
        tool_name: &str,
        emit: &mut impl FnMut(Result<AgentEvent>),
    ) {
        if tool_id.is_empty() || self.started_tool_calls.contains_key(tool_id) {
            return;
        }
        self.flush_text(emit);
        self.flush_thinking(emit);
        self.flush_all_tool_calls(emit);
        let visible = !tool_name.starts_with("__");
        if visible && !self.started_tool_calls.contains_key(tool_id) {
            self.started_tool_calls
                .insert(tool_id.to_string(), tool_name.to_string());
            let mut fields = JsonMap::new();
            fields.insert(
                "reply_id".to_string(),
                serde_json::Value::String(self.reply_id.clone()),
            );
            fields.insert(
                "tool_call_id".to_string(),
                serde_json::Value::String(tool_id.to_string()),
            );
            fields.insert(
                "tool_name".to_string(),
                serde_json::Value::String(tool_name.to_string()),
            );
            emit(Ok(AgentEvent::with_fields(
                AgentEventType::ToolCallStart,
                fields,
            )));
        }
    }

    fn flush_text(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        if self.text_started {
            self.text_started = false;
            let mut fields = JsonMap::new();
            fields.insert(
                "reply_id".to_string(),
                serde_json::Value::String(self.reply_id.clone()),
            );
            fields.insert("block_type".to_string(), Self::block_field("text"));
            emit(Ok(AgentEvent::with_fields(
                AgentEventType::TextBlockEnd,
                fields,
            )));
        }
    }

    fn flush_thinking(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        if self.thinking_started {
            self.thinking_started = false;
            let mut fields = JsonMap::new();
            fields.insert(
                "reply_id".to_string(),
                serde_json::Value::String(self.reply_id.clone()),
            );
            fields.insert("block_type".to_string(), Self::block_field("thinking"));
            emit(Ok(AgentEvent::with_fields(
                AgentEventType::ThinkingBlockEnd,
                fields,
            )));
        }
    }

    fn flush_all_tool_calls(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        for (tool_id, tool_name) in self.started_tool_calls.drain() {
            let mut fields = JsonMap::new();
            fields.insert(
                "reply_id".to_string(),
                serde_json::Value::String(self.reply_id.clone()),
            );
            fields.insert(
                "tool_call_id".to_string(),
                serde_json::Value::String(tool_id),
            );
            fields.insert(
                "tool_name".to_string(),
                serde_json::Value::String(tool_name),
            );
            emit(Ok(AgentEvent::with_fields(
                AgentEventType::ToolCallEnd,
                fields,
            )));
        }
    }

    fn flush_all(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        self.flush_text(emit);
        self.flush_thinking(emit);
        self.flush_all_tool_calls(emit);
    }
}

/// Calls the raw model stream, forwarding each text/thinking delta as an agent
/// event (so callers can render incrementally) while accumulating the chunks into
/// a single assistant message.
///
/// Emits the full block lifecycle: ModelCallStart → (TextBlockStart → TextBlockDelta(s) → TextBlockEnd)
/// / (ThinkingBlockStart → ThinkingBlockDelta(s) → ThinkingBlockEnd)
/// / (ToolCallStart → ToolCallDelta(s) → ToolCallEnd) → ModelCallEnd.
///
/// `emit_reaches_consumer` states whether events handed to `emit` actually reach a
/// downstream consumer. Retry suppression after partial output depends on it: the
/// structured-output path passes a discarding emitter, so its text chunks are not
/// visible to anyone and its retries must keep working.
async fn model_call_impl(
    core: &Arc<AgentCore>,
    ctx: Option<&crate::agent::RuntimeContext>,
    mc_input: ModelCallInput,
    emit_reaches_consumer: bool,
    mut emit: impl FnMut(Result<AgentEvent>),
) -> Result<Msg> {
    let mut model = Arc::clone(&mc_input.model);
    let messages = mc_input.messages;
    let tools = mc_input.tools;
    let options = mc_input.options;

    // Model-level retry policy (429 / 5xx / IO with exponential backoff).
    // The standard model defaults (7 attempts, 2s→60s backoff) apply unless an explicit
    // execution config was set on the request.
    let cfg = options
        .execution_config
        .clone()
        .unwrap_or_else(ExecutionConfig::model_defaults);
    let max_attempts = cfg.max_attempts.unwrap_or(1).max(1);

    // Generate a reply id for this model call, used for block lifecycle correlation.
    let reply_id = uuid::Uuid::new_v4().to_string().replace('-', "");

    // Whether this call has already streamed **visible** output (assistant text) to
    // the consumer. A retry replays the request from scratch, so a retry after a
    // partial answer would render that prefix twice. Call-local state on purpose:
    // putting it on `RuntimeContext` or the agent would leak across calls (and, on
    // the agent, across concurrent runs).
    //
    // Only text deltas count. Reasoning/thinking deltas are not rendered as answer
    // text, so a retry after them cannot duplicate anything the user reads; tool-call
    // argument deltas are likewise excluded to keep the suppression as narrow as the
    // problem it fixes.
    let mut visible_output_emitted = false;

    let mut attempt: u32 = 0;
    // Whether the call already switched from the primary model to the fallback; the chain
    // switches at most once per model call.
    let mut using_fallback = false;
    loop {
        let mut acc = ReasoningContext::default();
        let mut block_lifecycle = BlockLifecycle::new(reply_id.clone());

        // Emit ModelCallStart at the beginning of each attempt.
        {
            let mut fields = JsonMap::new();
            fields.insert(
                "reply_id".to_string(),
                serde_json::Value::String(reply_id.clone()),
            );
            emit(Ok(AgentEvent::with_fields(
                AgentEventType::ModelCallStart,
                fields,
            )));
        }

        let mut stream = model.stream(messages.clone(), tools.clone(), options.clone());
        let consume = async {
            loop {
                // An interrupt has to land while the provider is still streaming. Waiting on
                // the stream alone would keep the run (and its journal writer lease) alive
                // until the provider answered — for a stalled provider, forever.
                let next = tokio::select! {
                    next = stream.next() => next,
                    _ = interrupt_pending(core, ctx) => {
                        // The predicate fired: surface the precise error (cancelled vs
                        // interrupted) instead of guessing which signal it was.
                        return check_interrupted(core, ctx).and(Err(Error::Interrupted(
                            "agent execution interrupted".into(),
                        )));
                    }
                };
                let Some(chunk) = next else { break };
                let chunk = chunk?;
                for block in &chunk.content {
                    match block {
                        ContentBlock::Text(t) => {
                            block_lifecycle.start_text(&mut emit);
                            if !t.text.is_empty() {
                                emit(Ok(delta_event(AgentEventType::TextBlockDelta, &t.text)));
                                // The delta has left the framework: retrying now would
                                // make the consumer render this prefix a second time.
                                visible_output_emitted |= emit_reaches_consumer;
                            }
                        }
                        ContentBlock::Thinking(t) => {
                            block_lifecycle.start_thinking(&mut emit);
                            if !t.thinking.is_empty() {
                                emit(Ok(delta_event(
                                    AgentEventType::ThinkingBlockDelta,
                                    &t.thinking,
                                )));
                            }
                        }
                        ContentBlock::ToolUse(tu) => {
                            block_lifecycle.start_tool_call(&tu.id, &tu.name, &mut emit);
                            let delta = match tu.content.as_deref().filter(|c| !c.is_empty()) {
                                Some(content) => Some(content.to_string()),
                                None => serde_json::to_string(&tu.input).ok().filter(|s| s != "{}"),
                            };
                            if let Some(delta) = delta {
                                let mut fields = JsonMap::new();
                                fields.insert(
                                    "reply_id".to_string(),
                                    serde_json::Value::String(reply_id.clone()),
                                );
                                fields.insert(
                                    "tool_call_id".to_string(),
                                    serde_json::Value::String(tu.id.clone()),
                                );
                                fields.insert(
                                    "tool_name".to_string(),
                                    serde_json::Value::String(tu.name.clone()),
                                );
                                fields
                                    .insert("delta".to_string(), serde_json::Value::String(delta));
                                emit(Ok(AgentEvent::with_fields(
                                    AgentEventType::ToolCallDelta,
                                    fields,
                                )));
                            }
                        }
                        _ => {}
                    }
                }
                acc.accumulate(chunk);
                // Checked after accumulating so an interrupt still preserves the
                // chunks already received (Java parity: its InterruptedException
                // branch persists `buildFinalMessage()`).
                check_interrupted(core, ctx)?;
            }
            Ok::<_, Error>(())
        };

        match consume.await {
            Ok(()) => {
                // Flush all remaining blocks and emit ModelCallEnd.
                block_lifecycle.flush_all(&mut emit);
                {
                    let mut fields = JsonMap::new();
                    fields.insert(
                        "reply_id".to_string(),
                        serde_json::Value::String(reply_id.clone()),
                    );
                    let usage = acc.usage.clone().unwrap_or_default();
                    fields.insert(
                        "usage".to_string(),
                        serde_json::to_value(&usage).unwrap_or_default(),
                    );
                    emit(Ok(AgentEvent::with_fields(
                        AgentEventType::ModelCallEnd,
                        fields,
                    )));
                }
                return Ok(acc.build_final_message());
            }
            Err(e) => {
                // The stream borrows the model: release it before a possible switch.
                drop(stream);
                attempt += 1;
                // Retrying is only safe while nothing has been shown to the user.
                // Once visible text has been streamed, re-running the request would
                // make the consumer render the same prefix again; surface the error
                // instead and let the caller decide.
                let retry_suppressed =
                    visible_output_emitted && e.is_retryable() && attempt < max_attempts;
                if retry_suppressed {
                    tracing::warn!(
                        model = model.model_name(),
                        attempt,
                        max_attempts,
                        error = %e,
                        "model call failed after partial output was emitted; retry suppressed"
                    );
                    return Err(retry_suppressed_error(e));
                }
                if attempt >= max_attempts || !e.is_retryable() {
                    // The primary model gave up: switch to the configured fallback and
                    // give it its own (identical) retry budget. Interrupts and
                    // cancellations are not provider failures, so they never fail over;
                    // a call that already streamed visible output never does either
                    // (that case returned above, because replaying the request would
                    // render the prefix twice). Mirrors Java `ModelConfig.fallbackModel`.
                    if !using_fallback && !matches!(e, Error::Interrupted(_) | Error::Cancelled(_))
                    {
                        if let Some(fallback) = core.fallback_model.as_ref() {
                            if let Some(listener) = core.failover_listener.as_ref() {
                                listener.on_failover(model.model_name(), &e);
                            }
                            tracing::warn!(
                                primary = model.model_name(),
                                fallback = fallback.model_name(),
                                error = %e,
                                "primary model failed; switching to the fallback model"
                            );
                            model = Arc::clone(fallback);
                            using_fallback = true;
                            attempt = 0;
                            continue;
                        }
                    }
                    // Preserve what the model had already produced when the call was
                    // interrupted, so the turn can reconcile its dangling tool_use
                    // instead of silently dropping the partial response. Other
                    // failures drop it: a half-formed message must never be
                    // replayed to the provider. Mirrors Java's InterruptedException
                    // branch, which persists `buildFinalMessage()`.
                    if matches!(e, Error::Interrupted(_)) {
                        let partial = acc.build_final_message();
                        let has_content = partial.tool_use_blocks().next().is_some()
                            || !partial.text_content().is_empty();
                        if has_content {
                            *core.partial_assistant.lock().unwrap() = Some(partial);
                        }
                    }
                    return Err(e);
                }
                let delay = cfg.backoff_for(attempt - 1);
                tracing::warn!(
                    model = model.model_name(),
                    attempt,
                    max_attempts,
                    delay_ms = delay.as_millis(),
                    error = %e,
                    "model call failed; retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Marks a model failure whose retry was skipped because part of the answer had
/// already been streamed to the consumer.
///
/// The variant is preserved where it carries information (an `HttpStatus` keeps its
/// status code) so callers can still classify the failure; the note explains why the
/// retry policy did not fire. `is_retryable` stays truthful about the *transport*
/// error, but callers must not re-run the call — the guard exists precisely to stop
/// duplicated output.
fn retry_suppressed_error(e: Error) -> Error {
    const NOTE: &str = "retry suppressed: partial output already emitted";
    match e {
        Error::HttpStatus { status, message } => Error::HttpStatus {
            status,
            message: format!("{message} ({NOTE})"),
        },
        Error::Http(message) => Error::Http(format!("{message} ({NOTE})")),
        Error::Io(err) => Error::Io(std::io::Error::new(err.kind(), format!("{err} ({NOTE})"))),
        other => Error::Agent(format!("{other} ({NOTE})")),
    }
}

/// Builds the chunk callback handed to a tool through its emitter.
///
/// Every emitted chunk is published as one [`AgentEvent::ToolProgress`] per output block,
/// matching Java's `ToolProgressEvent` emission in `ReActAgent.internalChunkCallback`
/// (each block of `chunk.getOutput()` becomes its own event, carrying the chunk metadata).
///
/// Progress is display-only: per the `ToolEmitter` contract it never reaches the model. The
/// tool's return value travels separately as `TOOL_RESULT_TEXT_DELTA`, so consumers must not
/// fold these events into a tool result.
///
/// Returns `None` when the call has no event channel (e.g. structured-output retries), in
/// which case the emitter silently discards chunks.
fn tool_chunk_callback(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    call: &ToolUseBlock,
    reply_id: &str,
) -> Option<crate::tool::ChunkCallback> {
    let tx = delta_tx.clone()?;
    let id = call.id.clone();
    let name = call.name.clone();
    let reply_id = reply_id.to_string();
    Some(Arc::new(move |chunk: ToolResultBlock| {
        // Java attaches `chunk.getMetadata()` to each progress event; an empty map is
        // simply omitted from the serialized event.
        let metadata = chunk.metadata.clone();
        for block in chunk.output {
            let mut fields = JsonMap::new();
            fields.insert(
                "reply_id".to_string(),
                serde_json::Value::String(reply_id.clone()),
            );
            fields.insert(
                "tool_call_id".to_string(),
                serde_json::Value::String(id.clone()),
            );
            fields.insert(
                "tool_name".to_string(),
                serde_json::Value::String(name.clone()),
            );
            // Java carries the whole `ContentBlock`, not just its text, so non-text
            // progress (images, data blocks) survives the trip.
            fields.insert(
                "content".to_string(),
                serde_json::to_value(&block).unwrap_or(serde_json::Value::Null),
            );
            let event = AgentEvent::with_fields(AgentEventType::ToolProgress, fields)
                .with_metadata_map(metadata.clone());
            let _ = tx.unbounded_send(Ok(event));
        }
    }))
}

/// Builds the [`ToolExecutionContext`] for a tool call, including a streaming
/// emitter that knows the call id.
///
/// Java always hands each invocation its own `DefaultToolEmitter` built from the
/// `ToolUseBlock`, so a tool can read back the id of the call it is servicing
/// (`ToolEmitter.getToolCallId()`) as well as stream progress. Passing `None`
/// here would make the id unreachable and turn `emit` into a silent no-op.
fn tool_execution_context(
    call: &ToolUseBlock,
    runtime: JsonMap,
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    request_config: Option<Arc<crate::tool::ToolRequestConfig>>,
    reply_id: &str,
) -> ToolExecutionContext {
    ToolExecutionContext {
        tool_call: Some(call.clone()),
        runtime,
        emitter: Some(crate::tool::tool_emitter_for_call(
            call,
            tool_chunk_callback(delta_tx, call, reply_id),
        )),
        tool_request_config: request_config,
    }
}

/// The per-call tool view carried by the runtime context, if any.
fn tool_request_config(
    ctx: &crate::agent::RuntimeContext,
) -> Option<Arc<crate::tool::ToolRequestConfig>> {
    ctx.extension::<crate::tool::ToolRequestConfig>(
        crate::tool::tool_request_config::TOOL_REQUEST_CONFIG_CONTEXT_KEY,
    )
}

/// Builds the JSON runtime exposed to a tool call, carrying the call-scoped
/// identity (session / user / agent), the recent conversation history plus any
/// metadata the agent recorded.
fn tool_runtime(ctx: &crate::agent::RuntimeContext, history: &[Msg]) -> JsonMap {
    let mut rt = ctx.metadata.clone();
    if let Some(sid) = ctx.session_id() {
        rt.insert(
            "sessionId".to_string(),
            serde_json::Value::String(sid.to_string()),
        );
    }
    if let Some(uid) = ctx.user_id() {
        rt.insert(
            "userId".to_string(),
            serde_json::Value::String(uid.to_string()),
        );
    }
    if let Some(aid) = ctx.agent_id() {
        rt.insert(
            "agentId".to_string(),
            serde_json::Value::String(aid.to_string()),
        );
    }
    // Tools that need "what was said so far" read the history from this entry instead of a
    // structural field on `ToolExecutionContext`. Only the tail is passed: Java hands the
    // whole history to every tool call, the runtime channel caps it to bound the per-call
    // serialization cost. No messages means no key at all.
    if !history.is_empty() {
        let start = history
            .len()
            .saturating_sub(crate::tool::MAX_CONVERSATION_HISTORY);
        if let Ok(value) = serde_json::to_value(&history[start..]) {
            rt.insert(crate::tool::CONVERSATION_HISTORY_KEY.to_string(), value);
        }
    }
    rt
}

/// Executes the pending tool calls from the last assistant message and feeds the
/// results back into the context. The outer loop decides whether to continue.
///
/// Beyond the fed-back results, the phase reports whether a `returnDirect`
/// short-circuit applies (every executed tool declared it and succeeded). In that
/// case the results written to the context are replaced by placeholder-shaped
/// copies — the real payload travels back in [`ActingOutcome::results`] and becomes
/// the closing assistant message — keeping the context invariant
/// `[tool_use, tool_result(placeholder), assistant(full result)]`. Mirrors Java
/// `acting` + `notifyPostActingHook`.
async fn acting_phase(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    state: &mut AgentState,
) -> Result<ActingOutcome> {
    check_interrupted(core, Some(ctx))?;
    let pending = extract_pending_tool_calls(state);
    if pending.is_empty() {
        return Ok(ActingOutcome::default());
    }

    let reply_id = current_reply_id(state);
    let runtime = tool_runtime(ctx, state.context());
    let output_limit_reached = last_assistant_reached_output_limit(state);
    // Plan-mode enforcement (mirrors Java `PlanModeMiddleware.onActing`) is the only
    // acting decision that reads `AgentState`; resolve it up front so the execution
    // core runs without borrowing the state — that borrow is what would otherwise pin
    // the core to this frame and stop a middleware from driving it.
    let plan_denied: Vec<(String, bool)> = pending
        .iter()
        .map(|call| (call.id.clone(), plan_mode_denies(core, state, call)))
        .collect();
    let results = ActingResultsHandle::new();

    // A tool execution is the one boundary where a crash must never be recovered by replaying
    // the tool: the side effects may already have happened. Checkpoint the assistant message
    // that carries the tool calls *before* running them, and record every dispatch as an
    // uncertain outcome. A later recovery then reconciles them as interrupted instead of
    // re-executing or silently dropping the call.
    if let Some(recorder) = SessionRecorder::from_context(ctx) {
        recorder
            .checkpoint_now(state, "before_acting")
            .await
            .map_err(|error| Error::ActionObservation(error.to_string()))?;
        for call in &pending {
            recorder
                .record(
                    "tool/dispatch",
                    &serde_json::json!({
                        "toolCallId": call.id,
                        "toolName": call.name,
                    }),
                )
                .await
                .map_err(|error| Error::ActionObservation(error.to_string()))?;
        }
    }

    // A middleware may stop the turn from inside its acting stream (Java `RequestStopEvent`
    // observed by `acting().doOnNext`). Kept outside the branch so the single-pass path leaves
    // it as `None`.
    let mut stop: Option<GenerateReason> = None;
    if core
        .middleware_chain
        .participants(ExtensionPoint::OnActing)
        .is_empty()
    {
        // No middleware participates: keep the single-pass path — events go straight to
        // `delta_tx`, with no channel hop and no spawned task.
        let batch = ActingBatch {
            core,
            ctx,
            reply_id: &reply_id,
            runtime: &runtime,
            plan_denied: &plan_denied,
            cancel: None,
            output_limit_reached,
        };
        run_acting_calls(batch, &pending, &results).await?;
    } else {
        let mut stream = core.middleware_chain.wrap_acting(
            ctx.clone(),
            ActingInput::with_results(pending.clone(), results.clone()),
            acting_stream_core(
                core,
                ctx,
                &reply_id,
                &runtime,
                &plan_denied,
                output_limit_reached,
            ),
        );
        while let Some(item) = stream.next().await {
            // A stop request ends the phase: the rest of the stream (the remaining tool
            // calls, their post-acting hooks) must not run. The event is still forwarded so
            // observers see the request, and dropping the stream aborts the acting task.
            if let Ok(AgentEvent::RequestStop(data)) = &item {
                stop = Some(stop_reason(data, GenerateReason::ActingStopRequested));
                if let Some(tx) = &ctx.delta_tx {
                    let _ = tx.unbounded_send(item);
                }
                break;
            }
            if let Some(tx) = &ctx.delta_tx {
                let _ = tx.unbounded_send(item);
            }
        }
        // Dropping the stream cancels the detached acting task (see `ActingTaskGuard`).
        drop(stream);
    }

    let snapshot = results.snapshot();
    if let Some(message) = snapshot.error {
        return Err(Error::Agent(message));
    }
    // A stopped phase is a completed turn, not a broken one: whatever ran before the stop is
    // kept (paired with its `tool_use`) and the turn ends with the requested reason. Without
    // this the empty result set used to surface as "acting phase produced no result".
    if let Some(reason) = stop {
        let message = stop_message(ctx, &pending, &snapshot.results, reason);
        // A stopped phase still owes an answer for the calls that already ran: persist their
        // results so the context holds no dangling `tool_use` a resumed turn would trip over.
        // The closing message itself is *not* persisted (Java returns it the same way).
        if !snapshot.results.is_empty() {
            let result_msg = Msg::builder()
                .role(MsgRole::Tool)
                .content(
                    snapshot
                        .results
                        .iter()
                        .cloned()
                        .map(ContentBlock::ToolResult)
                        .collect(),
                )
                .build();
            state.context_mut().push(result_msg);
        }
        return Ok(ActingOutcome {
            results: snapshot.results,
            stopped: Some(message),
            ..Default::default()
        });
    }
    // A short-circuit needs the core to have run to completion: an offloaded phase (a
    // middleware completed the stream first) must not lift results into the closing
    // assistant message.
    let return_direct = snapshot.finished
        && snapshot.return_direct_eligible
        && !snapshot.suspended
        && snapshot.executed > 0;
    if snapshot.results.is_empty() {
        return Err(Error::Agent(format!(
            "acting phase produced no result for {} pending tool call(s): a middleware \
             completed the stream without executing them or recording placeholders",
            pending.len()
        )));
    }
    let results = snapshot.results;

    // A returnDirect batch short-circuits only when at least one tool ran and every
    // executed result was a returnDirect success; a suspended/denied/errored result
    // disqualifies the whole batch (Java: `pendingPairs.isEmpty() && !successPairs.isEmpty()
    // && allMatch(isReturnDirectToolCall)`).
    //
    // On a short-circuit the payload moves to the closing assistant message, so the
    // context gets placeholder-shaped copies instead. The messages are built from the
    // pre-hook execution results (`results`), which is what `ActingOutcome::results`
    // also carries back to the caller.
    let persisted: Vec<ContentBlock> = if return_direct {
        results
            .iter()
            .map(|rb| ContentBlock::ToolResult(placeholder_result_block(rb)))
            .collect()
    } else {
        results
            .iter()
            .cloned()
            .map(ContentBlock::ToolResult)
            .collect()
    };
    let result_msg = Msg::builder()
        .role(MsgRole::Tool)
        .content(persisted)
        .build();
    state.context_mut().push(result_msg);
    // Persist the settled tool results immediately. Waiting for the next `before_model`
    // checkpoint leaves the whole settle window unprotected: a crash in between replays
    // every `tool/dispatch` as uncertain even though the tools already ran and returned.
    if let Some(recorder) = SessionRecorder::from_context(ctx) {
        recorder
            .checkpoint_now(state, "after_acting")
            .await
            .map_err(|error| Error::ActionObservation(error.to_string()))?;
    }
    Ok(ActingOutcome {
        suspended: snapshot.suspended,
        return_direct,
        results,
        stopped: None,
    })
}

/// Guard that aborts an acting task when the consuming stream goes away.
///
/// The acting core runs on its own task so a middleware can wrap the event stream. Without
/// this guard, dropping the stream (a cancelled run, a dropped prompt future) leaves the tool
/// task running and writing against a recorder that is closing.
struct ActingTaskGuard {
    handle: Option<tokio::task::JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}

impl ActingTaskGuard {
    fn new(handle: tokio::task::JoinHandle<()>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            handle: Some(handle),
            cancel,
        }
    }
}

impl Drop for ActingTaskGuard {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Wraps an event stream so the acting task is aborted when the stream is dropped.
struct ActingStream {
    inner: EventStream,
    _guard: ActingTaskGuard,
}

impl futures::Stream for ActingStream {
    type Item = crate::error::Result<AgentEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Builds the acting core handed to [`MiddlewareChain::wrap_acting`].
///
/// The core runs the tool calls on a spawned task and streams their events through an
/// unbounded channel; [`ActingInput::results`] carries the outcome back. This mirrors
/// Java `ReActAgent.acting`, where `actingStream` writes into an `AtomicReference`
/// result holder and the middleware chain wraps the event flux.
fn acting_stream_core(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    reply_id: &str,
    runtime: &JsonMap,
    plan_denied: &[(String, bool)],
    output_limit_reached: bool,
) -> Box<dyn FnOnce(ActingInput) -> EventStream + Send> {
    let core = Arc::clone(core);
    let ctx = ctx.clone();
    let reply_id = reply_id.to_string();
    let runtime = runtime.clone();
    let plan_denied = plan_denied.to_vec();
    Box::new(move |input: ActingInput| {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let mut inner_ctx = ctx;
        inner_ctx.delta_tx = Some(tx);
        let results = input.results.clone();
        let calls = input.tool_calls;
        // The task must die with the stream: a cancelled run drops the stream but the tool
        // would otherwise keep writing against a recorder that is closing.
        let cancel = Arc::new(AtomicBool::new(false));
        let task_cancel = Arc::clone(&cancel);
        let handle = tokio::spawn(async move {
            if task_cancel.load(Ordering::SeqCst) {
                return;
            }
            let batch = ActingBatch {
                core: &core,
                ctx: &inner_ctx,
                reply_id: &reply_id,
                runtime: &runtime,
                plan_denied: &plan_denied,
                cancel: Some(&task_cancel),
                output_limit_reached,
            };
            if let Err(e) = run_acting_calls(batch, &calls, &results).await {
                results.fail(e.to_string());
            }
        });
        let guard = ActingTaskGuard::new(handle, cancel);
        Box::pin(ActingStream {
            inner: Box::pin(rx),
            _guard: guard,
        })
    })
}

/// One acting batch's per-run context, bundled so the entry points stay readable.
struct ActingBatch<'a> {
    core: &'a Arc<AgentCore>,
    ctx: &'a crate::agent::RuntimeContext,
    reply_id: &'a str,
    runtime: &'a JsonMap,
    plan_denied: &'a [(String, bool)],
    cancel: Option<&'a AtomicBool>,
    /// Whether the assistant message that requested these calls stopped at the
    /// output-token limit (selects the rejection text for malformed calls).
    output_limit_reached: bool,
}

impl ActingBatch<'_> {
    /// Runs one tool under the observation boundary and returns its result block.
    ///
    /// An acknowledgement failure is returned as-is: the caller turns it into a run-level
    /// error instead of a retryable tool result.
    async fn observe(
        &self,
        call: &ToolUseBlock,
        tctx: ToolExecutionContext,
    ) -> Result<ToolResultBlock> {
        crate::observation::observe(self.ctx, call, self.core.toolkit.call_tool(call, tctx)).await
    }

    /// Whether `call` may join a concurrent batch: it must be executable (not
    /// malformed), not denied by plan mode, approved by the permission engine, and the
    /// tool itself must declare concurrent invocations safe.
    fn is_parallel_eligible(&self, call: &ToolUseBlock) -> bool {
        if call.has_malformed_arguments() {
            return false;
        }
        if self
            .plan_denied
            .iter()
            .any(|(id, denied)| id == &call.id && *denied)
        {
            return false;
        }
        if !self.core.toolkit.is_concurrency_safe_tool(&call.name) {
            return false;
        }
        matches!(
            evaluate_permission(self.core, call).behavior,
            PermissionBehavior::Allow | PermissionBehavior::Passthrough
        )
    }
}

/// Executes the pending tool calls and records every result into `results`.
///
/// Split out of [`acting_phase`] so the work can run on a spawned task: it takes no
/// `&mut AgentState` (plan-mode denials are pre-computed by the caller) and reports
/// through `results` instead of returning them.
///
/// The batch parameters travel as one [`ActingBatch`] so the entry point stays readable.
async fn run_acting_calls(
    run: ActingBatch<'_>,
    pending: &[ToolUseBlock],
    results: &ActingResultsHandle,
) -> Result<()> {
    let mut index = 0;
    while index < pending.len() {
        let call = &pending[index];
        // A cancelled run must not start the next tool: the first one may have completed, but
        // continuing the batch would perform side effects after the user asked to stop.
        if run
            .cancel
            .is_some_and(|cancel| cancel.load(Ordering::SeqCst))
        {
            return Err(Error::Cancelled("execution cancelled by the caller".into()));
        }
        check_interrupted(run.core, Some(run.ctx))?;
        // A call whose streamed arguments never formed a valid JSON object (typically
        // truncated by a max-tokens stop) is not executable. Synthesize an ERROR result
        // with the original call id so tool_use ↔ tool_result pairing stays complete,
        // and never hand it to the permission gate, middleware chain or executor. The
        // loop continues to the next reasoning iteration; no automatic retry is
        // attempted this turn. Mirrors Java `ReActAgent.writeMalformedArgumentResults`.
        if call.has_malformed_arguments() {
            let message = if run.output_limit_reached {
                OUTPUT_LIMIT_TOOL_ARGUMENTS
            } else {
                INVALID_TOOL_ARGUMENTS
            };
            emit_tool_result_start(&run.ctx.delta_tx, &call.id, &call.name);
            emit_tool_result_text_delta(&run.ctx.delta_tx, &call.id, &call.name, message);
            results.record(error_result(call, message), false);
            emit_tool_result_end(
                &run.ctx.delta_tx,
                &call.id,
                &call.name,
                ToolResultState::Error,
                &JsonMap::new(),
            );
            index += 1;
            continue;
        }
        // Plan-mode enforcement (mirrors Java `PlanModeMiddleware.onActing`): while plan
        // mode is active, only read-only tools and the always-allowed orchestration/plan
        // tools may run; every other call gets a synthetic DENIED result without executing.
        let denied_by_plan = run
            .plan_denied
            .iter()
            .any(|(id, denied)| id == &call.id && *denied);
        if denied_by_plan {
            emit_tool_result_start(&run.ctx.delta_tx, &call.id, &call.name);
            emit_tool_result_text_delta(
                &run.ctx.delta_tx,
                &call.id,
                &call.name,
                PLAN_MODE_DENY_MESSAGE,
            );
            results.record(denied_result(call, PLAN_MODE_DENY_MESSAGE), false);
            emit_tool_result_end(
                &run.ctx.delta_tx,
                &call.id,
                &call.name,
                ToolResultState::Denied,
                &JsonMap::new(),
            );
            index += 1;
            continue;
        }
        let decision = evaluate_permission(run.core, call);
        match decision.behavior {
            PermissionBehavior::Deny => {
                // Emit ToolResultStart → TextDelta → End for denied calls.
                emit_tool_result_start(&run.ctx.delta_tx, &call.id, &call.name);
                let denied_text = "Permission denied by rules";
                emit_tool_result_text_delta(&run.ctx.delta_tx, &call.id, &call.name, denied_text);
                results.record(denied_result(call, &decision.message), false);
                emit_tool_result_end(
                    &run.ctx.delta_tx,
                    &call.id,
                    &call.name,
                    ToolResultState::Denied,
                    &JsonMap::new(),
                );
                index += 1;
            }
            PermissionBehavior::Allow | PermissionBehavior::Passthrough => {
                // Parallel mode (Java `ToolkitConfig.parallel`, default true) runs a
                // contiguous run of concurrency-safe, approved calls concurrently. Each
                // tool's events are emitted at its own actual start/finish, so a fast
                // call is visible while a slower sibling is still running; results are
                // still recorded in declaration order and the batch settles as a unit.
                // Non-concurrency-safe calls, asks and denials fall through to the
                // serial path below (matching Java's serial slots).
                if run.core.toolkit.config().parallel {
                    let mut end = index + 1;
                    while end < pending.len() && run.is_parallel_eligible(&pending[end]) {
                        end += 1;
                    }
                    if end > index + 1 {
                        run_parallel_batch(&run, &pending[index..end], results).await?;
                        index = end;
                        continue;
                    }
                }
                // Emit ToolResultStart for this tool execution.
                emit_tool_result_start(&run.ctx.delta_tx, &call.id, &call.name);
                let tctx = tool_execution_context(
                    call,
                    run.runtime.clone(),
                    &run.ctx.delta_tx,
                    tool_request_config(run.ctx),
                    run.reply_id,
                );
                // Record the action lifecycle around the real tool call. An acknowledgement
                // failure is a run-level error: it must never become a retryable tool result.
                match run.observe(call, tctx).await {
                    Ok(rb) => {
                        // Emit text delta for non-streaming results (streaming tools
                        // emit their own deltas via the emitter callback).
                        let mut result_text = String::new();
                        for block in &rb.output {
                            if let ContentBlock::Text(t) = block {
                                result_text.push_str(&t.text);
                            }
                        }
                        if !result_text.trim().is_empty() {
                            emit_tool_result_text_delta(
                                &run.ctx.delta_tx,
                                &call.id,
                                &call.name,
                                &result_text,
                            );
                        }
                        // Java #3435: a suspended call must not fabricate a result — no
                        // ToolResultEnd event is emitted (the external-execution requirement
                        // is the single signal). The block still carries `Suspended` state.
                        let was_suspended = rb.is_suspended();
                        let suspend_reason = rb.suspend_reason().map(str::to_string);
                        let final_state = rb.final_state();
                        let metadata = rb.metadata.clone();
                        let mut bound = bind_result(call, rb);
                        let rd = is_return_direct_result(run.core, &bound);
                        strip_event_only_metadata(run.core, &mut bound);
                        results.record(bound, rd);
                        if was_suspended {
                            emit_require_external_execution(
                                &run.ctx.delta_tx,
                                run.reply_id,
                                call,
                                suspend_reason.as_deref(),
                            );
                        } else {
                            emit_tool_result_end(
                                &run.ctx.delta_tx,
                                &call.id,
                                &call.name,
                                final_state,
                                &metadata,
                            );
                        }
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        // A failed **end** acknowledgement means the *result* could not be
                        // persisted, not that the tool failed. Replaying it would repeat real
                        // side effects, so the run fails with the real error and the tool stays
                        // uncertain in the journal (recovery must reconcile it explicitly).
                        // A failed **start** acknowledgement ([`Error::ActionDispatch`]) is the
                        // opposite: the tool never ran, so it falls through to the retryable
                        // tool-error path below.
                        if matches!(e, Error::ActionObservation(_)) {
                            results.fail(err_msg.clone());
                            emit_tool_result_end(
                                &run.ctx.delta_tx,
                                &call.id,
                                &call.name,
                                ToolResultState::Error,
                                &JsonMap::new(),
                            );
                            return Err(e);
                        }
                        results.record(error_result(call, &err_msg), false);
                        emit_tool_result_text_delta(
                            &run.ctx.delta_tx,
                            &call.id,
                            &call.name,
                            &err_msg,
                        );
                        emit_tool_result_end(
                            &run.ctx.delta_tx,
                            &call.id,
                            &call.name,
                            ToolResultState::Error,
                            &JsonMap::new(),
                        );
                    }
                }
                index += 1;
            }
            PermissionBehavior::Ask => {
                let handler = run.core.confirm_handler.lock().unwrap().clone();
                if let Some(handler) = handler {
                    // Emit ToolResultStart so ACP clients know this tool is being
                    // processed (even though it's waiting for user confirmation).
                    emit_tool_result_start(&run.ctx.delta_tx, &call.id, &call.name);
                    let confirm_result = handler(call.clone()).await;
                    if confirm_result.is_confirmed() {
                        // Use the (possibly modified) tool call if the user tweaked input.
                        let effective_call = confirm_result.tool_call.as_ref().unwrap_or(call);
                        emit_tool_result_start(
                            &run.ctx.delta_tx,
                            &effective_call.id,
                            &effective_call.name,
                        );
                        let tctx = tool_execution_context(
                            effective_call,
                            run.runtime.clone(),
                            &run.ctx.delta_tx,
                            tool_request_config(run.ctx),
                            run.reply_id,
                        );
                        // Same observation boundary as the allow path: a failed
                        // acknowledgement is a run-level error, never a tool result.
                        match run.observe(effective_call, tctx).await {
                            Ok(rb) => {
                                let mut result_text = String::new();
                                for block in &rb.output {
                                    if let ContentBlock::Text(t) = block {
                                        result_text.push_str(&t.text);
                                    }
                                }
                                if !result_text.trim().is_empty() {
                                    emit_tool_result_text_delta(
                                        &run.ctx.delta_tx,
                                        &effective_call.id,
                                        &effective_call.name,
                                        &result_text,
                                    );
                                }
                                let was_suspended = rb.is_suspended();
                                let suspend_reason = rb.suspend_reason().map(str::to_string);
                                let final_state = rb.final_state();
                                let metadata = rb.metadata.clone();
                                let mut bound = bind_result(effective_call, rb);
                                let rd = is_return_direct_result(run.core, &bound);
                                strip_event_only_metadata(run.core, &mut bound);
                                results.record(bound, rd);
                                if was_suspended {
                                    emit_require_external_execution(
                                        &run.ctx.delta_tx,
                                        run.reply_id,
                                        effective_call,
                                        suspend_reason.as_deref(),
                                    );
                                } else {
                                    emit_tool_result_end(
                                        &run.ctx.delta_tx,
                                        &effective_call.id,
                                        &effective_call.name,
                                        final_state,
                                        &metadata,
                                    );
                                }
                            }
                            Err(e) => {
                                let err_msg = e.to_string();
                                if matches!(e, Error::ActionObservation(_)) {
                                    results.fail(err_msg.clone());
                                    emit_tool_result_end(
                                        &run.ctx.delta_tx,
                                        &effective_call.id,
                                        &effective_call.name,
                                        ToolResultState::Error,
                                        &JsonMap::new(),
                                    );
                                    return Err(e);
                                }
                                emit_tool_result_text_delta(
                                    &run.ctx.delta_tx,
                                    &effective_call.id,
                                    &effective_call.name,
                                    &err_msg,
                                );
                                results.record(error_result(effective_call, &err_msg), false);
                                emit_tool_result_end(
                                    &run.ctx.delta_tx,
                                    &effective_call.id,
                                    &effective_call.name,
                                    ToolResultState::Error,
                                    &JsonMap::new(),
                                );
                            }
                        }
                    } else {
                        // User denied the tool call.
                        let denied_text = &confirm_result.message;
                        emit_tool_result_text_delta(
                            &run.ctx.delta_tx,
                            &call.id,
                            &call.name,
                            denied_text,
                        );
                        results.record(denied_result(call, denied_text), false);
                        emit_tool_result_end(
                            &run.ctx.delta_tx,
                            &call.id,
                            &call.name,
                            ToolResultState::Denied,
                            &JsonMap::new(),
                        );
                    }
                } else {
                    return Err(Error::Agent(format!(
                        "permission asking (HITL) is not yet supported; tool '{}' requires user confirmation",
                        call.name
                    )));
                }
                index += 1;
            }
        }
    }

    results.finish();
    Ok(())
}

/// Runs a contiguous batch of approved, concurrency-safe calls concurrently.
///
/// Every invocation emits its own `ToolResultStart` when it actually begins and its
/// text delta / `ToolResultEnd` (or `RequireExternalExecution` when suspended) when it
/// actually finishes, so events arrive in completion order and a fast call is visible
/// while a slow earlier call is still running. Results are recorded in declaration
/// order afterwards: middleware, the persisted tool message and the next reasoning
/// iteration still observe one settled batch, exactly like Java's
/// `ToolExecutionCallback` + ordered aggregation.
async fn run_parallel_batch(
    run: &ActingBatch<'_>,
    calls: &[ToolUseBlock],
    results: &ActingResultsHandle,
) -> Result<()> {
    let futures = calls.iter().map(|call| async move {
        // Start is emitted immediately before the invocation itself (Java
        // `ToolExecutionCallback.onStart`), never at batch admission.
        emit_tool_result_start(&run.ctx.delta_tx, &call.id, &call.name);
        let tctx = tool_execution_context(
            call,
            run.runtime.clone(),
            &run.ctx.delta_tx,
            tool_request_config(run.ctx),
            run.reply_id,
        );
        // Same observation boundary as the serial path: a failed start acknowledgement is a
        // retryable tool error (the tool never ran); only a failed end acknowledgement is a
        // run-level error.
        match run.observe(call, tctx).await {
            Ok(rb) => {
                emit_result_text_delta(run, call, &rb);
                // Java #3435: a suspended call emits neither a text delta for its result
                // nor `ToolResultEnd`; the external-execution requirement is the signal.
                if rb.is_suspended() {
                    emit_require_external_execution(
                        &run.ctx.delta_tx,
                        run.reply_id,
                        call,
                        rb.suspend_reason(),
                    );
                } else {
                    emit_tool_result_end(
                        &run.ctx.delta_tx,
                        &call.id,
                        &call.name,
                        rb.final_state(),
                        &rb.metadata,
                    );
                }
                Ok((call, rb))
            }
            Err(error) => {
                let message = error.to_string();
                if !matches!(error, Error::ActionObservation(_)) {
                    emit_tool_result_text_delta(&run.ctx.delta_tx, &call.id, &call.name, &message);
                }
                emit_tool_result_end(
                    &run.ctx.delta_tx,
                    &call.id,
                    &call.name,
                    ToolResultState::Error,
                    &JsonMap::new(),
                );
                Err((call, error, message))
            }
        }
    });
    let outcomes = futures::future::join_all(futures).await;
    for outcome in outcomes {
        match outcome {
            Ok((call, rb)) => {
                let mut bound = bind_result(call, rb);
                let return_direct = is_return_direct_result(run.core, &bound);
                strip_event_only_metadata(run.core, &mut bound);
                results.record(bound, return_direct);
            }
            Err((call, error, message)) => {
                // An unacknowledged **end** is a run-level failure: the batch is not
                // committed and recovery must reconcile the call explicitly. A failed start
                // never ran the tool, so it is recorded as an ordinary tool error.
                if matches!(error, Error::ActionObservation(_)) {
                    results.fail(message);
                    return Err(error);
                }
                results.record(error_result(call, &message), false);
            }
        }
    }
    Ok(())
}

/// Emits the text delta for a completed non-streaming tool result. Streaming tools
/// emit their own deltas through the execution emitter callback.
fn emit_result_text_delta(run: &ActingBatch<'_>, call: &ToolUseBlock, rb: &ToolResultBlock) {
    let mut result_text = String::new();
    for block in &rb.output {
        if let ContentBlock::Text(text) = block {
            result_text.push_str(&text.text);
        }
    }
    if !result_text.trim().is_empty() {
        emit_tool_result_text_delta(&run.ctx.delta_tx, &call.id, &call.name, &result_text);
    }
}

/// Produces the termination message when the maximum number of iterations is hit.
///
/// Mirrors the Java `summarizing()`: handles pending tool calls that were not completed
/// before max iterations, and emits `ExceedMaxItersEvent`.
async fn summarizing(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    state: &mut AgentState,
) -> Result<Msg> {
    check_interrupted(core, Some(ctx))?;

    // Handle pending tool calls that were not completed before max iterations.
    let pending = extract_pending_tool_calls(state);
    if !pending.is_empty() {
        for tool_use in &pending {
            let error_result = ToolResultBlock {
                id: Some(tool_use.id.clone()),
                name: Some(tool_use.name.clone()),
                output: vec![ContentBlock::Text(TextBlock::new(format!(
                    "Tool execution cancelled because maximum iterations limit was reached",
                )))],
                metadata: Default::default(),
                state: ToolResultState::Error,
            };
            let result_msg = Msg::builder()
                .role(MsgRole::Tool)
                .content(vec![ContentBlock::ToolResult(error_result)])
                .build();
            state.context_mut().push(result_msg);
        }
    }

    let msg = Msg::builder()
        .role(MsgRole::Assistant)
        .content(vec![ContentBlock::Text(TextBlock::new(
            "Reached the maximum number of iterations; the task could not be completed.",
        ))])
        .generate_reason(GenerateReason::MaxIterations)
        .build();
    state.context_mut().push(msg.clone());
    Ok(msg)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Enforces the single-system-message invariant before the provider call.
///
/// `run_agent_loop` already freezes the request-scoped system prompt and keeps
/// `AgentState.context` system-free. A reasoning middleware can still push an extra
/// SYSTEM message into `ReasoningInput.messages`; Java rejects that shape. Merge any
/// such additions into one leading system message instead of leaking multiple
/// system entries to the provider.
fn normalize_system_messages(mut messages: Vec<Msg>) -> Vec<Msg> {
    let system_count = messages
        .iter()
        .filter(|message| message.role == MsgRole::System)
        .count();
    if system_count == 0 {
        return messages;
    }
    if system_count == 1
        && messages
            .first()
            .is_some_and(|message| message.role == MsgRole::System)
    {
        return messages;
    }

    let merged = messages
        .iter()
        .filter(|message| message.role == MsgRole::System)
        .map(Msg::text_content)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    messages.retain(|message| message.role != MsgRole::System);
    if !merged.trim().is_empty() {
        messages.insert(0, Msg::system(merged));
    }
    messages
}

/// Builds the `generate_response` tool's parameter schema wrapping the target JSON
/// schema under a single `response` property.
fn generate_response_parameters(schema: &JsonSchema) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert(
        "response".to_string(),
        serde_json::Value::Object(schema.schema.clone()),
    );
    let mut params = serde_json::Map::new();
    params.insert("type".to_string(), serde_json::json!("object"));
    params.insert("properties".to_string(), serde_json::Value::Object(props));
    params.insert("required".to_string(), serde_json::json!(["response"]));
    serde_json::Value::Object(params)
}

/// Builds the transient structured-output reminder message injected before the model
/// sees the input, instructing it to call `generate_response` with the target schema.
fn structured_output_reminder(schema: &JsonSchema) -> Msg {
    let mut metadata = JsonMap::new();
    metadata.insert(
        crate::message::keys::metadata_keys::SYNTHETIC.to_string(),
        serde_json::json!(true),
    );
    // The reminder is injected before request preparation now, so it is marked as a
    // synthetic reminder of kind `structured_output`: the context renderer keeps it
    // out of the canonical history it compares for compaction (Java 51a74e2ef).
    metadata.insert(
        crate::message::keys::metadata_keys::REMINDER_KIND.to_string(),
        serde_json::json!("structured_output"),
    );
    metadata.insert(
        crate::message::keys::metadata_keys::STRUCTURED_OUTPUT_REMINDER.to_string(),
        serde_json::json!(true),
    );
    Msg::builder()
        .role(MsgRole::User)
        .name("system")
        .text_content(format!(
            "Produce the final structured response by calling the `generate_response` tool.\nJSON schema:\n{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(schema.schema.clone()))
                .unwrap_or_default()
        ))
        .metadata(metadata)
        .build()
}

/// Aborts execution if the caller cancelled it, or if the call's own run was interrupted.
///
/// The caller's cancel signal wins: it is the only signal that means "stop this execution
/// now", so it also marks the run cancelled (the outcome must not be reported as a failure).
/// The interrupt check then reads the run that belongs to *this* call — carried in the call's
/// context — instead of the agent-level slot, which a concurrent call may have replaced.
fn check_interrupted(
    core: &Arc<AgentCore>,
    ctx: Option<&crate::agent::RuntimeContext>,
) -> Result<()> {
    let run = ctx.and_then(|c| c.extension::<RunControl>(RUN_CONTROL_CONTEXT_KEY));
    if let Some(signal) = ctx.and_then(|c| c.extension::<CancelSignal>(CANCEL_SIGNAL_CONTEXT_KEY)) {
        if signal.is_cancelled() {
            // The caller asked this execution to stop: mark the run cancelled so the
            // outcome is not reported as a failure.
            if let Some(run) = &run {
                run.cancel();
            }
            return Err(Error::Cancelled("execution cancelled by the caller".into()));
        }
    }
    let interrupted = match run {
        Some(run) => run.interruption().is_interrupted(),
        None => match core.active_run.lock().unwrap().as_ref() {
            Some(run) => run.interruption().is_interrupted(),
            None => core.interrupt.is_interrupted(),
        },
    };
    if interrupted {
        return Err(Error::Interrupted("agent execution interrupted".into()));
    }
    Ok(())
}

/// Awaits until the current execution is interrupted or cancelled.
///
/// Uses the same predicate as the phase boundaries, so a run cannot be interruptible in one
/// place and immune in another. Polling rather than a notification keeps
/// [`InterruptControl`] unchanged; the timer is dropped as soon as a chunk arrives.
async fn interrupt_pending(core: &Arc<AgentCore>, ctx: Option<&crate::agent::RuntimeContext>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if check_interrupted(core, ctx).is_err() {
            return;
        }
    }
}

/// Collects the still-unanswered tool-use blocks of the most recent assistant
/// message. Calls already answered by a later TOOL message are skipped: treating
/// them as pending would append a duplicate TOOL result (an id answered twice is
/// rejected by OpenAI-compatible providers with HTTP 400).
fn extract_pending_tool_calls(state: &AgentState) -> Vec<ToolUseBlock> {
    let context = state.context();
    let Some(owner) = context.iter().rposition(|m| m.role() == MsgRole::Assistant) else {
        return Vec::new();
    };
    let answered: HashSet<&str> = context[owner + 1..]
        .iter()
        .filter(|m| m.role() == MsgRole::Tool)
        .flat_map(|m| m.tool_result_blocks())
        .filter_map(|tr| tr.id.as_deref())
        .collect();
    context[owner]
        .tool_use_blocks()
        // Server-tool calls are executed by the provider and answered inline; they must
        // never be dispatched to the local toolkit (Java `extractPendingToolCalls` filter).
        .filter(|tu| !tu.is_server_tool())
        .filter(|tu| !answered.contains(tu.id.as_str()))
        .cloned()
        .collect()
}

/// Builds a TOOL-role message carrying a single tool-result block.
fn tool_result_msg(result: ToolResultBlock) -> Msg {
    Msg::builder()
        .role(MsgRole::Tool)
        .content(vec![ContentBlock::ToolResult(result)])
        .build()
}

/// Patches orphaned pending tool calls with synthetic error results so the next
/// turn can proceed from a consistent context instead of failing on a dangling
/// tool_use. No-op when the caller supplied its own tool results. Mirrors Java
/// `maybePatchPendingToolCalls`; gated by `enable_pending_tool_recovery` at the
/// call site.
fn patch_pending_tool_calls(state: &mut AgentState, messages: &[Msg]) {
    let pending = extract_pending_tool_calls(state);
    if pending.is_empty() {
        return;
    }
    // The caller provided tool results itself: let the normal path validate them.
    if messages
        .iter()
        .any(|m| m.tool_result_blocks().next().is_some())
    {
        return;
    }
    for call in pending {
        let result = error_result(
            &call,
            &format!(
                "Previous tool execution failed or was interrupted. Tool: {}",
                call.name
            ),
        );
        state.context_mut().push(tool_result_msg(result));
    }
}

/// Reconciles an interrupted turn and returns the recovery message.
///
/// The assistant message written by the interrupted reasoning phase may carry
/// tool_use blocks that never ran; those are answered with synthetic error
/// results (so the persisted context has no dangling tool call), then an
/// assistant recovery message is appended. Mirrors Java `handleInterrupt`.
fn reconcile_interrupted(state: &mut AgentState) -> Msg {
    for call in extract_pending_tool_calls(state) {
        let result = error_result(
            &call,
            &format!(
                "Tool execution was interrupted before it could run. Tool: {}",
                call.name
            ),
        );
        state.context_mut().push(tool_result_msg(result));
    }
    let recovery = Msg::builder()
        .role(MsgRole::Assistant)
        .content(vec![ContentBlock::Text(TextBlock::new(
            "I noticed that you have interrupted me. What can I do for you?",
        ))])
        .generate_reason(GenerateReason::Interrupted)
        .build();
    state.context_mut().push(recovery.clone());
    recovery
}

/// Tool names always permitted while plan mode is active, mirroring Java
/// `PlanModeMiddleware.ALWAYS_ALLOWED`: the plan-control tools plus the read-only
/// orchestration tools (sub-agent spawn/send/list, async task output/list).
const PLAN_MODE_ALWAYS_ALLOWED: &[&str] = &[
    "plan_enter",
    "plan_write",
    "plan_exit",
    "todo_write",
    "agent_spawn",
    "agent_send",
    "agent_list",
    "task_output",
    "task_list",
];

/// Message returned for a tool call denied by plan mode (mirrors Java `DENY_MESSAGE`).
const PLAN_MODE_DENY_MESSAGE: &str = "Blocked: you are in PLAN mode (read-only). You may \
investigate and run read-only tools, record your plan with plan_write, and call plan_exit when \
ready to execute. Do not modify files or run mutating commands until the plan is approved.";

/// Result text for a truncated call (mirrors Java `OUTPUT_LIMIT_TOOL_ARGUMENTS`).
const OUTPUT_LIMIT_TOOL_ARGUMENTS: &str =
    "Tool call not executed: the response stopped at its output token limit before complete \
     arguments could be parsed. Do not repeat the same large arguments. Split the work into \
     smaller tool calls or provide a shorter argument payload, then reissue the call.";

/// Result text for a corrupt call whose arguments were never a JSON object (mirrors Java
/// `INVALID_TOOL_ARGUMENTS`).
const INVALID_TOOL_ARGUMENTS: &str = "Tool call not executed: its arguments were not a valid \
     JSON object. Reissue the call with corrected JSON object arguments.";

/// Whether the last assistant message stopped at the output-token limit, which selects the
/// more specific rejection text for a malformed tool call. Mirrors Java
/// `MessageUtils.lastAssistantReachedOutputLimit`.
fn last_assistant_reached_output_limit(state: &AgentState) -> bool {
    state
        .context()
        .iter()
        .rev()
        .find(|message| message.role() == MsgRole::Assistant)
        .and_then(|message| {
            message
                .metadata()
                .get(crate::message::metadata_keys::OUTPUT_LIMIT_REACHED)
        })
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Whether plan mode forbids executing `call`: active plan mode denies everything except
/// read-only tools and [`PLAN_MODE_ALWAYS_ALLOWED`]. Mirrors Java
/// `PlanModeMiddleware.onActing`.
fn plan_mode_denies(core: &Arc<AgentCore>, state: &AgentState, call: &ToolUseBlock) -> bool {
    if !state.plan_mode_context.plan_active {
        return false;
    }
    if PLAN_MODE_ALWAYS_ALLOWED.contains(&call.name.as_str()) {
        return false;
    }
    let is_read_only = core
        .toolkit
        .get_tool(&call.name)
        .map(|t| t.is_read_only())
        .unwrap_or(false);
    !is_read_only
}

/// Evaluates the permission decision for a single tool invocation.
fn evaluate_permission(core: &Arc<AgentCore>, call: &ToolUseBlock) -> PermissionDecision {
    // An unconfigured (trivial) permission context allows all tool calls, so a
    // default ReActAgent works out of the box without explicit rules.
    let engine = core.permission_engine.lock().unwrap();
    if engine.context().is_trivial() {
        return PermissionDecision::allow(format!("Permission granted for {}", call.name));
    }
    let is_read_only = core
        .toolkit
        .get_tool(&call.name)
        .map(|t| t.is_read_only())
        .unwrap_or(false);
    let matcher = |pattern: &str, input: &JsonMap| {
        let serialized = serde_json::to_string(input).unwrap_or_default();
        serialized.contains(pattern)
    };
    engine.check_permission(&call.name, is_read_only, &call.input, matcher)
}

/// Binds a tool's raw result block back to the originating call.
///
/// The state is normalized through [`ToolResultBlock::final_state`] (Java
/// `determineToolResultState`) so the block persisted in the context carries the
/// real terminal state (`Success`/`Error`/`Suspended`) instead of the `Running`
/// default that built-in tools leave behind. Without it, history queries via the
/// agent state would always observe `RUNNING` (Java #2745).
fn bind_result(call: &ToolUseBlock, rb: ToolResultBlock) -> ToolResultBlock {
    let state = rb.final_state();
    ToolResultBlock {
        id: Some(call.id.clone()),
        name: Some(call.name.clone()),
        output: rb.output,
        metadata: rb.metadata,
        state,
    }
}

/// Drops metadata that exists only for the event stream before a result is archived.
///
/// The `ToolResultEnd` event is built from its own copy of the metadata, so the transport
/// still receives it; what is dropped here is the *archived* copy. That keeps a
/// client-render payload (a whole old/new file diff, say) out of the context, the journal
/// and every later checkpoint.
fn strip_event_only_metadata(core: &AgentCore, bound: &mut ToolResultBlock) {
    if core.event_only_tool_metadata.is_empty() || bound.metadata.is_empty() {
        return;
    }
    for key in &core.event_only_tool_metadata {
        bound.metadata.remove(key);
    }
}

/// Whether every call in `calls` already has a `tool_result` in `state` and all of them are
/// `Denied`, mirroring Java `MessageUtils.allToolCallsDenied`.
///
/// A malformed call (whose result is an `ERROR`) or a call still missing its result keeps
/// the round from counting as "all denied": the loop must keep going in both cases.
fn all_tool_calls_denied(state: &AgentState, calls: &[ToolUseBlock]) -> bool {
    if calls.is_empty() {
        return false;
    }
    let ids: std::collections::HashSet<&str> = calls.iter().map(|c| c.id.as_str()).collect();
    let mut states: std::collections::HashMap<&str, ToolResultState> =
        std::collections::HashMap::new();
    for message in state.context() {
        for result in message.tool_result_blocks() {
            if let Some(id) = result.id.as_deref() {
                if ids.contains(id) {
                    states.insert(id, result.final_state());
                }
            }
        }
    }
    states.len() == ids.len() && states.values().all(|s| *s == ToolResultState::Denied)
}

/// The reason a round's permission rejections end the turn under `stopOnReject`, or `None`
/// when no call of the round was denied.
///
/// A round whose every call was denied reports `AllToolsDenied`; a partial rejection reports
/// `ActingStopRequested`, because the acting phase was cut short instead of running every call.
fn rejected_turn_reason(state: &AgentState, calls: &[ToolUseBlock]) -> Option<GenerateReason> {
    if !calls.iter().any(|call| call_denied(state, &call.id)) {
        return None;
    }
    Some(if all_tool_calls_denied(state, calls) {
        GenerateReason::AllToolsDenied
    } else {
        GenerateReason::ActingStopRequested
    })
}

/// Whether the tool result recorded for `id` in `state` is `Denied`.
fn call_denied(state: &AgentState, id: &str) -> bool {
    state.context().iter().any(|message| {
        message.tool_result_blocks().any(|result| {
            result.id.as_deref() == Some(id) && result.final_state() == ToolResultState::Denied
        })
    })
}

/// Emits `AllToolsDenied` through the acting middleware chain, giving middlewares the
/// opportunity to stop the turn, and returns the reason they requested.
///
/// Mirrors Java `ReActAgent.emitAllToolsDeniedThroughMiddleware`: the event is the only
/// signal (no tool ran), and a stop is honoured only when a middleware asks for it — the
/// default `GenerateReason::AllToolsDenied` applies when the request carries none.
async fn emit_all_tools_denied(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    denied: &[ToolUseBlock],
) -> Option<GenerateReason> {
    let mut fields = JsonMap::new();
    fields.insert(
        "denied_tool_calls".to_string(),
        serde_json::to_value(denied).unwrap_or_default(),
    );
    let event = AgentEvent::with_fields(AgentEventType::AllToolsDenied, fields);
    if core
        .middleware_chain
        .participants(ExtensionPoint::OnActing)
        .is_empty()
    {
        // Nobody can react to the signal: it still reaches the observers and the loop
        // continues with the next reasoning step.
        emit_delta(&ctx.delta_tx, event);
        return None;
    }
    let mut stream = core.middleware_chain.wrap_acting(
        ctx.clone(),
        ActingInput::new(denied.to_vec()),
        Box::new(move |_input: ActingInput| {
            Box::pin(futures::stream::iter(vec![Ok(event)])) as EventStream
        }),
    );
    let mut stop = None;
    while let Some(item) = stream.next().await {
        let Ok(event) = item else { continue };
        let is_stop = matches!(&event, AgentEvent::RequestStop(_));
        if let AgentEvent::RequestStop(data) = &event {
            stop = Some(stop_reason(data, GenerateReason::AllToolsDenied));
        }
        emit_delta(&ctx.delta_tx, event);
        if is_stop {
            break;
        }
    }
    stop
}

/// Builds the turn's result when a middleware stopped it outside the acting stream: the last
/// assistant message carries the requested reason (Java returns
/// `lastAssistantMessage().withGenerateReason(reason)`).
fn last_assistant_message_with_reason(state: &AgentState, reason: GenerateReason) -> Msg {
    match state
        .context()
        .iter()
        .rev()
        .find(|message| message.role == MsgRole::Assistant)
    {
        Some(message) => message.with_generate_reason(reason),
        None => Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Text(TextBlock::new(String::new()))])
            .generate_reason(reason)
            .build(),
    }
}

/// Builds a denied result block for a call.
fn denied_result(call: &ToolUseBlock, message: &str) -> ToolResultBlock {
    ToolResultBlock {
        id: Some(call.id.clone()),
        name: Some(call.name.clone()),
        output: vec![ContentBlock::Text(TextBlock::new(format!(
            "Permission denied: {}",
            message
        )))],
        metadata: Default::default(),
        state: ToolResultState::Denied,
    }
}

/// Builds an error result block for a call.
fn error_result(call: &ToolUseBlock, message: &str) -> ToolResultBlock {
    ToolResultBlock {
        id: Some(call.id.clone()),
        name: Some(call.name.clone()),
        output: vec![ContentBlock::Text(TextBlock::new(format!(
            "Error: {}",
            message
        )))],
        metadata: Default::default(),
        state: ToolResultState::Error,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use futures::stream::BoxStream;

    use super::*;
    use crate::message::{TextBlock, ToolUseBlock};
    use crate::middleware::Middleware;
    use crate::model::ChatResponse;
    use crate::permission::PermissionRule;
    use crate::tool::Tool;

    /// A mock model that pops queued responses, falling back to a fixed "done".
    struct MockModel {
        name: String,
        responses: Arc<Mutex<Vec<ChatResponse>>>,
    }

    impl MockModel {
        fn new(name: &str, responses: Vec<ChatResponse>) -> Self {
            Self {
                name: name.to_string(),
                responses: Arc::new(Mutex::new(responses)),
            }
        }
    }

    #[async_trait]
    impl Model for MockModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            let responses = Arc::clone(&self.responses);
            Box::pin(futures::stream::once(async move {
                let mut g = responses.lock().unwrap();
                let r = if g.is_empty() {
                    text_response("done")
                } else {
                    g.remove(0)
                };
                Ok(r)
            }))
        }
        fn model_name(&self) -> &str {
            &self.name
        }
    }

    /// A native-structured-output model that records every `GenerateOptions` it sees,
    /// so tests can assert a forced `ToolChoice::Specific` is being sent.
    struct OptionsRecordingModel {
        responses: Arc<Mutex<Vec<ChatResponse>>>,
        seen_choices: Arc<Mutex<Vec<Option<crate::model::ToolChoice>>>>,
    }

    impl OptionsRecordingModel {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                seen_choices: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl Model for OptionsRecordingModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            self.seen_choices
                .lock()
                .unwrap()
                .push(options.tool_choice.clone());
            let responses = Arc::clone(&self.responses);
            Box::pin(futures::stream::once(async move {
                let mut g = responses.lock().unwrap();
                let r = if g.is_empty() {
                    text_response("done")
                } else {
                    g.remove(0)
                };
                Ok(r)
            }))
        }
        fn model_name(&self) -> &str {
            "opts-recording"
        }

        fn supports_native_structured_output(&self) -> bool {
            true
        }

        fn supports_tool_choice_specific(&self) -> bool {
            true
        }
    }

    /// Builds an assistant message containing a server-tool call plus its inline result.
    fn server_tool_response(id: &str, name: &str) -> ChatResponse {
        let mut use_meta = JsonMap::new();
        use_meta.insert("serverTool".to_string(), serde_json::json!(true));
        let call = ToolUseBlock::builder()
            .id(id)
            .name(name)
            .input(JsonMap::new())
            .metadata(use_meta)
            .build();
        let mut res_meta = JsonMap::new();
        res_meta.insert("serverTool".to_string(), serde_json::json!(true));
        let result = ToolResultBlock {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            output: vec![ContentBlock::Text(TextBlock::new("search done"))],
            metadata: res_meta,
            state: crate::message::ToolResultState::Success,
        };
        ChatResponse::builder()
            .content(vec![
                ContentBlock::ToolUse(call),
                ContentBlock::ToolResult(result),
            ])
            .build()
    }

    fn text_response(text: &str) -> ChatResponse {
        ChatResponse::builder()
            .content(vec![ContentBlock::Text(TextBlock::new(text))])
            .build()
    }

    fn tool_use_response(id: &str, name: &str, input: JsonMap) -> ChatResponse {
        ChatResponse::builder()
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                id, name, input,
            ))])
            .build()
    }

    fn add_tool(counter: Arc<Mutex<u32>>) -> Arc<dyn Tool> {
        let counter = Arc::clone(&counter);
        Arc::new(crate::tool::FunctionTool::new(
            "add",
            "adds two integers",
            serde_json::json!({"type": "object"}),
            Box::new(move |args: JsonMap, _ctx: ToolExecutionContext| {
                let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                *counter.lock().unwrap() += 1;
                Ok(ToolResultBlock::text(format!("{}", a + b)))
            }),
        ))
    }

    /// A returnDirect tool: its successful result becomes the turn's final answer.
    fn return_direct_tool(payload: &'static str) -> Arc<dyn Tool> {
        Arc::new(
            crate::tool::FunctionTool::new(
                "final_answer",
                "returns the turn's final answer",
                serde_json::json!({"type": "object"}),
                Box::new(move |_args: JsonMap, _ctx: ToolExecutionContext| {
                    Ok(ToolResultBlock::success(payload))
                }),
            )
            .return_direct(true),
        )
    }

    #[tokio::test]
    async fn return_direct_tool_short_circuits_the_loop() {
        let toolkit = Toolkit::new();
        toolkit.register_tool(return_direct_tool("the final answer"));
        // A second model response is queued: if the loop failed to short-circuit it
        // would be consumed and the reply would be "should not happen".
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c-rd", "final_answer", JsonMap::new()),
                text_response("should not happen"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t4")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("go")]).await.unwrap();

        assert_eq!(reply.text_content(), "the final answer");
        assert_eq!(reply.generate_reason(), GenerateReason::ToolReturnDirect);
        assert_eq!(
            reply
                .metadata()
                .get(crate::message::metadata_keys::TOOL_RETURN_DIRECT),
            Some(&serde_json::json!(true))
        );
    }

    #[tokio::test]
    async fn return_direct_context_keeps_a_placeholder_tool_result() {
        // The payload moves to the closing assistant message, so the persisted context
        // must hold `[tool_use, tool_result(placeholder), assistant(full result)]`
        // rather than duplicating the payload back to the model.
        let toolkit = Toolkit::new();
        toolkit.register_tool(return_direct_tool("payload only once"));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c-rd2", "final_answer", JsonMap::new()),
                text_response("should not happen"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t4-placeholder")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("go")]).await.unwrap();

        let state = agent.agent_state();
        let tool_result = state
            .context()
            .iter()
            .flat_map(|m| m.tool_result_blocks())
            .find(|rb| rb.id.as_deref() == Some("c-rd2"))
            .expect("placeholder tool result persisted");
        assert_eq!(tool_result.state, ToolResultState::Success);
        assert!(
            !tool_result.text_content().contains("payload only once"),
            "payload must not be duplicated into the tool result: {}",
            tool_result.text_content()
        );
    }

    #[tokio::test]
    async fn non_return_direct_tool_does_not_short_circuit() {
        // The flag is per-tool: an ordinary tool must keep feeding results back.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(2));
        input.insert("b".to_string(), serde_json::json!(3));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", input),
                text_response("the answer is 5"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t4-off")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        assert_eq!(reply.text_content(), "the answer is 5");
        assert_eq!(reply.generate_reason(), GenerateReason::ModelStop);
    }

    /// Wiring guard: the observation boundary must be reachable from the *real* build entry.
    /// Poking `acting_phase` internals would hide a missing builder/context plumbing step.
    #[tokio::test]
    async fn tool_execution_records_action_facts_through_the_real_agent_entry() {
        let storage: Arc<dyn crate::session::AtomicSessionStorage> =
            Arc::new(crate::session::MemorySessionStorage::new());
        let log: Arc<dyn crate::session::SessionLog> =
            Arc::new(crate::session::JournalSessionLog::new(storage, "s1"));
        let recorder = Arc::new(
            crate::session::SessionRecorder::open(Arc::clone(&log), Some("t1"))
                .await
                .unwrap(),
        );

        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::new(Mutex::new(0))));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(2));
        input.insert("b".to_string(), serde_json::json!(3));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c-action", "add", input),
                text_response("5"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-action")
            .model(model)
            .toolkit(toolkit)
            .extension_arc(crate::session::RECORDER_CONTEXT_KEY, Arc::clone(&recorder))
            .build()
            .unwrap();
        agent.call(vec![Msg::user("compute")]).await.unwrap();

        let head = log.head().await.unwrap();
        let types: Vec<String> = log
            .scan(0, head.seq)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect();
        assert!(types.contains(&"action/start".to_string()), "{types:?}");
        assert!(types.contains(&"action/end".to_string()), "{types:?}");
        // The action id observed at the tool boundary is correlated with the tool call.
        assert!(!recorder.action_id("c-action").is_empty());
        recorder.close().await;
    }

    #[tokio::test]
    async fn mixed_return_direct_batch_is_fed_back_to_the_model() {
        // Java: a batch takes effect only when *every* executed tool is returnDirect.
        let toolkit = Toolkit::new();
        toolkit.register_tool(return_direct_tool("direct"));
        toolkit.register_tool(add_tool(Arc::new(Mutex::new(0))));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(1));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                ChatResponse::builder()
                    .content(vec![
                        ContentBlock::ToolUse(ToolUseBlock::new(
                            "c-rd3",
                            "final_answer",
                            JsonMap::new(),
                        )),
                        ContentBlock::ToolUse(ToolUseBlock::new("c-add3", "add", input)),
                    ])
                    .build(),
                text_response("the model summarized both"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t4-mixed")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("go")]).await.unwrap();
        assert_eq!(reply.text_content(), "the model summarized both");
        assert_eq!(reply.generate_reason(), GenerateReason::ModelStop);
    }

    #[tokio::test]
    async fn failing_return_direct_tool_does_not_short_circuit() {
        // A failed tool must never be presented as the final answer (Java:
        // DENIED/ERROR/INTERRUPTED disqualify the batch).
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(
            crate::tool::FunctionTool::new(
                "failing_direct",
                "fails",
                serde_json::json!({"type": "object"}),
                Box::new(|_, _| Ok(ToolResultBlock::error("boom"))),
            )
            .return_direct(true),
        ));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c-err", "failing_direct", JsonMap::new()),
                text_response("the model recovered"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t4-err")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("go")]).await.unwrap();
        assert_eq!(reply.text_content(), "the model recovered");
        assert_eq!(reply.generate_reason(), GenerateReason::ModelStop);
    }

    #[tokio::test]
    async fn return_direct_closing_message_emits_text_events_with_generate_reason() {
        let toolkit = Toolkit::new();
        toolkit.register_tool(return_direct_tool("streamed final answer"));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c-rd4", "final_answer", JsonMap::new()),
                text_response("should not happen"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t4-events")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let events: Vec<AgentEvent> = agent
            .stream_events(vec![Msg::user("go")], StreamOptions::defaults())
            .filter_map(|r| async move { r.ok() })
            .collect()
            .await;

        let delta = events
            .iter()
            .find(|e| {
                e.event_type() == AgentEventType::TextBlockDelta
                    && e.data().fields.get("delta")
                        == Some(&serde_json::json!("streamed final answer"))
            })
            .expect("closing message emitted a TextBlockDelta");
        assert_eq!(
            delta
                .metadata()
                .get(crate::message::metadata_keys::EVENT_GENERATE_REASON),
            Some(&serde_json::json!("TOOL_RETURN_DIRECT"))
        );
    }

    #[tokio::test]
    async fn plain_text_reply() {
        let model = Arc::new(MockModel::new("mock", vec![text_response("hello world")]));
        let agent = ReActAgent::builder()
            .name("t1")
            .model(model)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("hi")]).await.unwrap();
        assert_eq!(reply.text_content(), "hello world");
        assert_eq!(reply.generate_reason(), GenerateReason::ModelStop);
    }

    #[tokio::test]
    async fn call_owns_a_recorder_and_commits_the_turn() {
        // Java `ReActAgent.acquireCallExecution`: the call itself opens the recorder, so a
        // bare `ReActAgent::call` journals without any host-side `AgentSession`.
        let store = Arc::new(crate::session::InMemorySessionLogStore::new());
        let model = Arc::new(MockModel::new("mock", vec![text_response("journaled")]));
        let agent = ReActAgent::builder()
            .name("t-journal")
            .model(model)
            .state(
                AgentState::builder()
                    .session_id("s-journal")
                    .user_id("alice")
                    .build(),
            )
            .session_log_store(Arc::clone(&store) as Arc<dyn SessionLogStore>)
            .build()
            .unwrap();
        assert!(agent.session_log_enabled());

        let reply = agent.call(vec![Msg::user("hi")]).await.unwrap();
        assert_eq!(reply.text_content(), "journaled");

        let ctx = crate::agent::RuntimeContext::new();
        let key = agent.session_key(&ctx).expect("journal key");
        let log = store.open(&key, &ctx).await.unwrap();
        let types: Vec<String> = log
            .read_after(0, 256)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event_type)
            .collect();
        for expected in [
            "run/start",
            "turn/start",
            "input/received",
            "turn/completed",
            "run/end",
        ] {
            assert!(
                types.iter().any(|t| t == expected),
                "missing {expected} in {types:?}"
            );
        }
    }

    #[tokio::test]
    async fn caller_supplied_recorder_is_not_reopened() {
        // The journal allows one execution writer. When a host (harness `AgentSession`)
        // already attached a recorder, the call must reuse it — opening a second one would
        // fail with a lease/duplicate-run error, so success here *is* the assertion.
        let store = Arc::new(crate::session::InMemorySessionLogStore::new());
        let model = Arc::new(MockModel::new("mock", vec![text_response("journaled")]));
        let agent = ReActAgent::builder()
            .name("t-journal")
            .model(model)
            .state(
                AgentState::builder()
                    .session_id("s-journal")
                    .user_id("alice")
                    .build(),
            )
            .session_log_store(Arc::clone(&store) as Arc<dyn SessionLogStore>)
            .build()
            .unwrap();

        let ctx = crate::agent::RuntimeContext::new();
        let key = agent.state_session_key().expect("journal key");
        let log = store.open(&key, &ctx).await.unwrap();
        let recorder = Arc::new(SessionRecorder::open(log, Some("turn-host")).await.unwrap());
        let mut extensions = crate::agent::Extensions::new();
        extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));

        let reply = agent
            .call_with_format_and_extensions(vec![Msg::user("hi")], None, extensions)
            .await
            .unwrap();
        assert_eq!(reply.text_content(), "journaled");
        // The call did not close the host-owned recorder; the host still owns it.
        recorder.close().await;
    }

    #[tokio::test]
    async fn tool_call_loop() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", input),
                text_response("the answer is 3"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t2")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        assert_eq!(reply.text_content(), "the answer is 3");
        assert_eq!(*counter.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn tool_result_state_is_terminal_in_persisted_context() {
        // Built-in tools return plain blocks (`ToolResultBlock::text`), leaving the
        // `Running` default in place. Java #2745: the state persisted in the context
        // must be the resolved terminal state, otherwise history queries always
        // observe RUNNING.
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::new(Mutex::new(0))));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c1", "add", input), text_response("done")],
        ));
        let agent = ReActAgent::builder()
            .name("t2-state")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("compute")]).await.unwrap();

        let state = agent.agent_state();
        let stored = state
            .context()
            .iter()
            .flat_map(|m| m.tool_result_blocks())
            .find(|rb| rb.id.as_deref() == Some("c1"))
            .expect("tool result persisted in context");
        assert_eq!(stored.state, ToolResultState::Success);
    }

    #[tokio::test]
    async fn failing_tool_result_state_is_error_in_persisted_context() {
        let toolkit = Toolkit::new();
        // Java #2745: a tool that reports failure via its output text must surface
        // as ERROR, not SUCCESS.
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            "broken",
            "always fails",
            serde_json::json!({"type": "object"}),
            Box::new(|_, _| Ok(ToolResultBlock::text("[ERROR] boom"))),
        )));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c9", "broken", JsonMap::new()),
                text_response("recovered"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t2-err")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("go")]).await.unwrap();

        let state = agent.agent_state();
        let stored = state
            .context()
            .iter()
            .flat_map(|m| m.tool_result_blocks())
            .find(|rb| rb.id.as_deref() == Some("c9"))
            .expect("tool result persisted in context");
        assert_eq!(stored.state, ToolResultState::Error);
    }

    #[tokio::test]
    async fn tool_context_exposes_emitter_with_the_call_id() {
        // Java #2720: the emitter handed to a tool must know which tool call it
        // belongs to. Driven through the real acting phase so a regression that
        // passes `emitter: None` again fails here.
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            "probe",
            "records its emitter's call id",
            serde_json::json!({"type": "object"}),
            Box::new(move |_args: JsonMap, ctx: ToolExecutionContext| {
                let id = ctx
                    .emitter
                    .as_ref()
                    .and_then(|e| e.tool_call_id().map(str::to_string));
                captured.lock().unwrap().push(id);
                Ok(ToolResultBlock::success("probed"))
            }),
        )));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("call-probe-1", "probe", JsonMap::new()),
                text_response("done"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t3")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("go")]).await.unwrap();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[Some("call-probe-1".to_string())]
        );
    }

    /// A tool that emits one chunk carrying two output blocks and returns a distinct
    /// value, so progress and the return value can be told apart in the event stream.
    fn two_block_streamer_toolkit() -> Toolkit {
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            "streamer",
            "emits a two-block chunk then returns",
            serde_json::json!({"type": "object"}),
            Box::new(move |_args: JsonMap, ctx: ToolExecutionContext| {
                if let Some(e) = &ctx.emitter {
                    let mut metadata = JsonMap::new();
                    metadata.insert("step".to_string(), serde_json::json!(1));
                    let chunk = ToolResultBlock::builder()
                        .outputs(vec![
                            ContentBlock::Text(TextBlock::new("step one")),
                            ContentBlock::Text(TextBlock::new("step two")),
                        ])
                        .metadata(metadata)
                        .build();
                    crate::tool::emit_to(&Some(Arc::clone(e)), chunk);
                }
                Ok(ToolResultBlock::success("final answer"))
            }),
        )));
        toolkit
    }

    /// Runs the two-block streamer once and returns the collected event stream.
    async fn two_block_streamer_events() -> Vec<AgentEvent> {
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("call-stream-1", "streamer", JsonMap::new()).with_id("reply-1"),
                text_response("done"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t3-stream")
            .model(model)
            .toolkit(two_block_streamer_toolkit())
            .build()
            .unwrap();
        agent
            .stream_events(
                vec![Msg::user("go")],
                StreamOptions::builder().include_acting_chunk(true).build(),
            )
            .filter_map(|r| async move { r.ok() })
            .collect()
            .await
    }

    #[tokio::test]
    async fn tool_emitted_chunk_produces_one_progress_event_per_block() {
        // Java `ReActAgent.internalChunkCallback`: every block of the emitted chunk
        // becomes its own `ToolProgressEvent`, carrying reply id, call id, call name,
        // the block itself and the chunk metadata.
        let events = two_block_streamer_events().await;
        let progress: Vec<&AgentEvent> = events
            .iter()
            .filter(|e| e.event_type() == AgentEventType::ToolProgress)
            .collect();
        assert_eq!(
            progress.len(),
            2,
            "two output blocks must become two progress events, got {progress:#?}"
        );

        for (event, expected_text) in progress.iter().zip(["step one", "step two"]) {
            let fields = &event.data().fields;
            assert_eq!(fields.get("reply_id"), Some(&serde_json::json!("reply-1")));
            assert_eq!(
                fields.get("tool_call_id"),
                Some(&serde_json::json!("call-stream-1"))
            );
            assert_eq!(
                fields.get("tool_name"),
                Some(&serde_json::json!("streamer"))
            );
            // `content` is the whole block, not just its text (Java `ContentBlock content`).
            let content = fields.get("content").expect("progress carries its block");
            assert_eq!(
                content,
                &serde_json::json!({"type": "text", "text": expected_text}),
                "progress block is carried verbatim"
            );
            // The chunk metadata travels with each event (Java `withMetadata`).
            assert_eq!(event.metadata().get("step"), Some(&serde_json::json!(1)));
        }
    }

    #[tokio::test]
    async fn tool_return_value_still_arrives_as_a_result_delta() {
        // Anti-regression for the progress split: progress must not swallow the tool's
        // return value, which is the only thing the model ever sees. Java doc:
        // "emitted chunks do NOT affect what the LLM receives - only the tool method's
        // return value is sent to the LLM".
        let events = two_block_streamer_events().await;
        let deltas: Vec<&AgentEvent> = events
            .iter()
            .filter(|e| e.event_type() == AgentEventType::ToolResultTextDelta)
            .collect();
        assert_eq!(deltas.len(), 1, "the return value is emitted exactly once");
        assert_eq!(
            deltas[0].data().fields.get("delta"),
            Some(&serde_json::json!("final answer"))
        );
        assert_eq!(
            deltas[0].data().fields.get("tool_call_id"),
            Some(&serde_json::json!("call-stream-1"))
        );
        // Progress text is display-only and must not leak into the result stream.
        assert!(
            !serde_json::to_string(deltas[0])
                .unwrap()
                .contains("step one"),
            "progress text leaked into the tool result delta"
        );
        // The old borrowing is gone: no progress-shaped data delta is produced.
        assert!(
            events
                .iter()
                .all(|e| e.event_type() != AgentEventType::ToolResultDataDelta),
            "the progress path must no longer emit ToolResultDataDelta"
        );
    }

    #[tokio::test]
    async fn tool_progress_events_are_suppressed_when_acting_chunks_are_filtered() {
        // `include_acting_chunk` is Java's `isIncludeActingChunk`; progress belongs to the
        // acting-chunk family, so turning it off suppresses the events entirely.
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("call-stream-1", "streamer", JsonMap::new()),
                text_response("done"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t3-stream-off")
            .model(model)
            .toolkit(two_block_streamer_toolkit())
            .build()
            .unwrap();
        let events: Vec<AgentEvent> = agent
            .stream_events(
                vec![Msg::user("go")],
                StreamOptions::builder().include_acting_chunk(false).build(),
            )
            .filter_map(|r| async move { r.ok() })
            .collect()
            .await;
        assert!(
            events
                .iter()
                .all(|e| e.event_type() != AgentEventType::ToolProgress),
            "acting chunks are filtered, so no progress event is published"
        );
    }

    #[test]
    fn tool_progress_without_a_delta_channel_is_a_silent_no_op() {
        // The structured-output retry path has no event channel. The emitter is still
        // installed (so tools can call `emit` unconditionally) but discards, and the
        // callback itself is not built at all.
        let call = ToolUseBlock::new("call-so-1", "generate_response", JsonMap::new());
        assert!(
            tool_chunk_callback(&None, &call, "reply-so").is_none(),
            "no channel means no callback"
        );
        let tctx = tool_execution_context(&call, JsonMap::new(), &None, None, "reply-so");
        let emitter = tctx.emitter.expect("an emitter is always installed");
        // Must not panic, and must not reach any consumer.
        crate::tool::emit_to(&Some(emitter), ToolResultBlock::text("ignored progress"));
    }

    #[tokio::test]
    async fn suspended_tool_halts_with_tool_suspended() {
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            "external",
            "needs human",
            serde_json::json!({"type": "object"}),
            Box::new(|_, _| {
                Err(crate::tool::ToolSuspendException::new("waiting for input").to_error())
            }),
        )));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "external", JsonMap::new()),
                // If the loop incorrectly continued, this second response would be
                // consumed; the assertion below guards against that.
                text_response("should not happen"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-suspend")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("go")]).await.unwrap();
        assert_eq!(reply.generate_reason(), GenerateReason::ToolSuspended);
    }

    #[tokio::test]
    async fn suspended_tool_emits_require_external_execution_and_no_result_end() {
        // Java #3435: a suspended call emits RequireExternalExecution as the *single*
        // signal; no ToolResultTextDelta/ToolResultEnd may be fabricated for it.
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            "external",
            "needs human",
            serde_json::json!({"type": "object"}),
            Box::new(|_, _| {
                Err(crate::tool::ToolSuspendException::new("waiting for input").to_error())
            }),
        )));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("ext1", "external", JsonMap::new())],
        ));
        let agent = ReActAgent::builder()
            .name("t-suspend-events")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();

        let events: Vec<AgentEvent> = agent
            .stream_events(
                vec![Msg::user("go")],
                crate::agent::stream_options::StreamOptions::defaults(),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert!(
            !events
                .iter()
                .any(|e| e.event_type() == AgentEventType::ToolResultEnd),
            "a suspended tool must not emit ToolResultEnd; got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.event_type() == AgentEventType::ToolResultTextDelta),
            "a suspended tool must not emit result content; got {events:?}"
        );

        let require: Vec<&AgentEvent> = events
            .iter()
            .filter(|e| e.event_type() == AgentEventType::RequireExternalExecution)
            .collect();
        assert_eq!(
            require.len(),
            1,
            "expected exactly one require event; got {events:?}"
        );
        let fields = &require[0].data().fields;
        assert_eq!(fields.get("tool_call_id"), Some(&serde_json::json!("ext1")));
        assert_eq!(
            fields.get("tool_name"),
            Some(&serde_json::json!("external"))
        );
        assert_eq!(
            fields.get("reason"),
            Some(&serde_json::json!("waiting for input"))
        );
        // Mirrors Java `RequireExternalExecutionEvent.getToolCalls()`.
        let calls = fields.get("tool_calls").and_then(|v| v.as_array()).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].get("id"), Some(&serde_json::json!("ext1")));
        assert_eq!(calls[0].get("name"), Some(&serde_json::json!("external")));
    }

    /// A concurrency-safe tool that signals when its invocation starts and then blocks
    /// until the test releases it. A pre-filled release permit makes it return at once.
    struct GatedTool {
        name: &'static str,
        started: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
        result: &'static str,
    }

    impl GatedTool {
        fn new(
            name: &'static str,
            result: &'static str,
        ) -> (
            Self,
            Arc<tokio::sync::Semaphore>,
            Arc<tokio::sync::Semaphore>,
        ) {
            let started = Arc::new(tokio::sync::Semaphore::new(0));
            let release = Arc::new(tokio::sync::Semaphore::new(0));
            (
                Self {
                    name,
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                    result,
                },
                started,
                release,
            )
        }
    }

    #[async_trait]
    impl Tool for GatedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "gated test tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _args: JsonMap,
            _ctx: ToolExecutionContext,
        ) -> Result<ToolResultBlock> {
            self.started.add_permits(1);
            let _ = self.release.acquire().await;
            Ok(ToolResultBlock::text(self.result))
        }
    }

    fn two_calls(second_name: &str) -> ChatResponse {
        ChatResponse::builder()
            .content(vec![
                ContentBlock::ToolUse(ToolUseBlock::new("slow-call", "slow", JsonMap::new())),
                ContentBlock::ToolUse(ToolUseBlock::new("fast-call", second_name, JsonMap::new())),
            ])
            .build()
    }

    #[tokio::test]
    async fn a_fast_tool_is_visible_while_a_slower_earlier_tool_is_still_running() {
        // Mirrors Java `ProgressiveToolResultTest.completedToolIsVisibleBeforeSlowEarlierTool`:
        // in parallel mode a completed call's events must not wait for the whole batch,
        // while the persisted results keep declaration order and the next reasoning
        // iteration still waits for every call.
        let (slow, _slow_started, slow_release) = GatedTool::new("slow", "slow result");
        let (fast, _fast_started, fast_release) = GatedTool::new("fast", "fast result");
        // The fast tool has nothing to wait for.
        fast_release.add_permits(1);

        let toolkit = Toolkit::new();
        assert!(
            toolkit.config().parallel,
            "the Java default is parallel tool execution"
        );
        toolkit.register_tool(Arc::new(slow));
        toolkit.register_tool(Arc::new(fast));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![two_calls("fast"), text_response("done")],
        ));
        let agent = ReActAgent::builder()
            .name("t-progressive")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();

        let mut stream = agent.stream_events(
            vec![Msg::user("go")],
            crate::agent::stream_options::StreamOptions::defaults(),
        );
        let mut ends: Vec<String> = Vec::new();
        let mut saw_slow_start = false;
        let mut fast_finished_before_release = false;
        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                .await
                .expect("the fast tool's result never became visible while slow was pending");
            let Some(event) = next else { break };
            let event = event.unwrap();
            match event.event_type() {
                AgentEventType::ToolResultStart => {
                    if event.data().fields.get("tool_call_id")
                        == Some(&serde_json::json!("slow-call"))
                    {
                        saw_slow_start = true;
                    }
                }
                AgentEventType::ToolResultEnd => {
                    let id = event
                        .data()
                        .fields
                        .get("tool_call_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if id == "fast-call" && !fast_finished_before_release {
                        assert!(saw_slow_start, "the slow tool must already be running");
                        fast_finished_before_release = true;
                        slow_release.add_permits(1);
                    }
                    ends.push(id);
                }
                _ => {}
            }
        }

        assert!(
            fast_finished_before_release,
            "the fast result must arrive while the slow call is still pending"
        );
        assert_eq!(
            ends,
            vec!["fast-call".to_string(), "slow-call".to_string()],
            "events follow completion order in parallel mode"
        );

        let state = agent.state_handle();
        let guard = state.lock().unwrap();
        let result_ids: Vec<String> = guard
            .context()
            .iter()
            .flat_map(|message| message.tool_result_blocks())
            .filter_map(|result| result.id.clone())
            .collect();
        assert_eq!(
            result_ids,
            vec!["slow-call".to_string(), "fast-call".to_string()],
            "the persisted batch keeps declaration order"
        );
    }

    #[tokio::test]
    async fn serial_mode_emits_start_when_execution_begins() {
        // Mirrors Java `ProgressiveToolResultTest.serialToolsEmitStartWhenExecutionBegins`:
        // with parallel off, the second call's Start must not be emitted while the first
        // is still running, so Start→End spans the actual execution.
        let (slow, _slow_started, slow_release) = GatedTool::new("slow", "slow result");
        let (fast, _fast_started, fast_release) = GatedTool::new("fast", "fast result");
        fast_release.add_permits(1);

        let toolkit = Toolkit::with_config(
            crate::tool::ToolkitConfig::builder()
                .parallel(false)
                .build(),
        );
        toolkit.register_tool(Arc::new(slow));
        toolkit.register_tool(Arc::new(fast));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![two_calls("fast"), text_response("done")],
        ));
        let agent = ReActAgent::builder()
            .name("t-serial-progress")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();

        let mut stream = agent.stream_events(
            vec![Msg::user("go")],
            crate::agent::stream_options::StreamOptions::defaults(),
        );
        let mut sequence: Vec<String> = Vec::new();
        let mut released = false;
        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                .await
                .expect("serial execution stalled");
            let Some(event) = next else { break };
            let event = event.unwrap();
            let id = event
                .data()
                .fields
                .get("tool_call_id")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            match event.event_type() {
                AgentEventType::ToolResultStart => {
                    sequence.push(format!("start:{id}"));
                    if id == "slow-call" && !released {
                        released = true;
                        slow_release.add_permits(1);
                    }
                }
                AgentEventType::ToolResultEnd => sequence.push(format!("end:{id}")),
                _ => {}
            }
        }
        assert_eq!(
            sequence,
            vec![
                "start:slow-call",
                "end:slow-call",
                "start:fast-call",
                "end:fast-call",
            ],
            "serial mode must not admit the next call before the previous one finished"
        );
    }

    #[tokio::test]
    async fn successful_tool_emits_result_end_and_no_require_external_execution() {
        // The non-suspended path is unaffected: ToolResultEnd is emitted and no
        // external-execution requirement is raised.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c1", "add", input), text_response("done")],
        ));
        let agent = ReActAgent::builder()
            .name("t-ok-events")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();

        let events: Vec<AgentEvent> = agent
            .stream_events(
                vec![Msg::user("compute")],
                crate::agent::stream_options::StreamOptions::defaults(),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert!(
            events
                .iter()
                .any(|e| e.event_type() == AgentEventType::ToolResultEnd),
            "an executed tool must emit ToolResultEnd; got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.event_type() == AgentEventType::RequireExternalExecution),
            "an executed tool must not require external execution; got {events:?}"
        );
    }

    #[tokio::test]
    async fn truncated_tool_call_is_rejected_without_executing_and_pairs_its_result() {
        // Mirrors Java `TruncatedToolCallIntegrityTest`: a call whose streamed arguments
        // never formed a JSON object is answered with an ERROR result carrying the
        // original id, never handed to the handler, and the loop continues so the model
        // can reissue it. A completed sibling in the same response still runs.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));

        let mut truncated = ToolUseBlock::new("trunc-1", "add", JsonMap::new());
        truncated.content = Some("{\"a\": 1".to_string());
        let mut completed = ToolUseBlock::new("ok-1", "add", JsonMap::new());
        completed.content = Some("{\"a\":1,\"b\":2}".to_string());

        let model = Arc::new(SequenceRecordingModel::new(vec![
            ChatResponse::builder()
                .id("reply-1")
                .content(vec![
                    ContentBlock::ToolUse(truncated),
                    ContentBlock::ToolUse(completed),
                ])
                .finish_reason("length")
                .build(),
            text_response("recovered"),
        ]));
        let seen = Arc::clone(&model.seen);
        let agent = ReActAgent::builder()
            .name("t-truncated")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();

        let events: Vec<AgentEvent> = agent
            .stream_events(
                vec![Msg::user("write")],
                crate::agent::stream_options::StreamOptions::defaults(),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "the truncated call must not reach the handler; only the completed sibling runs"
        );

        let state = agent.state_handle();
        let guard = state.lock().unwrap();
        let result_for = |id: &str| -> Option<ToolResultBlock> {
            guard
                .context()
                .iter()
                .flat_map(|message| message.tool_result_blocks())
                .find(|result| result.id.as_deref() == Some(id))
                .cloned()
        };
        let rejected = result_for("trunc-1").expect("a result must pair the truncated call");
        assert_eq!(rejected.state, ToolResultState::Error);
        let rejected_text = rejected.text_content();
        assert!(
            rejected_text.contains("output token limit"),
            "the rejection must explain the truncation: {rejected_text}"
        );
        assert!(
            rejected_text.contains("not executed"),
            "the rejection must say the call did not run: {rejected_text}"
        );
        let sibling = result_for("ok-1").expect("the completed sibling must produce a result");
        assert_eq!(sibling.state, ToolResultState::Success);
        assert_eq!(sibling.text_content(), "3");

        // The assistant message records the output-limit stop so a resumed turn can pick
        // the same rejection text.
        let assistant = guard
            .context()
            .iter()
            .find(|message| message.tool_use_blocks().any(|tool| tool.id == "trunc-1"))
            .expect("assistant message with the tool calls");
        assert_eq!(
            assistant
                .metadata()
                .get(crate::message::metadata_keys::OUTPUT_LIMIT_REACHED),
            Some(&serde_json::json!(true))
        );

        // The next model request carries the ERROR result so the model can reissue it.
        let calls = seen.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "the loop must continue after rejecting the call"
        );
        let second_request = calls[1]
            .iter()
            .flat_map(|message| message.tool_result_blocks())
            .map(|result| result.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            second_request.contains("output token limit"),
            "second request must see the rejection: {second_request}"
        );

        drop(guard);
        let end = events
            .iter()
            .find(|event| {
                event.event_type() == AgentEventType::ToolResultEnd
                    && event.data().fields.get("tool_call_id")
                        == Some(&serde_json::json!("trunc-1"))
            })
            .expect("lifecycle events must be published for the truncated call");
        assert_eq!(
            end.data().fields.get("state"),
            Some(&serde_json::json!("ERROR"))
        );
    }

    #[test]
    fn malformed_tool_arguments_are_finalized_with_the_java_rules() {
        // Valid raw JSON fills the parsed arguments; invalid raw JSON is preserved in
        // `content` and marked malformed; only an output-limit stop turns an empty call
        // start into a malformed no-arg call.
        let mut valid = ToolUseBlock::new("valid", "add", JsonMap::new());
        valid.content = Some("{\"a\":1,\"b\":2}".to_string());
        let mut invalid = ToolUseBlock::new("invalid", "add", JsonMap::new());
        invalid.content = Some("{\"a\":1".to_string());
        let mut empty_truncated = ToolUseBlock::new("empty", "add", JsonMap::new());
        empty_truncated.content = Some(String::new());
        // A call whose arguments completed through `input` (no raw fragments) is not
        // ambiguous: the output-limit stop cannot invalidate it.
        let mut input_only = JsonMap::new();
        input_only.insert("a".to_string(), serde_json::json!(1));
        let input_only_completed = ToolUseBlock::new("complete", "add", input_only);
        let mut empty_complete = ToolUseBlock::new("no-args", "add", JsonMap::new());
        empty_complete.content = Some(String::new());

        let mut context = ReasoningContext {
            finish_reason: Some("length".to_string()),
            ..Default::default()
        };
        context.tool_calls = vec![valid, invalid, empty_truncated, input_only_completed];
        let message = context.build_final_message();
        let tools: Vec<ToolUseBlock> = message.tool_use_blocks().cloned().collect();
        assert_eq!(tools[0].input.get("a"), Some(&serde_json::json!(1)));
        assert_eq!(tools[0].input.get("b"), Some(&serde_json::json!(2)));
        assert!(!tools[0].has_malformed_arguments());
        assert!(tools[1].has_malformed_arguments());
        assert!(tools[1].input.is_empty());
        assert_eq!(
            tools[1].content.as_deref(),
            Some("{\"a\":1"),
            "the raw fragments must survive for diagnostics / replay"
        );
        assert!(tools[2].has_malformed_arguments());
        assert!(!tools[3].has_malformed_arguments());
        assert_eq!(
            message
                .metadata()
                .get(crate::message::metadata_keys::OUTPUT_LIMIT_REACHED),
            Some(&serde_json::json!(true))
        );

        // Without an output-limit stop a complete no-arg call stays executable.
        let mut context = ReasoningContext {
            finish_reason: Some("tool_calls".to_string()),
            ..Default::default()
        };
        context.tool_calls = vec![empty_complete];
        let message = context.build_final_message();
        let tool = message.tool_use_blocks().next().unwrap();
        assert!(!tool.has_malformed_arguments());
        assert!(message
            .metadata()
            .get(crate::message::metadata_keys::OUTPUT_LIMIT_REACHED)
            .is_none());
    }

    #[test]
    fn streamed_thinking_chunks_keep_their_metadata() {
        let mut first = ThinkingBlock::new("let me think");
        first.metadata = Some(JsonMap::from_iter([(
            "encryptedContent".to_string(),
            serde_json::json!("cipher-1"),
        )]));
        let mut second = ThinkingBlock::new(" harder");
        second.metadata = Some(JsonMap::from_iter([(
            "thinkingSignature".to_string(),
            serde_json::json!("sig-1"),
        )]));

        let mut context = ReasoningContext::default();
        for chunk in [first, second] {
            context.accumulate(
                ChatResponse::builder()
                    .content(vec![ContentBlock::Thinking(chunk)])
                    .build(),
            );
        }

        let message = context.build_final_message();
        let thinking = message
            .content()
            .iter()
            .find_map(|block| match block {
                ContentBlock::Thinking(block) => Some(block),
                _ => None,
            })
            .expect("the merged thinking block must survive");
        assert_eq!(thinking.thinking, "let me think harder");
        let metadata = thinking.metadata.as_ref().expect("metadata must survive");
        assert_eq!(
            metadata.get("encryptedContent"),
            Some(&serde_json::json!("cipher-1"))
        );
        assert_eq!(
            metadata.get("thinkingSignature"),
            Some(&serde_json::json!("sig-1"))
        );
    }

    #[test]
    fn a_thinking_chunk_without_metadata_does_not_clear_earlier_keys() {
        let mut with_metadata = ThinkingBlock::new("step one");
        with_metadata.metadata = Some(JsonMap::from_iter([(
            "encryptedContent".to_string(),
            serde_json::json!("cipher-1"),
        )]));
        let mut empty_metadata = ThinkingBlock::new(" step two");
        empty_metadata.metadata = Some(JsonMap::new());
        let mut replacement = ThinkingBlock::new(" step three");
        replacement.metadata = Some(JsonMap::from_iter([(
            "encryptedContent".to_string(),
            serde_json::json!("cipher-2"),
        )]));

        let mut context = ReasoningContext::default();
        for chunk in [with_metadata, empty_metadata, replacement] {
            context.accumulate(
                ChatResponse::builder()
                    .content(vec![ContentBlock::Thinking(chunk)])
                    .build(),
            );
        }

        let message = context.build_final_message();
        let thinking = message
            .content()
            .iter()
            .find_map(|block| match block {
                ContentBlock::Thinking(block) => Some(block),
                _ => None,
            })
            .expect("the merged thinking block must survive");
        assert_eq!(thinking.thinking, "step one step two step three");
        assert_eq!(
            thinking
                .metadata
                .as_ref()
                .and_then(|m| m.get("encryptedContent")),
            Some(&serde_json::json!("cipher-2")),
            "the empty map must not clear a key, a later value must win"
        );
    }

    #[test]
    fn tool_runtime_exposes_only_the_most_recent_messages() {
        let history: Vec<Msg> = (0..40).map(|i| Msg::user(format!("m{i}"))).collect();
        let runtime = tool_runtime(&crate::agent::RuntimeContext::new(), &history);
        let ctx = ToolExecutionContext {
            runtime,
            ..Default::default()
        };
        let seen: Vec<String> = ctx
            .conversation_history()
            .iter()
            .map(Msg::text_content)
            .collect();
        assert_eq!(seen.len(), crate::tool::MAX_CONVERSATION_HISTORY);
        assert_eq!(seen.first().map(String::as_str), Some("m8"));
        assert_eq!(seen.last().map(String::as_str), Some("m39"));
    }

    #[test]
    fn tool_runtime_omits_the_history_entry_when_there_is_nothing_to_share() {
        let runtime = tool_runtime(&crate::agent::RuntimeContext::new(), &[]);
        assert!(runtime.get(crate::tool::CONVERSATION_HISTORY_KEY).is_none());
    }

    #[test]
    fn tool_result_end_state_names_are_distinct_per_state() {
        // The `TOOL_RESULT_END` `state` field is the only tool-outcome signal most
        // consumers see. Every state needs its own name: collapsing `Interrupted`
        // onto `ERROR` makes a user cancellation indistinguishable from a tool
        // failure in AG-UI / Studio.
        let cases = [
            (ToolResultState::Success, "SUCCESS"),
            (ToolResultState::Error, "ERROR"),
            (ToolResultState::Denied, "DENIED"),
            (ToolResultState::Running, "RUNNING"),
            (ToolResultState::Interrupted, "INTERRUPTED"),
            (ToolResultState::Suspended, "SUSPENDED"),
        ];
        for (state, expected) in cases {
            assert_eq!(
                tool_result_state_str(state),
                expected,
                "wrong wire name for {state:?}"
            );
        }
        assert_ne!(
            tool_result_state_str(ToolResultState::Interrupted),
            tool_result_state_str(ToolResultState::Error),
            "an interruption must not be reported as a tool failure"
        );

        // And the projection actually applied to the emitted event carries it.
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let tx = Some(tx);
        emit_tool_result_end(
            &tx,
            "c-int",
            "some_tool",
            ToolResultState::Interrupted,
            &JsonMap::new(),
        );
        let event = rx
            .try_next()
            .unwrap()
            .expect("a ToolResultEnd event")
            .unwrap();
        assert_eq!(event.event_type(), AgentEventType::ToolResultEnd);
        assert_eq!(
            event.data().fields.get("state"),
            Some(&serde_json::json!("INTERRUPTED")),
            "the emitted event must not downgrade an interrupt to ERROR"
        );
    }

    /// A tool that answers with render-only metadata, like the filesystem tools whose
    /// `acp_diffs` carry a whole old/new file pair for the client to render.
    struct DiffTool;

    #[async_trait]
    impl Tool for DiffTool {
        fn name(&self) -> &str {
            "diff_write"
        }
        fn description(&self) -> &str {
            "writes a file and reports the diff"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _args: JsonMap,
            _ctx: ToolExecutionContext,
        ) -> Result<ToolResultBlock> {
            let mut result = ToolResultBlock::text("written");
            result.metadata.insert(
                "acp_diffs".to_string(),
                serde_json::json!([{"path": "a.txt", "old_text": "old", "new_text": "new"}]),
            );
            Ok(result)
        }
    }

    #[tokio::test]
    async fn render_only_metadata_reaches_the_event_but_not_the_archived_history() {
        // The ACP client renders file diffs from the tool-result event, and a provider
        // request never reads tool-result metadata. Keeping the same bytes in the archived
        // message would put a second copy of every edited file into the context, the journal
        // and every checkpoint (95% of a 9 MB state in production).
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(DiffTool));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "diff_write", JsonMap::new()),
                text_response("done"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-event-only-metadata")
            .model(model)
            .toolkit(toolkit)
            .event_only_tool_metadata(vec!["acp_diffs".to_string()])
            .build()
            .unwrap();

        let mut stream = agent.stream_events(
            vec![Msg::user("write a.txt")],
            crate::agent::stream_options::StreamOptions::defaults(),
        );
        let mut carried = false;
        while let Some(event) = stream.next().await {
            let event = event.unwrap();
            if event.event_type() == AgentEventType::ToolResultEnd {
                carried = event
                    .data()
                    .fields
                    .get("metadata")
                    .is_some_and(|metadata| metadata.get("acp_diffs").is_some());
            }
        }
        assert!(
            carried,
            "the tool-result event must still carry the diff for the client"
        );

        let archived = agent.core.state.lock().unwrap().context().to_vec();
        let tool_result = archived
            .iter()
            .flat_map(Msg::tool_result_blocks)
            .find(|block| block.name.as_deref() == Some("diff_write"))
            .expect("the tool result is archived");
        assert!(
            !tool_result.metadata.contains_key("acp_diffs"),
            "the archived copy must not keep the render-only payload, kept {:?}",
            tool_result.metadata.keys().collect::<Vec<_>>()
        );
    }

    /// A model that fails with a retryable error on its first call, optionally after
    /// streaming some visible text, and succeeds on the next one. Records how many
    /// times it was invoked so a test can tell a retry from a suppressed retry.
    struct FlakyStreamModel {
        calls: Arc<Mutex<u32>>,
        partial_text: &'static str,
    }

    impl FlakyStreamModel {
        fn new(partial_text: &'static str) -> Self {
            Self {
                calls: Arc::new(Mutex::new(0)),
                partial_text,
            }
        }
    }

    #[async_trait]
    impl Model for FlakyStreamModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            let calls = Arc::clone(&self.calls);
            let partial = self.partial_text;
            let mut call_no = calls.lock().unwrap();
            *call_no += 1;
            let attempt = *call_no;
            drop(call_no);
            Box::pin(futures::stream::iter(if attempt == 1 {
                let mut chunks: Vec<Result<ChatResponse>> = Vec::new();
                if !partial.is_empty() {
                    chunks.push(Ok(text_response(partial)));
                }
                chunks.push(Err(Error::http_status(503, "upstream unavailable")));
                chunks
            } else {
                vec![Ok(text_response("second attempt"))]
            }))
        }

        fn model_name(&self) -> &str {
            "flaky-stream"
        }
    }

    /// Fast retry policy so the tests do not wait out the production backoff.
    fn fast_retry_options(max_attempts: u32) -> GenerateOptions {
        GenerateOptions::builder()
            .execution_config(
                ExecutionConfig::builder()
                    .max_attempts(max_attempts)
                    .initial_backoff(std::time::Duration::from_millis(1))
                    .max_backoff(std::time::Duration::from_millis(1))
                    .build(),
            )
            .build()
    }

    #[tokio::test]
    async fn retry_is_suppressed_after_visible_text_was_emitted() {
        // A retry replays the request from scratch, so retrying after a partial
        // answer would render that prefix twice. Once a text delta has reached the
        // consumer, the failure must surface instead of being retried.
        let model = Arc::new(FlakyStreamModel::new("partial answer"));
        let calls = Arc::clone(&model.calls);
        let agent = ReActAgent::builder()
            .name("t-retry-visible")
            .model(model)
            .build()
            .unwrap();
        agent.set_generate_options_defaults(fast_retry_options(3));

        let mut stream = agent.stream_events(
            vec![Msg::user("hi")],
            crate::agent::stream_options::StreamOptions::defaults(),
        );

        // Drain the whole stream: the merge of lifecycle events and forwarded deltas
        // does not promise an order between the error and the deltas already queued.
        let items: Vec<Result<AgentEvent>> = stream.collect().await;
        let deltas: Vec<String> = items
            .iter()
            .filter_map(|item| item.as_ref().ok())
            .filter(|ev| ev.event_type() == AgentEventType::TextBlockDelta)
            .filter_map(|ev| {
                ev.data()
                    .fields
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        let error = items.into_iter().find_map(|item| item.err());

        assert_eq!(
            deltas.join(""),
            "partial answer",
            "the prefix reached the consumer once"
        );
        let error = error.expect("the failure must be surfaced, not retried");
        assert!(
            error.to_string().contains("retry suppressed"),
            "the error must explain why it was not retried; got: {error}"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "the model must not be called again after visible output"
        );
    }

    #[tokio::test]
    async fn retry_still_happens_when_no_visible_text_was_emitted() {
        // The guard must stay narrow: a retryable failure that produced no visible
        // text (here: nothing at all) must still be retried.
        let model = Arc::new(FlakyStreamModel::new(""));
        let calls = Arc::clone(&model.calls);
        let agent = ReActAgent::builder()
            .name("t-retry-invisible")
            .model(model)
            .build()
            .unwrap();
        agent.set_generate_options_defaults(fast_retry_options(3));

        let reply = agent
            .call(vec![Msg::user("hi")])
            .await
            .expect("the second attempt succeeds");
        assert_eq!(reply.text_content(), "second attempt");
        assert_eq!(
            *calls.lock().unwrap(),
            2,
            "a retryable failure with no visible output must still be retried"
        );
    }

    #[tokio::test]
    async fn reasoning_only_output_still_allows_a_retry() {
        // Thinking deltas are not answer text, so they cannot duplicate anything the
        // user reads: a failure after them must still be retried.
        let model = Arc::new(ThinkingThenFailModel::default());
        let calls = Arc::clone(&model.calls);
        let agent = ReActAgent::builder()
            .name("t-retry-thinking")
            .model(model)
            .build()
            .unwrap();
        agent.set_generate_options_defaults(fast_retry_options(3));

        let reply = agent
            .call(vec![Msg::user("hi")])
            .await
            .expect("the second attempt succeeds");
        assert_eq!(reply.text_content(), "second attempt");
        assert_eq!(
            *calls.lock().unwrap(),
            2,
            "thinking output alone must not suppress a retry"
        );
    }

    /// A model that always fails retryably and counts its invocations.
    struct AlwaysFailingModel {
        name: &'static str,
        message: &'static str,
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl Model for AlwaysFailingModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            *self.calls.lock().unwrap() += 1;
            let message = self.message;
            Box::pin(futures::stream::iter(vec![Err(Error::http_status(
                503, message,
            ))]))
        }

        fn model_name(&self) -> &str {
            self.name
        }
    }

    /// Answers with a fixed text and counts its invocations.
    struct CountingTextModel {
        name: &'static str,
        text: &'static str,
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl Model for CountingTextModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            *self.calls.lock().unwrap() += 1;
            let text = self.text;
            Box::pin(futures::stream::once(
                async move { Ok(text_response(text)) },
            ))
        }

        fn model_name(&self) -> &str {
            self.name
        }
    }

    /// Records what a failover reported.
    #[derive(Default)]
    struct RecordingFailover {
        primaries: Arc<Mutex<Vec<String>>>,
        error: Arc<Mutex<Option<String>>>,
    }

    impl crate::agent::FailoverListener for RecordingFailover {
        fn on_failover(&self, primary_model: &str, error: &Error) {
            self.primaries
                .lock()
                .unwrap()
                .push(primary_model.to_string());
            *self.error.lock().unwrap() = Some(error.to_string());
        }
    }

    #[tokio::test]
    async fn fallback_model_takes_over_after_the_primary_exhausts_its_budget() {
        // CORE-03: core had a single model and no switch, so a failing primary surfaced
        // its error instead of trying the configured backup.
        let primary_calls = Arc::new(Mutex::new(0));
        let fallback_calls = Arc::new(Mutex::new(0));
        let listener: Arc<RecordingFailover> = Arc::new(RecordingFailover::default());
        let listener_for_agent = Arc::clone(&listener);
        let agent = ReActAgent::builder()
            .name("t-failover")
            .model(Arc::new(AlwaysFailingModel {
                name: "primary",
                message: "primary down",
                calls: Arc::clone(&primary_calls),
            }))
            .fallback_model(Arc::new(CountingTextModel {
                name: "fallback",
                text: "from fallback",
                calls: Arc::clone(&fallback_calls),
            }))
            .failover_listener(listener_for_agent)
            .build()
            .unwrap();
        agent.set_generate_options_defaults(fast_retry_options(2));

        let reply = agent.call(vec![Msg::user("hi")]).await.unwrap();

        assert_eq!(reply.text_content(), "from fallback");
        assert_eq!(
            *primary_calls.lock().unwrap(),
            2,
            "the primary must exhaust its own retry budget first"
        );
        assert_eq!(*fallback_calls.lock().unwrap(), 1);
        assert_eq!(
            *listener.primaries.lock().unwrap(),
            vec!["primary".to_string()],
            "the switch must be observable exactly once"
        );
        assert!(listener
            .error
            .lock()
            .unwrap()
            .as_deref()
            .unwrap_or_default()
            .contains("primary down"));
    }

    #[tokio::test]
    async fn a_failing_fallback_surfaces_its_error_without_switching_again() {
        let primary_calls = Arc::new(Mutex::new(0));
        let fallback_calls = Arc::new(Mutex::new(0));
        let listener: Arc<RecordingFailover> = Arc::new(RecordingFailover::default());
        let listener_for_agent = Arc::clone(&listener);
        let agent = ReActAgent::builder()
            .name("t-failover-failing")
            .model(Arc::new(AlwaysFailingModel {
                name: "primary",
                message: "primary down",
                calls: Arc::clone(&primary_calls),
            }))
            .fallback_model(Arc::new(AlwaysFailingModel {
                name: "fallback",
                message: "fallback down",
                calls: Arc::clone(&fallback_calls),
            }))
            .failover_listener(listener_for_agent)
            .build()
            .unwrap();
        agent.set_generate_options_defaults(fast_retry_options(1));

        let err = agent.call(vec![Msg::user("hi")]).await.unwrap_err();

        assert!(err.to_string().contains("fallback down"), "{err}");
        assert_eq!(*primary_calls.lock().unwrap(), 1);
        assert_eq!(*fallback_calls.lock().unwrap(), 1);
        assert_eq!(
            listener.primaries.lock().unwrap().len(),
            1,
            "a failing fallback must not report a second switch"
        );
    }

    /// Streams a thinking chunk, then fails retryably; succeeds on the second call.
    #[derive(Default)]
    struct ThinkingThenFailModel {
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl Model for ThinkingThenFailModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            let mut call_no = self.calls.lock().unwrap();
            *call_no += 1;
            let attempt = *call_no;
            drop(call_no);
            Box::pin(futures::stream::iter(if attempt == 1 {
                vec![
                    Ok(ChatResponse::builder()
                        .content(vec![ContentBlock::Thinking(ThinkingBlock::new("hmm"))])
                        .build()),
                    Err(Error::http_status(503, "upstream unavailable")),
                ]
            } else {
                vec![Ok(text_response("second attempt"))]
            }))
        }

        fn model_name(&self) -> &str {
            "thinking-then-fail"
        }
    }

    #[tokio::test]
    async fn max_iters_terminates() {
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "nope", JsonMap::new()),
                tool_use_response("c2", "nope", JsonMap::new()),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t3")
            .model(model)
            .max_iters(2)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("loop")]).await.unwrap();
        assert_eq!(reply.generate_reason(), GenerateReason::MaxIterations);
    }

    /// A model that records every message list it is called with.
    struct RecordingModel {
        seen: Arc<Mutex<Vec<Vec<Msg>>>>,
    }

    impl RecordingModel {
        fn new() -> Self {
            Self {
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl Model for RecordingModel {
        fn stream(
            &self,
            messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            self.seen.lock().unwrap().push(messages);
            Box::pin(futures::stream::once(async { Ok(text_response("ok")) }))
        }
        fn model_name(&self) -> &str {
            "recording"
        }
    }

    /// Records every request and pops queued responses, so a test can force multiple
    /// reasoning iterations while inspecting the exact system message sent each time.
    struct SequenceRecordingModel {
        seen: Arc<Mutex<Vec<Vec<Msg>>>>,
        responses: Arc<Mutex<Vec<ChatResponse>>>,
    }

    impl SequenceRecordingModel {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                seen: Arc::new(Mutex::new(Vec::new())),
                responses: Arc::new(Mutex::new(responses)),
            }
        }
    }

    #[async_trait]
    impl Model for SequenceRecordingModel {
        fn stream(
            &self,
            messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            self.seen.lock().unwrap().push(messages);
            let response = {
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    text_response("done")
                } else {
                    responses.remove(0)
                }
            };
            Box::pin(futures::stream::once(async move { Ok(response) }))
        }
        fn model_name(&self) -> &str {
            "sequence-recording"
        }
    }

    /// Middleware that rewrites the reasoning input before the model call.
    struct RewriteReasoningMiddleware;

    impl crate::middleware::Middleware for RewriteReasoningMiddleware {
        fn on_reasoning(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            mut input: ReasoningInput,
            next: Next<ReasoningInput>,
        ) -> EventStream {
            input.messages.push(Msg::user("REWRITTEN"));
            next(input)
        }
    }

    /// Middleware that attaches a freshly built SYSTEM prompt to every call, like
    /// `WorkspaceContextMiddleware` does for workspace/memory context.
    struct SystemInjectingMiddleware(&'static str);

    impl crate::middleware::Middleware for SystemInjectingMiddleware {
        fn on_agent(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            mut input: AgentInput,
            next: Next<AgentInput>,
        ) -> EventStream {
            input.messages.insert(0, Msg::system(self.0));
            next(input)
        }
    }

    /// Middleware that uses the request-scoped system prompt channel.
    struct RequestSystemPromptMiddleware(&'static str);

    impl crate::middleware::Middleware for RequestSystemPromptMiddleware {
        fn on_agent(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            mut input: AgentInput,
            next: Next<AgentInput>,
        ) -> EventStream {
            input.append_system_prompt(self.0);
            next(input)
        }
    }

    /// Middleware that counts how often the system prompt transformer runs.
    struct CountingSystemPromptMiddleware {
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl crate::middleware::Middleware for CountingSystemPromptMiddleware {
        async fn on_system_prompt(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            current_prompt: String,
        ) -> String {
            *self.calls.lock().unwrap() += 1;
            format!("{current_prompt}\nFROZEN")
        }
    }

    /// Middleware that captures the full pre-call state snapshot and appends prompt material.
    struct PreCallSnapshotMiddleware {
        snapshots: Arc<Mutex<Vec<(usize, usize)>>>,
    }

    #[async_trait]
    impl crate::middleware::Middleware for PreCallSnapshotMiddleware {
        async fn on_pre_call(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            state: &mut crate::state::AgentState,
            input_messages: &[Msg],
            system_prompt: &mut String,
        ) {
            self.snapshots
                .lock()
                .unwrap()
                .push((state.context().len(), input_messages.len()));
            system_prompt.push_str("\nPRE_CALL");
        }
    }

    /// Middleware that incorrectly appends a second SYSTEM message at reasoning time.
    struct ExtraSystemMiddleware;

    impl crate::middleware::Middleware for ExtraSystemMiddleware {
        fn on_reasoning(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            mut input: ReasoningInput,
            next: Next<ReasoningInput>,
        ) -> EventStream {
            input.messages.push(Msg::system("EXTRA SYSTEM"));
            next(input)
        }
    }

    /// Middleware that fails before the model core runs.
    struct FailingReasoningMiddleware;

    impl crate::middleware::Middleware for FailingReasoningMiddleware {
        fn on_reasoning(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            _input: ReasoningInput,
            _next: Next<ReasoningInput>,
        ) -> EventStream {
            Box::pin(futures::stream::once(async {
                Err(Error::Agent("context preparation failed".into()))
            }))
        }
    }

    /// Middleware that records each `on_agent_state_ready` notification and can
    /// optionally decline participation.
    struct StateReadyMiddleware {
        calls: Arc<Mutex<Vec<(usize, Option<String>)>>>,
        participate: bool,
    }

    impl crate::middleware::Middleware for StateReadyMiddleware {
        fn on_agent_state_ready(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            state: &mut crate::state::AgentState,
            input_messages: &[Msg],
        ) {
            let session = state.session_id().to_string();
            self.calls.lock().unwrap().push((
                input_messages.len(),
                if session.is_empty() {
                    None
                } else {
                    Some(session)
                },
            ));
        }

        fn active_points(&self) -> Vec<crate::middleware::ExtensionPoint> {
            if self.participate {
                crate::middleware::ExtensionPoint::ALL.to_vec()
            } else {
                Vec::new()
            }
        }
    }

    /// Middleware that records the run id visible on the runtime context.
    struct RunIdRecordingMiddleware {
        seen: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl crate::middleware::Middleware for RunIdRecordingMiddleware {
        fn on_agent_state_ready(
            &self,
            ctx: &crate::agent::RuntimeContext,
            _state: &mut crate::state::AgentState,
            _input_messages: &[Msg],
        ) {
            self.seen
                .lock()
                .unwrap()
                .push(ctx.run_id().map(str::to_string));
        }
    }

    #[tokio::test]
    async fn agent_state_ready_callback_runs_through_the_real_assembly_path() {
        // Built through the public builder + `call`, so a regression that drops the
        // `notify_agent_state_ready` wiring from the real execution entry point fails
        // here (unlike a test that pokes the chain's internals directly).
        let calls: Arc<Mutex<Vec<(usize, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(StateReadyMiddleware {
            calls: Arc::clone(&calls),
            participate: true,
        }));
        let model = Arc::new(MockModel::new("mock", vec![text_response("ok")]));
        let agent = ReActAgent::builder()
            .name("t-state-ready")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("hi")]).await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "on_agent_state_ready must fire once per call"
        );
        assert_eq!(
            recorded[0].0, 1,
            "the callback must receive this call's input messages"
        );
    }

    #[tokio::test]
    async fn agent_state_ready_skips_non_participating_middlewares() {
        // `active_points()` is a participation switch: an empty set disables the
        // middleware at every point while keeping its registration.
        let calls: Arc<Mutex<Vec<(usize, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(StateReadyMiddleware {
            calls: Arc::clone(&calls),
            participate: false,
        }));
        let model = Arc::new(MockModel::new("mock", vec![text_response("ok")]));
        let agent = ReActAgent::builder()
            .name("t-state-ready-off")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("hi")]).await.unwrap();

        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_id_is_assigned_to_the_runtime_context_and_cleared_after_the_call() {
        // Each call runs under its own `RunControl`: the context carries its
        // run_id (Java `RuntimeContext.getRunId`), and no run stays installed
        // once the call returns.
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(RunIdRecordingMiddleware {
            seen: Arc::clone(&seen),
        }));
        let model = Arc::new(MockModel::new("mock", vec![text_response("ok")]));
        let agent = ReActAgent::builder()
            .name("t-run-id")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("hi")]).await.unwrap();

        let recorded = seen.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "the run id must be visible to the pipeline"
        );
        let run_id = recorded[0].clone().expect("a run id must be assigned");
        assert_eq!(run_id.len(), 32);
        assert!(run_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            agent.active_run().is_none(),
            "the run must be cleared after the call"
        );
    }

    #[tokio::test]
    async fn reasoning_result_hook_fires_once_per_reasoning_step() {
        // Java `PostReasoningEvent`: the produced assistant message must reach the
        // middleware chain through the real builder + `call` path (a regression that
        // drops `notify_reasoning_result` from `run_reasoning` fails here).
        struct Recorder(Arc<Mutex<Vec<String>>>);

        #[async_trait]
        impl crate::middleware::Middleware for Recorder {
            async fn on_reasoning_result(
                &self,
                _ctx: &crate::agent::RuntimeContext,
                message: &Msg,
            ) {
                self.0.lock().unwrap().push(message.text_content());
            }

            fn active_points(&self) -> Vec<crate::middleware::ExtensionPoint> {
                vec![crate::middleware::ExtensionPoint::OnReasoningResult]
            }
        }

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(Recorder(Arc::clone(&seen))));
        let model = Arc::new(MockModel::new("mock", vec![text_response("reasoned once")]));
        let agent = ReActAgent::builder()
            .name("t-reasoning-result")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("hi")]).await.unwrap();

        assert_eq!(*seen.lock().unwrap(), vec!["reasoned once".to_string()]);
    }

    #[tokio::test]
    async fn reasoning_middleware_rewrites_reach_the_model() {
        let model = Arc::new(RecordingModel::new());
        let seen = Arc::clone(&model.seen);
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(RewriteReasoningMiddleware));
        let agent = ReActAgent::builder()
            .name("t-rewrite")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("hi")]).await.unwrap();
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].iter().any(|m| m.text_content() == "REWRITTEN"),
            "reasoning middleware rewrite must reach the model call"
        );
    }

    #[tokio::test]
    async fn legacy_injected_system_prompts_are_request_scoped() {
        // Regression: ACP passes only the new user input to the agent, so a middleware
        // attaching workspace context created a new SYSTEM message every turn. The loop
        // used to persist each one, growing the request by a full system prompt per turn
        // until the context compiler rejected it as over budget. Legacy SYSTEM messages
        // are now folded into the request-scoped prompt and never enter the state.
        let model = Arc::new(RecordingModel::new());
        let seen = Arc::clone(&model.seen);
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(SystemInjectingMiddleware("WORKSPACE CONTEXT")));
        let agent = ReActAgent::builder()
            .name("t-system-dedup")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();

        agent.call(vec![Msg::user("first")]).await.unwrap();
        agent.call(vec![Msg::user("second")]).await.unwrap();

        let state = agent.state_handle();
        let guard = state.lock().unwrap();
        let systems: Vec<_> = guard
            .context()
            .iter()
            .filter(|message| message.role == MsgRole::System)
            .collect();
        assert_eq!(
            systems.len(),
            0,
            "request-scoped system prompts must never enter the conversation state"
        );

        // The request-scoped prompt still reaches every model request.
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for call in calls.iter() {
            assert!(
                call.iter().any(|message| message.role == MsgRole::System
                    && message.text_content().contains("WORKSPACE CONTEXT")),
                "every request must carry the current system prompt"
            );
        }
    }

    #[tokio::test]
    async fn persisted_system_messages_are_purged_from_restored_state() {
        // Older Rust builds persisted the workspace system prompt into AgentState.
        // A restored session must not keep sending that stale copy now that the
        // prompt is request-scoped.
        let model = Arc::new(RecordingModel::new());
        let seen = Arc::clone(&model.seen);
        let agent = ReActAgent::builder()
            .name("t-system-migration")
            .model(model)
            .sys_prompt("BASE SYSTEM")
            .build()
            .unwrap();

        agent.call(vec![Msg::user("first")]).await.unwrap();
        {
            let state = agent.state_handle();
            let mut guard = state.lock().unwrap();
            guard
                .context_mut()
                .insert(0, Msg::system("STALE WORKSPACE PROMPT"));
        }
        agent.call(vec![Msg::user("second")]).await.unwrap();

        {
            let state = agent.state_handle();
            let guard = state.lock().unwrap();
            assert!(
                guard
                    .context()
                    .iter()
                    .all(|message| message.role != MsgRole::System),
                "restored SYSTEM material must be dropped from conversation state"
            );
        }

        let calls = seen.lock().unwrap();
        let last = calls.last().expect("the second call reached the model");
        let systems: Vec<_> = last
            .iter()
            .filter(|message| message.role == MsgRole::System)
            .collect();
        assert_eq!(systems.len(), 1);
        assert!(systems[0].text_content().contains("BASE SYSTEM"));
        assert!(!systems[0].text_content().contains("STALE WORKSPACE PROMPT"));
    }

    #[tokio::test]
    async fn system_prompt_is_frozen_once_per_call_and_prepended_to_every_request() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));

        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(SequenceRecordingModel::new(vec![
            tool_use_response("c1", "add", input),
            text_response("done"),
        ]));
        let seen = Arc::clone(&model.seen);

        let prompt_calls: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(RequestSystemPromptMiddleware("REQUEST SYSTEM")));
        chain.add(Box::new(CountingSystemPromptMiddleware {
            calls: Arc::clone(&prompt_calls),
        }));
        let agent = ReActAgent::builder()
            .name("t-system-freeze")
            .model(model)
            .toolkit(toolkit)
            .sys_prompt("BASE SYSTEM")
            .middleware_chain(chain)
            .build()
            .unwrap();

        agent.call(vec![Msg::user("compute")]).await.unwrap();

        assert_eq!(
            *prompt_calls.lock().unwrap(),
            1,
            "on_system_prompt must run once per call, not once per reasoning step"
        );

        let calls = seen.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "the tool call must force a second reasoning step"
        );
        for call in calls.iter() {
            let systems: Vec<_> = call
                .iter()
                .filter(|message| message.role == MsgRole::System)
                .collect();
            assert_eq!(
                systems.len(),
                1,
                "base and request-scoped prompt material must be merged into one system message"
            );
            assert!(systems[0].text_content().contains("BASE SYSTEM"));
            assert!(systems[0].text_content().contains("REQUEST SYSTEM"));
            assert!(systems[0].text_content().contains("FROZEN"));
            assert_eq!(systems[0].text_content(), calls[0][0].text_content());
        }

        let state = agent.state_handle();
        let guard = state.lock().unwrap();
        assert!(
            guard
                .context()
                .iter()
                .all(|message| message.role != MsgRole::System),
            "the frozen prompt must not be persisted"
        );
    }

    #[tokio::test]
    async fn pre_call_hook_sees_the_restored_state_snapshot() {
        let model = Arc::new(RecordingModel::new());
        let seen = Arc::clone(&model.seen);
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(PreCallSnapshotMiddleware {
            snapshots: Arc::clone(&snapshots),
        }));
        let agent = ReActAgent::builder()
            .name("t-pre-call")
            .model(model)
            .sys_prompt("BASE SYSTEM")
            .middleware_chain(chain)
            .build()
            .unwrap();

        agent.call(vec![Msg::user("first")]).await.unwrap();
        agent.call(vec![Msg::user("second")]).await.unwrap();

        let snapshots = snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            snapshots[0],
            (0, 1),
            "first call sees an empty restored history"
        );
        assert!(
            snapshots[1].0 > 0,
            "second call sees the previous turn in state"
        );

        let calls = seen.lock().unwrap();
        for call in calls.iter() {
            let system = call
                .iter()
                .find(|message| message.role == MsgRole::System)
                .expect("a frozen system message");
            assert!(system.text_content().contains("PRE_CALL"));
        }
    }

    #[tokio::test]
    async fn reasoning_middleware_cannot_leave_multiple_system_messages() {
        let model = Arc::new(RecordingModel::new());
        let seen = Arc::clone(&model.seen);
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(ExtraSystemMiddleware));
        let agent = ReActAgent::builder()
            .name("t-system-normalize")
            .model(model)
            .sys_prompt("BASE SYSTEM")
            .middleware_chain(chain)
            .build()
            .unwrap();

        agent.call(vec![Msg::user("hi")]).await.unwrap();

        let calls = seen.lock().unwrap();
        let systems: Vec<_> = calls[0]
            .iter()
            .filter(|message| message.role == MsgRole::System)
            .collect();
        assert_eq!(
            systems.len(),
            1,
            "the provider must see exactly one system message"
        );
        assert!(systems[0].text_content().contains("BASE SYSTEM"));
        assert!(systems[0].text_content().contains("EXTRA SYSTEM"));
    }

    #[tokio::test]
    async fn reasoning_middleware_failure_is_not_reported_as_empty_completion() {
        // The context compiler reports an over-budget request as a stream error before
        // the model core runs. `run_reasoning` must surface that error instead of
        // collapsing it into "model produced no message".
        let model = Arc::new(MockModel::new("mock", vec![text_response("unused")]));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(FailingReasoningMiddleware));
        let agent = ReActAgent::builder()
            .name("t-reasoning-error")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let error = agent
            .call(vec![Msg::user("hi")])
            .await
            .expect_err("the middleware failure must abort the call");
        assert!(
            error.to_string().contains("context preparation failed"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn max_iters_does_not_duplicate_answered_tool_results() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c1", "add", input)],
        ));
        let agent = ReActAgent::builder()
            .name("t-max-iters")
            .model(model)
            .toolkit(toolkit)
            .max_iters(1)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        assert_eq!(reply.generate_reason(), GenerateReason::MaxIterations);
        assert_eq!(*counter.lock().unwrap(), 1);
        let context = agent.core.state.lock().unwrap().context().to_vec();
        let answers = context
            .iter()
            .filter(|m| m.role() == MsgRole::Tool)
            .flat_map(|m| m.tool_result_blocks())
            .filter(|tr| tr.id.as_deref() == Some("c1"))
            .count();
        assert_eq!(
            answers, 1,
            "an answered tool call must not be reported twice"
        );
    }

    #[tokio::test]
    async fn permission_denies_tool() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let ctx = PermissionContextState::builder()
            .add_deny_rule(
                "add",
                PermissionRule::new("add", None, PermissionBehavior::Deny, "test"),
            )
            .build();
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c1", "add", input), text_response("done")],
        ));
        let agent = ReActAgent::builder()
            .name("t4")
            .model(model)
            .toolkit(toolkit)
            .permission_context(ctx)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        // The tool must never have been invoked.
        assert_eq!(*counter.lock().unwrap(), 0);
        assert_eq!(reply.text_content(), "done");
    }

    /// Counts `on_acting` invocations and records the tool-call ids it was handed.
    struct ActingProbeMiddleware {
        calls: Arc<Mutex<usize>>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Middleware for ActingProbeMiddleware {
        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnActing]
        }

        fn on_acting(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            input: ActingInput,
            next: Next<ActingInput>,
        ) -> EventStream {
            *self.calls.lock().unwrap() += 1;
            self.seen
                .lock()
                .unwrap()
                .extend(input.tool_calls.iter().map(|c| c.id.clone()));
            next(input)
        }
    }

    #[tokio::test]
    async fn acting_middleware_is_invoked_and_the_tool_still_runs_once() {
        // GAP-16: `wrap_acting` used to be dead code — `on_acting` existed but no code
        // path ever called it. This test drives the real entry point.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c-acting", "add", input),
                text_response("done"),
            ],
        ));
        let calls = Arc::new(Mutex::new(0usize));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(ActingProbeMiddleware {
            calls: Arc::clone(&calls),
            seen: Arc::clone(&seen),
        }));
        let agent = ReActAgent::builder()
            .name("t-acting")
            .model(model)
            .toolkit(toolkit)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        assert_eq!(reply.text_content(), "done");
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "on_acting must be invoked once per acting phase"
        );
        assert_eq!(*seen.lock().unwrap(), vec!["c-acting".to_string()]);
        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "the wrapped core must execute the tool exactly once"
        );
    }

    #[tokio::test]
    async fn acting_middleware_that_completes_without_results_is_a_diagnosable_error() {
        // A middleware that swallows the acting phase must not look like a completed
        // turn: the agent has pending calls but no results to feed back.
        struct ShortCircuit;
        impl Middleware for ShortCircuit {
            fn active_points(&self) -> Vec<ExtensionPoint> {
                vec![ExtensionPoint::OnActing]
            }
            fn on_acting(
                &self,
                _ctx: &crate::agent::RuntimeContext,
                _input: ActingInput,
                _next: Next<ActingInput>,
            ) -> EventStream {
                Box::pin(futures::stream::empty())
            }
        }

        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c-swallowed", "add", JsonMap::new())],
        ));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(ShortCircuit));
        let agent = ReActAgent::builder()
            .name("t-acting-short")
            .model(model)
            .toolkit(toolkit)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let err = agent.call(vec![Msg::user("compute")]).await.unwrap_err();
        assert!(
            err.to_string().contains("produced no result"),
            "unexpected error: {err}"
        );
    }

    /// A `RequestStop` payload carrying an explicit `generate_reason`, like a middleware
    /// that knows which `GenerateReason` the stop means.
    fn stop_event(reason: Option<GenerateReason>) -> AgentEvent {
        let mut data = EventData::default();
        if let Some(reason) = reason {
            data.fields.insert(
                crate::message::metadata_keys::EVENT_GENERATE_REASON.to_string(),
                serde_json::to_value(reason).unwrap(),
            );
        }
        AgentEvent::RequestStop(data)
    }

    /// Stops the turn from the reasoning boundary without calling the model.
    struct ReasoningStopMiddleware {
        reason: Option<GenerateReason>,
    }

    impl Middleware for ReasoningStopMiddleware {
        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnReasoning]
        }

        fn on_reasoning(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            _input: ReasoningInput,
            _next: Next<ReasoningInput>,
        ) -> EventStream {
            let event = stop_event(self.reason);
            Box::pin(futures::stream::once(async move { Ok(event) }))
        }
    }

    #[tokio::test]
    async fn reasoning_stop_request_ends_the_turn_with_the_phase_default_reason() {
        // CORE-01: a `RequestStop` used to be swallowed. Because the middleware never calls
        // `next`, the model was never invoked and the turn failed with "model produced no
        // message" instead of ending as a (resumable) stop.
        let model = Arc::new(MockModel::new(
            "mock",
            vec![text_response("must not be reached")],
        ));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(ReasoningStopMiddleware { reason: None }));
        let agent = ReActAgent::builder()
            .name("t-stop-reasoning")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("hi")]).await.unwrap();

        assert_eq!(
            reply.generate_reason(),
            GenerateReason::ReasoningStopRequested
        );
        // The model core never ran, so no model text can appear in the reply.
        assert!(reply.text_content().is_empty());
    }

    /// Stops the turn from the acting boundary without delegating to the core.
    struct ActingStopMiddleware {
        reason: Option<GenerateReason>,
    }

    impl Middleware for ActingStopMiddleware {
        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnActing]
        }

        fn on_acting(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            _input: ActingInput,
            _next: Next<ActingInput>,
        ) -> EventStream {
            let event = stop_event(self.reason);
            Box::pin(futures::stream::once(async move { Ok(event) }))
        }
    }

    #[tokio::test]
    async fn acting_stop_request_without_results_ends_the_turn_instead_of_erroring() {
        // CORE-01: the acting phase used to demand a result for every pending call, so a
        // middleware that only emitted `RequestStop` produced the hard error "acting phase
        // produced no result". A stop is a legitimate outcome, not a broken phase.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c-stop", "add", JsonMap::new())],
        ));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(ActingStopMiddleware { reason: None }));
        let agent = ReActAgent::builder()
            .name("t-stop-acting")
            .model(model)
            .toolkit(toolkit)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        assert_eq!(reply.generate_reason(), GenerateReason::ActingStopRequested);
        assert_eq!(
            *counter.lock().unwrap(),
            0,
            "a stopped acting phase must not execute the pending calls"
        );
    }

    /// Runs the acting core, then asks the agent to stop with an explicit reason.
    struct ActingStopAfterCore {
        reason: GenerateReason,
    }

    impl Middleware for ActingStopAfterCore {
        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnActing]
        }

        fn on_acting(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            input: ActingInput,
            next: Next<ActingInput>,
        ) -> EventStream {
            let event = stop_event(Some(self.reason));
            let inner = next(input);
            Box::pin(inner.chain(futures::stream::once(async move { Ok(event) })))
        }
    }

    #[tokio::test]
    async fn acting_stop_request_keeps_partial_results_and_its_own_generate_reason() {
        // The reason carried by the event wins over the phase default, and the results that
        // ran before the stop stay in the turn: the reply pairs each `tool_result` with its
        // `tool_use`, so the context holds no dangling call (Java `buildStopMsg`).
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c-partial", "add", input)],
        ));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(ActingStopAfterCore {
            reason: GenerateReason::MiddlewareStopRequested,
        }));
        let agent = ReActAgent::builder()
            .name("t-stop-acting-partial")
            .model(model)
            .toolkit(toolkit)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        assert_eq!(
            reply.generate_reason(),
            GenerateReason::MiddlewareStopRequested
        );
        assert_eq!(*counter.lock().unwrap(), 1);
        assert_eq!(reply.tool_use_blocks().count(), 1);
        let results: Vec<ToolResultBlock> = reply.tool_result_blocks().cloned().collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id.as_deref(), Some("c-partial"));
    }

    /// Watches the acting stream and, when it sees `AllToolsDenied`, records it and
    /// optionally asks the agent to stop — the Java middleware reaction.
    struct AllToolsDeniedWatcher {
        stop_with: Option<Option<GenerateReason>>,
        seen: Arc<Mutex<u32>>,
    }

    impl Middleware for AllToolsDeniedWatcher {
        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnActing]
        }

        fn on_acting(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            input: ActingInput,
            next: Next<ActingInput>,
        ) -> EventStream {
            let mut inner = next(input);
            let seen = Arc::clone(&self.seen);
            let stop_with = self.stop_with;
            Box::pin(async_stream::stream! {
                while let Some(item) = inner.next().await {
                    let denied = matches!(&item, Ok(AgentEvent::AllToolsDenied(_)));
                    if denied {
                        *seen.lock().unwrap() += 1;
                    }
                    yield item;
                    if denied {
                        if let Some(reason) = stop_with {
                            yield Ok(stop_event(reason));
                        }
                    }
                }
            })
        }
    }

    /// A permission context that denies the `add` tool.
    fn deny_add() -> PermissionContextState {
        PermissionContextState::builder()
            .add_deny_rule(
                "add",
                PermissionRule::new("add", None, PermissionBehavior::Deny, "test"),
            )
            .build()
    }

    #[tokio::test]
    async fn an_all_denied_round_lets_a_middleware_stop_the_turn() {
        // CORE-02: `AllToolsDenied` had no emission point, so a middleware could never
        // observe "nothing ran" and stop the turn; the agent reasoned again instead.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", input),
                text_response("must not be reached"),
            ],
        ));
        let seen = Arc::new(Mutex::new(0));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(AllToolsDeniedWatcher {
            stop_with: Some(None),
            seen: Arc::clone(&seen),
        }));
        let agent = ReActAgent::builder()
            .name("t-all-denied-stop")
            .model(model)
            .toolkit(toolkit)
            .permission_context(deny_add())
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        // A stop without an explicit reason defaults to `AllToolsDenied`.
        assert_eq!(reply.generate_reason(), GenerateReason::AllToolsDenied);
        assert_eq!(*counter.lock().unwrap(), 0);
        assert_eq!(*seen.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn an_all_denied_round_without_a_stop_keeps_reasoning() {
        // Backward compatible: the signal alone changes nothing.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", JsonMap::new()),
                text_response("done"),
            ],
        ));
        let seen = Arc::new(Mutex::new(0));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(AllToolsDeniedWatcher {
            stop_with: None,
            seen: Arc::clone(&seen),
        }));
        let agent = ReActAgent::builder()
            .name("t-all-denied-continue")
            .model(model)
            .toolkit(toolkit)
            .permission_context(deny_add())
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        assert_eq!(reply.text_content(), "done");
        assert_eq!(reply.generate_reason(), GenerateReason::ModelStop);
        assert_eq!(*counter.lock().unwrap(), 0);
        assert_eq!(*seen.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_partially_denied_round_is_not_reported_as_all_denied() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            "echo",
            "echoes its input",
            serde_json::json!({"type": "object"}),
            Box::new(|_args: JsonMap, _ctx: ToolExecutionContext| {
                Ok(ToolResultBlock::text("echoed"))
            }),
        )));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                // One round with a denied call and an allowed one: the round as a whole is
                // not "all denied", so no signal and no stop.
                ChatResponse::builder()
                    .content(vec![
                        ContentBlock::ToolUse(ToolUseBlock::new("c1", "add", JsonMap::new())),
                        ContentBlock::ToolUse(ToolUseBlock::new("c2", "echo", JsonMap::new())),
                    ])
                    .build(),
                text_response("done"),
            ],
        ));
        let seen = Arc::new(Mutex::new(0));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(AllToolsDeniedWatcher {
            stop_with: Some(None),
            seen: Arc::clone(&seen),
        }));
        let agent = ReActAgent::builder()
            .name("t-partial-denied")
            .model(model)
            .toolkit(toolkit)
            // A non-trivial context asks for every unlisted mutating tool, so the allowed
            // call needs an explicit allow rule (the round must contain a real success).
            .permission_context(
                PermissionContextState::builder()
                    .add_deny_rule(
                        "add",
                        PermissionRule::new("add", None, PermissionBehavior::Deny, "test"),
                    )
                    .add_allow_rule(
                        "echo",
                        PermissionRule::new("echo", None, PermissionBehavior::Allow, "test"),
                    )
                    .build(),
            )
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        assert_eq!(reply.text_content(), "done");
        assert_eq!(
            *seen.lock().unwrap(),
            0,
            "one allowed call disqualifies the round"
        );
        assert_eq!(*counter.lock().unwrap(), 0, "the denied call never runs");
    }

    #[tokio::test]
    async fn stop_on_reject_is_off_by_default_so_a_denial_is_fed_back() {
        // The default must keep the pre-existing loop: the rejection is reported to the model
        // as a tool result and the turn continues.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", JsonMap::new()),
                text_response("done"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-stop-on-reject-off")
            .model(model)
            .toolkit(toolkit)
            .permission_context(deny_add())
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        assert!(!agent.stop_on_reject());
        assert_eq!(reply.text_content(), "done");
        assert_eq!(reply.generate_reason(), GenerateReason::ModelStop);
        assert_eq!(*counter.lock().unwrap(), 0, "the denied call never runs");
    }

    #[tokio::test]
    async fn stop_on_reject_ends_an_all_denied_turn_without_another_reasoning_round() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        // Only one response: a second reasoning round would return `done` instead.
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c1", "add", JsonMap::new())],
        ));
        let agent = ReActAgent::builder()
            .name("t-stop-on-reject-on")
            .model(model)
            .toolkit(toolkit)
            .permission_context(deny_add())
            .stop_on_reject(true)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        assert!(agent.stop_on_reject());
        assert_ne!(reply.text_content(), "done");
        assert_eq!(reply.generate_reason(), GenerateReason::AllToolsDenied);
        assert_eq!(*counter.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn stop_on_reject_also_ends_a_partially_denied_turn() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            "echo",
            "echoes its input",
            serde_json::json!({"type": "object"}),
            Box::new(|_args: JsonMap, _ctx: ToolExecutionContext| {
                Ok(ToolResultBlock::text("echoed"))
            }),
        )));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![ChatResponse::builder()
                .content(vec![
                    ContentBlock::ToolUse(ToolUseBlock::new("c1", "add", JsonMap::new())),
                    ContentBlock::ToolUse(ToolUseBlock::new("c2", "echo", JsonMap::new())),
                ])
                .build()],
        ));
        let agent = ReActAgent::builder()
            .name("t-stop-on-reject-partial")
            .model(model)
            .toolkit(toolkit)
            .permission_context(
                PermissionContextState::builder()
                    .add_deny_rule(
                        "add",
                        PermissionRule::new("add", None, PermissionBehavior::Deny, "test"),
                    )
                    .add_allow_rule(
                        "echo",
                        PermissionRule::new("echo", None, PermissionBehavior::Allow, "test"),
                    )
                    .build(),
            )
            .stop_on_reject(true)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();

        // The allowed call still ran; the rejection is what ended the turn.
        assert_eq!(reply.generate_reason(), GenerateReason::ActingStopRequested);
        assert_eq!(*counter.lock().unwrap(), 0);
    }

    #[test]
    fn all_tool_calls_denied_requires_a_denied_result_for_every_call() {
        let calls = vec![
            ToolUseBlock::new("c1", "add", JsonMap::new()),
            ToolUseBlock::new("c2", "echo", JsonMap::new()),
        ];
        let denied = |id: &str, name: &str| ToolResultBlock {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            output: vec![],
            metadata: Default::default(),
            state: ToolResultState::Denied,
        };
        let mut state = AgentState::default();

        // No result at all: the round cannot be judged as all-denied.
        assert!(!all_tool_calls_denied(&state, &calls));
        // Only one of two calls answered: same.
        state.context_mut().push(
            Msg::builder()
                .role(MsgRole::Tool)
                .content(vec![ContentBlock::ToolResult(denied("c1", "add"))])
                .build(),
        );
        assert!(!all_tool_calls_denied(&state, &calls));
        // A denied call plus an errored one: not all denied.
        state.context_mut().push(
            Msg::builder()
                .role(MsgRole::Tool)
                .content(vec![ContentBlock::ToolResult(error_result(
                    &calls[1],
                    "bad input",
                ))])
                .build(),
        );
        assert!(!all_tool_calls_denied(&state, &calls));

        let mut state = AgentState::default();
        state.context_mut().push(
            Msg::builder()
                .role(MsgRole::Tool)
                .content(vec![
                    ContentBlock::ToolResult(denied("c1", "add")),
                    ContentBlock::ToolResult(denied("c2", "echo")),
                ])
                .build(),
        );
        assert!(all_tool_calls_denied(&state, &calls));
        assert!(!all_tool_calls_denied(&state, &[]));
    }

    /// A model that records the `response_format` of every request it serves.
    struct FormatRecordingModel {
        seen: Arc<Mutex<Vec<Option<ResponseFormat>>>>,
    }

    #[async_trait]
    impl Model for FormatRecordingModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            self.seen
                .lock()
                .unwrap()
                .push(options.response_format.clone());
            Box::pin(futures::stream::once(async { Ok(text_response("done")) }))
        }

        fn model_name(&self) -> &str {
            "format-recording"
        }
    }

    /// Adds a `json_object` format when the caller asked for none.
    struct FormatInjector;

    #[async_trait]
    impl Middleware for FormatInjector {
        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnFormat]
        }

        async fn on_format(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            format: &mut Option<ResponseFormat>,
        ) {
            if format.is_none() {
                *format = Some(ResponseFormat::json_object());
            }
        }
    }

    #[tokio::test]
    async fn on_format_middleware_rewrites_the_request_the_provider_sees() {
        // STUDIO-12: core had no framework-level format interception point, so a middleware
        // could not rewrite the structured-output request at all.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let model = Arc::new(FormatRecordingModel {
            seen: Arc::clone(&seen),
        });
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(FormatInjector));
        let agent = ReActAgent::builder()
            .name("t-format")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("hi")]).await.unwrap();

        assert_eq!(reply.text_content(), "done");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some(ResponseFormat::json_object())],
            "the rewrite must reach the provider request"
        );
    }

    #[tokio::test]
    async fn dropping_the_acting_stream_aborts_the_tool_task() {
        // Regression for detached tools: a cancelled run drops the event stream, but the
        // tool used to keep running (and writing against a recorder that is closing). The
        // acting task must be aborted when its stream is dropped.
        struct BlockingTool {
            started: Arc<AtomicBool>,
            aborted: Arc<AtomicBool>,
        }

        struct AbortGuard(Arc<AtomicBool>);
        impl Drop for AbortGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        #[async_trait]
        impl Tool for BlockingTool {
            fn name(&self) -> &str {
                "block"
            }
            fn description(&self) -> &str {
                "blocks until the task is aborted"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn call(
                &self,
                _args: JsonMap,
                _ctx: ToolExecutionContext,
            ) -> Result<ToolResultBlock> {
                self.started.store(true, Ordering::SeqCst);
                let _guard = AbortGuard(Arc::clone(&self.aborted));
                futures::future::pending::<()>().await;
                unreachable!("pending never resolves")
            }
        }

        let started = Arc::new(AtomicBool::new(false));
        let aborted = Arc::new(AtomicBool::new(false));
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(BlockingTool {
            started: Arc::clone(&started),
            aborted: Arc::clone(&aborted),
        }));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c-block", "block", JsonMap::new())],
        ));
        // A participating middleware is what routes the phase through the spawned acting
        // task; without it the single-pass path has no detached task to cancel.
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(ActingProbeMiddleware {
            calls: Arc::new(Mutex::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
        }));
        let agent = ReActAgent::builder()
            .name("t-acting-cancel")
            .model(model)
            .toolkit(toolkit)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let mut stream = agent.stream_events_with_extensions(
            vec![Msg::user("block")],
            StreamOptions::defaults(),
            Default::default(),
        );
        // Drain until the tool is actually running, then drop the stream as a cancelled run
        // (or a dropped prompt future) would.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !started.load(Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the tool never started"
            );
            tokio::select! {
                _ = stream.next() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
        drop(stream);

        // Aborting is asynchronous: the task is dropped at its next yield point.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !aborted.load(Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the tool task kept running after its stream was dropped"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn plan_mode_denies_mutating_tool() {
        // While plan mode is active, a non-read-only tool is denied without executing.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response("c1", "add", input), text_response("done")],
        ));
        let agent = ReActAgent::builder()
            .name("t-plan")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        // Activate plan mode on the persisted state the call will run against.
        agent
            .state_handle()
            .lock()
            .unwrap()
            .plan_mode_context
            .set_plan_active(true);
        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            0,
            "mutating tool must be denied in plan mode"
        );
        assert_eq!(reply.text_content(), "done");
        // The denied call is recorded as a DENIED tool result, not silently dropped.
        let context = agent.core.state.lock().unwrap().context().to_vec();
        let denied = context
            .iter()
            .filter(|m| m.role() == MsgRole::Tool)
            .flat_map(|m| m.tool_result_blocks())
            .any(|tr| tr.id.as_deref() == Some("c1") && tr.state == ToolResultState::Denied);
        assert!(denied, "plan-mode denial must surface a Denied tool result");
    }

    #[tokio::test]
    async fn subscriber_receives_events() {
        let model = Arc::new(MockModel::new("mock", vec![text_response("hi")]));
        let agent = ReActAgent::builder()
            .name("t5")
            .model(model)
            .build()
            .unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        agent.subscribe(Box::new(move |ev| {
            s.lock()
                .unwrap()
                .push(ev.event_type().wire_name().to_string());
        }));
        agent.call(vec![Msg::user("hi")]).await.unwrap();
        let names = seen.lock().unwrap().clone();
        assert!(names.iter().any(|n| n == "AGENT_START"));
        assert!(names.iter().any(|n| n == "AGENT_RESULT"));
        assert!(names.iter().any(|n| n == "AGENT_END"));
    }

    #[tokio::test]
    async fn stream_events_filters_by_category() {
        let model = Arc::new(MockModel::new("mock", vec![text_response("hi")]));
        let agent = ReActAgent::builder()
            .name("t-stream")
            .model(model)
            .build()
            .unwrap();

        // Defaults stream everything, including AGENT_START.
        let events: Vec<String> = agent
            .stream_events(
                vec![Msg::user("hi")],
                crate::agent::stream_options::StreamOptions::defaults(),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap().event_type().wire_name().to_string())
            .collect();
        assert!(events.iter().any(|n| n == "AGENT_START"));
        assert!(events.iter().any(|n| n == "AGENT_END"));

        // A non-agent category hides the lifecycle events.
        let opts = crate::agent::stream_options::StreamOptions::builder()
            .event_types(&[crate::agent::stream_options::EventCategory::ToolResult])
            .build();
        let events: Vec<String> = agent
            .stream_events(vec![Msg::user("hi")], opts)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap().event_type().wire_name().to_string())
            .collect();
        assert!(events.is_empty());
    }
    #[tokio::test]
    async fn stream_events_emits_text_delta() {
        // A streamed model response must surface TEXT_BLOCK_DELTA events carrying the
        // incremental text, so callers can render token-by-token while the model
        // generates (边生成边打印).
        let model = Arc::new(MockModel::new("mock", vec![text_response("hello world")]));
        let agent = ReActAgent::builder()
            .name("t-delta")
            .model(model)
            .build()
            .unwrap();

        let events: Vec<AgentEvent> = agent
            .stream_events(
                vec![Msg::user("hi")],
                crate::agent::stream_options::StreamOptions::defaults(),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let deltas: Vec<String> = events
            .iter()
            .filter(|ev| ev.event_type() == AgentEventType::TextBlockDelta)
            .filter_map(|ev| {
                ev.data()
                    .fields
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        assert!(
            !deltas.is_empty(),
            "expected at least one TEXT_BLOCK_DELTA; got {events:?}"
        );
        assert_eq!(deltas.join(""), "hello world");
    }

    #[tokio::test]
    async fn stream_events_emits_tool_call_arguments_as_delta() {
        // OpenAI-compatible providers flush a fully parsed tool call at
        // end-of-stream (no incremental `content`), so the arguments must still
        // reach the stream as a ToolCallDelta — otherwise downstream consumers
        // (e.g. the ACP CLI) render empty tool-call input.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", input),
                text_response("the answer is 3"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-args-delta")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();

        let events: Vec<AgentEvent> = agent
            .stream_events(
                vec![Msg::user("compute")],
                crate::agent::stream_options::StreamOptions::defaults(),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let args: Vec<String> = events
            .iter()
            .filter(|ev| ev.event_type() == AgentEventType::ToolCallDelta)
            .filter_map(|ev| {
                ev.data()
                    .fields
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        assert_eq!(
            args.len(),
            1,
            "expected exactly one ToolCallDelta; got {args:?}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&args[0]).unwrap();
        assert_eq!(parsed["a"], serde_json::json!(1));
        assert_eq!(parsed["b"], serde_json::json!(2));
    }

    #[tokio::test]
    async fn clear_context_resets_message_history_but_keeps_session() {
        let model = Arc::new(MockModel::new("mock", vec![text_response("hi")]));
        let agent = ReActAgent::builder()
            .name("t-clear")
            .model(model)
            .build()
            .unwrap();

        let session_id = agent.core.state.lock().unwrap().session_id().to_string();

        // seed some context
        agent
            .core
            .state
            .lock()
            .unwrap()
            .context_mut()
            .push(Msg::user("hello"));
        assert_eq!(agent.core.state.lock().unwrap().context().len(), 1);

        // clear_context_for with a wrong session is a no-op
        assert!(!agent.clear_context_for("other-session"));
        assert_eq!(agent.core.state.lock().unwrap().context().len(), 1);

        // clear_context_for with the right session clears
        assert!(agent.clear_context_for(&session_id));
        assert_eq!(agent.core.state.lock().unwrap().context().len(), 0);
        assert_eq!(agent.core.state.lock().unwrap().session_id(), session_id);
    }

    #[tokio::test]
    async fn call_with_schema_parses_structured_output() {
        let model = Arc::new(MockModel::new(
            "mock",
            vec![text_response("{\"name\":\"alice\",\"age\":30}")],
        ));
        let agent = ReActAgent::builder()
            .name("t-structured")
            .model(model)
            .build()
            .unwrap();
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), serde_json::json!("object"));
        let reply = agent
            .call_with_schema(
                vec![Msg::user("give json")],
                crate::formatter::JsonSchema::new("person", schema),
            )
            .await
            .unwrap();
        assert!(reply.has_structured_data());
        let data = reply.structured_data().unwrap();
        assert_eq!(data["name"], serde_json::json!("alice"));
        assert_eq!(data["age"], serde_json::json!(30));
    }

    #[tokio::test]
    async fn call_with_schema_fallback_via_generate_response_tool() {
        // The mock model does not support native structured output, so the agent must
        // route through the synthetic `generate_response` tool path.
        let mut resp_input = JsonMap::new();
        resp_input.insert(
            "response".to_string(),
            serde_json::json!({"name": "bob", "age": 42}),
        );
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("g1", STRUCTURED_OUTPUT_TOOL_NAME, resp_input),
                text_response("unused"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-fallback")
            .model(model)
            .build()
            .unwrap();
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), serde_json::json!("object"));
        schema.insert(
            "properties".into(),
            serde_json::json!({"name": {"type": "string"}, "age": {"type": "integer"}}),
        );
        let reply = agent
            .call_with_schema(
                vec![Msg::user("give json")],
                crate::formatter::JsonSchema::new("person", schema),
            )
            .await
            .unwrap();
        assert!(reply.has_structured_data());
        let data = reply.structured_data().unwrap();
        assert_eq!(data["name"], serde_json::json!("bob"));
        assert_eq!(data["age"], serde_json::json!(42));
    }

    /// The `generate_response` arguments used by the structured-output tests.
    fn structured_response_input() -> JsonMap {
        let mut input = JsonMap::new();
        input.insert(
            "response".to_string(),
            serde_json::json!({"name": "bob", "age": 42}),
        );
        input
    }

    /// Records the messages/tools each provider call receives.
    struct RequestRecordingModel {
        responses: Arc<Mutex<Vec<ChatResponse>>>,
        seen: Arc<Mutex<Vec<(Vec<Msg>, Vec<String>)>>>,
    }

    impl RequestRecordingModel {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl Model for RequestRecordingModel {
        fn stream(
            &self,
            messages: Vec<Msg>,
            tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            self.seen
                .lock()
                .unwrap()
                .push((messages, tools.into_iter().map(|t| t.name).collect()));
            let responses = Arc::clone(&self.responses);
            Box::pin(futures::stream::once(async move {
                let mut g = responses.lock().unwrap();
                let r = if g.is_empty() {
                    text_response("done")
                } else {
                    g.remove(0)
                };
                Ok(r)
            }))
        }

        fn model_name(&self) -> &str {
            "request-recording"
        }
    }

    /// Captures what the request-preparation seam (`on_model_call`) receives.
    struct PreparationProbe {
        seen: Arc<Mutex<Vec<(usize, bool, bool)>>>,
    }

    #[async_trait]
    impl Middleware for PreparationProbe {
        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnModelCall]
        }

        fn on_model_call(
            &self,
            _ctx: &crate::agent::RuntimeContext,
            input: ModelCallInput,
            next: Next<ModelCallInput>,
        ) -> EventStream {
            let reminder = input
                .messages
                .iter()
                .any(|m| m.text_content().contains(STRUCTURED_OUTPUT_TOOL_NAME));
            let so_tool = input
                .tools
                .iter()
                .any(|t| t.name == STRUCTURED_OUTPUT_TOOL_NAME);
            self.seen
                .lock()
                .unwrap()
                .push((input.messages.len(), reminder, so_tool));
            next(input)
        }
    }

    #[tokio::test]
    async fn structured_output_request_is_prepared_with_reminder_and_schema() {
        // Java 51a74e2ef: the reminder and the synthetic `generate_response` schema must be
        // part of the request *before* preparation, so a budget check inspects exactly what
        // the provider receives.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(PreparationProbe {
            seen: Arc::clone(&seen),
        }));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![tool_use_response(
                "g1",
                STRUCTURED_OUTPUT_TOOL_NAME,
                structured_response_input(),
            )],
        ));
        let agent = ReActAgent::builder()
            .name("t-so-prepare")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();

        agent
            .call_with_schema(vec![Msg::user("give json")], person_schema())
            .await
            .unwrap();

        let captured = seen.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "one prepared request");
        let (count, reminder, so_tool) = captured[0];
        assert!(
            count >= 2,
            "the reminder is a separate request-local message, got {count} messages"
        );
        assert!(
            reminder,
            "preparation must see the structured-output reminder"
        );
        assert!(so_tool, "preparation must see the generate_response schema");
    }

    #[tokio::test]
    async fn structured_output_uses_the_prepared_request() {
        // The provider must receive the request the preparation seam handed over, not the
        // one built before it (Java: "Use the preparer's returned messages, tools, and
        // options directly").
        struct Rewriter;

        #[async_trait]
        impl Middleware for Rewriter {
            fn active_points(&self) -> Vec<ExtensionPoint> {
                vec![ExtensionPoint::OnModelCall]
            }

            fn on_model_call(
                &self,
                _ctx: &crate::agent::RuntimeContext,
                mut input: ModelCallInput,
                next: Next<ModelCallInput>,
            ) -> EventStream {
                input.messages.push(Msg::user("PREPARED-MARKER"));
                next(input)
            }
        }

        let model = Arc::new(RequestRecordingModel::new(vec![tool_use_response(
            "g1",
            STRUCTURED_OUTPUT_TOOL_NAME,
            structured_response_input(),
        )]));
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(Rewriter));
        let agent = ReActAgent::builder()
            .name("t-so-prepared")
            .model(Arc::clone(&model) as Arc<dyn Model>)
            .middleware_chain(chain)
            .build()
            .unwrap();

        let reply = agent
            .call_with_schema(vec![Msg::user("give json")], person_schema())
            .await
            .unwrap();
        assert!(reply.has_structured_data());

        let seen = model.seen.lock().unwrap().clone();
        let (messages, _tools) = &seen[0];
        assert!(
            messages
                .last()
                .is_some_and(|m| m.text_content().contains("PREPARED-MARKER")),
            "the prepared request must reach the provider"
        );
    }

    #[tokio::test]
    async fn structured_output_propagates_preparation_rejection() {
        // A rejection that happens before any provider call must surface as that error, not
        // as the indistinguishable "model produced no message".
        struct Rejecter;

        #[async_trait]
        impl Middleware for Rejecter {
            fn active_points(&self) -> Vec<ExtensionPoint> {
                vec![ExtensionPoint::OnModelCall]
            }

            fn on_model_call(
                &self,
                _ctx: &crate::agent::RuntimeContext,
                _input: ModelCallInput,
                _next: Next<ModelCallInput>,
            ) -> EventStream {
                Box::pin(futures::stream::once(async {
                    Err(Error::Agent("request rejected by preparation".into()))
                }))
            }
        }

        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(Rejecter));
        let agent = ReActAgent::builder()
            .name("t-so-reject")
            .model(Arc::new(MockModel::new("mock", Vec::new())))
            .middleware_chain(chain)
            .build()
            .unwrap();

        let err = agent
            .call_with_schema(vec![Msg::user("give json")], person_schema())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("request rejected by preparation"),
            "got {err}"
        );
    }

    /// A schema that requires `name:string` and `age:integer`.
    fn person_schema() -> crate::formatter::JsonSchema {
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), serde_json::json!("object"));
        schema.insert(
            "properties".into(),
            serde_json::json!({"name": {"type": "string"}, "age": {"type": "integer"}}),
        );
        schema.insert("required".into(), serde_json::json!(["name", "age"]));
        crate::formatter::JsonSchema::new("person", schema)
    }

    #[tokio::test]
    async fn native_structured_output_rejects_schema_mismatch_and_falls_back() {
        // First call returns a schema-invalid native reply (age is a string), so the
        // agent must discard it and route through the synthetic tool path.
        let mut resp_input = JsonMap::new();
        resp_input.insert(
            "response".to_string(),
            serde_json::json!({"name": "ann", "age": 7}),
        );
        let model = Arc::new(OptionsRecordingModel::new(vec![
            text_response("{\"name\":\"ann\",\"age\":\"seven\"}"),
            tool_use_response("g1", STRUCTURED_OUTPUT_TOOL_NAME, resp_input),
        ]));
        let agent = ReActAgent::builder()
            .name("t-native-validate")
            .model(model.clone() as Arc<dyn Model>)
            .build()
            .unwrap();
        // Native path is used (no tools bound -> `supports_native_structured_output()`).
        let reply = agent
            .call_with_schema(vec![Msg::user("give json")], person_schema())
            .await
            .unwrap();
        assert!(reply.has_structured_data());
        let data = reply.structured_data().unwrap();
        assert_eq!(data["name"], serde_json::json!("ann"));
        assert_eq!(data["age"], serde_json::json!(7));
    }

    #[tokio::test]
    async fn native_structured_output_validation_can_be_disabled() {
        // With validation off, the schema-invalid native reply is accepted as-is (still
        // valid JSON) instead of triggering a fallback round trip.
        let model = Arc::new(OptionsRecordingModel::new(vec![text_response(
            "{\"name\":\"ann\",\"age\":\"seven\"}",
        )]));
        let agent = ReActAgent::builder()
            .name("t-native-no-validate")
            .model(model.clone() as Arc<dyn Model>)
            .validate_structured_output(false)
            .build()
            .unwrap();
        let reply = agent
            .call_with_schema(vec![Msg::user("give json")], person_schema())
            .await
            .unwrap();
        assert!(reply.has_structured_data());
        assert_eq!(
            reply.structured_data().unwrap()["age"],
            serde_json::json!("seven")
        );
        assert_eq!(
            model.seen_choices.lock().unwrap().len(),
            1,
            "no fallback round trip"
        );
    }

    #[tokio::test]
    async fn fallback_forces_specific_tool_choice_on_retry() {
        // The model returns plain text twice (never calling `generate_response`), then
        // finally calls it. The retries must carry a forced `ToolChoice::Specific`.
        let mut resp_input = JsonMap::new();
        resp_input.insert(
            "response".to_string(),
            serde_json::json!({"name": "z", "age": 1}),
        );
        let model = Arc::new(OptionsRecordingModel::new(vec![
            text_response("no tool"),
            text_response("still no tool"),
            tool_use_response("g1", STRUCTURED_OUTPUT_TOOL_NAME, resp_input),
        ]));
        let agent = ReActAgent::builder()
            .name("t-fallback-force")
            .model(model.clone() as Arc<dyn Model>)
            .build()
            .unwrap();
        let reply = agent
            .call_with_schema(vec![Msg::user("give json")], person_schema())
            .await
            .unwrap();
        assert!(reply.has_structured_data());
        let choices = model.seen_choices.lock().unwrap().clone();
        // 1) native attempt (invalid JSON -> rejected, no forced choice)
        // 2) fallback attempt #1 (no forced choice, force_retries == 0)
        // 3) fallback attempt #2 (forced `generate_response`)
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[0], None);
        assert_eq!(choices[1], None);
        assert_eq!(
            choices[2],
            Some(crate::model::ToolChoice::Specific {
                tool_name: STRUCTURED_OUTPUT_TOOL_NAME.into()
            })
        );
    }

    #[tokio::test]
    async fn todo_tool_is_registered_and_persists_to_state() {
        // The built-in todo_write tool should be present and its mutations should persist
        // into AgentState.tasks_context after a call.
        let mut input = JsonMap::new();
        input.insert(
            "todos".to_string(),
            serde_json::json!([{"content": "step one", "status": "in_progress"}]),
        );
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("t1", "todo_write", input),
                text_response("done"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-todo")
            .model(model)
            .build()
            .unwrap();
        // The agent's toolkit must expose todo_write.
        assert!(agent.toolkit().get_tool("todo_write").is_some());
        agent.call(vec![Msg::user("plan")]).await.unwrap();
        let state = agent.agent_state();
        let tasks = state.tasks_context().tasks().to_vec();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].subject, "step one");
        assert_eq!(tasks[0].state, crate::state::context::TaskState::InProgress);
    }

    #[test]
    fn task_requirements_are_off_by_default() {
        // Java `ReactConfig.taskRequirementsEnabled` defaults to false: a session that never
        // asked for requirement tracking must not grow the model-callable surface.
        let agent = ReActAgent::builder()
            .name("t-req-off")
            .model(Arc::new(MockModel::new("mock", Vec::new())))
            .build()
            .unwrap();

        assert!(!agent.task_requirements_enabled());
        assert!(agent
            .toolkit()
            .get_tool("task_requirement_propose")
            .is_none());
    }

    #[test]
    fn task_requirements_register_the_propose_tool() {
        let agent = ReActAgent::builder()
            .name("t-req-on")
            .model(Arc::new(MockModel::new("mock", Vec::new())))
            .task_requirements_enabled(true)
            .build()
            .unwrap();

        assert!(agent.task_requirements_enabled());
        let tool = agent
            .toolkit()
            .get_tool("task_requirement_propose")
            .expect("the propose tool must be registered");
        let args: JsonMap = serde_json::from_value(serde_json::json!({
            "kind": "ACCEPTANCE_CRITERION",
            "text": "The report must end with a cost table",
            "source_ref": "user-message-3"
        }))
        .unwrap();

        let result =
            futures::executor::block_on(tool.call(args, ToolExecutionContext::default())).unwrap();

        assert_eq!(
            result.final_state(),
            crate::message::ToolResultState::Success
        );
        assert!(result.text_content().contains("status=CANDIDATE"));
    }

    #[tokio::test]
    async fn a_proposal_made_during_a_call_survives_into_the_agents_state() {
        // The tool proposes into the shared task bag; the turn boundary must carry that write
        // into the persisted `AgentState.tasks_context`, not overwrite it with a stale copy.
        let mut input = JsonMap::new();
        input.insert("kind".to_string(), serde_json::json!("CONSTRAINT"));
        input.insert(
            "text".to_string(),
            serde_json::json!("Never call the billing API twice"),
        );
        input.insert(
            "source_ref".to_string(),
            serde_json::json!("user-message-5"),
        );
        let agent = ReActAgent::builder()
            .name("t-req-state")
            .model(Arc::new(MockModel::new(
                "mock",
                vec![
                    tool_use_response("r1", "task_requirement_propose", input),
                    text_response("noted"),
                ],
            )))
            .task_requirements_enabled(true)
            .build()
            .unwrap();

        agent
            .call(vec![Msg::user("remember the constraint")])
            .await
            .unwrap();

        let state = agent.agent_state();
        let requirements = state.tasks_context().requirements();
        assert_eq!(requirements.len(), 1);
        assert_eq!(
            requirements[0].kind,
            crate::state::RequirementKind::Constraint
        );
        assert_eq!(requirements[0].text, "Never call the billing API twice");
        assert_eq!(requirements[0].proposed_source_ref, "user-message-5");
    }

    #[tokio::test]
    async fn permission_switch_takes_effect_on_next_call() {
        // A single agent whose permission engine is replaced between calls: the
        // *next* tool call is already evaluated against the new context.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", input.clone()),
                text_response("first"),
                tool_use_response("c2", "add", input.clone()),
                text_response("second"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-switch")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();

        // Trivial (default) context: the tool executes.
        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        assert_eq!(reply.text_content(), "first");
        assert_eq!(*counter.lock().unwrap(), 1);

        // Switch to Explore (read-only): the very next call denies the write tool.
        agent.set_permission_context(
            crate::permission::PermissionContextState::builder()
                .mode(crate::permission::PermissionMode::Explore)
                .build(),
        );
        let reply = agent.call(vec![Msg::user("compute again")]).await.unwrap();
        assert_eq!(reply.text_content(), "second");
        assert_eq!(
            *counter.lock().unwrap(),
            1,
            "the write tool must not run under Explore"
        );
    }

    #[tokio::test]
    async fn ask_approved_executes_the_tool() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", input),
                text_response("the answer is 3"),
            ],
        ));
        // Force an `Ask` decision for the add tool.
        let ctx = crate::permission::PermissionContextState::builder()
            .add_ask_rule(
                "add",
                crate::permission::PermissionRule::new(
                    "add",
                    None,
                    crate::permission::PermissionBehavior::Ask,
                    "test",
                ),
            )
            .build();
        let handler: ConfirmHandler =
            Arc::new(|_call: ToolUseBlock| Box::pin(async move { ConfirmResult::allowed() }));
        let agent = ReActAgent::builder()
            .name("t-ask-ok")
            .model(model)
            .toolkit(toolkit)
            .permission_context(ctx)
            .confirm_handler(handler)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        assert_eq!(reply.text_content(), "the answer is 3");
        assert_eq!(*counter.lock().unwrap(), 1, "approved tool must run");
    }

    #[tokio::test]
    async fn ask_denied_skips_tool_and_reports_permission_denied() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", input),
                text_response("ok, skipping"),
            ],
        ));
        let ctx = crate::permission::PermissionContextState::builder()
            .add_ask_rule(
                "add",
                crate::permission::PermissionRule::new(
                    "add",
                    None,
                    crate::permission::PermissionBehavior::Ask,
                    "test",
                ),
            )
            .build();
        let handler: ConfirmHandler = Arc::new(|_call: ToolUseBlock| {
            Box::pin(async move { ConfirmResult::denied("user said no") })
        });
        let agent = ReActAgent::builder()
            .name("t-ask-no")
            .model(model)
            .toolkit(toolkit)
            .permission_context(ctx)
            .confirm_handler(handler)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        assert_eq!(reply.text_content(), "ok, skipping");
        assert_eq!(*counter.lock().unwrap(), 0, "denied tool must not run");

        // The denial is fed back to the model as a Tool message.
        let state = agent.agent_state();
        let denied_feedback = state.context().iter().any(|m| {
            m.tool_result_blocks().any(|rb| {
                rb.output.iter().any(|block| {
                    matches!(
                        block,
                        crate::message::ContentBlock::Text(t)
                            if t.text.contains("Permission denied: user said no")
                    )
                })
            })
        });
        assert!(
            denied_feedback,
            "the denial must be fed back to the model as a Tool result"
        );
    }

    #[tokio::test]
    async fn confirm_handler_can_be_swapped_at_runtime() {
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        // Ask rules for add; the agent starts with NO handler so an Ask would error.
        let ctx = crate::permission::PermissionContextState::builder()
            .add_ask_rule(
                "add",
                crate::permission::PermissionRule::new(
                    "add",
                    None,
                    crate::permission::PermissionBehavior::Ask,
                    "test",
                ),
            )
            .build();
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("c1", "add", input.clone()),
                text_response("after approved"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t-handler-swap")
            .model(model)
            .toolkit(toolkit)
            .permission_context(ctx)
            .build()
            .unwrap();

        // Install the handler *after* build; the next call must honor it.
        agent.set_confirm_handler(Some(Arc::new(|_call: ToolUseBlock| {
            Box::pin(async move { ConfirmResult::allowed() })
        })));
        let reply = agent.call(vec![Msg::user("compute")]).await.unwrap();
        assert_eq!(reply.text_content(), "after approved");
        assert_eq!(*counter.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn server_tool_call_is_not_dispatched_and_ends_the_turn() {
        // A provider-executed (server) tool call arrives with its result already inline.
        // The agent must not dispatch it to the local toolkit and must treat the turn as
        // finished (no acting phase, no synthetic "tool not found" error).
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![server_tool_response("s1", "google_search")],
        ));
        let agent = ReActAgent::builder()
            .name("t-server-tool")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let reply = agent.call(vec![Msg::user("search the web")]).await.unwrap();
        assert_eq!(
            *counter.lock().unwrap(),
            0,
            "server tool must not hit the local toolkit"
        );
        // The turn ends on the provider's assistant message, not a fabricated error result.
        assert!(reply
            .tool_use_blocks()
            .any(|t| t.id == "s1" && t.is_server_tool()));
    }

    /// Provider-executed calls must never be dispatched locally, even when the inline
    /// result is missing (e.g. a chunked stream that dropped it).
    #[test]
    fn pending_tool_calls_skip_server_tools_without_inline_results() {
        let mut server_call = ToolUseBlock::new("s1", "google_search", JsonMap::new());
        server_call.metadata.insert(
            ToolUseBlock::METADATA_SERVER_TOOL.to_string(),
            serde_json::json!(true),
        );
        let mut local_call = ToolUseBlock::new("c1", "add", JsonMap::new());
        local_call.id = "c1".to_string();
        let mut state = AgentState::default();
        state.context.push(Msg::assistant_blocks(vec![
            ContentBlock::ToolUse(server_call),
            ContentBlock::ToolUse(local_call),
        ]));
        let pending = extract_pending_tool_calls(&state);
        assert_eq!(pending.len(), 1, "only the local tool call stays pending");
        assert_eq!(pending[0].id, "c1");
    }

    /// A model that raises the agent's interrupt signal right before yielding its
    /// response, simulating an interrupt that lands after the reasoning phase has
    /// completed but before the acting phase runs.
    struct InterruptingModel {
        name: String,
        responses: Arc<Mutex<Vec<ChatResponse>>>,
        agent: Arc<Mutex<Option<std::sync::Weak<ReActAgent>>>>,
    }

    impl InterruptingModel {
        fn new(name: &str, responses: Vec<ChatResponse>) -> Self {
            Self {
                name: name.to_string(),
                responses: Arc::new(Mutex::new(responses)),
                agent: Arc::new(Mutex::new(None)),
            }
        }

        fn attach(&self, agent: &Arc<ReActAgent>) {
            *self.agent.lock().unwrap() = Some(Arc::downgrade(agent));
        }
    }

    #[async_trait]
    impl Model for InterruptingModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            let responses = Arc::clone(&self.responses);
            let agent = Arc::clone(&self.agent);
            Box::pin(futures::stream::once(async move {
                let upgraded = agent.lock().unwrap().as_ref().and_then(|w| w.upgrade());
                if let Some(a) = upgraded {
                    a.interrupt();
                }
                let mut g = responses.lock().unwrap();
                let r = if g.is_empty() {
                    text_response("done")
                } else {
                    g.remove(0)
                };
                Ok(r)
            }))
        }

        fn model_name(&self) -> &str {
            &self.name
        }
    }

    #[tokio::test]
    async fn interrupt_preserves_progress_and_returns_recovery() {
        // The interrupt lands after the reasoning phase produced an assistant
        // tool_use but before the acting phase could run. The turn must still
        // commit the user input and that assistant message, answer the dangling
        // tool_use, and return a recovery reply instead of discarding the turn.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let mut toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(1));
        input.insert("b".to_string(), serde_json::json!(2));
        let model = Arc::new(InterruptingModel::new(
            "mock",
            vec![tool_use_response("c1", "add", input)],
        ));
        let agent = Arc::new(
            ReActAgent::builder()
                .name("t-interrupt")
                .model(model.clone() as Arc<dyn Model>)
                .toolkit(toolkit)
                .build()
                .unwrap(),
        );
        model.attach(&agent);

        let reply = agent.call(vec![Msg::user("compute 1+2")]).await.unwrap();
        assert_eq!(reply.generate_reason(), GenerateReason::Interrupted);

        let state = agent.core.state.lock().unwrap();
        let ctx = state.context();
        assert!(
            ctx.iter()
                .any(|m| m.role() == MsgRole::User && m.text_content() == "compute 1+2"),
            "the user input must survive an interrupt; got {ctx:?}"
        );
        assert!(
            ctx.iter()
                .any(|m| m.role() == MsgRole::Assistant && m.tool_use_blocks().next().is_some()),
            "the assistant message produced before the interrupt must survive"
        );
        // The dangling tool_use is answered with a synthesized error result so the
        // committed context stays provider-valid.
        let answered = ctx
            .iter()
            .filter(|m| m.role() == MsgRole::Tool)
            .flat_map(|m| m.tool_result_blocks())
            .any(|t| t.id.as_deref() == Some("c1"));
        assert!(
            answered,
            "the dangling c1 tool_use must be answered; got {ctx:?}"
        );
        assert_eq!(
            *counter.lock().unwrap(),
            0,
            "the interrupted tool must not run"
        );
    }

    #[tokio::test]
    async fn interrupt_flag_is_reset_on_the_next_call() {
        // A user interrupt must not wedge the agent for good: Java gives every call
        // a fresh InterruptControl, so the next call has to run normally rather
        // than immediately failing on the stale signal.
        let model = Arc::new(MockModel::new("mock", vec![text_response("hello")]));
        let agent = ReActAgent::builder()
            .name("t-reset")
            .model(model)
            .build()
            .unwrap();

        agent.interrupt();
        let reply = agent.call(vec![Msg::user("hi")]).await.unwrap();
        assert_eq!(reply.text_content(), "hello");
    }

    #[tokio::test]
    async fn pending_tool_recovery_patches_orphaned_tool_use() {
        // A persisted state may hold an assistant tool_use with no matching result
        // (the process died mid-acting). With recovery enabled the next call
        // answers it with a synthetic error result instead of failing.
        let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let mut toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::clone(&counter)));
        let initial = AgentState::builder()
            .session_id("s-recovery")
            .add_message(Msg::user("compute"))
            .add_message(
                Msg::builder()
                    .role(MsgRole::Assistant)
                    .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                        "c9",
                        "add",
                        JsonMap::new(),
                    ))])
                    .build(),
            )
            .build();
        let model = Arc::new(MockModel::new("mock", vec![text_response("resumed")]));
        let agent = ReActAgent::builder()
            .name("t-patch")
            .model(model)
            .toolkit(toolkit)
            .state(initial)
            .enable_pending_tool_recovery(true)
            .build()
            .unwrap();

        let reply = agent.call(vec![Msg::user("continue")]).await.unwrap();
        assert_eq!(reply.text_content(), "resumed");
        let state = agent.core.state.lock().unwrap();
        let ctx = state.context();
        let answered = ctx
            .iter()
            .filter(|m| m.role() == MsgRole::Tool)
            .flat_map(|m| m.tool_result_blocks())
            .any(|t| t.id.as_deref() == Some("c9"));
        assert!(
            answered,
            "orphaned c9 tool_use must be patched; got {ctx:?}"
        );
        assert_eq!(
            *counter.lock().unwrap(),
            0,
            "the orphaned tool must not execute"
        );
    }

    #[tokio::test]
    async fn unresolvable_pending_tool_call_fails_fast() {
        // Same dangling state as the recovery test above, but with recovery disabled
        // and a prompt that carries no tool results: the loop must refuse with an
        // actionable error instead of shipping a dangling tool_use to the provider
        // (HTTP 400) or letting the harness sanitizer silently drop it. Mirrors Java
        // `doCallInner`'s `IllegalStateException`.
        let mut toolkit = Toolkit::new();
        toolkit.register_tool(add_tool(Arc::new(Mutex::new(0))));
        let initial = AgentState::builder()
            .session_id("s-unresolvable")
            .add_message(Msg::user("compute"))
            .add_message(
                Msg::builder()
                    .role(MsgRole::Assistant)
                    .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                        "c9",
                        "add",
                        JsonMap::new(),
                    ))])
                    .build(),
            )
            .build();
        let model = Arc::new(MockModel::new("mock", vec![text_response("unreachable")]));
        let agent = ReActAgent::builder()
            .name("t-unresolvable")
            .model(model)
            .toolkit(toolkit)
            .state(initial)
            .build()
            .unwrap();

        let err = agent
            .call(vec![Msg::user("continue")])
            .await
            .expect_err("a dangling tool call with no recovery must fail fast");
        let text = err.to_string();
        assert!(
            text.contains("Pending tool calls exist without results"),
            "expected the fail-fast guard; got: {text}"
        );
        assert!(
            text.contains("c9"),
            "the error must name the pending id; got: {text}"
        );
        // The turn must not have been committed: no follow-up reply was produced.
        let state = agent.core.state.lock().unwrap();
        assert!(
            !state
                .context()
                .iter()
                .any(|m| m.text_content().contains("unreachable")),
            "the model must not have been called"
        );
    }

    /// A model whose stream never completes: the call stays in flight until it is dropped.
    struct BlockingModel;

    #[async_trait]
    impl Model for BlockingModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            Box::pin(futures::stream::pending())
        }

        fn model_name(&self) -> &str {
            "blocking"
        }
    }

    /// A model that keeps producing chunks, so the loop keeps reaching its checkpoints
    /// (where a cancellation is observed) while the call is in flight.
    struct TickingModel;

    #[async_trait]
    impl Model for TickingModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            Box::pin(futures::stream::unfold((), |_| async move {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                Some((Ok(text_response("tick")), ()))
            }))
        }

        fn model_name(&self) -> &str {
            "ticking"
        }
    }

    #[tokio::test]
    async fn a_caller_cancel_signal_ends_the_run_as_cancelled() {
        // GAP-19: the caller holds a signal and stops the execution from another task.
        // The agent observes it at its next checkpoint and reports the run as cancelled
        // (not as a failure).
        let agent = ReActAgent::builder()
            .name("t-cancel")
            .model(Arc::new(TickingModel))
            .build()
            .unwrap();
        let signal = CancelSignal::new();
        let call_signal = signal.clone();
        let handle = tokio::spawn(async move {
            agent
                .call_with_cancel(vec![Msg::user("hi")], call_signal)
                .await
        });
        // Let the call start, then ask it to stop.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(signal.cancel());

        let error = handle
            .await
            .expect("the call task must not panic")
            .expect_err("a cancelled call must not produce a reply");
        assert!(
            matches!(error, Error::Cancelled(_)),
            "expected a cancellation, got: {error}"
        );
    }

    #[tokio::test]
    async fn dropping_the_call_future_marks_the_run_cancelled_and_clears_the_slot() {
        // The disposal path: a call whose future is dropped (timeout, abort, lost lease)
        // never reaches its own cleanup. The run must still end as cancelled, the agent-level
        // slot must not keep a dead run, and the next call must run normally.
        let agent = Arc::new(
            ReActAgent::builder()
                .name("t-drop")
                .model(Arc::new(BlockingModel))
                .build()
                .unwrap(),
        );
        let call_agent = Arc::clone(&agent);
        let handle = tokio::spawn(async move { call_agent.call(vec![Msg::user("hi")]).await });
        tokio::task::yield_now().await;

        let run = agent.active_run().expect("the call must be in flight");
        handle.abort();
        let _ = handle.await;

        assert_eq!(
            run.status(),
            RunStatus::Cancelled,
            "a dropped call must end its run as cancelled"
        );
        assert!(
            agent.active_run().is_none(),
            "the agent-level slot must not keep a dead run"
        );
    }

    /// A model that keeps the first call ticking (so it can be cancelled at a checkpoint) and
    /// answers every later call immediately, so a cancelled turn can be followed by one that
    /// must complete.
    struct FirstCallBlocksModel {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Model for FirstCallBlocksModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<crate::model::ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, Result<ChatResponse>> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Box::pin(futures::stream::unfold((), |_| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Some((Ok(text_response("tick")), ()))
                }));
            }
            Box::pin(futures::stream::once(async { Ok(text_response("done")) }))
        }

        fn model_name(&self) -> &str {
            "first-call-blocks"
        }
    }

    #[tokio::test]
    async fn a_call_admitted_after_a_cancelled_one_completes_and_owns_the_state() {
        // Java #3924: the same-call gate is released on cancellation, while the cancelled
        // turn's state write may still be in flight; the next call then runs against stale
        // state and is overwritten by the late write. Rust holds the gate and the write in
        // the same future, so a call admitted right after a cancellation must both run and
        // leave *its own* turn as the final state.
        let agent = Arc::new(
            ReActAgent::builder()
                .name("t-reentry")
                .model(Arc::new(FirstCallBlocksModel {
                    calls: AtomicUsize::new(0),
                }))
                .build()
                .unwrap(),
        );
        let signal = CancelSignal::new();
        let first_agent = Arc::clone(&agent);
        let first_signal = signal.clone();
        let first = tokio::spawn(async move {
            first_agent
                .call_with_cancel(vec![Msg::user("first")], first_signal)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(signal.cancel());
        let error = first
            .await
            .expect("the call task must not panic")
            .expect_err("a cancelled call must not produce a reply");
        assert!(matches!(error, Error::Cancelled(_)), "got: {error}");

        let reply = agent
            .call(vec![Msg::user("second")])
            .await
            .expect("a call right after a cancellation must run");
        assert_eq!(reply.text_content(), "done");

        let context = agent.agent_state().context().to_vec();
        let texts: Vec<String> = context.iter().map(Msg::text_content).collect();
        assert_eq!(texts.last().map(String::as_str), Some("done"));
        assert!(
            !texts.iter().any(|text| text.contains("tick")),
            "the cancelled turn must not appear as an assistant reply: {texts:?}"
        );
    }
}
