//! A2A server side: agent runner contract, message conversion and the JSON-RPC request
//! handler.
//!
//! Mirrors `io.agentscope.core.a2a.server`:
//!
//! * [`AgentRunner`] / [`AgentRequestOptions`] - `executor.runner`
//! * [`runner::BaseReActAgentRunner`] / [`runner::ReActAgentWithBuilderRunner`] -
//!   `executor.runner.{BaseReActAgentRunner,ReActAgentWithBuilderRunner}`
//! * [`executor::AgentExecuteProperties`] - `executor.AgentExecuteProperties`
//! * [`ServerMessageConvert`] - `utils.MessageConvertUtil`
//! * [`A2aTaskRegistry`] - `tasks` (`InMemoryTaskStore`)
//! * [`A2aRequestHandler`] - `request.AgentScopeA2aRequestHandler`
//! * [`registry::AgentRegistry`] / [`registry::AgentRegistryService`] - `registry`
//! * [`transport`] - `transport.*` (`TransportWrapper`, `DeploymentProperties`, JSON-RPC)
//! * [`server::AgentScopeA2aServer`] - `AgentScopeA2aServer`
//!
//! The Java implementation delegates to the `a2a-java-sdk` server stack
//! (`DefaultRequestHandler`, `InMemoryQueueManager`, `BasePushNotificationSender`). Those are
//! SDK internals with no Rust counterpart, so this module provides the equivalent protocol
//! surface: JSON-RPC dispatch over a task store plus the AgentScope message conversion that
//! the SDK-agnostic layer actually contributes.

pub mod card;
pub mod executor;
pub mod registry;
pub mod runner;
pub mod server;
pub mod transport;

pub use executor::{AgentExecuteProperties, AgentOutputProjector};
pub use registry::{AgentRegistry, AgentRegistryService, RegistrationReport};
pub use runner::{
    BaseReActAgentRunner, ReActAgentBuilderFactory, ReActAgentWithBuilderRunner, TaskAgent,
    TaskAgentFactory,
};
pub use server::{A2aServerBuildError, AgentScopeA2aServer, AgentScopeA2aServerBuilder};
pub use transport::{
    DeploymentProperties, JsonRpcTransportMetadata, JsonRpcTransportWrapper,
    JsonRpcTransportWrapperBuilder, TransportHeaders, TransportMetadata, TransportResponse,
    TransportWrapper, TransportWrapperBuilder,
};

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use agentscope_core::event::AgentEvent;
use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use futures::stream::BoxStream;

use agentscope_extensions_a2a_client::client::SendResult;
use agentscope_extensions_a2a_client::error::A2aError;
use agentscope_extensions_a2a_client::message::{content_block_to_part, MessageConstants};
use agentscope_extensions_a2a_client::model::{
    methods, AgentCard, Artifact, JsonRpcError, JsonRpcRequest, JsonRpcResponse, Message,
    MessageRole, MessageSendParams, Part, Task, TaskIdParams, TaskQueryParams, TaskState,
    TaskStatus,
};

/// Server-side context attribute keys, mirroring Java
/// `io.agentscope.core.a2a.server.constants.A2aServerConstants`.
pub mod server_constants {
    /// Context key marking a streaming request.
    pub const IS_STREAM_KEY: &str = "isStream";
}

/// Per-request options handed to an [`AgentRunner`].
///
/// Mirrors Java `io.agentscope.core.a2a.server.executor.runner.AgentRequestOptions`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRequestOptions {
    task_id: Option<String>,
    session_id: Option<String>,
    user_id: Option<String>,
}

impl AgentRequestOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    pub fn set_task_id(&mut self, task_id: impl Into<String>) {
        self.task_id = Some(task_id.into());
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn set_session_id(&mut self, session_id: impl Into<String>) {
        self.session_id = Some(session_id.into());
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    pub fn set_user_id(&mut self, user_id: impl Into<String>) {
        self.user_id = Some(user_id.into());
    }

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.set_task_id(task_id);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.set_session_id(session_id);
        self
    }

    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.set_user_id(user_id);
        self
    }
}

/// Runs an AgentScope agent on behalf of the A2A server.
///
/// Mirrors Java `io.agentscope.core.a2a.server.executor.runner.AgentRunner`: `Msg` in,
/// [`AgentEvent`] out. Java returns a `Flux`; the Rust contract returns the events collected
/// for one request, which callers can re-stream if they need incremental delivery.
#[async_trait::async_trait]
pub trait AgentRunner: Send + Sync {
    /// Agent name, used as the agent card's `name` default.
    fn agent_name(&self) -> String;

    /// Agent description, used as the agent card's `description` default.
    fn agent_description(&self) -> String;

    /// Executes one request and returns the emitted events in order.
    async fn stream_events(
        &self,
        request_messages: Vec<Msg>,
        options: AgentRequestOptions,
    ) -> Result<Vec<AgentEvent>, A2aError>;

    /// Stops a running task, mirroring Java `AgentRunner#stop(String)`.
    fn stop(&self, task_id: &str);

    /// Optionally executes one request as an incremental event stream.
    ///
    /// [`stream_events`](Self::stream_events) is the message-level contract and buffers the
    /// whole run before returning, which makes `message/stream` degrade to one final chunk.
    /// Runners backed by a streaming agent (a core `ReActAgent`, for instance) implement this to
    /// let the A2A server relay each `AgentEvent` as it happens, mirroring Java `AgentRunner`
    /// returning a `Flux<AgentEvent>`.
    ///
    /// The default returns `None`, and the server then replays the buffered
    /// [`stream_events`](Self::stream_events) result.
    fn stream_events_incremental(
        &self,
        request_messages: Vec<Msg>,
        options: AgentRequestOptions,
    ) -> Option<BoxStream<'static, Result<AgentEvent, A2aError>>> {
        let _ = (request_messages, options);
        None
    }
}

/// Any [`AgentRunner`] can describe the agent for agent-card defaults.
impl<T: AgentRunner + ?Sized> crate::card::AgentDescriptor for T {
    fn agent_name(&self) -> String {
        AgentRunner::agent_name(self)
    }

    fn agent_description(&self) -> String {
        AgentRunner::agent_description(self)
    }
}

/// Private adapter so a `&dyn AgentRunner` can be passed where a
/// `&dyn AgentDescriptor` is expected (trait-object upcasting through a blanket impl
/// is not performed implicitly).
struct RunnerDescriptor<'a>(&'a dyn AgentRunner);

impl crate::card::AgentDescriptor for RunnerDescriptor<'_> {
    fn agent_name(&self) -> String {
        self.0.agent_name()
    }

    fn agent_description(&self) -> String {
        self.0.agent_description()
    }
}

/// In-memory task store, mirroring the `InMemoryTaskStore` used by
/// `AgentScopeA2aRequestHandler.Builder` when no store is supplied.
#[derive(Debug, Default)]
pub struct A2aTaskRegistry {
    tasks: Mutex<HashMap<String, Task>>,
}

impl A2aTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores or replaces a task.
    pub fn put(&self, task: Task) {
        self.tasks.lock().unwrap().insert(task.id.clone(), task);
    }

    /// Fetches a task by id.
    pub fn get(&self, task_id: &str) -> Option<Task> {
        self.tasks.lock().unwrap().get(task_id).cloned()
    }

    /// Replaces a task's status, returning the updated task.
    pub fn update_status(&self, task_id: &str, status: TaskStatus) -> Option<Task> {
        let mut guard = self.tasks.lock().unwrap();
        let task = guard.get_mut(task_id)?;
        task.status = status;
        Some(task.clone())
    }

    /// Appends an artifact, returning the updated task.
    pub fn add_artifact(&self, task_id: &str, artifact: Artifact) -> Option<Task> {
        let mut guard = self.tasks.lock().unwrap();
        let task = guard.get_mut(task_id)?;
        task.artifacts.get_or_insert_with(Vec::new).push(artifact);
        Some(task.clone())
    }

    /// Cancels a task, mirroring `TaskUpdater#cancel` (never downgrades a terminal task).
    pub fn cancel(&self, task_id: &str) -> Option<Task> {
        let mut guard = self.tasks.lock().unwrap();
        let task = guard.get_mut(task_id)?;
        if !task.status.state.is_terminal() {
            task.status = TaskStatus::new(TaskState::Canceled);
        }
        Some(task.clone())
    }

    /// Number of tracked tasks.
    pub fn len(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    /// Whether the registry holds no tasks.
    pub fn is_empty(&self) -> bool {
        self.tasks.lock().unwrap().is_empty()
    }
}

/// AgentScope message conversion used by the A2A server.
///
/// Mirrors `io.agentscope.core.a2a.server.utils.MessageConvertUtil`.
pub struct ServerMessageConvert;

impl ServerMessageConvert {
    /// Converts `msgs` into one A2A message tagged with `taskId` / `contextId`.
    pub fn convert_from_msgs_to_message(msgs: &[Msg], task_id: &str, context_id: &str) -> Message {
        let mut message =
            agentscope_extensions_a2a_client::message::MessageConvert::convert_from_msgs(msgs);
        message.task_id = Some(task_id.to_string());
        message.context_id = Some(context_id.to_string());
        message
    }

    /// Converts a single [`Msg`] into an A2A message with role `agent`.
    ///
    /// Mirrors Java `convertFromMsgToMessage(Msg, String, String)`: per-message metadata is
    /// nested under the message id and the role is always `agent`.
    pub fn convert_from_msg_to_message(msg: &Msg, task_id: &str, context_id: &str) -> Message {
        let mut metadata = serde_json::Map::new();
        if !msg.metadata.is_empty() {
            metadata.insert(
                msg.id.clone(),
                serde_json::Value::Object(msg.metadata.clone()),
            );
        }
        let mut message = Message::new(
            uuid::Uuid::new_v4().to_string(),
            MessageRole::Agent,
            Self::convert_from_content_blocks(msg, false),
        );
        message.metadata = Some(metadata);
        message.task_id = Some(task_id.to_string());
        message.context_id = Some(context_id.to_string());
        message
    }

    /// Converts a [`Msg`]'s content blocks into A2A parts, stamping source metadata.
    ///
    /// Mirrors Java `convertFromContentBlocks(Msg, boolean)`.
    pub fn convert_from_content_blocks(msg: &Msg, streaming_chunk: bool) -> Vec<Part> {
        let mut parts = Vec::new();
        for block in &msg.content {
            let Some(mut part) = content_block_to_part(block) else {
                continue;
            };
            let metadata = part_metadata_mut(&mut part);
            metadata.insert(
                MessageConstants::MSG_ID_METADATA_KEY.to_string(),
                serde_json::Value::String(msg.id.clone()),
            );
            metadata.insert(
                MessageConstants::SOURCE_NAME_METADATA_KEY.to_string(),
                serde_json::Value::String(msg.name.clone().unwrap_or_default()),
            );
            if streaming_chunk {
                metadata.insert(
                    MessageConstants::STREAM_CHUNK_METADATA_KEY.to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            parts.push(part);
        }
        parts
    }

    /// Merges consecutive messages that share id, name and role, concatenating adjacent
    /// text/thinking blocks.
    ///
    /// Mirrors Java `compactStreamingChunks`: used by executor fallback paths that
    /// accumulate stream deltas, never for user-supplied message lists.
    pub fn compact_streaming_chunks(msgs: &[Msg]) -> Vec<Msg> {
        let mut result: Vec<Msg> = Vec::new();
        for msg in msgs {
            match result.last_mut() {
                Some(current) if can_merge_streaming_chunk(current, msg) => {
                    let merged = merge_content(&current.content, &msg.content);
                    *current = current.with_content(merged);
                }
                _ => result.push(msg.clone()),
            }
        }
        result
    }

    /// Converts an A2A message back into [`Msg`]s.
    ///
    /// Mirrors Java `convertFromMessageToMsgs`: parts are regrouped by the AgentScope
    /// message id carried in their metadata (a random id per part when absent), preserving
    /// first-seen order, and per-message metadata is restored from the message's nested
    /// metadata map.
    pub fn convert_from_message_to_msgs(message: &Message) -> Vec<Msg> {
        let mut order: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut parts_by_id: HashMap<String, Vec<ContentBlock>> = HashMap::new();
        let mut name_by_id: HashMap<String, String> = HashMap::new();
        let mut role_by_id: HashMap<String, MsgRole> = HashMap::new();

        for part in &message.parts {
            let msg_id = msg_id_of(part);
            if seen.insert(msg_id.clone()) {
                order.push(msg_id.clone());
                parts_by_id.insert(msg_id.clone(), Vec::new());
            }
            let block = agentscope_extensions_a2a_client::message::part_to_content_block(part);
            parts_by_id.entry(msg_id.clone()).or_default().extend(block);
            if let Some(name) = MessageConstants::metadata_value(
                part_metadata(part),
                MessageConstants::SOURCE_NAME_METADATA_KEY,
            ) {
                name_by_id.insert(msg_id.clone(), name);
            }
            if let Some(role) = MessageConstants::metadata_value(
                part_metadata(part),
                MessageConstants::MSG_ROLE_METADATA_KEY,
            )
            .and_then(|raw| parse_msg_role(&raw))
            {
                role_by_id.entry(msg_id.clone()).or_insert(role);
            }
        }

        order
            .into_iter()
            .map(|msg_id| {
                let content = parts_by_id.remove(&msg_id).unwrap_or_default();
                let fallback = convert_role(message.role);
                let role = role_by_id.get(&msg_id).copied().unwrap_or(fallback);
                let mut builder = Msg::builder()
                    .id(msg_id.clone())
                    .role(validated_role(role, &content))
                    .content(content)
                    .metadata(msg_metadata(message, &msg_id));
                if let Some(name) = name_by_id.get(&msg_id) {
                    builder = builder.name(name.clone());
                }
                builder.build()
            })
            .collect()
    }
}

/// Java `getMsgId`: the part's stamped message id, else a fresh random one.
fn msg_id_of(part: &Part) -> String {
    MessageConstants::metadata_value(part_metadata(part), MessageConstants::MSG_ID_METADATA_KEY)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Java `getMsgRole`: the part's stamped role name, if it parses as a [`MsgRole`].
fn parse_msg_role(raw: &str) -> Option<MsgRole> {
    match raw.to_ascii_lowercase().as_str() {
        "user" => Some(MsgRole::User),
        "assistant" => Some(MsgRole::Assistant),
        "system" => Some(MsgRole::System),
        "tool" => Some(MsgRole::Tool),
        _ => None,
    }
}

/// Java `convertRole`: A2A `agent` maps to `assistant`, everything else to `user`.
fn convert_role(role: MessageRole) -> MsgRole {
    match role {
        MessageRole::Agent => MsgRole::Assistant,
        MessageRole::User => MsgRole::User,
    }
}

/// Java `getMsgMetadata`: the nested per-message metadata, when it is a JSON object.
fn msg_metadata(message: &Message, msg_id: &str) -> agentscope_core::message::JsonMap {
    message
        .metadata
        .as_ref()
        .and_then(|m| m.get(msg_id))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Guards against constructing a [`Msg`] that `agentscope-core` would reject.
///
/// `agentscope_core::message::Msg::new` panics when a `user` message carries thinking/hint/
/// tool blocks and when a `system` message carries anything but text. A2A messages coming
/// from a non-AgentScope peer have no role metadata and may legitimately contain such parts
/// under `role: user`, so the role is promoted to `assistant` in that case instead of
/// panicking. Recorded in the module tracker as a deliberate deviation.
fn validated_role(role: MsgRole, content: &[ContentBlock]) -> MsgRole {
    match role {
        MsgRole::User => {
            if content.iter().all(|block| {
                matches!(
                    block,
                    ContentBlock::Text(_)
                        | ContentBlock::Data(_)
                        | ContentBlock::Image(_)
                        | ContentBlock::Audio(_)
                        | ContentBlock::Video(_)
                )
            }) {
                MsgRole::User
            } else {
                MsgRole::Assistant
            }
        }
        MsgRole::System => {
            if content.iter().all(|b| matches!(b, ContentBlock::Text(_))) {
                MsgRole::System
            } else {
                MsgRole::Assistant
            }
        }
        other => other,
    }
}

/// Java `canMergeStreamingChunk`.
fn can_merge_streaming_chunk(current: &Msg, next: &Msg) -> bool {
    !current.id.is_empty()
        && current.id == next.id
        && current.name == next.name
        && current.role == next.role
}

/// Java `mergeContent` + `appendMergedBlock`: concatenates adjacent same-kind text blocks.
fn merge_content(first: &[ContentBlock], second: &[ContentBlock]) -> Vec<ContentBlock> {
    let mut merged = first.to_vec();
    for block in second {
        match (merged.last_mut(), block) {
            (Some(ContentBlock::Text(previous)), ContentBlock::Text(next)) => {
                previous.text.push_str(&next.text);
            }
            (Some(ContentBlock::Thinking(previous)), ContentBlock::Thinking(next)) => {
                previous.thinking.push_str(&next.thinking);
            }
            _ => merged.push(block.clone()),
        }
    }
    merged
}

/// Read-only metadata of any part variant.
fn part_metadata(part: &Part) -> Option<&agentscope_core::message::JsonMap> {
    match part {
        Part::Text(p) => p.metadata.as_ref(),
        Part::File(p) => p.metadata.as_ref(),
        Part::Data(p) => p.metadata.as_ref(),
    }
}

/// Mutable metadata of any part variant, creating the map when absent.
fn part_metadata_mut(part: &mut Part) -> &mut agentscope_core::message::JsonMap {
    let slot = match part {
        Part::Text(p) => &mut p.metadata,
        Part::File(p) => &mut p.metadata,
        Part::Data(p) => &mut p.metadata,
    };
    slot.get_or_insert_with(agentscope_core::message::JsonMap::new)
}

/// JSON-RPC request handler for an A2A server.
///
/// Mirrors `io.agentscope.core.a2a.server.request.AgentScopeA2aRequestHandler`, which wraps
/// the SDK's `DefaultRequestHandler` with an `AgentExecutor`, an in-memory task store and a
/// queue manager. This handler implements the same dispatch table over [`AgentRunner`], with
/// no background queues so that tests stay deterministic:
///
/// * `message/send` - runs the agent to completion, records the artifacts, returns the task
/// * `message/stream` - relays the agent's incremental output as status/artifact events
/// * `tasks/get` / `tasks/cancel` - task store lookups
/// * `agent/getAuthenticatedExtendedCard` - returns the served card
///
/// Output assembly follows [`AgentExecuteProperties`]: which agent events become visible
/// [`Msg`]s is decided by [`AgentOutputProjector`], and a non-blocking run only completes with a
/// message when `complete_with_message` is set (a blocking `message/send` always does, as in
/// Java).
pub struct A2aRequestHandler {
    runner: Arc<dyn AgentRunner>,
    registry: Arc<A2aTaskRegistry>,
    agent_card: AgentCard,
    execute_properties: AgentExecuteProperties,
}

impl A2aRequestHandler {
    /// Creates a handler serving `runner` with the given agent card and default execution
    /// properties.
    pub fn new(
        runner: Arc<dyn AgentRunner>,
        agent_card: AgentCard,
        registry: Arc<A2aTaskRegistry>,
    ) -> Self {
        Self::new_with_properties(
            runner,
            agent_card,
            registry,
            AgentExecuteProperties::default(),
        )
    }

    /// Creates a handler with explicit [`AgentExecuteProperties`].
    pub fn new_with_properties(
        runner: Arc<dyn AgentRunner>,
        agent_card: AgentCard,
        registry: Arc<A2aTaskRegistry>,
        execute_properties: AgentExecuteProperties,
    ) -> Self {
        Self {
            runner,
            registry,
            agent_card,
            execute_properties,
        }
    }

    /// The agent execution properties driving output assembly.
    pub fn execute_properties(&self) -> AgentExecuteProperties {
        self.execute_properties
    }

    /// Builds a handler whose card is derived from `converter` inputs.
    pub fn with_card_config(
        runner: Arc<dyn AgentRunner>,
        config: &crate::card::ConfigurableAgentCard,
        transports: &[crate::card::TransportProperties],
        registry: Arc<A2aTaskRegistry>,
    ) -> Self {
        let descriptor = RunnerDescriptor(runner.as_ref());
        let agent_card = crate::card::create_agent_card(config, &descriptor, transports);
        Self::new(runner, agent_card, registry)
    }

    /// The served agent card.
    pub fn agent_card(&self) -> &AgentCard {
        &self.agent_card
    }

    /// The task registry.
    pub fn registry(&self) -> &Arc<A2aTaskRegistry> {
        &self.registry
    }

    /// Dispatches one JSON-RPC request.
    pub async fn handle(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        let id = request.id.clone();
        match self.dispatch(request).await {
            Ok(result) => JsonRpcResponse::success(result, id),
            Err(error) => JsonRpcResponse::failure(error, id),
        }
    }

    /// Dispatches one JSON-RPC request, returning the raw `result` payload.
    pub async fn dispatch(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<serde_json::Value, JsonRpcError> {
        match request.method.as_str() {
            methods::MESSAGE_SEND => self.message_send(request).await,
            methods::MESSAGE_STREAM => self.message_stream(request).await,
            methods::TASKS_GET => self.tasks_get(request),
            methods::TASKS_CANCEL => self.tasks_cancel(request),
            methods::AGENT_CARD_GET => serde_json::to_value(&self.agent_card)
                .map_err(|e| JsonRpcError::new(JsonRpcError::INTERNAL_ERROR, e.to_string())),
            other => Err(JsonRpcError::new(
                JsonRpcError::METHOD_NOT_FOUND,
                format!("unsupported method: {other}"),
            )),
        }
    }

    /// `message/send`: runs the agent and answers with the finished task.
    async fn message_send(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<serde_json::Value, JsonRpcError> {
        let params: MessageSendParams = parse_params(request)?;
        let task = self.run_task(&params).await?;
        let result = SendResult::Task(Box::new(task));
        serde_json::to_value(&result).map_err(internal)
    }

    /// `message/stream`: relays the run as a sequence of status/artifact events.
    ///
    /// Java's streaming methods return a `Flux` of responses (one SSE frame each); collecting
    /// that stream is what a non-streaming caller observes, so this shares the incremental
    /// implementation with [`stream_responses`](Self::stream_responses).
    async fn message_stream(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<serde_json::Value, JsonRpcError> {
        let mut stream = self.stream_responses(request)?;
        let mut results: Vec<serde_json::Value> = Vec::new();
        while let Some(response) = futures::StreamExt::next(&mut stream).await {
            if let Some(error) = response.error {
                return Err(error);
            }
            if let Some(result) = response.result {
                results.push(result);
            }
        }
        Ok(serde_json::Value::Array(results))
    }

    /// Answers a streaming method with a stream of JSON-RPC responses, one per A2A event.
    ///
    /// The stream is lazy: the agent is started when the first item is polled, and each
    /// agent event is relayed before the run finishes — this is the Rust counterpart of Java
    /// `JsonRpcTransportWrapper#handleStreamRequest` returning a `Flux<String>`.
    pub fn stream_responses(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<BoxStream<'static, JsonRpcResponse>, JsonRpcError> {
        let state = StreamState {
            runner: self.runner.clone(),
            registry: self.registry.clone(),
            execute_properties: self.execute_properties,
            request: request.clone(),
            id: request.id.clone(),
            phase: StreamPhase::Start,
            pending: VecDeque::new(),
        };
        Ok(Box::pin(futures::stream::unfold(state, next_stream_item)))
    }

    /// Runs one submitted message through the runner and records the resulting task.
    ///
    /// Mirrors Java `processTaskBlocking`: a blocking request always completes with a message
    /// carrying the compacted output.
    async fn run_task(&self, params: &MessageSendParams) -> Result<Task, JsonRpcError> {
        let (task_id, context_id, mut events) =
            start_run(&self.runner, &self.registry, params).await?;
        let mut projector = AgentOutputProjector::new(self.execute_properties);
        while let Some(event) = futures::StreamExt::next(&mut events).await {
            let event = event
                .map_err(|e| JsonRpcError::new(JsonRpcError::INTERNAL_ERROR, e.to_string()))?;
            record_artifact_if_any(&self.registry, &task_id, &mut projector, &event);
        }

        let compacted = projector.compacted();
        let mut status = TaskStatus::new(TaskState::Completed);
        if !compacted.is_empty() {
            status.message = Some(ServerMessageConvert::convert_from_msgs_to_message(
                &compacted,
                &task_id,
                &context_id,
            ));
        }
        self.registry
            .update_status(&task_id, status)
            .ok_or_else(|| JsonRpcError::new(JsonRpcError::TASK_NOT_FOUND, "task vanished"))
    }

    /// `tasks/get`.
    fn tasks_get(&self, request: &JsonRpcRequest) -> Result<serde_json::Value, JsonRpcError> {
        let params: TaskQueryParams = parse_params(request)?;
        let task = self
            .registry
            .get(&params.id)
            .ok_or_else(|| JsonRpcError::new(JsonRpcError::TASK_NOT_FOUND, "task not found"))?;
        serde_json::to_value(&task).map_err(internal)
    }

    /// `tasks/cancel`.
    fn tasks_cancel(&self, request: &JsonRpcRequest) -> Result<serde_json::Value, JsonRpcError> {
        let params: TaskIdParams = parse_params(request)?;
        self.runner.stop(&params.id);
        let task = self
            .registry
            .cancel(&params.id)
            .ok_or_else(|| JsonRpcError::new(JsonRpcError::TASK_NOT_FOUND, "task not found"))?;
        serde_json::to_value(&task).map_err(internal)
    }
}

/// Starts one submitted message and returns `(task_id, context_id, event stream)`.
///
/// Mirrors the executor's submit step: record the task as `submitted`, flip it to `working`, and
/// open the agent run. Runners that expose an incremental stream are used as-is; the others are
/// replayed from their buffered [`AgentRunner::stream_events`] result.
async fn start_run(
    runner: &Arc<dyn AgentRunner>,
    registry: &Arc<A2aTaskRegistry>,
    params: &MessageSendParams,
) -> Result<
    (
        String,
        String,
        BoxStream<'static, Result<AgentEvent, A2aError>>,
    ),
    JsonRpcError,
> {
    let msgs = ServerMessageConvert::convert_from_message_to_msgs(&params.message);
    let task_id = params
        .message
        .task_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let context_id = params
        .message
        .context_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    registry.put(Task::new(
        task_id.clone(),
        TaskStatus::new(TaskState::Submitted),
    ));
    registry.update_status(&task_id, TaskStatus::new(TaskState::Working));

    let options = AgentRequestOptions::new()
        .with_task_id(task_id.clone())
        .with_session_id(resolve_session_id(&params.message, &context_id))
        .with_user_id(resolve_user_id(&params.message).unwrap_or_default());

    let stream = match runner.stream_events_incremental(msgs.clone(), options.clone()) {
        Some(stream) => stream,
        None => {
            let events = runner
                .stream_events(msgs, options)
                .await
                .map_err(|e| JsonRpcError::new(JsonRpcError::INTERNAL_ERROR, e.to_string()))?;
            Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
        }
    };
    Ok((task_id, context_id, stream))
}

/// Resolves the session id of one request, mirroring Java
/// `AgentScopeAgentExecutor#getSessionId(RequestContext)` (`e6f015e14`).
///
/// A non-blank legacy `message.metadata["sessionId"]` wins so existing session keys keep
/// working; otherwise the SDK-resolved A2A `contextId` is used. The protocol message's
/// `contextId` is never rewritten.
fn resolve_session_id(message: &Message, context_id: &str) -> String {
    if let Some(legacy) = message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("sessionId"))
    {
        let text = metadata_text(legacy);
        if !text.trim().is_empty() {
            return text;
        }
    }
    context_id.to_string()
}

/// Resolves the user id of one request from `message.metadata["userId"]`, mirroring Java
/// `AgentScopeAgentExecutor#getUserId`.
fn resolve_user_id(message: &Message) -> Option<String> {
    message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("userId"))
        .map(metadata_text)
}

/// Renders a metadata value the way Java's `String.valueOf(Object)` does: strings verbatim,
/// everything else through its JSON text.
fn metadata_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Projects `event` and, when it produces client-visible output, stores it as a task artifact.
fn record_artifact_if_any(
    registry: &Arc<A2aTaskRegistry>,
    task_id: &str,
    projector: &mut AgentOutputProjector,
    event: &AgentEvent,
) {
    let Some(msg) = projector.project(event) else {
        return;
    };
    let parts = ServerMessageConvert::convert_from_content_blocks(&msg, false);
    if !parts.is_empty() {
        registry.add_artifact(task_id, Artifact::new(task_id.to_string(), parts));
    }
}

/// The phases of one streaming request, mirroring the reactive pipeline Java builds out of
/// `emitter.startWork()` → per-event `TaskUpdater` calls → terminal status.
enum StreamPhase {
    /// Nothing started yet: the agent is opened on first poll.
    Start,
    /// The agent is running; events arrive incrementally.
    Running {
        events: BoxStream<'static, Result<AgentEvent, A2aError>>,
        projector: AgentOutputProjector,
        task_id: String,
        context_id: String,
    },
    /// The run finished; one final item was already emitted.
    Done,
}

/// Mutable state carried through [`futures::stream::unfold`].
struct StreamState {
    runner: Arc<dyn AgentRunner>,
    registry: Arc<A2aTaskRegistry>,
    execute_properties: AgentExecuteProperties,
    request: JsonRpcRequest,
    id: Option<serde_json::Value>,
    phase: StreamPhase,
    pending: VecDeque<JsonRpcResponse>,
}

impl StreamState {
    /// Queues a successful response carrying `value` as its `result`.
    fn push_result<T: serde::Serialize>(&mut self, value: &T) {
        let response = match serde_json::to_value(value) {
            Ok(value) => JsonRpcResponse::success(value, self.id.clone()),
            Err(error) => JsonRpcResponse::failure(internal(error), self.id.clone()),
        };
        self.pending.push_back(response);
    }

    /// Queues a JSON-RPC error response.
    fn push_error(&mut self, error: JsonRpcError) {
        self.pending
            .push_back(JsonRpcResponse::failure(error, self.id.clone()));
    }

    /// Completes the task: record the artifacts, then emit the final status update.
    fn finish_succeeded(
        &mut self,
        task_id: String,
        context_id: String,
        projector: AgentOutputProjector,
    ) {
        let compacted = projector.compacted();
        let mut status = TaskStatus::new(TaskState::Completed);
        if self.execute_properties.complete_with_message() && !compacted.is_empty() {
            status.message = Some(ServerMessageConvert::convert_from_msgs_to_message(
                &compacted,
                &task_id,
                &context_id,
            ));
        }
        self.registry.update_status(&task_id, status.clone());
        let mut event = agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent::new(
            task_id, status, true,
        );
        event.context_id = Some(context_id);
        self.push_result(&event);
        self.phase = StreamPhase::Done;
    }

    /// Fails the task and emits the terminal status update carrying the error message.
    fn finish_failed(&mut self, task_id: String, context_id: String, message: &str) {
        let status = TaskStatus {
            state: TaskState::Failed,
            message: Some(Message::new(
                uuid::Uuid::new_v4().to_string(),
                MessageRole::Agent,
                vec![Part::text(message)],
            )),
            timestamp: None,
        };
        self.registry.update_status(&task_id, status.clone());
        let mut event = agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent::new(
            task_id, status, true,
        );
        event.context_id = Some(context_id);
        self.push_result(&event);
        self.phase = StreamPhase::Done;
    }
}

/// One poll of the streaming request, mirroring Java's per-event pipeline.
async fn next_stream_item(mut state: StreamState) -> Option<(JsonRpcResponse, StreamState)> {
    loop {
        if let Some(response) = state.pending.pop_front() {
            return Some((response, state));
        }
        match std::mem::replace(&mut state.phase, StreamPhase::Done) {
            StreamPhase::Start => {
                let params = match parse_params::<MessageSendParams>(&state.request) {
                    Ok(params) => params,
                    Err(error) => {
                        state.push_error(error);
                        continue;
                    }
                };
                match start_run(&state.runner, &state.registry, &params).await {
                    Ok((task_id, context_id, events)) => {
                        let mut status = TaskStatus::new(TaskState::Working);
                        status.timestamp = None;
                        let mut event =
                            agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent::new(
                                task_id.clone(),
                                status,
                                false,
                            );
                        event.context_id = Some(context_id.clone());
                        state.push_result(&event);
                        state.phase = StreamPhase::Running {
                            events,
                            projector: AgentOutputProjector::new(state.execute_properties),
                            task_id,
                            context_id,
                        };
                    }
                    Err(error) => {
                        state.push_error(error);
                    }
                }
            }
            StreamPhase::Running {
                mut events,
                mut projector,
                task_id,
                context_id,
            } => match futures::StreamExt::next(&mut events).await {
                Some(Ok(event)) => {
                    if let Some(msg) = projector.project(&event) {
                        let parts = ServerMessageConvert::convert_from_content_blocks(&msg, false);
                        if !parts.is_empty() {
                            let artifact = Artifact::new(task_id.clone(), parts);
                            state.registry.add_artifact(&task_id, artifact.clone());
                            let mut update =
                                agentscope_extensions_a2a_client::model::TaskArtifactUpdateEvent::new(
                                    task_id.clone(),
                                    artifact,
                                );
                            update.context_id = Some(context_id.clone());
                            update.append = Some(true);
                            update.last_chunk = Some(false);
                            state.push_result(&update);
                        }
                    }
                    state.phase = StreamPhase::Running {
                        events,
                        projector,
                        task_id,
                        context_id,
                    };
                }
                Some(Err(error)) => {
                    state.finish_failed(task_id, context_id, &error.to_string());
                }
                None => {
                    state.finish_succeeded(task_id, context_id, projector);
                }
            },
            StreamPhase::Done => return None,
        }
    }
}

/// Decodes JSON-RPC `params`, mirroring the SDK's `-32602` on malformed input.
fn parse_params<T: serde::de::DeserializeOwned>(
    request: &JsonRpcRequest,
) -> Result<T, JsonRpcError> {
    let params = request
        .params
        .clone()
        .filter(|p| !p.is_null())
        .ok_or_else(|| JsonRpcError::new(JsonRpcError::INVALID_PARAMS, "missing params"))?;
    serde_json::from_value(params)
        .map_err(|e| JsonRpcError::new(JsonRpcError::INVALID_PARAMS, e.to_string()))
}

fn internal(err: serde_json::Error) -> JsonRpcError {
    JsonRpcError::new(JsonRpcError::INTERNAL_ERROR, err.to_string())
}

/// Builds an `axum` router exposing the A2A endpoints.
///
/// Mirrors the surface installed by the Java transport wrappers
/// (`JsonRpcTransportWrapper` + the well-known card endpoint). The well-known card is served
/// as `GET` and all JSON-RPC methods are served as `POST /`.
pub fn router(handler: Arc<A2aRequestHandler>) -> axum::Router {
    use axum::routing::{get, post};

    axum::Router::new()
        .route("/", post(jsonrpc_endpoint))
        .route(
            agentscope_extensions_a2a_client::model::WELL_KNOWN_AGENT_CARD_PATH,
            get(agent_card_endpoint),
        )
        .with_state(handler)
}

/// `POST /`: hands the raw body to the JSON-RPC transport wrapper.
///
/// Mirrors a host controller calling `JsonRpcTransportWrapper#handleRequest`: a non-streaming
/// method answers with one JSON body, a streaming method answers with one SSE frame per
/// response (Java returns a `Flux` there, which Spring renders as `text/event-stream`).
async fn jsonrpc_endpoint(
    axum::extract::State(handler): axum::extract::State<Arc<A2aRequestHandler>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let wrapper = JsonRpcTransportWrapper::new(handler);
    let headers: TransportHeaders = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    match wrapper
        .handle_request(&body, &headers, &TransportMetadata::new())
        .await
    {
        TransportResponse::Json(payload) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            payload,
        )
            .into_response(),
        TransportResponse::Stream(stream) => {
            let frames = futures::StreamExt::map(stream, |payload| {
                Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default().data(payload),
                )
            });
            axum::response::sse::Sse::new(frames).into_response()
        }
    }
}

/// `GET /.well-known/agent-card.json`.
async fn agent_card_endpoint(
    axum::extract::State(handler): axum::extract::State<Arc<A2aRequestHandler>>,
) -> axum::Json<AgentCard> {
    axum::Json(handler.agent_card().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::EventData;
    use agentscope_core::message::{TextBlock, ThinkingBlock};

    /// Deterministic [`AgentRunner`] stub emitting one `AGENT_RESULT` text block.
    struct EchoRunner {
        name: String,
        description: String,
    }

    #[async_trait::async_trait]
    impl AgentRunner for EchoRunner {
        fn agent_name(&self) -> String {
            self.name.clone()
        }

        fn agent_description(&self) -> String {
            self.description.clone()
        }

        async fn stream_events(
            &self,
            request_messages: Vec<Msg>,
            _options: AgentRequestOptions,
        ) -> Result<Vec<AgentEvent>, A2aError> {
            let echoed = request_messages
                .iter()
                .map(|m| m.text_content())
                .collect::<Vec<_>>()
                .join("");
            let mut data = EventData::default();
            data.fields.insert(
                "content".to_string(),
                serde_json::json!([{ "type": "text", "text": format!("echo:{echoed}") }]),
            );
            Ok(vec![AgentEvent::AgentResult(data)])
        }

        fn stop(&self, _task_id: &str) {}
    }

    fn runner(name: &str, description: &str) -> Arc<dyn AgentRunner> {
        Arc::new(EchoRunner {
            name: name.to_string(),
            description: description.to_string(),
        })
    }

    fn registry() -> Arc<A2aTaskRegistry> {
        Arc::new(A2aTaskRegistry::new())
    }

    fn handler() -> A2aRequestHandler {
        A2aRequestHandler::new(
            runner("echo-agent", "echoes the input"),
            AgentCard::new("echo-agent", "http://localhost"),
            registry(),
        )
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn convert_from_msg_to_message_sets_agent_role_and_task_ids() {
        let msg = Msg::builder()
            .id("m1")
            .name("alice")
            .role(MsgRole::Assistant)
            .text_content("hello")
            .metadata_entry("k", serde_json::Value::String("v".to_string()))
            .build();
        let message = ServerMessageConvert::convert_from_msg_to_message(&msg, "t1", "c1");
        assert_eq!(message.role, MessageRole::Agent);
        assert_eq!(message.task_id.as_deref(), Some("t1"));
        assert_eq!(message.context_id.as_deref(), Some("c1"));

        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["parts"][0]["metadata"]["_agentscope_msg_id"], "m1");
        assert_eq!(json["metadata"]["m1"]["k"], "v");
    }

    #[test]
    fn convert_from_content_blocks_marks_streaming_chunks() {
        let msg = Msg::builder()
            .id("m1")
            .role(MsgRole::Assistant)
            .text_content("chunk")
            .build();
        let parts = ServerMessageConvert::convert_from_content_blocks(&msg, true);
        let json = serde_json::to_value(&parts[0]).unwrap();
        assert_eq!(json["metadata"]["_agentscope_stream_chunk"], true);
    }

    #[test]
    fn compact_streaming_chunks_merges_same_message_text() {
        let deltas = vec![
            Msg::builder()
                .id("m1")
                .role(MsgRole::Assistant)
                .text_content("Hel")
                .build(),
            Msg::builder()
                .id("m1")
                .role(MsgRole::Assistant)
                .text_content("lo")
                .build(),
            Msg::builder()
                .id("m2")
                .role(MsgRole::Assistant)
                .text_content("!")
                .build(),
        ];
        let compacted = ServerMessageConvert::compact_streaming_chunks(&deltas);
        assert_eq!(compacted.len(), 2);
        assert_eq!(
            compacted[0].content,
            vec![ContentBlock::Text(TextBlock::new("Hello"))]
        );
        assert_eq!(compacted[1].id, "m2");
    }

    #[test]
    fn compact_streaming_chunks_merges_thinking_blocks() {
        let deltas = vec![
            Msg::builder()
                .id("m1")
                .role(MsgRole::Assistant)
                .block(ContentBlock::Thinking(ThinkingBlock::new("a")))
                .build(),
            Msg::builder()
                .id("m1")
                .role(MsgRole::Assistant)
                .block(ContentBlock::Thinking(ThinkingBlock::new("b")))
                .build(),
        ];
        let compacted = ServerMessageConvert::compact_streaming_chunks(&deltas);
        assert_eq!(compacted.len(), 1);
        assert_eq!(
            compacted[0].content,
            vec![ContentBlock::Thinking(ThinkingBlock::new("ab"))]
        );
    }

    #[test]
    fn convert_from_message_to_msgs_regroups_parts_by_message_id() {
        // Round trip a batch of msgs through the A2A wire format and back.
        let msgs = vec![
            Msg::builder()
                .id("m1")
                .name("alice")
                .role(MsgRole::Assistant)
                .text_content("one")
                .metadata_entry("k", serde_json::Value::String("v".to_string()))
                .build(),
            Msg::builder()
                .id("m2")
                .name("bob")
                .role(MsgRole::Assistant)
                .text_content("two")
                .build(),
        ];
        let message =
            agentscope_extensions_a2a_client::message::MessageConvert::convert_from_msgs(&msgs);
        let restored = ServerMessageConvert::convert_from_message_to_msgs(&message);

        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].id, "m1");
        assert_eq!(restored[0].name.as_deref(), Some("alice"));
        assert_eq!(restored[0].role, MsgRole::Assistant);
        assert_eq!(
            restored[0].content,
            vec![ContentBlock::Text(TextBlock::new("one"))]
        );
        assert_eq!(restored[0].metadata["k"], "v");
        assert_eq!(restored[1].id, "m2");
        assert_eq!(
            restored[1].content,
            vec![ContentBlock::Text(TextBlock::new("two"))]
        );
    }

    #[test]
    fn foreign_message_without_metadata_degrades_to_one_msg() {
        let message = Message::new("m", MessageRole::User, vec![Part::text("plain")]);
        let msgs = ServerMessageConvert::convert_from_message_to_msgs(&message);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, MsgRole::User);
        assert_eq!(
            msgs[0].content,
            vec![ContentBlock::Text(TextBlock::new("plain"))]
        );
    }

    #[test]
    fn foreign_message_with_thinking_under_user_role_is_promoted() {
        let thinking_part = Part::Text(agentscope_extensions_a2a_client::model::TextPart {
            text: "hmm".to_string(),
            metadata: Some(MessageConstants::build_type_metadata("thinking")),
        });
        let message = Message::new("m", MessageRole::User, vec![thinking_part]);
        let msgs = ServerMessageConvert::convert_from_message_to_msgs(&message);
        // core::Msg would panic for a user message holding a thinking block.
        assert_eq!(msgs[0].role, MsgRole::Assistant);
    }

    /// Records the [`AgentRequestOptions`] of every run.
    struct CapturingRunner {
        seen: Arc<Mutex<Vec<AgentRequestOptions>>>,
    }

    #[async_trait::async_trait]
    impl AgentRunner for CapturingRunner {
        fn agent_name(&self) -> String {
            "capturing-agent".to_string()
        }

        fn agent_description(&self) -> String {
            "captures the request options".to_string()
        }

        async fn stream_events(
            &self,
            _request_messages: Vec<Msg>,
            options: AgentRequestOptions,
        ) -> Result<Vec<AgentEvent>, A2aError> {
            self.seen.lock().unwrap().push(options);
            let mut data = EventData::default();
            data.fields
                .insert("reply".to_string(), serde_json::json!("ok"));
            Ok(vec![AgentEvent::AgentResult(data)])
        }

        fn stop(&self, _task_id: &str) {}
    }

    // Java: e6f015e14 — a standard A2A client that only sends `contextId` must give the runner a
    // session id; an explicit legacy `metadata.sessionId` keeps precedence.
    #[test]
    fn a_standard_client_session_id_comes_from_the_context_id() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler = A2aRequestHandler::new(
            Arc::new(CapturingRunner { seen: seen.clone() }),
            AgentCard::new("capturing-agent", "http://localhost"),
            registry(),
        );
        let mut message = Message::new("m1", MessageRole::User, vec![Part::text("ping")]);
        message.context_id = Some("ctx-1".to_string());

        let params = MessageSendParams {
            message,
            configuration: None,
            metadata: None,
        };
        let request = JsonRpcRequest::new(
            methods::MESSAGE_SEND,
            Some(serde_json::to_value(params).unwrap()),
            Some(serde_json::Value::from(1)),
        );
        rt().block_on(handler.handle(&request));

        let options = seen.lock().unwrap().first().cloned().expect("one run");
        assert_eq!(options.session_id(), Some("ctx-1"));
        // Java returns "" for a missing `metadata.userId`.
        assert_eq!(options.user_id(), Some(""));
    }

    #[test]
    fn legacy_session_metadata_wins_over_the_context_id() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("sessionId".to_string(), serde_json::json!("legacy-session"));
        metadata.insert("userId".to_string(), serde_json::json!("alice"));
        let message = Message {
            message_id: "m1".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text("ping")],
            metadata: Some(metadata),
            context_id: Some("ctx-1".to_string()),
            task_id: None,
            kind: "message".to_string(),
        };

        assert_eq!(resolve_session_id(&message, "ctx-1"), "legacy-session");
        assert_eq!(resolve_user_id(&message).as_deref(), Some("alice"));
    }

    // Java: null, empty and whitespace-only legacy values fall back to the protocol context id.
    #[test]
    fn blank_legacy_session_metadata_falls_back_to_the_context_id() {
        for legacy in ["", "   "] {
            let mut metadata = serde_json::Map::new();
            metadata.insert("sessionId".to_string(), serde_json::json!(legacy));
            let message = Message {
                message_id: "m1".to_string(),
                role: MessageRole::User,
                parts: vec![Part::text("ping")],
                metadata: Some(metadata),
                context_id: Some("ctx-1".to_string()),
                task_id: None,
                kind: "message".to_string(),
            };
            assert_eq!(resolve_session_id(&message, "ctx-1"), "ctx-1");
        }

        // No metadata at all: the protocol context id is used.
        let message = Message::new("m1", MessageRole::User, vec![Part::text("ping")]);
        assert_eq!(resolve_session_id(&message, "ctx-9"), "ctx-9");
        assert_eq!(resolve_user_id(&message), None);
    }

    #[test]
    fn message_send_runs_the_agent_and_returns_a_completed_task() {
        let handler = handler();
        let params = MessageSendParams {
            message: Message::new("m1", MessageRole::User, vec![Part::text("ping")]),
            configuration: None,
            metadata: None,
        };
        let request = JsonRpcRequest::new(
            methods::MESSAGE_SEND,
            Some(serde_json::to_value(params).unwrap()),
            Some(serde_json::Value::from(1)),
        );
        let response = rt().block_on(handler.handle(&request));
        assert!(!response.is_error());

        let result: SendResult = serde_json::from_value(response.result.unwrap()).unwrap();
        let task = result.as_task().unwrap();
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(
            task.artifacts.as_ref().unwrap()[0].parts[0].kind(),
            agentscope_extensions_a2a_client::model::PartKind::Text
        );
        assert_eq!(handler.registry().len(), 1);
    }

    #[test]
    fn message_stream_relays_the_run_incrementally() {
        let handler = handler();
        let params = MessageSendParams {
            message: Message::new("m1", MessageRole::User, vec![Part::text("ping")]),
            configuration: None,
            metadata: None,
        };
        let request = JsonRpcRequest::new(
            methods::MESSAGE_STREAM,
            Some(serde_json::to_value(params).unwrap()),
            None,
        );
        let response = rt().block_on(handler.handle(&request));
        let events: Vec<serde_json::Value> =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(
            events.len() >= 3,
            "expected working + artifact + final: {events:?}"
        );

        // 1. the run starts with a non-final `working` status update.
        let first: agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent =
            serde_json::from_value(events[0].clone()).unwrap();
        assert!(!first.is_final);
        assert_eq!(first.status.state, TaskState::Working);

        // 2. the agent output arrives as artifact updates.
        let artifact: agentscope_extensions_a2a_client::model::TaskArtifactUpdateEvent =
            serde_json::from_value(events[1].clone()).unwrap();
        assert_eq!(artifact.append, Some(true));
        assert_eq!(artifact.last_chunk, Some(false));

        // 3. the run ends with the final `completed` status update.
        let last: agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent =
            serde_json::from_value(events.last().unwrap().clone()).unwrap();
        assert!(last.is_final);
        assert_eq!(last.status.state, TaskState::Completed);
    }

    #[tokio::test]
    async fn stream_responses_emits_before_the_run_finishes() {
        use futures::channel::mpsc;
        use futures::StreamExt;

        /// Runner whose event stream is fed by the test, so a buffering server would hang.
        struct ChannelRunner {
            receiver: Mutex<Option<mpsc::UnboundedReceiver<Result<AgentEvent, A2aError>>>>,
        }

        #[async_trait::async_trait]
        impl AgentRunner for ChannelRunner {
            fn agent_name(&self) -> String {
                "channel-agent".to_string()
            }

            fn agent_description(&self) -> String {
                "fed by the test".to_string()
            }

            async fn stream_events(
                &self,
                _request_messages: Vec<Msg>,
                _options: AgentRequestOptions,
            ) -> Result<Vec<AgentEvent>, A2aError> {
                unreachable!("the incremental path is used")
            }

            fn stream_events_incremental(
                &self,
                _request_messages: Vec<Msg>,
                _options: AgentRequestOptions,
            ) -> Option<BoxStream<'static, Result<AgentEvent, A2aError>>> {
                let receiver = self.receiver.lock().unwrap().take()?;
                Some(Box::pin(receiver))
            }

            fn stop(&self, _task_id: &str) {}
        }

        let (sender, receiver) = mpsc::unbounded();
        let handler = A2aRequestHandler::new(
            Arc::new(ChannelRunner {
                receiver: Mutex::new(Some(receiver)),
            }),
            AgentCard::new("channel-agent", "http://localhost"),
            registry(),
        );
        let params = MessageSendParams {
            message: Message::new("m1", MessageRole::User, vec![Part::text("ping")]),
            configuration: None,
            metadata: None,
        };
        let request = JsonRpcRequest::new(
            methods::MESSAGE_STREAM,
            Some(serde_json::to_value(params).unwrap()),
            None,
        );
        let mut stream = handler.stream_responses(&request).unwrap();

        // Nothing is sent yet: the first item is the `working` status update.
        let first = stream.next().await.unwrap();
        let first: agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent =
            serde_json::from_value(first.result.unwrap()).unwrap();
        assert_eq!(first.status.state, TaskState::Working);

        // Feed one delta: the server must relay it while the run is still open.
        let mut delta = agentscope_core::event::EventData::default();
        delta.fields.insert(
            "reply_id".to_string(),
            serde_json::Value::String("reply-1".to_string()),
        );
        delta.fields.insert(
            "delta".to_string(),
            serde_json::Value::String("Hel".to_string()),
        );
        sender
            .unbounded_send(Ok(AgentEvent::TextBlockDelta(delta)))
            .unwrap();

        let second = stream.next().await.unwrap();
        let second: agentscope_extensions_a2a_client::model::TaskArtifactUpdateEvent =
            serde_json::from_value(second.result.unwrap()).unwrap();
        assert_eq!(second.append, Some(true));

        // The stream is still open — a buffering implementation could not have answered yet.
        sender
            .unbounded_send(Ok(AgentEvent::TextBlockDelta({
                let mut next = agentscope_core::event::EventData::default();
                next.fields.insert(
                    "reply_id".to_string(),
                    serde_json::Value::String("reply-1".to_string()),
                );
                next.fields.insert(
                    "delta".to_string(),
                    serde_json::Value::String("lo".to_string()),
                );
                next
            })))
            .unwrap();
        let third = stream.next().await.unwrap();
        assert!(third.result.unwrap()["artifact"]["parts"].is_array());

        drop(sender);
        let last = stream.next().await.unwrap();
        let last: agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent =
            serde_json::from_value(last.result.unwrap()).unwrap();
        assert!(last.is_final);
        assert_eq!(last.status.state, TaskState::Completed);
        assert!(stream.next().await.is_none());

        // Both deltas were persisted as artifacts of the completed task.
        let task = handler.registry().get(&last.task_id).unwrap();
        assert_eq!(task.artifacts.as_ref().unwrap().len(), 2);
        assert_eq!(task.status.state, TaskState::Completed);
    }

    #[test]
    fn require_inner_message_gates_tool_output() {
        /// Emits one text delta and one tool-result delta.
        struct InnerRunner;

        #[async_trait::async_trait]
        impl AgentRunner for InnerRunner {
            fn agent_name(&self) -> String {
                "inner".to_string()
            }

            fn agent_description(&self) -> String {
                "inner".to_string()
            }

            async fn stream_events(
                &self,
                _request_messages: Vec<Msg>,
                _options: AgentRequestOptions,
            ) -> Result<Vec<AgentEvent>, A2aError> {
                let mut text = agentscope_core::event::EventData::default();
                text.fields.insert(
                    "reply_id".to_string(),
                    serde_json::Value::String("reply-1".to_string()),
                );
                text.fields.insert(
                    "delta".to_string(),
                    serde_json::Value::String("answer".to_string()),
                );
                let mut tool = agentscope_core::event::EventData::default();
                tool.fields.insert(
                    "tool_call_id".to_string(),
                    serde_json::Value::String("call-1".to_string()),
                );
                tool.fields.insert(
                    "delta".to_string(),
                    serde_json::Value::String("tool output".to_string()),
                );
                Ok(vec![
                    AgentEvent::TextBlockDelta(text),
                    AgentEvent::ToolResultTextDelta(tool),
                ])
            }

            fn stop(&self, _task_id: &str) {}
        }

        let runner: Arc<dyn AgentRunner> = Arc::new(InnerRunner);
        let params = MessageSendParams {
            message: Message::new("m1", MessageRole::User, vec![Part::text("ping")]),
            configuration: None,
            metadata: None,
        };
        let request = JsonRpcRequest::new(
            methods::MESSAGE_SEND,
            Some(serde_json::to_value(params.clone()).unwrap()),
            None,
        );

        let quiet = A2aRequestHandler::new(
            runner.clone(),
            AgentCard::new("inner", "http://localhost"),
            registry(),
        );
        let task: Task =
            serde_json::from_value(rt().block_on(quiet.handle(&request)).result.unwrap()).unwrap();
        assert_eq!(task.artifacts.as_ref().unwrap().len(), 1);
        assert_eq!(
            task.artifacts.as_ref().unwrap()[0].parts[0].kind(),
            agentscope_extensions_a2a_client::model::PartKind::Text
        );

        let verbose = A2aRequestHandler::new_with_properties(
            runner,
            AgentCard::new("inner", "http://localhost"),
            registry(),
            AgentExecuteProperties::builder()
                .require_inner_message(true)
                .build(),
        );
        let task: Task =
            serde_json::from_value(rt().block_on(verbose.handle(&request)).result.unwrap())
                .unwrap();
        let artifacts = task.artifacts.as_ref().unwrap();
        assert_eq!(
            artifacts.len(),
            2,
            "the tool result becomes its own artifact"
        );
        let part = serde_json::to_value(&artifacts[1].parts[0]).unwrap();
        assert_eq!(part["metadata"]["_agentscope_block_type"], "tool_result");
    }

    #[test]
    fn complete_with_message_only_applies_to_non_blocking_runs() {
        let params = MessageSendParams {
            message: Message::new("m1", MessageRole::User, vec![Part::text("ping")]),
            configuration: None,
            metadata: None,
        };
        let send = JsonRpcRequest::new(
            methods::MESSAGE_SEND,
            Some(serde_json::to_value(params.clone()).unwrap()),
            None,
        );
        let stream = JsonRpcRequest::new(
            methods::MESSAGE_STREAM,
            Some(serde_json::to_value(params).unwrap()),
            None,
        );

        // Blocking: always completes with a message, whatever the properties say.
        for properties in [
            AgentExecuteProperties::default(),
            AgentExecuteProperties::builder()
                .complete_with_message(true)
                .build(),
        ] {
            let handler = A2aRequestHandler::new_with_properties(
                runner("echo-agent", "echoes"),
                AgentCard::new("echo-agent", "http://localhost"),
                registry(),
                properties,
            );
            let task: Task =
                serde_json::from_value(rt().block_on(handler.handle(&send)).result.unwrap())
                    .unwrap();
            let message = task
                .status
                .message
                .expect("a blocking run completes with a message");
            assert_eq!(
                message.parts[0].kind(),
                agentscope_extensions_a2a_client::model::PartKind::Text
            );
        }

        // Non-blocking: only when `complete_with_message` is set.
        let quiet = A2aRequestHandler::new(
            runner("echo-agent", "echoes"),
            AgentCard::new("echo-agent", "http://localhost"),
            registry(),
        );
        let events: Vec<serde_json::Value> =
            serde_json::from_value(rt().block_on(quiet.handle(&stream)).result.unwrap()).unwrap();
        let last: agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent =
            serde_json::from_value(events.last().unwrap().clone()).unwrap();
        assert!(last.status.message.is_none());

        let verbose = A2aRequestHandler::new_with_properties(
            runner("echo-agent", "echoes"),
            AgentCard::new("echo-agent", "http://localhost"),
            registry(),
            AgentExecuteProperties::builder()
                .complete_with_message(true)
                .build(),
        );
        let events: Vec<serde_json::Value> =
            serde_json::from_value(rt().block_on(verbose.handle(&stream)).result.unwrap()).unwrap();
        let last: agentscope_extensions_a2a_client::model::TaskStatusUpdateEvent =
            serde_json::from_value(events.last().unwrap().clone()).unwrap();
        let message = last
            .status
            .message
            .expect("complete_with_message asks for a final message");
        assert_eq!(
            message.parts[0].kind(),
            agentscope_extensions_a2a_client::model::PartKind::Text
        );
    }

    #[test]
    fn tasks_get_returns_the_stored_task_and_reports_missing_ones() {
        let handler = handler();
        let task = Task::new("t-1", TaskStatus::new(TaskState::Working));
        handler.registry().put(task);

        let found = rt().block_on(handler.handle(&JsonRpcRequest::new(
            methods::TASKS_GET,
            Some(serde_json::json!({"id": "t-1"})),
            None,
        )));
        let task: Task = serde_json::from_value(found.result.unwrap()).unwrap();
        assert_eq!(task.id, "t-1");

        let missing = rt().block_on(handler.handle(&JsonRpcRequest::new(
            methods::TASKS_GET,
            Some(serde_json::json!({"id": "nope"})),
            None,
        )));
        assert_eq!(missing.error.unwrap().code, JsonRpcError::TASK_NOT_FOUND);
    }

    #[test]
    fn tasks_cancel_marks_a_task_canceled_and_never_downgrades_terminal_tasks() {
        let handler = handler();
        handler
            .registry()
            .put(Task::new("t-1", TaskStatus::new(TaskState::Working)));
        let canceled = rt().block_on(handler.handle(&JsonRpcRequest::new(
            methods::TASKS_CANCEL,
            Some(serde_json::json!({"id": "t-1"})),
            None,
        )));
        let task: Task = serde_json::from_value(canceled.result.unwrap()).unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);

        let completed = handler
            .registry()
            .update_status("t-1", TaskStatus::new(TaskState::Completed))
            .unwrap();
        assert_eq!(completed.status.state, TaskState::Completed);
        let again = handler.registry().cancel("t-1").unwrap();
        assert_eq!(again.status.state, TaskState::Completed);
    }

    #[test]
    fn unknown_method_is_reported_as_method_not_found() {
        let handler = handler();
        let response =
            rt().block_on(handler.handle(&JsonRpcRequest::new("does/not/exist", None, None)));
        assert_eq!(response.error.unwrap().code, JsonRpcError::METHOD_NOT_FOUND);
    }

    #[test]
    fn malformed_params_are_reported_as_invalid_params() {
        let handler = handler();
        let response = rt().block_on(handler.handle(&JsonRpcRequest::new(
            methods::TASKS_GET,
            Some(serde_json::json!({"not_id": 1})),
            None,
        )));
        assert_eq!(response.error.unwrap().code, JsonRpcError::INVALID_PARAMS);
    }

    #[test]
    fn card_endpoint_serves_the_configured_card() {
        let handler = A2aRequestHandler::with_card_config(
            runner("card-agent", "described"),
            &crate::card::ConfigurableAgentCard::builder()
                .name("custom")
                .build(),
            &[crate::card::TransportProperties::builder("JSONRPC")
                .host("localhost")
                .port(8080)
                .build()],
            registry(),
        );
        assert_eq!(handler.agent_card().name, "custom");
        assert_eq!(handler.agent_card().url, "http://localhost:8080");
        let card = rt().block_on(handler.handle(&JsonRpcRequest::new(
            methods::AGENT_CARD_GET,
            None,
            None,
        )));
        let card: AgentCard = serde_json::from_value(card.result.unwrap()).unwrap();
        assert_eq!(card.name, "custom");
    }

    #[test]
    fn runner_can_be_used_as_an_agent_descriptor() {
        let runner = runner("desc-agent", "a description");
        let descriptor = RunnerDescriptor(runner.as_ref());
        let descriptor: &dyn crate::card::AgentDescriptor = &descriptor;
        assert_eq!(descriptor.agent_name(), "desc-agent");
        assert_eq!(descriptor.agent_description(), "a description");
    }
}
