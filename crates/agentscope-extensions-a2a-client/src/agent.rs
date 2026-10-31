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
//!   Java does through its session memory.
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

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::message::Msg;
use agentscope_core::state::AgentState;
use agentscope_core::tool::Toolkit;
use agentscope_core::{Agent, CallableAgent, Error, JsonSchema, Result as CoreResult};

use crate::card::AgentCardResolver;
use crate::client::{A2aClient, JsonRpcTransport, ReqwestJsonRpcTransport, SendResult};
use crate::error::A2aError;
use crate::message::MessageConvert;
use crate::model::{MessageSendParams, Task, TaskIdParams, TaskQueryParams};
use crate::result::task_result_message;

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
            .field("auth_headers", &self.auth_headers.keys().collect::<Vec<_>>())
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
/// one at a time (Java serializes them per session), and the conversation history is kept in the
/// agent state.
pub struct A2aAgent {
    name: String,
    description: String,
    resolver: Arc<dyn AgentCardResolver>,
    config: A2aAgentConfig,
    state: Mutex<AgentState>,
    toolkit: Toolkit,
    running: Arc<Mutex<Vec<Arc<InFlightCall>>>>,
    /// Serializes calls of this agent, mirroring Java `callSerializationKey(session)`.
    ///
    /// An async mutex because the guard is held across the request's awaits and
    /// [`CallableAgent::call`] must stay `Send`.
    call_gate: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for A2aAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aAgent")
            .field("name", &self.name)
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
        &self.name
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
        self.client().agent_card().await
    }

    /// The current history of the agent state (Java `historyOf(session)`).
    pub fn history(&self) -> Vec<Msg> {
        self.state
            .lock()
            .map(|state| state.context.clone())
            .unwrap_or_default()
    }

    /// The running call ids, sorted for stable assertions.
    pub fn running_call_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .running
            .lock()
            .map(|running| running.iter().map(|call| call.request_id.clone()).collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// Sends `messages` with `message/send` and returns the terminal reply.
    ///
    /// Mirrors Java `A2aAgent#doCall`: history plus the new input are sent, and the reply is
    /// appended to the history.
    pub async fn exchange(&self, messages: Vec<Msg>) -> Result<Msg, A2aError> {
        // Java serializes calls that share a session: they share the history.
        let _gate = self.call_gate.lock().await;
        self.append_history(&messages);
        let history = self.history();
        let client = self.client();
        let request_message = MessageConvert::convert_from_msgs(&history);
        let params = MessageSendParams {
            message: request_message,
            configuration: None,
            metadata: None,
        };
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

    /// Sends `messages` over `message/stream` and returns the terminal reply.
    ///
    /// Mirrors Java `A2aAgent#streamEvents` result handling: the reply is the one
    /// [`TaskResultAccumulator`](crate::result::TaskResultAccumulator) derives from the events.
    pub async fn exchange_stream(&self, messages: Vec<Msg>) -> Result<Msg, A2aError> {
        let _gate = self.call_gate.lock().await;
        self.append_history(&messages);
        let history = self.history();
        let client = self.client();
        let params = MessageSendParams {
            message: MessageConvert::convert_from_msgs(&history),
            configuration: None,
            metadata: None,
        };
        let events = client.message_stream(params).await?;
        let mut accumulator = crate::result::TaskResultAccumulator::new();
        for event in &events {
            if let Some(reply) = accumulator.observe(event, &self.name) {
                self.append_history(std::slice::from_ref(&reply));
                return Ok(reply);
            }
        }
        // A stream that carried a plain message reply ends with that message.
        if let Some(message) = events.iter().find_map(|event| match event {
            crate::model::StreamEvent::Message(message) => Some(message),
            _ => None,
        }) {
            let reply = MessageConvert::convert_from_message(message, &self.name);
            self.append_history(std::slice::from_ref(&reply));
            return Ok(reply);
        }
        Err(A2aError::InvalidRequest(
            "a2a stream ended without a terminal result".to_string(),
        ))
    }

    /// Cancels the running call's remote task, mirroring Java `cancelRemoteTask`.
    ///
    /// Returns `None` when no call carries a task id yet.
    pub async fn cancel_remote_task(&self) -> Result<Option<Msg>, A2aError> {
        let task_id = self
            .running
            .lock()
            .ok()
            .and_then(|running| running.iter().find_map(|call| call.task_id()));
        let Some(task_id) = task_id else {
            return Ok(None);
        };
        Ok(Some(self.cancel_task(&self.client(), &task_id).await?))
    }

    /// Builds a client for one call: the card is resolved when the client is used, i.e. once per
    /// call (Java `startCall` -> `Mono.using`).
    fn client(&self) -> A2aClient {
        A2aClient::new(self.name.clone(), self.resolver.clone())
            .with_transport(self.config.transport.clone())
            .with_auth_headers(self.config.auth_headers.clone())
    }

    fn append_history(&self, messages: &[Msg]) {
        if messages.is_empty() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.context.extend(messages.iter().cloned());
        }
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

impl Agent for A2aAgent {
    fn agent_id(&self) -> &str {
        &self.name
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn interrupt(&self) {
        if let Ok(running) = self.running.lock() {
            for call in running.iter() {
                call.cancel();
            }
        }
    }

    fn agent_state(&self) -> AgentState {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    fn toolkit(&self) -> &Toolkit {
        &self.toolkit
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

    /// Builds the agent.
    pub fn build(self) -> A2aAgent {
        A2aAgent {
            name: self.name,
            description: self.description,
            resolver: self.resolver,
            config: self.config,
            state: Mutex::new(self.state),
            toolkit: Toolkit::new(),
            running: Arc::new(Mutex::new(Vec::new())),
            call_gate: tokio::sync::Mutex::new(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

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
                send_reply,
                get_replies: Mutex::new(get_replies),
                cancel_reply,
            })
        }

        fn methods(&self) -> Vec<String> {
            self.methods.lock().unwrap().clone()
        }

        fn next_get(&self) -> JsonRpcResponse {
            let mut replies = self.get_replies.lock().unwrap();
            if replies.len() > 1 {
                replies.remove(0)
            } else {
                replies.first().cloned().unwrap_or_else(|| self.send_reply.clone())
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
            let response = match request.method.as_str() {
                crate::model::methods::TASKS_GET => self.next_get(),
                crate::model::methods::TASKS_CANCEL => self.cancel_reply.clone(),
                _ => self.send_reply.clone(),
            };
            Box::pin(async move { Ok(response) })
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    fn completed_task_reply(artifacts: Option<Vec<Artifact>>, message: Option<&str>) -> JsonRpcResponse {
        let mut status = TaskStatus::new(TaskState::Completed);
        status.message = message.map(|text| {
            Message::new("m-status", MessageRole::Agent, vec![Part::text(text)])
        });
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
                Some(vec![Artifact::new("art-1", vec![Part::text("real answer")])]),
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
        assert!(transport.methods().contains(&methods::TASKS_CANCEL.to_string()));
    }

    // Java: `1524b3191` — a card that cannot be resolved fails the call immediately.
    #[test]
    fn an_unresolvable_card_fails_fast_and_names_the_agent() {
        let transport = ScriptedTransport::new(
            completed_task_reply(None, Some("never reached")),
            Vec::new(),
            JsonRpcResponse::success(serde_json::Value::Null, Some(2.into())),
        );
        let agent = A2aAgent::builder(
            "missing",
            Arc::new(ScriptedResolver { card: Err(()) }),
        )
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
        assert!(text.contains("ScriptedResolver"), "error names the resolver: {text}");
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
}
