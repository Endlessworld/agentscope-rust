//! ReAct agent execution loop, mirroring `io.agentscope.core.agent.ReActAgent`.
//!
//! The ReAct agent runs an iterative reasoning → acting loop: it streams a model
//! call, accumulates the reasoning result, and if the model requested tool calls
//! it executes them and feeds the results back, repeating until the model stops
//! or the maximum number of iterations is reached.
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;

use crate::error::{Error, Result};
use crate::event::{AgentEvent, AgentEventType, ConfirmResult, EventData};
use crate::formatter::{JsonSchema, ResponseFormat};
use crate::interruption::InterruptControl;
use crate::agent::run::{AgentRun, RunControl, RunStatus};
use crate::message::{
    ChatUsage, ContentBlock, GenerateReason, JsonMap, Msg, MsgRole, TextBlock, ThinkingBlock,
    ToolResultBlock, ToolResultState, ToolUseBlock,
};
use crate::middleware::{
    AgentInput, EventStream, MiddlewareChain, ModelCallInput, Next, ReasoningInput, TaskReminderMiddleware,
};
use crate::model::{ExecutionConfig, GenerateOptions, Model, ModelUtils};use crate::permission::{
    PermissionBehavior, PermissionContextState, PermissionDecision, PermissionEngine,
    PermissionRule,
};
use crate::state::AgentState;
use crate::state::context::TaskContextState;
use crate::tool::{TodoWriteTool, ToolExecutionContext, Toolkit};
use super::agent_traits::{Agent, CallableAgent, ObservableAgent};
use super::lifecycle::{CallGate, ExecutionGuard};
use super::stream_options::StreamOptions;

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
pub type ConfirmHandler = Arc<dyn Fn(ToolUseBlock) -> Pin<Box<dyn Future<Output = ConfirmResult> + Send>> + Send + Sync>;


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
    /// Per-call generation defaults (reasoning effort, retry policy, ...)
    /// merged into every model request built by this agent.
    generate_options_defaults: Mutex<GenerateOptions>,
    toolkit: Toolkit,
    sys_prompt: String,
    max_iters: i32,
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
    /// Whether native structured-output responses are validated against the requested
    /// JSON Schema before being accepted (default `true`). Mirrors Java
    /// `validateStructuredOutput`.
    validate_structured_output: bool,
}

/// Accumulates streamed model chunks into a single assistant message.
#[derive(Debug, Default)]
struct ReasoningContext {
    id: String,
    text: String,
    thinking: String,
    tool_calls: Vec<ToolUseBlock>,
    tool_results: Vec<ToolResultBlock>,
    usage: Option<ChatUsage>,
}

impl ReasoningContext {
    /// Merges a streamed chunk into this accumulator.
    fn accumulate(&mut self, chunk: crate::model::ChatResponse) {
        if self.id.is_empty() {
            self.id = chunk.id.clone();
        }
        for block in chunk.content {
            match block {
                ContentBlock::Text(t) => self.text.push_str(&t.text),
                ContentBlock::Thinking(t) => self.thinking.push_str(&t.thinking),
                ContentBlock::ToolUse(t) => self.tool_calls.push(t),
                ContentBlock::ToolResult(t) => self.tool_results.push(t),
                _ => {}
            }
        }
        if chunk.usage.total_tokens() > 0 {
            self.usage = Some(chunk.usage);
        }
    }

    /// Builds the final assistant message from the accumulated content.
    fn build_final_message(self) -> Msg {
        let mut blocks = Vec::new();
        if !self.thinking.is_empty() {
            blocks.push(ContentBlock::Thinking(ThinkingBlock::new(self.thinking)));
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text(TextBlock::new(self.text)));
        }
        for tc in self.tool_calls {
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
        b.build()
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

    /// Internal: runs the full lifecycle (graceful-shutdown guard + per-session
    /// serialization) and returns the reply message.
    async fn call_with_format(
        &self,
        messages: Vec<Msg>,
        response_format: Option<ResponseFormat>,
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

        // Seed the shared task bag from the persisted state, then restore it after the run
        // so any `todo_write` mutations survive.
        let seed = self.core.state.lock().unwrap().tasks_context.clone();
        *self.core.tasks.lock().unwrap() = seed;
        let key = self.core.state.lock().unwrap().session_id().to_string();
        let gate = Arc::clone(&self.core.call_gate);
        let this = self.clone();
        let result = gate
            .run(&key, async move {
                let (stream, slot) = this.run_stream(messages, response_format);
                let events: Vec<Result<AgentEvent>> = stream.collect().await;
                for ev in events.into_iter().flatten() {
                    if let Some(sink) = this.core.subscriber.lock().unwrap().as_ref() {
                        sink(ev);
                    }
                }
                let result = slot.lock().unwrap().take().unwrap_or_else(|| {
                    Err(Error::Agent("agent produced no result message".into()))
                });
                // Persist task-list mutations made by the `todo_write` tool back into state.
                let tasks = this.core.tasks.lock().unwrap().clone();
                this.core.state.lock().unwrap().tasks_context = tasks;
                result
            })
            .await;

        // A cancellation must win over the run's own outcome so the terminal status
        // is not reported as `Completed` after `cancel()` was observed.
        let status = if run.is_cancelled() {
            RunStatus::Cancelled
        } else if result.is_ok() {
            RunStatus::Completed
        } else {
            RunStatus::Failed
        };
        run.finish(status);
        *self.core.active_run.lock().unwrap() = None;
        result
    }

    /// Builds the (middleware-wrapped) event stream for a single agent invocation,
    /// together with a slot the core loop writes its final result into.
    fn run_stream(
        &self,
        messages: Vec<Msg>,
        response_format: Option<ResponseFormat>,
    ) -> (EventStream, Arc<Mutex<Option<Result<Msg>>>>) {
        let input = AgentInput::new(messages);
        let mut ctx = self.new_context(&input);
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
        let agent_core: Box<dyn FnOnce(AgentInput) -> EventStream + Send> =
            Box::new(move |input: AgentInput| {
                let fut = async move {
                    let mut events = Vec::new();
                    events.push(Ok(AgentEvent::AgentStart(EventData::default())));
                    let res = run_agent_loop(&core, &ctx_core, input.messages).await;
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
                            events.push(Err(Error::Agent(e.to_string())));
                        }
                    }
                    *slot_core.lock().unwrap() = Some(res);
                    events.push(Ok(AgentEvent::AgentEnd(EventData::default())));
                    futures::stream::iter(events)
                };
                Box::pin(futures::stream::once(fut).flatten())
            });
        let stream = self.core.middleware_chain.wrap_agent(ctx, input, agent_core);
        // Merge the forwarded delta events into the agent-level stream.
        let stream = Box::pin(futures::stream::select(stream, delta_rx));
        (stream, slot)
    }

    /// Streams fine-grained agent events, filtered by the given [`StreamOptions`].
    pub fn stream_events(&self, messages: Vec<Msg>, options: StreamOptions) -> EventStream {
        let (stream, _slot) = self.run_stream(messages, None);
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
            match self.call_native_structured(messages.clone(), schema.clone()).await {
                Ok(reply) => Ok(reply),
                Err(e) => {
                    // A rejected native response (transport error, empty/invalid JSON or
                    // schema-invalid) falls back to the synthetic tool path; the rejected
                    // native reply never entered the context (Java `onErrorResume`).
                    tracing::warn!("native structured output rejected, falling back to tool path: {e}");
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
    async fn call_fallback_structured(&self, messages: Vec<Msg>, schema: JsonSchema) -> Result<Msg> {
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

        let mut state = self.core.state.lock().unwrap().clone();
        for m in messages {
            state.context_mut().push(m);
        }
        // The reminder is request-local: it steers this retry request only and never
        // enters `state.context`, so hooks, middleware and session persistence observe
        // only the caller's real messages (Java `appendTransientSoToolReminder`).
        let so_reminder = structured_output_reminder(&schema);

        let tools = toolkit.tool_schemas();
        let mut last_text = String::new();
        // The assistant message that produced the structured output. Its identity and
        // token accounting are carried over to the rebuilt reply below.
        let mut so_reply: Option<Msg> = None;
        // Number of retries spent steering the model toward `generate_response` without a
        // tool call. Capped at 3 (Java `soForceToolChoiceCount`) to avoid an endless loop.
        let mut force_retries = 0u32;
        for _ in 0..self.core.max_iters {
            let mut msgs = state.context().to_vec();
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
            let msg = model_call_impl(&self.core, input, |_| {}).await?;
            state.context_mut().push(msg.clone());

            let mut gen_called = false;
            for tc in msg.tool_use_blocks() {
                if tc.name == STRUCTURED_OUTPUT_TOOL_NAME {
                    // No delta channel here: the structured-output retry path is not
                    // streamed, so the emitter (still carrying the call id) discards.
                    let tctx = tool_execution_context(tc, Default::default(), &None);
                    if let Ok(rb) = toolkit.call_tool(tc, tctx).await {
                        state.context_mut().push(
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
        *self.core.state.lock().unwrap() = state;

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

    async fn call_with_schema(&self, messages: Vec<Msg>, schema: JsonSchema) -> Result<Msg> {
        ReActAgent::call_with_schema(self, messages, schema).await
    }
}

impl ObservableAgent for ReActAgent {
    fn subscribe(&self, sink: Box<dyn Fn(AgentEvent) + Send + Sync>) {
        *self.core.subscriber.lock().unwrap() = Some(Arc::new(sink));
    }
}

/// Builder for [`ReActAgent`].
pub struct ReActAgentBuilder {
    id: String,
    name: String,
    description: String,
    model: Option<Arc<dyn Model>>,
    toolkit: Option<Toolkit>,
    sys_prompt: String,
    max_iters: i32,
    middleware_chain: Option<MiddlewareChain>,
    state: Option<AgentState>,
    enable_pending_tool_recovery: bool,
    permission_context: Option<PermissionContextState>,
    enable_todo_tool: bool,
    confirm_handler: Option<ConfirmHandler>,
    validate_structured_output: bool,
}

impl Default for ReActAgentBuilder {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            model: None,
            toolkit: None,
            sys_prompt: String::new(),
            max_iters: 0,
            middleware_chain: None,
            state: None,
            enable_pending_tool_recovery: false,
            permission_context: None,
            enable_todo_tool: true,
            confirm_handler: None,
            validate_structured_output: true,
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
    /// Sets the toolkit used for tool execution.
    pub fn toolkit(mut self, toolkit: Toolkit) -> Self {
        self.toolkit = Some(toolkit);
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

    /// Builds the [`ReActAgent`].
    pub fn build(self) -> Result<ReActAgent> {
        let model = self.model.ok_or_else(|| {
            Error::Agent("ReActAgent requires a model".into())
        })?;
        let id = if self.id.is_empty() {
            uuid::Uuid::new_v4().to_string().replace('-', "")
        } else {
            self.id
        };
        let name = if self.name.is_empty() { id.clone() } else { self.name };
        let max_iters = if self.max_iters <= 0 { DEFAULT_MAX_ITERS } else { self.max_iters };
        let initial_state = self.state.unwrap_or_default();
        let tasks: Arc<Mutex<TaskContextState>> =
            Arc::new(Mutex::new(initial_state.tasks_context.clone()));

        let toolkit = self.toolkit.unwrap_or_default();
        let mut chain = self.middleware_chain.unwrap_or_default();
        if self.enable_todo_tool {
            toolkit.register_tool(Arc::new(TodoWriteTool::new(Arc::clone(&tasks))));
            chain.add(Box::new(TaskReminderMiddleware::new(Arc::clone(&tasks))));
        }

        let core = Arc::new(AgentCore {
            model: Mutex::new(model),
            generate_options_defaults: Mutex::new(GenerateOptions::default()),
            toolkit,
            sys_prompt: self.sys_prompt,
            max_iters,
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
        });
        Ok(ReActAgent { id, name, description: self.description, core })
    }
}

// ---------------------------------------------------------------------------
// Execution loop
// ---------------------------------------------------------------------------

/// Entry point of the reasoning→acting loop. Copies the shared state out, appends
/// the input messages, runs the iteration loop and writes the state back.
async fn run_agent_loop(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    messages: Vec<Msg>,
) -> Result<Msg> {
    let mut state = core.state.lock().unwrap().clone();

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

    for m in messages {
        state.context_mut().push(m);
    }
    state.set_cur_iter(0);
    // Snapshot plan mode so a plan-control tool's direct write to the shared state can
    // be detected and carried over (the loop otherwise runs on a private clone).
    let plan_mode_at_start = state.plan_mode_context.clone();
    let result = match execute_iteration(core, ctx, &mut state, 0).await {
        // An interrupt is not a failure: reconcile the dangling tool_use left by
        // the interrupted turn and append a recovery message, so the caller sees
        // a normal reply and the next turn continues from a consistent context.
        // Mirrors Java `handleInterrupt`.
        Err(Error::Interrupted(_)) => Ok(reconcile_interrupted(&mut state)),
        other => other,
    };
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
) -> Result<Msg> {
    loop {
        let msg = reasoning_phase(core, ctx, state, iter).await?;
        let reason = msg.generate_reason();
        let has_tool_calls = has_actionable_tool_calls(&msg);
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
            GenerateReason::ReasoningStopRequested | GenerateReason::MaxIterations => {
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
        start.insert("reply_id".to_string(), serde_json::Value::String(reply_id.clone()));
        start.insert("block_id".to_string(), serde_json::Value::String(block_id.clone()));
        start.insert(
            "block_type".to_string(),
            serde_json::Value::String("text".to_string()),
        );
        emit_delta(
            &ctx.delta_tx,
            AgentEvent::with_fields(AgentEventType::TextBlockStart, start),
        );

        let mut delta = JsonMap::new();
        delta.insert("reply_id".to_string(), serde_json::Value::String(reply_id.clone()));
        delta.insert("block_id".to_string(), serde_json::Value::String(block_id.clone()));
        delta.insert(
            "block_type".to_string(),
            serde_json::Value::String("text".to_string()),
        );
        delta.insert("delta".to_string(), serde_json::Value::String(t.text.clone()));
        emit_delta(
            &ctx.delta_tx,
            AgentEvent::with_fields(AgentEventType::TextBlockDelta, delta).with_metadata(
                crate::message::metadata_keys::EVENT_GENERATE_REASON,
                serde_json::Value::String("TOOL_RETURN_DIRECT".to_string()),
            ),
        );

        let mut end = JsonMap::new();
        end.insert("reply_id".to_string(), serde_json::Value::String(reply_id.clone()));
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
) -> Result<Msg> {
    check_interrupted(core)?;
    state.set_cur_iter(iter);
    // Publish the state accumulated so far (this turn's input plus the previous
    // iteration's assistant/tool messages) into the shared state before the
    // reasoning chain runs. Without this, `core.state` would stay frozen at the
    // previous turn boundary for the whole turn, so a per-iteration checkpoint
    // middleware would have nothing to persist and an abrupt process death
    // (kill -9 / OOM / power loss) would lose the entire turn.
    *core.state.lock().unwrap() = state.clone();
    if iter >= core.max_iters {
        return summarizing(core, state).await;
    }

    let sys_prompt = core
        .middleware_chain
        .apply_system_prompt(ctx, core.sys_prompt.clone());
    let mut messages = Vec::new();
    if !sys_prompt.trim().is_empty() {
        messages.push(Msg::system(sys_prompt));
    }
    messages.extend(state.context().to_vec());

    let tools = core.toolkit.tool_schemas();
    let model = core.model.lock().unwrap().clone();
    let mut ob = GenerateOptions::builder().model_name(model.model_name());
    if let Some(rf) = &ctx.response_format {
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

    let final_msg = match run_reasoning(core, ctx, input).await {
        Ok(msg) => msg,
        Err(e) => {
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
    state.context_mut().push(final_msg.clone());
    Ok(final_msg)
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
        Box::new(move |mc_input: ModelCallInput| -> EventStream {
            let slot = Arc::clone(&slot);
            let core = Arc::clone(&core);
            let delta_tx = delta_tx.clone();
            let fut = async move {
                let msg = model_call_impl(&core, mc_input, |ev| {
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
            let mc_input = ModelCallInput::new(r.messages, r.tools, r.options, model);
            chain.wrap_model_call(ctx.clone(), mc_input, model_core)
        })
    };

    let stream = chain.wrap_reasoning(ctx.clone(), input, reasoning_core);
    let _events: Vec<_> = stream.collect().await;
    let result = slot.lock().unwrap().take().unwrap_or_else(|| {
        Err(Error::Agent("model produced no message".into()))
    });
    result
}
/// Helper: emit an agent event through the delta_tx channel if available.
fn emit_delta(delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>, ev: AgentEvent) {
    if let Some(tx) = delta_tx {
        let _ = tx.unbounded_send(Ok(ev));
    }
}

/// Builds an agent event carrying tool-call metadata fields.
fn tool_event(event_type: AgentEventType, tool_call_id: &str, tool_name: &str) -> AgentEvent {
    let mut fields = JsonMap::new();
    fields.insert("tool_call_id".to_string(), serde_json::Value::String(tool_call_id.to_string()));
    fields.insert("tool_name".to_string(), serde_json::Value::String(tool_name.to_string()));
    AgentEvent::with_fields(event_type, fields)
}


/// Emits a ToolResultStart event through the delta_tx channel.
fn emit_tool_result_start(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    tool_call_id: &str,
    tool_name: &str,
) {
    let mut fields = JsonMap::new();
    fields.insert("tool_call_id".to_string(), serde_json::Value::String(tool_call_id.to_string()));
    fields.insert("tool_name".to_string(), serde_json::Value::String(tool_name.to_string()));
    emit_delta(delta_tx, AgentEvent::with_fields(AgentEventType::ToolResultStart, fields));
}

/// Emits a ToolResultTextDelta event through the delta_tx channel.
fn emit_tool_result_text_delta(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    tool_call_id: &str,
    tool_name: &str,
    delta: &str,
) {
    let mut fields = JsonMap::new();
    fields.insert("tool_call_id".to_string(), serde_json::Value::String(tool_call_id.to_string()));
    fields.insert("tool_name".to_string(), serde_json::Value::String(tool_name.to_string()));
    fields.insert("delta".to_string(), serde_json::Value::String(delta.to_string()));
    emit_delta(delta_tx, AgentEvent::with_fields(AgentEventType::ToolResultTextDelta, fields));
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
    fields.insert("tool_call_id".to_string(), serde_json::Value::String(tool_call_id.to_string()));
    fields.insert("tool_name".to_string(), serde_json::Value::String(tool_name.to_string()));
    let state_str = match state {
        ToolResultState::Success => "SUCCESS",
        ToolResultState::Error => "ERROR",
        ToolResultState::Denied => "DENIED",
        ToolResultState::Running => "RUNNING",
        ToolResultState::Interrupted => "ERROR",
        ToolResultState::Suspended => "SUSPENDED",
    };
    fields.insert("state".to_string(), serde_json::Value::String(state_str.to_string()));
    if !metadata.is_empty() {
        fields.insert(
            "metadata".to_string(),
            serde_json::to_value(metadata).unwrap_or(serde_json::Value::Null),
        );
    }
    emit_delta(delta_tx, AgentEvent::with_fields(AgentEventType::ToolResultEnd, fields));
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
    fields.insert("reply_id".to_string(), serde_json::Value::String(reply_id.to_string()));
    fields.insert("tool_call_id".to_string(), serde_json::Value::String(call.id.clone()));
    fields.insert("tool_name".to_string(), serde_json::Value::String(call.name.clone()));
    if let Some(reason) = reason.filter(|r| !r.is_empty()) {
        fields.insert("reason".to_string(), serde_json::Value::String(reason.to_string()));
    }
    fields.insert(
        "tool_calls".to_string(),
        serde_json::to_value(std::slice::from_ref(call)).unwrap_or(serde_json::Value::Array(Vec::new())),
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
    fields.insert("delta".to_string(), serde_json::Value::String(delta.to_string()));
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
            fields.insert("reply_id".to_string(), serde_json::Value::String(self.reply_id.clone()));
            fields.insert("block_type".to_string(), Self::block_field("text"));
            emit(Ok(AgentEvent::with_fields(AgentEventType::TextBlockStart, fields)));
        }
    }

    fn start_thinking(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        if !self.thinking_started {
            self.thinking_started = true;
            let mut fields = JsonMap::new();
            fields.insert("reply_id".to_string(), serde_json::Value::String(self.reply_id.clone()));
            fields.insert("block_type".to_string(), Self::block_field("thinking"));
            emit(Ok(AgentEvent::with_fields(AgentEventType::ThinkingBlockStart, fields)));
        }
    }

    fn start_tool_call(&mut self, tool_id: &str, tool_name: &str, emit: &mut impl FnMut(Result<AgentEvent>)) {
        if tool_id.is_empty() || self.started_tool_calls.contains_key(tool_id) {
            return;
        }
        self.flush_text(emit);
        self.flush_thinking(emit);
        self.flush_all_tool_calls(emit);
        let visible = !tool_name.starts_with("__");
        if visible && !self.started_tool_calls.contains_key(tool_id) {
            self.started_tool_calls.insert(tool_id.to_string(), tool_name.to_string());
            let mut fields = JsonMap::new();
            fields.insert("reply_id".to_string(), serde_json::Value::String(self.reply_id.clone()));
            fields.insert("tool_call_id".to_string(), serde_json::Value::String(tool_id.to_string()));
            fields.insert("tool_name".to_string(), serde_json::Value::String(tool_name.to_string()));
            emit(Ok(AgentEvent::with_fields(AgentEventType::ToolCallStart, fields)));
        }
    }

    fn flush_text(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        if self.text_started {
            self.text_started = false;
            let mut fields = JsonMap::new();
            fields.insert("reply_id".to_string(), serde_json::Value::String(self.reply_id.clone()));
            fields.insert("block_type".to_string(), Self::block_field("text"));
            emit(Ok(AgentEvent::with_fields(AgentEventType::TextBlockEnd, fields)));
        }
    }

    fn flush_thinking(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        if self.thinking_started {
            self.thinking_started = false;
            let mut fields = JsonMap::new();
            fields.insert("reply_id".to_string(), serde_json::Value::String(self.reply_id.clone()));
            fields.insert("block_type".to_string(), Self::block_field("thinking"));
            emit(Ok(AgentEvent::with_fields(AgentEventType::ThinkingBlockEnd, fields)));
        }
    }

    fn flush_all_tool_calls(&mut self, emit: &mut impl FnMut(Result<AgentEvent>)) {
        for (tool_id, tool_name) in self.started_tool_calls.drain() {
            let mut fields = JsonMap::new();
            fields.insert("reply_id".to_string(), serde_json::Value::String(self.reply_id.clone()));
            fields.insert("tool_call_id".to_string(), serde_json::Value::String(tool_id));
            fields.insert("tool_name".to_string(), serde_json::Value::String(tool_name));
            emit(Ok(AgentEvent::with_fields(AgentEventType::ToolCallEnd, fields)));
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
async fn model_call_impl(
    core: &Arc<AgentCore>,
    mc_input: ModelCallInput,
    mut emit: impl FnMut(Result<AgentEvent>),
) -> Result<Msg> {
    let model = Arc::clone(&mc_input.model);
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

    let mut attempt: u32 = 0;
    loop {
        let mut acc = ReasoningContext::default();
        let mut block_lifecycle = BlockLifecycle::new(reply_id.clone());

        // Emit ModelCallStart at the beginning of each attempt.
        {
            let mut fields = JsonMap::new();
            fields.insert("reply_id".to_string(), serde_json::Value::String(reply_id.clone()));
            emit(Ok(AgentEvent::with_fields(AgentEventType::ModelCallStart, fields)));
        }

        let mut stream = model.stream(messages.clone(), tools.clone(), options.clone());
        let consume = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                for block in &chunk.content {
                    match block {
                        ContentBlock::Text(t) => {
                            block_lifecycle.start_text(&mut emit);
                            if !t.text.is_empty() {
                                emit(Ok(delta_event(AgentEventType::TextBlockDelta, &t.text)));
                            }
                        }
                        ContentBlock::Thinking(t) => {
                            block_lifecycle.start_thinking(&mut emit);
                            if !t.thinking.is_empty() {
                                emit(Ok(delta_event(AgentEventType::ThinkingBlockDelta, &t.thinking)));
                            }
                        }
                        ContentBlock::ToolUse(tu) => {
                            block_lifecycle.start_tool_call(&tu.id, &tu.name, &mut emit);
                            let delta = match tu.content.as_deref().filter(|c| !c.is_empty()) {
                                Some(content) => Some(content.to_string()),
                                None => serde_json::to_string(&tu.input)
                                    .ok()
                                    .filter(|s| s != "{}"),
                            };
                            if let Some(delta) = delta {
                                let mut fields = JsonMap::new();
                                fields.insert("reply_id".to_string(), serde_json::Value::String(reply_id.clone()));
                                fields.insert("tool_call_id".to_string(), serde_json::Value::String(tu.id.clone()));
                                fields.insert("tool_name".to_string(), serde_json::Value::String(tu.name.clone()));
                                fields.insert("delta".to_string(), serde_json::Value::String(delta));
                                emit(Ok(AgentEvent::with_fields(AgentEventType::ToolCallDelta, fields)));
                            }
                        }
                        _ => {}
                    }
                }
                acc.accumulate(chunk);
                // Checked after accumulating so an interrupt still preserves the
                // chunks already received (Java parity: its InterruptedException
                // branch persists `buildFinalMessage()`).
                check_interrupted(core)?;
            }
            Ok::<_, Error>(())
        };

        match consume.await {
            Ok(()) => {
                // Flush all remaining blocks and emit ModelCallEnd.
                block_lifecycle.flush_all(&mut emit);
                {
                    let mut fields = JsonMap::new();
                    fields.insert("reply_id".to_string(), serde_json::Value::String(reply_id.clone()));
                    let usage = acc.usage.clone().unwrap_or_default();
                    fields.insert("usage".to_string(), serde_json::to_value(&usage).unwrap_or_default());
                    emit(Ok(AgentEvent::with_fields(AgentEventType::ModelCallEnd, fields)));
                }
                return Ok(acc.build_final_message());
            }
            Err(e) => {
                attempt += 1;
                if attempt >= max_attempts || !e.is_retryable() {
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

/// Builds the chunk callback handed to a tool through its emitter.
///
/// Each emitted chunk is forwarded as a `ToolResultDataDelta` event carrying the
/// originating tool call's id/name, so consumers can correlate streamed progress
/// with the exact call. Returns `None` when the call has no event channel (e.g.
/// structured-output retries), in which case the emitter silently discards chunks.
fn tool_chunk_callback(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    call: &ToolUseBlock,
) -> Option<crate::tool::ChunkCallback> {
    let tx = delta_tx.clone()?;
    let id = call.id.clone();
    let name = call.name.clone();
    Some(Arc::new(move |chunk: ToolResultBlock| {
        for block in chunk.output {
            let mut fields = JsonMap::new();
            fields.insert("tool_call_id".to_string(), serde_json::Value::String(id.clone()));
            fields.insert("tool_name".to_string(), serde_json::Value::String(name.clone()));
            fields.insert(
                "data".to_string(),
                serde_json::to_value(&block).unwrap_or(serde_json::Value::Null),
            );
            let _ = tx.unbounded_send(Ok(AgentEvent::with_fields(
                AgentEventType::ToolResultDataDelta,
                fields,
            )));
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
) -> ToolExecutionContext {
    ToolExecutionContext {
        tool_call: Some(call.clone()),
        runtime,
        emitter: Some(crate::tool::tool_emitter_for_call(
            call,
            tool_chunk_callback(delta_tx, call),
        )),
    }
}

/// Builds the JSON runtime exposed to a tool call, carrying the call-scoped
/// identity (session / user / agent) plus any metadata the agent recorded.
fn tool_runtime(ctx: &crate::agent::RuntimeContext) -> JsonMap {
    let mut rt = ctx.metadata.clone();
    if let Some(sid) = ctx.session_id() {
        rt.insert("sessionId".to_string(), serde_json::Value::String(sid.to_string()));
    }
    if let Some(uid) = ctx.user_id() {
        rt.insert("userId".to_string(), serde_json::Value::String(uid.to_string()));
    }
    if let Some(aid) = ctx.agent_id() {
        rt.insert("agentId".to_string(), serde_json::Value::String(aid.to_string()));
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
    check_interrupted(core)?;
    let pending = extract_pending_tool_calls(state);
    if pending.is_empty() {
        return Ok(ActingOutcome::default());
    }

    let mut results = Vec::new();
    let mut suspended = false;
    let mut return_direct_eligible = true;
    let mut executed = 0usize;
    let reply_id = current_reply_id(state);
    let runtime = tool_runtime(ctx);
    for call in &pending {
        // Plan-mode enforcement (mirrors Java `PlanModeMiddleware.onActing`): while plan
        // mode is active, only read-only tools and the always-allowed orchestration/plan
        // tools may run; every other call gets a synthetic DENIED result without executing.
        if plan_mode_denies(core, state, call) {
            emit_tool_result_start(&ctx.delta_tx, &call.id, &call.name);
            emit_tool_result_text_delta(
                &ctx.delta_tx,
                &call.id,
                &call.name,
                PLAN_MODE_DENY_MESSAGE,
            );
            return_direct_eligible = false;
            results.push(denied_result(call, PLAN_MODE_DENY_MESSAGE));
            emit_tool_result_end(
                &ctx.delta_tx,
                &call.id,
                &call.name,
                ToolResultState::Denied,
                &JsonMap::new(),
            );
            continue;
        }
        let decision = evaluate_permission(core, call);
        match decision.behavior {
            PermissionBehavior::Deny => {
                // Emit ToolResultStart → TextDelta → End for denied calls.
                emit_tool_result_start(&ctx.delta_tx, &call.id, &call.name);
                let denied_text = "Permission denied by rules";
                emit_tool_result_text_delta(&ctx.delta_tx, &call.id, &call.name, denied_text);
                let denied = denied_result(call, &decision.message);
                return_direct_eligible = false;
                results.push(denied);
                emit_tool_result_end(&ctx.delta_tx, &call.id, &call.name, ToolResultState::Denied, &JsonMap::new());
            }
            PermissionBehavior::Allow | PermissionBehavior::Passthrough => {
                // Emit ToolResultStart for this tool execution.
                emit_tool_result_start(&ctx.delta_tx, &call.id, &call.name);
                let tctx = tool_execution_context(call, runtime.clone(), &ctx.delta_tx);
                match core.toolkit.call_tool(call, tctx).await {
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
                            emit_tool_result_text_delta(&ctx.delta_tx, &call.id, &call.name, &result_text);
                        }
                        // Java #3435: a suspended call must not fabricate a result — no
                        // ToolResultEnd event is emitted (the external-execution requirement
                        // is the single signal). The block still carries `Suspended` state.
                        let was_suspended = rb.is_suspended();
                        let suspend_reason = rb.suspend_reason().map(str::to_string);
                        let final_state = rb.final_state();
                        let metadata = rb.metadata.clone();
                        let bound = bind_result(call, rb);
                        suspended = suspended || bound.is_suspended();
                        executed += 1;
                        if !is_return_direct_result(core, &bound) {
                            return_direct_eligible = false;
                        }
                        results.push(bound);
                        if was_suspended {
                            emit_require_external_execution(
                                &ctx.delta_tx,
                                &reply_id,
                                call,
                                suspend_reason.as_deref(),
                            );
                        } else {
                            emit_tool_result_end(&ctx.delta_tx, &call.id, &call.name, final_state, &metadata);
                        }
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        emit_tool_result_text_delta(&ctx.delta_tx, &call.id, &call.name, &err_msg);
                        return_direct_eligible = false;
                        results.push(error_result(call, &err_msg));
                        emit_tool_result_end(&ctx.delta_tx, &call.id, &call.name, ToolResultState::Error, &JsonMap::new());
                    }
                }
            }
            PermissionBehavior::Ask => {
                let handler = core.confirm_handler.lock().unwrap().clone();
                if let Some(handler) = handler {
                    // Emit ToolResultStart so ACP clients know this tool is being
                    // processed (even though it's waiting for user confirmation).
                    emit_tool_result_start(&ctx.delta_tx, &call.id, &call.name);
                    let confirm_result = handler(call.clone()).await;
                    if confirm_result.is_confirmed() {
                        // Use the (possibly modified) tool call if the user tweaked input.
                        let effective_call = confirm_result.tool_call.as_ref().unwrap_or(call);
                        emit_tool_result_start(&ctx.delta_tx, &effective_call.id, &effective_call.name);
                        let tctx = tool_execution_context(
                            effective_call,
                            runtime.clone(),
                            &ctx.delta_tx,
                        );
                        match core.toolkit.call_tool(effective_call, tctx).await {
                            Ok(rb) => {
                                let mut result_text = String::new();
                                for block in &rb.output {
                                    if let ContentBlock::Text(t) = block {
                                        result_text.push_str(&t.text);
                                    }
                                }
                                if !result_text.trim().is_empty() {
                                    emit_tool_result_text_delta(&ctx.delta_tx, &effective_call.id, &effective_call.name, &result_text);
                                }
                                let was_suspended = rb.is_suspended();
                                let suspend_reason = rb.suspend_reason().map(str::to_string);
                                let final_state = rb.final_state();
                                let metadata = rb.metadata.clone();
                                let bound = bind_result(effective_call, rb);
                                suspended = suspended || bound.is_suspended();
                                executed += 1;
                                if !is_return_direct_result(core, &bound) {
                                    return_direct_eligible = false;
                                }
                                results.push(bound);
                                if was_suspended {
                                    emit_require_external_execution(
                                        &ctx.delta_tx,
                                        &reply_id,
                                        effective_call,
                                        suspend_reason.as_deref(),
                                    );
                                } else {
                                    emit_tool_result_end(&ctx.delta_tx, &effective_call.id, &effective_call.name, final_state, &metadata);
                                }
                            }
                            Err(e) => {
                                let err_msg = e.to_string();
                                emit_tool_result_text_delta(&ctx.delta_tx, &effective_call.id, &effective_call.name, &err_msg);
                                return_direct_eligible = false;
                                results.push(error_result(effective_call, &err_msg));
                                emit_tool_result_end(&ctx.delta_tx, &effective_call.id, &effective_call.name, ToolResultState::Error, &JsonMap::new());
                            }
                        }
                    } else {
                        // User denied the tool call.
                        let denied_text = &confirm_result.message;
                        emit_tool_result_text_delta(&ctx.delta_tx, &call.id, &call.name, denied_text);
                        return_direct_eligible = false;
                        results.push(denied_result(call, denied_text));
                        emit_tool_result_end(&ctx.delta_tx, &call.id, &call.name, ToolResultState::Denied, &JsonMap::new());
                    }
                } else {
                    return Err(Error::Agent(format!(
                        "permission asking (HITL) is not yet supported; tool '{}' requires user confirmation",
                        call.name
                    )));
                }
            }
        }
    }

    // A returnDirect batch short-circuits only when at least one tool ran and every
    // executed result was a returnDirect success; a suspended/denied/errored result
    // disqualifies the whole batch (Java: `pendingPairs.isEmpty() && !successPairs.isEmpty()
    // && allMatch(isReturnDirectToolCall)`).
    let return_direct = return_direct_eligible && !suspended && executed > 0;

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
    Ok(ActingOutcome {
        suspended,
        return_direct,
        results,
    })
}

/// Produces the termination message when the maximum number of iterations is hit.
///
/// Mirrors the Java `summarizing()`: handles pending tool calls that were not completed
/// before max iterations, and emits `ExceedMaxItersEvent`.
async fn summarizing(core: &Arc<AgentCore>, state: &mut AgentState) -> Result<Msg> {
    check_interrupted(core)?;

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

/// Builds the `generate_response` tool's parameter schema wrapping the target JSON
/// schema under a single `response` property.
fn generate_response_parameters(schema: &JsonSchema) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert("response".to_string(), serde_json::Value::Object(schema.schema.clone()));
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

/// Aborts execution if the agent's active run (or, absent one, the agent itself)
/// has been interrupted.
fn check_interrupted(core: &Arc<AgentCore>) -> Result<()> {
    let interrupted = match core.active_run.lock().unwrap().as_ref() {
        Some(run) => run.interruption().is_interrupted(),
        None => core.interrupt.is_interrupted(),
    };
    if interrupted {
        return Err(Error::Interrupted("agent execution interrupted".into()));
    }
    Ok(())
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
    if messages.iter().any(|m| m.tool_result_blocks().next().is_some()) {
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
    use futures::stream::BoxStream;

    use super::*;
    use crate::message::{TextBlock, ToolUseBlock};
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
            self.seen_choices.lock().unwrap().push(options.tool_choice.clone());
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
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(id, name, input))])
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
            vec![
                tool_use_response("c1", "add", input),
                text_response("done"),
            ],
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
    async fn failing_tool_result_state_is_error_in_persisted_context() {        let toolkit = Toolkit::new();
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

    #[tokio::test]
    async fn tool_emitted_chunks_reach_the_event_stream() {
        // The emitter's chunks must surface as ToolResultDataDelta events carrying
        // the call id, otherwise the emitter is effectively dead wiring.
        let toolkit = Toolkit::new();
        toolkit.register_tool(Arc::new(crate::tool::FunctionTool::new(
            "streamer",
            "emits a chunk then returns",
            serde_json::json!({"type": "object"}),
            Box::new(move |_args: JsonMap, ctx: ToolExecutionContext| {
                if let Some(e) = &ctx.emitter {
                    crate::tool::emit_to(
                        &Some(Arc::clone(e)),
                        ToolResultBlock::text("partial progress"),
                    );
                }
                Ok(ToolResultBlock::success("final"))
            }),
        )));
        let model = Arc::new(MockModel::new(
            "mock",
            vec![
                tool_use_response("call-stream-1", "streamer", JsonMap::new()),
                text_response("done"),
            ],
        ));
        let agent = ReActAgent::builder()
            .name("t3-stream")
            .model(model)
            .toolkit(toolkit)
            .build()
            .unwrap();
        let events: Vec<AgentEvent> = agent
            .stream_events(
                vec![Msg::user("go")],
                StreamOptions::builder().include_acting_chunk(true).build(),
            )
            .filter_map(|r| async move { r.ok() })
            .collect()
            .await;

        let delta = events
            .iter()
            .find(|e| e.event_type() == AgentEventType::ToolResultDataDelta)
            .expect("emitted chunk surfaced as a ToolResultDataDelta event");
        assert_eq!(
            delta.data().fields.get("tool_call_id"),
            Some(&serde_json::json!("call-stream-1"))
        );
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
        assert_eq!(require.len(), 1, "expected exactly one require event; got {events:?}");
        let fields = &require[0].data().fields;
        assert_eq!(fields.get("tool_call_id"), Some(&serde_json::json!("ext1")));
        assert_eq!(fields.get("tool_name"), Some(&serde_json::json!("external")));
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
            vec![
                tool_use_response("c1", "add", input),
                text_response("done"),
            ],
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
            Self { seen: Arc::new(Mutex::new(Vec::new())) }
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
                if session.is_empty() { None } else { Some(session) },
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
            self.seen.lock().unwrap().push(ctx.run_id().map(str::to_string));
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
        assert_eq!(recorded.len(), 1, "on_agent_state_ready must fire once per call");
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
        chain.add(Box::new(RunIdRecordingMiddleware { seen: Arc::clone(&seen) }));
        let model = Arc::new(MockModel::new("mock", vec![text_response("ok")]));
        let agent = ReActAgent::builder()
            .name("t-run-id")
            .model(model)
            .middleware_chain(chain)
            .build()
            .unwrap();
        agent.call(vec![Msg::user("hi")]).await.unwrap();

        let recorded = seen.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "the run id must be visible to the pipeline");
        let run_id = recorded[0].clone().expect("a run id must be assigned");
        assert_eq!(run_id.len(), 32);
        assert!(run_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(agent.active_run().is_none(), "the run must be cleared after the call");
    }

    #[tokio::test]
    async fn reasoning_middleware_rewrites_reach_the_model() {        let model = Arc::new(RecordingModel::new());
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
        assert_eq!(answers, 1, "an answered tool call must not be reported twice");
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
            vec![
                tool_use_response("c1", "add", input),
                text_response("done"),
            ],
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
        assert_eq!(*counter.lock().unwrap(), 0, "mutating tool must be denied in plan mode");
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
        let agent = ReActAgent::builder().name("t5").model(model).build().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        agent.subscribe(Box::new(move |ev| {
            s.lock().unwrap().push(ev.event_type().wire_name().to_string());
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
        let agent = ReActAgent::builder().name("t-stream").model(model).build().unwrap();

        // Defaults stream everything, including AGENT_START.
        let events: Vec<String> = agent
            .stream_events(vec![Msg::user("hi")], crate::agent::stream_options::StreamOptions::defaults())
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
        let agent = ReActAgent::builder().name("t-delta").model(model).build().unwrap();

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
                ev.data().fields.get("delta").and_then(|v| v.as_str()).map(|s| s.to_string())
            })
            .collect();
        assert!(!deltas.is_empty(), "expected at least one TEXT_BLOCK_DELTA; got {events:?}");
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
                ev.data().fields.get("delta").and_then(|v| v.as_str()).map(|s| s.to_string())
            })
            .collect();
        assert_eq!(args.len(), 1, "expected exactly one ToolCallDelta; got {args:?}");
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
        agent.core.state.lock().unwrap().context_mut().push(Msg::user("hello"));
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
        assert_eq!(reply.structured_data().unwrap()["age"], serde_json::json!("seven"));
        assert_eq!(model.seen_choices.lock().unwrap().len(), 1, "no fallback round trip");
    }

    #[tokio::test]
    async fn fallback_forces_specific_tool_choice_on_retry() {
        // The model returns plain text twice (never calling `generate_response`), then
        // finally calls it. The retries must carry a forced `ToolChoice::Specific`.
        let mut resp_input = JsonMap::new();
        resp_input.insert("response".to_string(), serde_json::json!({"name": "z", "age": 1}));
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
            Some(crate::model::ToolChoice::Specific { tool_name: STRUCTURED_OUTPUT_TOOL_NAME.into() })
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
            vec![tool_use_response("t1", "todo_write", input), text_response("done")],
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
        assert_eq!(*counter.lock().unwrap(), 0, "server tool must not hit the local toolkit");
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
        server_call
            .metadata
            .insert(ToolUseBlock::METADATA_SERVER_TOOL.to_string(), serde_json::json!(true));
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
                let r = if g.is_empty() { text_response("done") } else { g.remove(0) };
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
        assert!(answered, "the dangling c1 tool_use must be answered; got {ctx:?}");
        assert_eq!(*counter.lock().unwrap(), 0, "the interrupted tool must not run");
    }

    #[tokio::test]
    async fn interrupt_flag_is_reset_on_the_next_call() {
        // A user interrupt must not wedge the agent for good: Java gives every call
        // a fresh InterruptControl, so the next call has to run normally rather
        // than immediately failing on the stale signal.
        let model = Arc::new(MockModel::new("mock", vec![text_response("hello")]));
        let agent = ReActAgent::builder().name("t-reset").model(model).build().unwrap();

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
        assert!(answered, "orphaned c9 tool_use must be patched; got {ctx:?}");
        assert_eq!(*counter.lock().unwrap(), 0, "the orphaned tool must not execute");
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
}
