//! High-level remote A2A agent facade, mirroring `io.agentscope.core.a2a.agent.A2aAgent` and
//! `A2aAgentConfig`.
//!
//! [`A2aClient`](crate::client::A2aClient) only speaks JSON-RPC. [`A2aAgent`] adds the pieces the
//! Java facade owns on top of its SDK client:
//!
//! * the agent card is resolved **once per call** and a missing card fails fast (Java
//!   `A2aAgent#startCall`, `1524b3191`);
//! * the terminal reply follows the client's result rules — completed artifacts win over the
//!   status message, a completed status update without artifacts keeps its message
//!   (`6e1ee68d7`, `00e4a66b6`);
//! * a non-terminal `message/send` task is settled by polling `tasks/get`, and
//!   [`Agent::interrupt`] makes the running call issue `tasks/cancel` for that task
//!   (Java `runningCalls` + `cancelRemoteTask`);
//! * every call appends the input and the reply to the agent state's history, which is what
//!   Java does through its session memory;
//! * every call runs the configured `Middleware` chain: `on_agent` wraps the whole invocation
//!   and `on_reasoning` wraps the exchange with the remote agent, mirroring Java `8cc6795b0`
//!   (`agentMiddlewares` / `reasoningMiddlewares`).
//!
//! **Documented deviations from Java** (all forced by the Rust core surface, which is not
//! changed by this crate):
//!
//! 1. Rust `Agent::agent_state()` has no session parameter, so the facade keeps **one** history
//!    slot. Java keeps an `AgentState` per `(userId, sessionId)`; hosts that need several remote
//!    sessions build one [`A2aAgent`] per session (the core agents behave the same way).
//! 2. Rust `Agent::interrupt()` is synchronous, so it only marks the running call as cancelled;
//!    the run loop performs the remote `tasks/cancel` at its next poll boundary and reports the
//!    outcome as the Java `Task <id> interrupt successfully.` message.
//! 3. A remote agent has no local tools, so [`Agent::toolkit`] exposes an empty [`Toolkit`];
//!    core/harness tool injection does not apply to it.
//! 4. The A2A wire protocol has no structured-output extension, so `call_with_schema` reports
//!    [`A2aError::UnsupportedMethod`] instead of silently degrading.
//! 5. Java's `Hook` (pre/post reasoning, handed to `ClientEventContext`) has no Rust trait; the
//!    middlewares cover the same points — `on_reasoning` around the exchange and
//!    `on_reasoning_result` with the message it produced.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::{AgentEvent, EventData};
use agentscope_core::message::Msg;
use agentscope_core::middleware::{
    AgentInput, EventStream, Middleware, MiddlewareChain, ReasoningInput,
};
use agentscope_core::model::GenerateOptions;
use agentscope_core::state::AgentState;
use agentscope_core::tool::Toolkit;
use agentscope_core::{
    Agent, CallableAgent, Error, JsonSchema, ObservableAgent, Result as CoreResult,
};
use futures::stream::BoxStream;
use futures::StreamExt;

use crate::card::AgentCardResolver;
use crate::client::{A2aClient, JsonRpcTransport, ReqwestJsonRpcTransport, SendResult};
use crate::error::A2aError;
use crate::message::{message_constants, MessageConstants, MessageConvert};
use crate::model::{MessageSendParams, Part, StreamEvent, Task, TaskIdParams, TaskQueryParams};
use crate::result::{task_result_message, TaskResultAccumulator};

/// Default number of `tasks/get` polls before a call gives up on a non-terminal task.
pub const DEFAULT_POLL_ATTEMPTS: usize = 100;
/// Default delay between two `tasks/get` polls.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Config of an [`A2aAgent`], mirroring Java `A2aAgentConfig`.
///
/// Java carries the `a2a-java-sdk` `clientTransports` map and a `ClientConfig`; the Rust client
/// has one injectable [`JsonRpcTransport`] plus auth headers, so that pair replaces the map.
/// The poll knobs are Rust-only: Java's SDK blocks on the event stream instead of polling.
#[derive(Clone)]
pub struct A2aAgentConfig {
    transport: Arc<dyn JsonRpcTransport>,
    auth_headers: BTreeMap<String, String>,
    poll_attempts: usize,
    poll_interval: Duration,
}

impl std::fmt::Debug for A2aAgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAgentConfig")
            .field(
                "auth_headers",
                &self.auth_headers.keys().collect::<Vec<_>>(),
            )
            .field("poll_attempts", &self.poll_attempts)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl Default for A2aAgentConfig {
    fn default() -> Self {
        Self {
            transport: Arc::new(ReqwestJsonRpcTransport::new()),
            auth_headers: BTreeMap::new(),
            poll_attempts: DEFAULT_POLL_ATTEMPTS,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

impl A2aAgentConfig {
    /// Creates a configuration with the default transport and polling settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the Java-style fluent builder.
    pub fn builder() -> A2aAgentConfigBuilder {
        A2aAgentConfigBuilder::default()
    }

    /// Replaces the JSON-RPC transport (Java `A2aAgentConfig#withTransport`).
    pub fn with_transport(mut self, transport: Arc<dyn JsonRpcTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Sets the headers sent with every request.
    pub fn with_auth_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.auth_headers = headers;
        self
    }

    /// Caps how many `tasks/get` polls one call performs.
    pub fn with_poll_attempts(mut self, attempts: usize) -> Self {
        self.poll_attempts = attempts;
        self
    }

    /// Sets the delay between two `tasks/get` polls.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// The injected transport.
    pub fn transport(&self) -> &Arc<dyn JsonRpcTransport> {
        &self.transport
    }

    /// The configured auth headers.
    pub fn auth_headers(&self) -> &BTreeMap<String, String> {
        &self.auth_headers
    }

    /// The configured poll cap.
    pub fn poll_attempts(&self) -> usize {
        self.poll_attempts
    }

    /// The configured poll interval.
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }
}

/// Builder for [`A2aAgentConfig`].
#[derive(Clone, Default)]
pub struct A2aAgentConfigBuilder {
    transport: Option<Arc<dyn JsonRpcTransport>>,
    auth_headers: BTreeMap<String, String>,
    poll_attempts: Option<usize>,
    poll_interval: Option<Duration>,
}

impl A2aAgentConfigBuilder {
    /// Injects a custom transport.
    pub fn transport(mut self, transport: Arc<dyn JsonRpcTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Sets the auth headers.
    pub fn auth_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.auth_headers = headers;
        self
    }

    /// Caps the number of `tasks/get` polls.
    pub fn poll_attempts(mut self, attempts: usize) -> Self {
        self.poll_attempts = Some(attempts);
        self
    }

    /// Sets the delay between two polls.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = Some(interval);
        self
    }

    /// Builds the configuration.
    pub fn build(self) -> A2aAgentConfig {
        let defaults = A2aAgentConfig::default();
        A2aAgentConfig {
            transport: self.transport.unwrap_or(defaults.transport),
            auth_headers: self.auth_headers,
            poll_attempts: self.poll_attempts.unwrap_or(defaults.poll_attempts),
            poll_interval: self.poll_interval.unwrap_or(defaults.poll_interval),
        }
    }
}

/// One running call, mirroring Java `A2aCall`.
#[derive(Debug)]
struct InFlightCall {
    request_id: String,
    task_id: Mutex<Option<String>>,
    cancelled: AtomicBool,
}

impl InFlightCall {
    fn new(request_id: String, task_id: Option<String>) -> Self {
        Self {
            request_id,
            task_id: Mutex::new(task_id),
            cancelled: AtomicBool::new(false),
        }
    }

    fn task_id(&self) -> Option<String> {
        self.task_id.lock().ok().and_then(|id| id.clone())
    }

    fn set_task_id(&self, task_id: &str) {
        if let Ok(mut slot) = self.task_id.lock() {
            *slot = Some(task_id.to_string());
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Removes a finished call from the registry, mirroring Java `endCall`.
struct CallGuard {
    running: Arc<Mutex<Vec<Arc<InFlightCall>>>>,
    call: Arc<InFlightCall>,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if let Ok(mut running) = self.running.lock() {
            running.retain(|each| !Arc::ptr_eq(each, &self.call));
        }
    }
}

/// A remote A2A agent usable as a local [`Agent`] / [`CallableAgent`].
///
/// Mirrors Java `A2aAgent`: every call resolves the agent card again, calls of the same agent run
/// one at a time (Java serializes them per session), the conversation history is kept in the
/// agent state, and the configured [`Middleware`] chain wraps every call (`on_agent`) and the
/// exchange with the remote agent (`on_reasoning`).
pub struct A2aAgent {
    /// Everything one call needs, owned so middleware closures can be `'static`.
    parts: CallParts,
    description: String,
    toolkit: Toolkit,
}

/// A subscriber registered through [`ObservableAgent::subscribe`].
type A2aEventSink = Box<dyn Fn(AgentEvent) + Send + Sync>;

/// The slot a call publishes its reply (or its failure) into.
///
/// Java keeps the reply of the exchange in an `AtomicReference` (`A2aAgent#execute`); publishing
/// it here lets the agent-level stream emit it as `AgentResult` once the reasoning middlewares
/// are done, i.e. where Java's `runAgent` emits it.
type CallOutcome = Arc<Mutex<Option<Result<Msg, A2aError>>>>;

/// How one call talks to the remote agent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallMode {
    /// `message/send` plus `tasks/get` polls (the blocking `call` path, Java `doCall`).
    Send,
    /// `message/stream`, collected before the reply is returned (Java `streamEvents` collected).
    StreamBuffered,
    /// `message/stream`, relayed frame by frame (Java `streamEvents`).
    StreamIncremental,
}

/// The owned handles of one call, cloned into the middleware closures.
#[derive(Clone)]
struct CallParts {
    name: String,
    resolver: Arc<dyn AgentCardResolver>,
    config: A2aAgentConfig,
    state: Arc<Mutex<AgentState>>,
    running: Arc<Mutex<Vec<Arc<InFlightCall>>>>,
    /// Sinks registered through [`ObservableAgent::subscribe`], mirroring Java
    /// `ClientEventContext#setEventEmitter`.
    sinks: Arc<Mutex<Vec<A2aEventSink>>>,
    /// Serializes calls of this agent, mirroring Java `callSerializationKey(session)`.
    ///
    /// An async mutex because the guard is held across the request's awaits and
    /// [`CallableAgent::call`] must stay `Send`; the streaming path additionally takes an
    /// **owned** guard so the returned stream can be `'static`.
    gate: Arc<tokio::sync::Mutex<()>>,
    /// The middlewares of this agent, in the core chain's priority order.
    chain: MiddlewareChain,
}

impl std::fmt::Debug for A2aAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAgent")
            .field("name", &self.parts.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

impl A2aAgent {
    /// Creates an agent that resolves its card through `resolver`.
    pub fn new(name: impl Into<String>, resolver: Arc<dyn AgentCardResolver>) -> Self {
        Self::builder(name, resolver).build()
    }

    /// Returns the Java-style fluent builder.
    pub fn builder(
        name: impl Into<String>,
        resolver: Arc<dyn AgentCardResolver>,
    ) -> A2aAgentBuilder {
        A2aAgentBuilder::new(name, resolver)
    }

    /// The agent name sent to the remote peer and used to resolve the card.
    pub fn agent_name(&self) -> &str {
        &self.parts.name
    }

    /// The configured description, when the builder provided one.
    pub fn configured_description(&self) -> &str {
        &self.description
    }

    /// Resolves the card and reads its description, mirroring Java's build-time description
    /// lookup. Falls back to the configured description when the card cannot be resolved.
    pub async fn resolve_description(&self) -> String {
        match self.resolve_card().await {
            Ok(card) => card
                .description
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| self.description.clone()),
            Err(_) => self.description.clone(),
        }
    }

    /// Resolves the remote card once, failing fast with the agent and resolver names.
    pub async fn resolve_card(&self) -> Result<crate::model::AgentCard, A2aError> {
        self.parts.client().agent_card().await
    }

    /// The current history of the agent state (Java `historyOf(session)`).
    pub fn history(&self) -> Vec<Msg> {
        self.parts.history()
    }

    /// The running call ids, sorted for stable assertions.
    pub fn running_call_ids(&self) -> Vec<String> {
        self.parts.running_call_ids()
    }

    /// Sends `messages` with `message/send` and returns the terminal reply.
    ///
    /// Mirrors Java `A2aAgent#doCall`: history plus the new input are sent, and the reply is
    /// appended to the history.
    pub async fn exchange(&self, messages: Vec<Msg>) -> Result<Msg, A2aError> {
        self.run_call(messages, CallMode::Send).await
    }

    /// Sends `messages` over `message/stream` and returns the terminal reply.
    ///
    /// Mirrors Java `A2aAgent#streamEvents` result handling: the reply is the one
    /// [`TaskResultAccumulator`](crate::result::TaskResultAccumulator) derives from the events.
    pub async fn exchange_stream(&self, messages: Vec<Msg>) -> Result<Msg, A2aError> {
        self.run_call(messages, CallMode::StreamBuffered).await
    }

    /// Drives one call through the middleware chain and returns the reply it produced.
    async fn run_call(&self, messages: Vec<Msg>, mode: CallMode) -> Result<Msg, A2aError> {
        let (events, outcome) = self.parts.call_stream(messages, mode);
        drain_call(events, outcome).await
    }

    /// Sends `messages` over `message/stream` and relays the remote run as core events.
    ///
    /// Mirrors Java `A2aAgent#streamEvents`: an `AGENT_START`, one `TEXT_BLOCK_DELTA` (or
    /// `THINKING_BLOCK_DELTA`) per text part of every artifact update, then `AGENT_RESULT` plus
    /// `AGENT_END` once the task reaches a terminal state. Each emitted event is also pushed to the
    /// sinks registered through [`ObservableAgent::subscribe`], which is Java's
    /// `ClientEventContext#setEventEmitter`.
    ///
    /// The call is serialized like [`exchange`](Self::exchange): the gate is taken on the first
    /// poll and released when the stream ends or is dropped.
    pub fn stream_events(
        &self,
        messages: Vec<Msg>,
    ) -> BoxStream<'static, Result<AgentEvent, A2aError>> {
        let (events, outcome) = self
            .parts
            .call_stream(messages, CallMode::StreamIncremental);
        let sinks = self.parts.sinks.clone();
        Box::pin(events.map(move |item| match item {
            Ok(event) => {
                emit_to_sinks(&sinks, &event);
                Ok(event)
            }
            // `Middleware` streams carry core errors; restore the `A2aError` the call recorded so
            // library callers still see the original variant.
            Err(error) => Err(restore_error(error, &outcome)),
        }))
    }

    /// Cancels the running call's remote task, mirroring Java `cancelRemoteTask`.
    ///
    /// Returns `None` when no call carries a task id yet.
    pub async fn cancel_remote_task(&self) -> Result<Option<Msg>, A2aError> {
        let task_id = self.parts.running_task_id();
        let Some(task_id) = task_id else {
            return Ok(None);
        };
        Ok(Some(
            self.parts
                .cancel_task(&self.parts.client(), &task_id)
                .await?,
        ))
    }
}

impl CallParts {
    /// Builds a client for one call: the card is resolved when the client is used, i.e. once per
    /// call (Java `startCall` -> `Mono.using`).
    fn client(&self) -> A2aClient {
        A2aClient::new(self.name.clone(), self.resolver.clone())
            .with_transport(self.config.transport.clone())
            .with_auth_headers(self.config.auth_headers.clone())
    }

    fn history(&self) -> Vec<Msg> {
        history_of(&self.state)
    }

    /// The identity of the call's session scope: the user id and the non-empty session id of
    /// the agent state, which is what Java takes from the `RuntimeContext` of the call.
    fn identity(&self) -> (Option<String>, Option<String>) {
        match self.state.lock() {
            Ok(state) => (
                state.user_id.clone(),
                (!state.session_id.is_empty()).then(|| state.session_id.clone()),
            ),
            Err(_) => (None, None),
        }
    }

    fn append_history(&self, messages: &[Msg]) {
        append_to_history(&self.state, messages);
    }

    /// The runtime context handed to the middlewares: the session scope of the agent state.
    fn runtime_context(&self) -> RuntimeContext {
        let state = self
            .state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default();
        let mut context = RuntimeContext::new().with_agent_id(self.name.clone());
        if !state.session_id.is_empty() {
            context = context.with_session_id(state.session_id.clone());
        }
        if let Some(user_id) = state.user_id.clone() {
            context = context.with_user_id(user_id);
        }
        context
    }

    fn running_call_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .running
            .lock()
            .map(|running| running.iter().map(|call| call.request_id.clone()).collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    fn running_task_id(&self) -> Option<String> {
        self.running
            .lock()
            .ok()
            .and_then(|running| running.iter().find_map(|call| call.task_id()))
    }

    fn register_call(&self, task: &Task) -> Arc<InFlightCall> {
        let call = Arc::new(InFlightCall::new(
            uuid::Uuid::new_v4().to_string(),
            Some(task.id.clone()),
        ));
        if let Ok(mut running) = self.running.lock() {
            running.push(call.clone());
        }
        call
    }

    /// Sends `messages` with `message/send` and settles the task, mirroring Java `doCall`.
    async fn send_call(&self, messages: Vec<Msg>) -> Result<Msg, A2aError> {
        // Java serializes calls that share a session: they share the history.
        let _gate = self.gate.lock().await;
        self.append_history(&messages);
        let (user_id, session_id) = self.identity();
        let params = MessageSendParams {
            message: MessageConvert::convert_from_msgs_with_identity(
                &self.history(),
                user_id.as_deref(),
                session_id.as_deref(),
            ),
            configuration: None,
            metadata: None,
        };
        let client = self.client();
        let reply = match client.message_send(params).await? {
            SendResult::Message(message) => {
                Ok(MessageConvert::convert_from_message(&message, &self.name))
            }
            SendResult::Task(task) => self.settle(client, *task).await,
        };
        if let Ok(reply) = &reply {
            self.append_history(std::slice::from_ref(reply));
        }
        reply
    }

    /// Sends `messages` over `message/stream` and returns the reply plus the relayed deltas.
    async fn stream_call(&self, messages: Vec<Msg>) -> Result<(Msg, Vec<AgentEvent>), A2aError> {
        let _gate = self.gate.lock().await;
        self.append_history(&messages);
        let (user_id, session_id) = self.identity();
        let params = MessageSendParams {
            message: MessageConvert::convert_from_msgs_with_identity(
                &self.history(),
                user_id.as_deref(),
                session_id.as_deref(),
            ),
            configuration: None,
            metadata: None,
        };
        let events = self.client().message_stream(params).await?;
        let mut deltas = Vec::new();
        let mut accumulator = TaskResultAccumulator::new();
        for event in &events {
            deltas.extend(stream_event_to_agent_events(event));
            if let Some(reply) = accumulator.observe(event, &self.name) {
                self.append_history(std::slice::from_ref(&reply));
                return Ok((reply, deltas));
            }
        }
        // A stream that carried a plain message reply ends with that message.
        if let Some(message) = events.iter().find_map(|event| match event {
            StreamEvent::Message(message) => Some(message),
            _ => None,
        }) {
            let reply = MessageConvert::convert_from_message(message, &self.name);
            self.append_history(std::slice::from_ref(&reply));
            return Ok((reply, deltas));
        }
        Err(A2aError::InvalidRequest(
            "a2a stream ended without a terminal result".to_string(),
        ))
    }

    /// Runs one call through the middleware chain, as core events.
    ///
    /// The agent middlewares wrap the whole invocation (`AgentStart` … `AgentEnd`) and the
    /// reasoning middlewares wrap the exchange with the remote agent, mirroring Java
    /// `buildAgentStream` and `execute` (`8cc6795b0`). The reply is published as the terminal
    /// `AgentResult`; a failure, including one a middleware injected, ends the stream with an
    /// error carrying the original message.
    fn call_stream(&self, messages: Vec<Msg>, mode: CallMode) -> (EventStream, CallOutcome) {
        let outcome: CallOutcome = Arc::new(Mutex::new(None));
        let handler = self.clone();
        let core = {
            let inner = handler.clone();
            let outcome = outcome.clone();
            Box::new(move |input: AgentInput| -> EventStream {
                let context = inner.runtime_context();
                let exchange = inner.clone();
                let reasoning_outcome = outcome.clone();
                let reasoning = inner.chain.wrap_reasoning(
                    context.clone(),
                    ReasoningInput::new(input.messages, Vec::new(), GenerateOptions::default()),
                    Box::new(move |input: ReasoningInput| {
                        exchange.exchange(input.messages, mode, reasoning_outcome)
                    }),
                );
                let finish = inner.clone();
                let tail = futures::stream::once(async move {
                    finish.terminal_events(outcome, &context).await
                })
                .flat_map(futures::stream::iter);
                Box::pin(
                    futures::stream::once(async {
                        Ok(AgentEvent::AgentStart(EventData::default()))
                    })
                    .chain(reasoning)
                    .chain(tail)
                    .chain(futures::stream::once(async {
                        Ok(AgentEvent::AgentEnd(EventData::default()))
                    })),
                )
            })
        };
        let events = self
            .chain
            .wrap_agent(self.runtime_context(), AgentInput::new(messages), core);
        (events, outcome)
    }

    /// The agent-level tail of a call: the published reply as `AgentResult`, or its failure.
    ///
    /// The published outcome is **not** consumed here: `drain_call` (the blocking callers) and
    /// `restore_error` (the streaming caller) read it afterwards to hand the caller the reply or the
    /// original [`A2aError`] variant.
    async fn terminal_events(
        &self,
        outcome: CallOutcome,
        context: &RuntimeContext,
    ) -> Vec<CoreResult<AgentEvent>> {
        let published = outcome.lock().ok().and_then(|slot| match slot.as_ref() {
            Some(Ok(reply)) => Some(Ok(reply.clone())),
            Some(Err(error)) => Some(Err(error.to_string())),
            None => None,
        });
        match published {
            Some(Ok(reply)) => {
                // Java `PostReasoningEvent`: the hooks see the message the remote agent produced.
                self.chain.notify_reasoning_result(context, &reply).await;
                let mut data = EventData::default();
                if let Ok(value) = serde_json::to_value(&reply) {
                    data.fields.insert("reply".to_string(), value);
                }
                vec![Ok(AgentEvent::AgentResult(data))]
            }
            Some(Err(text)) => vec![Err(Error::Agent(text))],
            None => vec![Err(Error::Agent(
                "a2a call finished without a result".to_string(),
            ))],
        }
    }

    /// Runs the exchange with the remote agent, mirroring Java `A2aAgent#exchange`.
    ///
    /// It relays the TEXT/THINKING deltas of the remote run and publishes the terminal reply.
    fn exchange(&self, messages: Vec<Msg>, mode: CallMode, outcome: CallOutcome) -> EventStream {
        match mode {
            CallMode::Send => {
                let handler = self.clone();
                Box::pin(
                    futures::stream::once(async move {
                        let result = handler.send_call(messages).await;
                        publish(&outcome, result);
                        Vec::new()
                    })
                    .flat_map(futures::stream::iter),
                )
            }
            CallMode::StreamBuffered => {
                let handler = self.clone();
                Box::pin(
                    futures::stream::once(async move {
                        let events: Vec<CoreResult<AgentEvent>> =
                            match handler.stream_call(messages).await {
                                Ok((reply, deltas)) => {
                                    publish(&outcome, Ok(reply));
                                    deltas.into_iter().map(Ok).collect()
                                }
                                Err(error) => {
                                    publish(&outcome, Err(error));
                                    Vec::new()
                                }
                            };
                        events
                    })
                    .flat_map(futures::stream::iter),
                )
            }
            CallMode::StreamIncremental => Box::pin(futures::stream::unfold(
                IncrementalCall::new(self.clone(), messages, outcome),
                next_incremental_item,
            )),
        }
    }

    /// Settles a non-terminal `message/send` task by polling `tasks/get`, mirroring the blocking
    /// contract of Java `A2aAgent#doCall`.
    async fn settle(&self, client: A2aClient, task: Task) -> Result<Msg, A2aError> {
        if let Some(reply) = task_result_message(&task, &self.name) {
            return Ok(reply);
        }
        let call = self.register_call(&task);
        let _guard = CallGuard {
            running: self.running.clone(),
            call: call.clone(),
        };
        let mut current = task;
        for _ in 0..self.config.poll_attempts {
            if call.is_cancelled() {
                return self.cancel_task(&client, &current.id).await;
            }
            tokio::time::sleep(self.config.poll_interval).await;
            current = client
                .tasks_get(TaskQueryParams {
                    id: current.id.clone(),
                    history_length: None,
                    metadata: None,
                })
                .await?;
            call.set_task_id(&current.id);
            if let Some(reply) = task_result_message(&current, &self.name) {
                return Ok(reply);
            }
        }
        Err(A2aError::InvalidRequest(format!(
            "a2a task {} did not reach a terminal state",
            current.id
        )))
    }

    /// Issues `tasks/cancel` and reports it the way Java's `cancelRemoteTask` does.
    async fn cancel_task(&self, client: &A2aClient, task_id: &str) -> Result<Msg, A2aError> {
        match client
            .tasks_cancel(TaskIdParams {
                id: task_id.to_string(),
                metadata: None,
            })
            .await
        {
            Ok(_) => Ok(Msg::assistant(format!(
                "Task {task_id} interrupt successfully."
            ))),
            // Java turns a cancel failure into a message instead of an error.
            Err(error) => Ok(Msg::assistant(error.to_string())),
        }
    }
}

/// Records the reply (or the failure) of one call, mirroring Java's `AtomicReference<Msg>`.
fn publish(outcome: &CallOutcome, result: Result<Msg, A2aError>) {
    if let Ok(mut slot) = outcome.lock() {
        *slot = Some(result);
    }
}

/// Maps a core error back to the failure the call recorded.
///
/// `Middleware` streams only carry core errors, so `stream_events` restores the original
/// [`A2aError`] variant instead of flattening every failure into one kind.
fn restore_error(error: Error, outcome: &CallOutcome) -> A2aError {
    if let Ok(mut slot) = outcome.lock() {
        if matches!(slot.as_ref(), Some(Err(_))) {
            if let Some(Err(failure)) = slot.take() {
                return failure;
            }
        }
    }
    A2aError::InvalidRequest(error.to_string())
}

/// Drives a call's event stream to the end and returns the reply the chain produced.
async fn drain_call(events: EventStream, outcome: CallOutcome) -> Result<Msg, A2aError> {
    let mut first_error = None;
    let mut events = events;
    while let Some(item) = events.next().await {
        if let Err(error) = item {
            first_error.get_or_insert(error);
        }
    }
    match outcome.lock().ok().and_then(|mut slot| slot.take()) {
        Some(result) => result,
        None => Err(A2aError::InvalidRequest(match first_error {
            Some(error) => error.to_string(),
            None => "a2a call finished without a result".to_string(),
        })),
    }
}

/// The current history of the agent state, mirroring Java `historyOf(session)`.
fn history_of(state: &Arc<Mutex<AgentState>>) -> Vec<Msg> {
    state
        .lock()
        .map(|state| state.context.clone())
        .unwrap_or_default()
}

/// Appends `messages` to the agent state, mirroring Java's per-call history update.
fn append_to_history(state: &Arc<Mutex<AgentState>>, messages: &[Msg]) {
    if messages.is_empty() {
        return;
    }
    if let Ok(mut state) = state.lock() {
        state.context.extend(messages.iter().cloned());
    }
}

/// One incremental `message/stream` call, mirroring Java `A2aAgent#streamEvents`' frame loop.
struct IncrementalCall {
    handler: CallParts,
    outcome: CallOutcome,
    messages: Vec<Msg>,
    /// Held for the whole stream, so two calls of one agent never interleave.
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    frames: Option<BoxStream<'static, Result<StreamEvent, A2aError>>>,
    accumulator: TaskResultAccumulator,
    /// The plain message reply, which ends a stream that carries no task.
    message_reply: Option<Msg>,
    pending: VecDeque<AgentEvent>,
    done: bool,
}

impl IncrementalCall {
    fn new(handler: CallParts, messages: Vec<Msg>, outcome: CallOutcome) -> Self {
        Self {
            handler,
            outcome,
            messages,
            guard: None,
            frames: None,
            accumulator: TaskResultAccumulator::new(),
            message_reply: None,
            pending: VecDeque::new(),
            done: false,
        }
    }

    /// Serializes the call and opens the remote stream on the first poll.
    async fn open(&mut self) {
        self.guard = Some(self.handler.gate.clone().lock_owned().await);
        self.handler.append_history(&self.messages);
        let (user_id, session_id) = self.handler.identity();
        let params = MessageSendParams {
            message: MessageConvert::convert_from_msgs_with_identity(
                &self.handler.history(),
                user_id.as_deref(),
                session_id.as_deref(),
            ),
            configuration: None,
            metadata: None,
        };
        match self
            .handler
            .client()
            .message_stream_incremental(params)
            .await
        {
            Ok(frames) => self.frames = Some(frames),
            Err(error) => {
                publish(&self.outcome, Err(error));
                self.done = true;
            }
        }
    }
}

/// One poll of an incremental call, mirroring Java `A2aAgent#streamEvents`.
async fn next_incremental_item(
    mut call: IncrementalCall,
) -> Option<(CoreResult<AgentEvent>, IncrementalCall)> {
    loop {
        if let Some(event) = call.pending.pop_front() {
            return Some((Ok(event), call));
        }
        if call.done {
            return None;
        }
        let Some(mut frames) = call.frames.take() else {
            call.open().await;
            continue;
        };
        match frames.next().await {
            Some(Ok(event)) => {
                call.pending.extend(stream_event_to_agent_events(&event));
                if let Some(reply) = call.accumulator.observe(&event, &call.handler.name) {
                    call.handler.append_history(std::slice::from_ref(&reply));
                    publish(&call.outcome, Ok(reply));
                    call.done = true;
                } else {
                    if let StreamEvent::Message(message) = &event {
                        let reply =
                            MessageConvert::convert_from_message(message, &call.handler.name);
                        call.message_reply = Some(reply);
                    }
                    call.frames = Some(frames);
                }
            }
            Some(Err(error)) => {
                publish(&call.outcome, Err(error));
                call.done = true;
            }
            None => {
                match call.message_reply.take() {
                    Some(reply) => {
                        call.handler.append_history(std::slice::from_ref(&reply));
                        publish(&call.outcome, Ok(reply));
                    }
                    None => publish(
                        &call.outcome,
                        Err(A2aError::InvalidRequest(
                            "a2a stream ended without a terminal result".to_string(),
                        )),
                    ),
                }
                call.done = true;
            }
        }
    }
}

/// Maps one A2A stream event to the core events a local subscriber sees.
///
/// Java `ClientEventContext#emitDeltas` forwards the TEXT and THINKING blocks of every artifact
/// update; status updates and task payloads only feed the terminal result, so they produce no
/// delta events here.
fn stream_event_to_agent_events(event: &StreamEvent) -> Vec<AgentEvent> {
    let StreamEvent::ArtifactUpdate(update) = event else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for part in &update.artifact.parts {
        let Part::Text(text) = part else {
            continue;
        };
        let mut data = EventData::default();
        data.fields.insert(
            "reply_id".to_string(),
            serde_json::Value::String(update.artifact.artifact_id.clone()),
        );
        data.fields.insert(
            "delta".to_string(),
            serde_json::Value::String(text.text.clone()),
        );
        let block_type = MessageConstants::metadata_value(
            text.metadata.as_ref(),
            MessageConstants::BLOCK_TYPE_METADATA_KEY,
        );
        if block_type.as_deref() == Some(message_constants::block_content::TYPE_THINKING) {
            events.push(AgentEvent::ThinkingBlockDelta(data));
        } else {
            events.push(AgentEvent::TextBlockDelta(data));
        }
    }
    events
}

/// Pushes `event` to every registered sink, mirroring Java `ClientEventContext`'s emitter.
fn emit_to_sinks(sinks: &Arc<Mutex<Vec<A2aEventSink>>>, event: &AgentEvent) {
    if let Ok(sinks) = sinks.lock() {
        for sink in sinks.iter() {
            sink(event.clone());
        }
    }
}

impl Agent for A2aAgent {
    fn agent_id(&self) -> &str {
        &self.parts.name
    }

    fn name(&self) -> &str {
        &self.parts.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn interrupt(&self) {
        if let Ok(running) = self.parts.running.lock() {
            for call in running.iter() {
                call.cancel();
            }
        }
    }

    fn agent_state(&self) -> AgentState {
        self.parts
            .state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    fn toolkit(&self) -> &Toolkit {
        &self.toolkit
    }
}

impl ObservableAgent for A2aAgent {
    fn subscribe(&self, sink: Box<dyn Fn(AgentEvent) + Send + Sync>) {
        if let Ok(mut sinks) = self.parts.sinks.lock() {
            sinks.push(sink);
        }
    }
}

#[async_trait::async_trait]
impl CallableAgent for A2aAgent {
    async fn call(&self, messages: Vec<Msg>) -> CoreResult<Msg> {
        self.exchange(messages)
            .await
            .map_err(|error| Error::Agent(error.to_string()))
    }

    async fn call_with_schema(&self, messages: Vec<Msg>, schema: JsonSchema) -> CoreResult<Msg> {
        let _ = schema;
        // The A2A wire protocol in this SDK has no structured-output extension (Java's
        // `A2aAgent` only implements `doCall`), so refuse instead of faking a parsed reply.
        let _ = messages;
        Err(Error::Agent(
            "A2A agents do not support structured output: the A2A protocol has no response-schema \
             extension"
                .to_string(),
        ))
    }
}

/// Builder for [`A2aAgent`], mirroring Java `A2aAgent.Builder`.
pub struct A2aAgentBuilder {
    name: String,
    description: String,
    resolver: Arc<dyn AgentCardResolver>,
    config: A2aAgentConfig,
    state: AgentState,
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl std::fmt::Debug for A2aAgentBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAgentBuilder")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

impl A2aAgentBuilder {
    fn new(name: impl Into<String>, resolver: Arc<dyn AgentCardResolver>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            resolver,
            config: A2aAgentConfig::default(),
            state: AgentState::default(),
            middlewares: Vec::new(),
        }
    }

    /// Sets the description (Java reads it from the resolved card; see
    /// [`A2aAgent::resolve_description`] for the async lookup).
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the agent config.
    pub fn config(mut self, config: A2aAgentConfig) -> Self {
        self.config = config;
        self
    }

    /// Seeds the agent state (history / session id), mirroring Java's session-backed state.
    pub fn agent_state(mut self, state: AgentState) -> Self {
        self.state = state;
        self
    }

    /// Sets the middlewares of this agent, mirroring Java `A2aAgent.Builder#middleware`.
    ///
    /// `Middleware::on_agent` wraps the whole call and `Middleware::on_reasoning` wraps the
    /// exchange with the remote agent; ordering, `active_points` filtering and the onion nesting
    /// are the core `MiddlewareChain` semantics (higher `order()` wraps further out).
    pub fn middlewares(mut self, middlewares: Vec<Arc<dyn Middleware>>) -> Self {
        self.middlewares = middlewares;
        self
    }

    /// Builds the agent.
    pub fn build(self) -> A2aAgent {
        let mut chain = MiddlewareChain::new();
        for middleware in self.middlewares {
            chain.add_arc(middleware);
        }
        A2aAgent {
            parts: CallParts {
                name: self.name,
                resolver: self.resolver,
                config: self.config,
                state: Arc::new(Mutex::new(self.state)),
                running: Arc::new(Mutex::new(Vec::new())),
                sinks: Arc::new(Mutex::new(Vec::new())),
                gate: Arc::new(tokio::sync::Mutex::new(())),
                chain,
            },
            description: self.description,
            toolkit: Toolkit::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    use agentscope_core::middleware::{ExtensionPoint, Next};

    use crate::model::{
        methods, AgentCard, Artifact, JsonRpcRequest, JsonRpcResponse, Message, MessageRole, Part,
        TaskState, TaskStatus,
    };

    /// Truthful resolver used by the tests: either a card or a failure.
    struct ScriptedResolver {
        card: Result<AgentCard, ()>,
    }

    impl AgentCardResolver for ScriptedResolver {
        fn get_agent_card(
            &self,
            _agent_name: &str,
        ) -> Pin<Box<dyn Future<Output = Result<AgentCard, A2aError>> + Send + '_>> {
            let card = self.card.clone();
            Box::pin(async move {
                card.map_err(|_| A2aError::Transport("no card for this name".to_string()))
            })
        }
    }

    fn resolver() -> Arc<dyn AgentCardResolver> {
        Arc::new(ScriptedResolver {
            card: Ok(AgentCard::new("remote", "http://remote:9000")),
        })
    }

    /// Transport that answers by method name and records every call.
    struct ScriptedTransport {
        methods: Mutex<Vec<String>>,
        requests: Mutex<Vec<JsonRpcRequest>>,
        send_reply: JsonRpcResponse,
        get_replies: Mutex<Vec<JsonRpcResponse>>,
        cancel_reply: JsonRpcResponse,
    }

    impl ScriptedTransport {
        fn new(
            send_reply: JsonRpcResponse,
            get_replies: Vec<JsonRpcResponse>,
            cancel_reply: JsonRpcResponse,
        ) -> Arc<Self> {
            Arc::new(Self {
                methods: Mutex::new(Vec::new()),
                requests: Mutex::new(Vec::new()),
                send_reply,
                get_replies: Mutex::new(get_replies),
                cancel_reply,
            })
        }

        fn methods(&self) -> Vec<String> {
            self.methods.lock().unwrap().clone()
        }

        fn requests(&self) -> Vec<JsonRpcRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn next_get(&self) -> JsonRpcResponse {
            let mut replies = self.get_replies.lock().unwrap();
            if replies.len() > 1 {
                replies.remove(0)
            } else {
                replies
                    .first()
                    .cloned()
                    .unwrap_or_else(|| self.send_reply.clone())
            }
        }
    }

    impl JsonRpcTransport for ScriptedTransport {
        fn send(
            &self,
            _url: &str,
            request: &JsonRpcRequest,
            _headers: &BTreeMap<String, String>,
        ) -> Pin<Box<dyn Future<Output = Result<JsonRpcResponse, A2aError>> + Send + '_>> {
            self.methods.lock().unwrap().push(request.method.clone());
            self.requests.lock().unwrap().push(request.clone());
            let response = match request.method.as_str() {
                crate::model::methods::TASKS_GET => self.next_get(),
                crate::model::methods::TASKS_CANCEL => self.cancel_reply.clone(),
                _ => self.send_reply.clone(),
            };
            Box::pin(async move { Ok(response) })
        }

        fn send_stream(
            &self,
            _url: String,
            request: JsonRpcRequest,
            _headers: BTreeMap<String, String>,
        ) -> futures::stream::BoxStream<'static, Result<JsonRpcResponse, A2aError>> {
            self.methods.lock().unwrap().push(request.method.clone());
            self.requests.lock().unwrap().push(request);
            let response = self.send_reply.clone();
            Box::pin(futures::stream::once(async move { Ok(response) }))
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    fn completed_task_reply(
        artifacts: Option<Vec<Artifact>>,
        message: Option<&str>,
    ) -> JsonRpcResponse {
        let mut status = TaskStatus::new(TaskState::Completed);
        status.message = message
            .map(|text| Message::new("m-status", MessageRole::Agent, vec![Part::text(text)]));
        let mut task = Task::new("t-1", status);
        task.artifacts = artifacts;
        JsonRpcResponse::success(serde_json::to_value(&task).unwrap(), Some(1.into()))
    }

    fn working_task() -> JsonRpcResponse {
        let task = Task::new("t-1", TaskStatus::new(TaskState::Working));
        JsonRpcResponse::success(serde_json::to_value(&task).unwrap(), Some(1.into()))
    }

    fn agent_with(transport: Arc<ScriptedTransport>) -> A2aAgent {
        A2aAgent::builder("remote", resolver())
            .config(
                A2aAgentConfig::builder()
                    .transport(transport)
                    .poll_attempts(4)
                    .poll_interval(Duration::from_millis(1))
                    .build(),
            )
            .build()
    }

    // Java: A2aAgentTest — a completed task answers with its artifacts, not with `done`.
    #[test]
    fn call_returns_the_task_artifacts() {
        let transport = ScriptedTransport::new(
            completed_task_reply(
                Some(vec![Artifact::new(
                    "art-1",
                    vec![Part::text("real answer")],
                )]),
                Some("done"),
            ),
            Vec::new(),
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = agent_with(transport.clone());

        let reply = rt()
            .block_on(agent.call(vec![Msg::user("hi")]))
            .expect("call succeeds");

        assert_eq!(reply.text_content(), "real answer");
        assert_eq!(transport.methods(), vec![methods::MESSAGE_SEND.to_string()]);
        // History keeps the input and the reply, mirroring `addToHistory`.
        let history = agent.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text_content(), "hi");
        assert_eq!(history[1].text_content(), "real answer");
    }

    // Java: a completed task without artifacts answers with its status message.
    #[test]
    fn call_falls_back_to_the_status_message() {
        let transport = ScriptedTransport::new(
            completed_task_reply(None, Some("done")),
            Vec::new(),
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = agent_with(transport);

        let reply = rt()
            .block_on(agent.call(vec![Msg::user("hi")]))
            .expect("call succeeds");
        assert_eq!(reply.text_content(), "done");
    }

    // Java: a plain message reply (no task) is converted straight through.
    #[test]
    fn call_returns_a_plain_message_reply() {
        let message = Message::new("m-1", MessageRole::Agent, vec![Part::text("direct")]);
        let transport = ScriptedTransport::new(
            JsonRpcResponse::success(serde_json::to_value(&message).unwrap(), Some(1.into())),
            Vec::new(),
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = agent_with(transport);

        let reply = rt()
            .block_on(agent.call(vec![Msg::user("hi")]))
            .expect("call succeeds");
        assert_eq!(reply.text_content(), "direct");
    }

    // Java: `A2aAgent#exchange` — the user and session id of the call now travel on the wire.
    #[test]
    fn call_sends_the_session_identity_on_the_wire() {
        let transport = ScriptedTransport::new(
            completed_task_reply(None, Some("done")),
            Vec::new(),
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = A2aAgent::builder("remote", resolver())
            .config(
                A2aAgentConfig::builder()
                    .transport(transport.clone())
                    .poll_attempts(4)
                    .poll_interval(Duration::from_millis(1))
                    .build(),
            )
            .agent_state(
                AgentState::builder()
                    .user_id("alice")
                    .session_id("session-1")
                    .build(),
            )
            .build();

        rt().block_on(agent.call(vec![Msg::user("hi")]))
            .expect("call succeeds");

        let requests = transport.requests();
        let request = requests.first().expect("the send request");
        let params: crate::model::MessageSendParams =
            serde_json::from_value(request.params.clone().unwrap()).unwrap();
        assert_eq!(params.message.context_id.as_deref(), Some("session-1"));
        assert_eq!(
            params.message.metadata.unwrap()
                [crate::message::MessageConstants::USER_ID_METADATA_KEY],
            serde_json::json!("alice")
        );
    }

    // Java: A2aAgent#doCall blocks until the task is terminal — Rust polls `tasks/get`.
    #[test]
    fn a_working_task_is_settled_by_polling() {
        let transport = ScriptedTransport::new(
            working_task(),
            vec![completed_task_reply(None, Some("late answer"))],
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = agent_with(transport.clone());

        let reply = rt()
            .block_on(agent.call(vec![Msg::user("hi")]))
            .expect("call succeeds");
        assert_eq!(reply.text_content(), "late answer");
        assert_eq!(
            transport.methods(),
            vec![
                methods::MESSAGE_SEND.to_string(),
                methods::TASKS_GET.to_string()
            ]
        );
        assert!(agent.running_call_ids().is_empty());
    }

    // Java: `interrupt()` cancels the running call's remote task and reports the hint message.
    #[test]
    fn interrupt_cancels_the_running_task() {
        let transport = ScriptedTransport::new(
            working_task(),
            vec![working_task()],
            JsonRpcResponse::success(
                serde_json::to_value(Task::new("t-1", TaskStatus::new(TaskState::Canceled)))
                    .unwrap(),
                Some(2.into()),
            ),
        );
        let agent = Arc::new(
            A2aAgent::builder("remote", resolver())
                .config(
                    A2aAgentConfig::builder()
                        .transport(transport.clone())
                        .poll_attempts(500)
                        .poll_interval(Duration::from_millis(1))
                        .build(),
                )
                .build(),
        );
        let interrupting = agent.clone();
        let runtime = rt();
        let handle = runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            interrupting.interrupt();
        });

        let reply = runtime
            .block_on(agent.call(vec![Msg::user("hi")]))
            .expect("call succeeds");
        runtime.block_on(handle).unwrap();

        assert_eq!(reply.text_content(), "Task t-1 interrupt successfully.");
        assert!(transport
            .methods()
            .contains(&methods::TASKS_CANCEL.to_string()));
    }

    // Java: `1524b3191` — a card that cannot be resolved fails the call immediately.
    #[test]
    fn an_unresolvable_card_fails_fast_and_names_the_agent() {
        let transport = ScriptedTransport::new(
            completed_task_reply(None, Some("never reached")),
            Vec::new(),
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = A2aAgent::builder("missing", Arc::new(ScriptedResolver { card: Err(()) }))
            .config(
                A2aAgentConfig::builder()
                    .transport(transport.clone())
                    .build(),
            )
            .build();

        let error = rt()
            .block_on(agent.call(vec![Msg::user("hi")]))
            .expect_err("the call fails");
        let text = error.to_string();
        assert!(text.contains("missing"), "error names the agent: {text}");
        assert!(
            text.contains("ScriptedResolver"),
            "error names the resolver: {text}"
        );
        assert!(
            transport.methods().is_empty(),
            "nothing is sent when the card is missing"
        );
    }

    #[test]
    fn structured_output_is_reported_as_unsupported() {
        let transport = ScriptedTransport::new(
            completed_task_reply(None, Some("unused")),
            Vec::new(),
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = agent_with(transport);
        let schema = JsonSchema::builder().build();

        let error = rt()
            .block_on(agent.call_with_schema(vec![Msg::user("hi")], schema))
            .expect_err("structured output is refused");
        assert!(error.to_string().contains("structured output"));
    }

    #[test]
    fn the_toolkit_of_a_remote_agent_is_empty() {
        let transport = ScriptedTransport::new(
            completed_task_reply(None, Some("unused")),
            Vec::new(),
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = agent_with(transport);
        assert!(Agent::toolkit(&agent).tool_names().is_empty());
    }

    /// Transport that answers `message/stream` with the scripted frames.
    struct StreamTransport {
        frames: Mutex<Vec<JsonRpcResponse>>,
        methods: Mutex<Vec<String>>,
    }

    impl StreamTransport {
        fn new(frames: Vec<JsonRpcResponse>) -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(frames),
                methods: Mutex::new(Vec::new()),
            })
        }
    }

    impl JsonRpcTransport for StreamTransport {
        fn send(
            &self,
            _url: &str,
            _request: &JsonRpcRequest,
            _headers: &BTreeMap<String, String>,
        ) -> Pin<Box<dyn Future<Output = Result<JsonRpcResponse, A2aError>> + Send + '_>> {
            unreachable!("only the streaming method is used")
        }

        fn send_stream(
            &self,
            _url: String,
            request: JsonRpcRequest,
            _headers: BTreeMap<String, String>,
        ) -> futures::stream::BoxStream<'static, Result<JsonRpcResponse, A2aError>> {
            self.methods.lock().unwrap().push(request.method.clone());
            let frames = std::mem::take(&mut *self.frames.lock().unwrap());
            Box::pin(futures::stream::iter(frames.into_iter().map(Ok)))
        }
    }

    fn artifact_update(text: &str) -> JsonRpcResponse {
        let artifact = Artifact::new("artifact-1", vec![Part::text(text)]);
        let mut update = crate::model::TaskArtifactUpdateEvent::new("t-1", artifact);
        // Streamed chunks of one block arrive as appended parts, like a real A2A server does.
        update.append = Some(true);
        JsonRpcResponse::success(serde_json::to_value(&update).unwrap(), Some(1.into()))
    }

    fn completed_update() -> JsonRpcResponse {
        let update = crate::model::TaskStatusUpdateEvent::new(
            "t-1",
            TaskStatus::new(TaskState::Completed),
            true,
        );
        JsonRpcResponse::success(serde_json::to_value(&update).unwrap(), Some(2.into()))
    }

    // Java: A2aAgent#streamEvents — deltas are relayed, then `AgentResult` carries the reply.
    #[test]
    fn stream_events_relays_deltas_and_the_terminal_result() {
        use futures::StreamExt;

        let transport = StreamTransport::new(vec![
            artifact_update("Hel"),
            artifact_update("lo"),
            completed_update(),
        ]);
        let agent = A2aAgent::builder("remote", resolver())
            .config(
                A2aAgentConfig::builder()
                    .transport(transport.clone())
                    .build(),
            )
            .build();

        // Subscribers see exactly what the stream emits (Java `ClientEventContext#emit`).
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        agent.subscribe(Box::new(move |event| {
            recorder.lock().unwrap().push(event.event_type());
        }));

        let events = rt().block_on(async {
            let mut stream = agent.stream_events(vec![Msg::user("hi")]);
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event.unwrap());
            }
            events
        });

        let kinds: Vec<_> = events.iter().map(|event| event.event_type()).collect();
        assert_eq!(
            kinds,
            vec![
                agentscope_core::event::AgentEventType::AgentStart,
                agentscope_core::event::AgentEventType::TextBlockDelta,
                agentscope_core::event::AgentEventType::TextBlockDelta,
                agentscope_core::event::AgentEventType::AgentResult,
                agentscope_core::event::AgentEventType::AgentEnd,
            ],
            "{:?}",
            events
        );
        assert_eq!(events[1].data().fields["delta"], "Hel");
        assert_eq!(events[2].data().fields["delta"], "lo");

        // The terminal result is the reply the accumulator derived from the artifacts.
        let reply: Msg = serde_json::from_value(events[3].data().fields["reply"].clone()).unwrap();
        assert_eq!(reply.text_content(), "Hello");

        // The stream went through `message/stream` and kept the reply in the history.
        assert_eq!(
            transport.methods.lock().unwrap().as_slice(),
            [crate::model::methods::MESSAGE_STREAM]
        );
        let history = agent.history();
        assert_eq!(history.last().unwrap().text_content(), "Hello");

        // Every emitted event reached the sink.
        assert_eq!(*seen.lock().unwrap(), kinds);
    }

    /// Wraps `inner`, recording `<point>:<label>:out` once the stream is exhausted.
    ///
    /// `Middleware` has no "after" callback, so a middleware that wants to observe the leave leg of
    /// the onion wraps the stream it got from `next` and notes its end.
    fn record_leave(
        seen: Arc<Mutex<Vec<String>>>,
        label: &'static str,
        point: &'static str,
        inner: EventStream,
    ) -> EventStream {
        struct State {
            inner: EventStream,
            seen: Arc<Mutex<Vec<String>>>,
            label: &'static str,
            point: &'static str,
        }
        Box::pin(futures::stream::unfold(
            State {
                inner,
                seen,
                label,
                point,
            },
            |mut state| async move {
                match state.inner.next().await {
                    Some(item) => Some((item, state)),
                    None => {
                        state
                            .seen
                            .lock()
                            .unwrap()
                            .push(format!("{}:{}:out", state.point, state.label));
                        None
                    }
                }
            },
        ))
    }

    /// Middleware that records when it enters and leaves the agent and reasoning points.
    struct OrderRecorder {
        label: &'static str,
        order: i32,
        points: Vec<ExtensionPoint>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl OrderRecorder {
        fn new(
            label: &'static str,
            order: i32,
            points: &[ExtensionPoint],
            seen: Arc<Mutex<Vec<String>>>,
        ) -> Self {
            Self {
                label,
                order,
                points: points.to_vec(),
                seen,
            }
        }
    }

    impl Middleware for OrderRecorder {
        fn order(&self) -> i32 {
            self.order
        }

        fn active_points(&self) -> Vec<ExtensionPoint> {
            self.points.clone()
        }

        fn on_agent(
            &self,
            _ctx: &RuntimeContext,
            input: AgentInput,
            next: Next<AgentInput>,
        ) -> EventStream {
            self.seen
                .lock()
                .unwrap()
                .push(format!("agent:{}:in", self.label));
            record_leave(self.seen.clone(), self.label, "agent", next(input))
        }

        fn on_reasoning(
            &self,
            _ctx: &RuntimeContext,
            input: ReasoningInput,
            next: Next<ReasoningInput>,
        ) -> EventStream {
            self.seen
                .lock()
                .unwrap()
                .push(format!("reasoning:{}:in", self.label));
            record_leave(self.seen.clone(), self.label, "reasoning", next(input))
        }
    }

    /// Middleware recording the message `on_reasoning_result` is notified with (Java hook).
    struct ReasoningResultRecorder {
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Middleware for ReasoningResultRecorder {
        async fn on_reasoning_result(&self, _ctx: &RuntimeContext, message: &Msg) {
            self.seen.lock().unwrap().push(message.text_content());
        }

        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnReasoningResult]
        }
    }

    // Java: `8cc6795b0` — `agentMiddlewares` (`onAgent`) wrap the whole call, `reasoningMiddlewares`
    // (`onReasoning`) wrap the exchange with the remote agent, and the sorted hooks get the
    // post-reasoning message.
    #[test]
    fn middlewares_wrap_the_whole_call_and_the_remote_exchange() {
        let transport = StreamTransport::new(vec![
            artifact_update("Hel"),
            artifact_update("lo"),
            completed_update(),
        ]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(Vec::new()));
        let agent = A2aAgent::builder("remote", resolver())
            .config(
                A2aAgentConfig::builder()
                    .transport(transport.clone())
                    .build(),
            )
            .middlewares(vec![
                Arc::new(OrderRecorder::new(
                    "outer",
                    10,
                    &[ExtensionPoint::OnAgent, ExtensionPoint::OnReasoning],
                    seen.clone(),
                )),
                Arc::new(OrderRecorder::new(
                    "agent-only",
                    7,
                    &[ExtensionPoint::OnAgent],
                    seen.clone(),
                )),
                Arc::new(OrderRecorder::new(
                    "inner",
                    5,
                    &[ExtensionPoint::OnAgent, ExtensionPoint::OnReasoning],
                    seen.clone(),
                )),
                Arc::new(ReasoningResultRecorder {
                    seen: replies.clone(),
                }),
            ])
            .build();

        let events = rt().block_on(async {
            let mut stream = agent.stream_events(vec![Msg::user("hi")]);
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event.unwrap());
            }
            events
        });
        assert!(
            matches!(events.last(), Some(AgentEvent::AgentEnd(_))),
            "{events:?}"
        );

        // Highest `order()` enters first and leaves last; `agent-only` never reaches reasoning.
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "agent:outer:in",
                "agent:agent-only:in",
                "agent:inner:in",
                "reasoning:outer:in",
                "reasoning:inner:in",
                "reasoning:inner:out",
                "reasoning:outer:out",
                "agent:inner:out",
                "agent:agent-only:out",
                "agent:outer:out",
            ]
        );
        // The post-reasoning notification carries the reply of the remote agent.
        assert_eq!(replies.lock().unwrap().clone(), vec!["Hello".to_string()]);
        assert_eq!(
            transport.methods.lock().unwrap().as_slice(),
            [crate::model::methods::MESSAGE_STREAM]
        );
    }
}
