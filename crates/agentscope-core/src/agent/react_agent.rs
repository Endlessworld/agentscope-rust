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
}

/// Accumulates streamed model chunks into a single assistant message.
#[derive(Debug, Default)]
struct ReasoningContext {
    id: String,
    text: String,
    thinking: String,
    tool_calls: Vec<ToolUseBlock>,
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

    /// Returns the model bound to this agent.
    pub fn model(&self) -> Arc<dyn Model> {
        self.core.model.lock().unwrap().clone()
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
        self.core.interrupt.trigger(None, None);
    }

    /// Interrupts execution only when the current session matches `session_id`,
    /// mirroring the per-session `HarnessAgent#interrupt(userId, sessionId)` overload.
    pub fn interrupt_session(&self, session_id: &str) -> bool {
        let matches = self.core.state.lock().unwrap().session_id() == session_id;
        if matches {
            self.core.interrupt.trigger(None, None);
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
        // Seed the shared task bag from the persisted state, then restore it after the run
        // so any `todo_write` mutations survive.
        let seed = self.core.state.lock().unwrap().tasks_context.clone();
        *self.core.tasks.lock().unwrap() = seed;
        let key = self.core.state.lock().unwrap().session_id().to_string();
        let gate = Arc::clone(&self.core.call_gate);
        let this = self.clone();
        gate.run(&key, async move {
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
        .await
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
            self.call_native_structured(messages, schema).await
        } else {
            self.call_fallback_structured(messages, schema).await
        }
    }

    /// Native structured-output path: passes the schema via `response_format` and parses
    /// the model's JSON text reply into structured-output metadata.
    async fn call_native_structured(&self, messages: Vec<Msg>, schema: JsonSchema) -> Result<Msg> {
        let reply = self
            .call_with_format(messages, Some(ResponseFormat::json_schema(schema)))
            .await?;
        let text = reply.text_content();
        let parsed = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
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
        state.context_mut().push(structured_output_reminder(&schema));

        let tools = toolkit.tool_schemas();
        let mut last_text = String::new();
        for _ in 0..self.core.max_iters {
            let msgs = state.context().to_vec();
            let model = self.core.model.lock().unwrap().clone();
            let options = GenerateOptions::builder()
                .model_name(model.model_name())
                .build();
            let input = ModelCallInput::new(msgs, tools.clone(), options, Arc::clone(&model));
            let msg = model_call_impl(&self.core, input, |_| {}).await?;
            state.context_mut().push(msg.clone());

            let mut gen_called = false;
            for tc in msg.tool_use_blocks() {
                if tc.name == STRUCTURED_OUTPUT_TOOL_NAME {
                    let tctx = crate::tool::ToolExecutionContext {
                        tool_call: Some(tc.clone()),
                        runtime: Default::default(),
                        emitter: None,
                    };
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
                break;
            }
            if msg.tool_use_blocks().next().is_none() {
                last_text = msg.text_content();
                break;
            }
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
        let mut reply = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Text(TextBlock::new(text))])
            .build();
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
        self.core.interrupt.trigger(None, None);
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
            middleware_chain: Arc::new(chain),
            call_gate: Arc::new(CallGate::new()),
            subscriber: Arc::new(Mutex::new(None)),
            tasks,
            confirm_handler: Mutex::new(self.confirm_handler),
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
    for m in messages {
        state.context_mut().push(m);
    }
    state.set_cur_iter(0);
    let result = execute_iteration(core, ctx, &mut state, 0).await?;
    *core.state.lock().unwrap() = state;
    Ok(result)
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
        let has_tool_calls = msg.tool_use_blocks().next().is_some();
        let suspended = match reason {
            GenerateReason::ModelStop | GenerateReason::ToolCalls => {
                if has_tool_calls {
                    let suspended = acting_phase(core, ctx, state).await?;
                    iter += 1;
                    suspended
                } else {
                    return Ok(msg);
                }
            }
            GenerateReason::ReasoningStopRequested | GenerateReason::MaxIterations => {
                return Ok(msg);
            }
            _ => {
                if has_tool_calls {
                    let suspended = acting_phase(core, ctx, state).await?;
                    iter += 1;
                    suspended
                } else {
                    return Ok(msg);
                }
            }
        };
        if suspended {
            return Ok(msg.with_generate_reason(GenerateReason::ToolSuspended));
        }
    }
}

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

    let final_msg = run_reasoning(core, ctx, input).await?;
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
fn emit_tool_result_end(
    delta_tx: &Option<futures::channel::mpsc::UnboundedSender<crate::error::Result<AgentEvent>>>,
    tool_call_id: &str,
    tool_name: &str,
    state: ToolResultState,
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
    };
    fields.insert("state".to_string(), serde_json::Value::String(state_str.to_string()));
    emit_delta(delta_tx, AgentEvent::with_fields(AgentEventType::ToolResultEnd, fields));
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
    // The standard model defaults (3 attempts) apply unless an explicit
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
                check_interrupted(core)?;
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
async fn acting_phase(
    core: &Arc<AgentCore>,
    ctx: &crate::agent::RuntimeContext,
    state: &mut AgentState,
) -> Result<bool> {
    check_interrupted(core)?;
    let pending = extract_pending_tool_calls(state);
    if pending.is_empty() {
        return Ok(false);
    }

    let mut results = Vec::new();
    let mut suspended = false;
    let runtime = tool_runtime(ctx);
    for call in &pending {
        let decision = evaluate_permission(core, call);
        match decision.behavior {
            PermissionBehavior::Deny => {
                // Emit ToolResultStart → TextDelta → End for denied calls.
                emit_tool_result_start(&ctx.delta_tx, &call.id, &call.name);
                let denied_text = "Permission denied by rules";
                emit_tool_result_text_delta(&ctx.delta_tx, &call.id, &call.name, denied_text);
                results.push(denied_result(call, &decision.message));
                emit_tool_result_end(&ctx.delta_tx, &call.id, &call.name, ToolResultState::Denied);
            }
            PermissionBehavior::Allow | PermissionBehavior::Passthrough => {
                // Emit ToolResultStart for this tool execution.
                emit_tool_result_start(&ctx.delta_tx, &call.id, &call.name);
                let tctx = ToolExecutionContext {
                    tool_call: Some(call.clone()),
                    runtime: runtime.clone(),
                    emitter: None,
                };
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
                        let final_state = if rb.state == ToolResultState::Error { ToolResultState::Error } else { ToolResultState::Success };
                        let bound = bind_result(call, rb);
                        suspended = suspended || bound.is_suspended();
                        results.push(bound);
                        emit_tool_result_end(&ctx.delta_tx, &call.id, &call.name, final_state);
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        emit_tool_result_text_delta(&ctx.delta_tx, &call.id, &call.name, &err_msg);
                        results.push(error_result(call, &err_msg));
                        emit_tool_result_end(&ctx.delta_tx, &call.id, &call.name, ToolResultState::Error);
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
                        let tctx = ToolExecutionContext {
                            tool_call: Some(effective_call.clone()),
                            runtime: runtime.clone(),
                            emitter: None,
                        };
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
                                let final_state = if rb.state == ToolResultState::Error { ToolResultState::Error } else { ToolResultState::Success };
                                let bound = bind_result(effective_call, rb);
                                suspended = suspended || bound.is_suspended();
                                results.push(bound);
                                emit_tool_result_end(&ctx.delta_tx, &effective_call.id, &effective_call.name, final_state);
                            }
                            Err(e) => {
                                let err_msg = e.to_string();
                                emit_tool_result_text_delta(&ctx.delta_tx, &effective_call.id, &effective_call.name, &err_msg);
                                results.push(error_result(effective_call, &err_msg));
                                emit_tool_result_end(&ctx.delta_tx, &effective_call.id, &effective_call.name, ToolResultState::Error);
                            }
                        }
                    } else {
                        // User denied the tool call.
                        let denied_text = &confirm_result.message;
                        emit_tool_result_text_delta(&ctx.delta_tx, &call.id, &call.name, denied_text);
                        results.push(denied_result(call, denied_text));
                        emit_tool_result_end(&ctx.delta_tx, &call.id, &call.name, ToolResultState::Denied);
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

    let result_msg = Msg::builder()
        .role(MsgRole::Tool)
        .content(results.into_iter().map(ContentBlock::ToolResult).collect())
        .build();
    state.context_mut().push(result_msg);
    Ok(suspended)
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

/// Aborts execution if the agent has been interrupted.
fn check_interrupted(core: &Arc<AgentCore>) -> Result<()> {
    if core.interrupt.is_interrupted() {
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
        .filter(|tu| !answered.contains(tu.id.as_str()))
        .cloned()
        .collect()
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
fn bind_result(call: &ToolUseBlock, rb: ToolResultBlock) -> ToolResultBlock {
    ToolResultBlock {
        id: Some(call.id.clone()),
        name: Some(call.name.clone()),
        output: rb.output,
        metadata: rb.metadata,
        state: rb.state,
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
}
