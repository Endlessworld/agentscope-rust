//! Agent Protocol: the HTTP task protocol used by `agentscope-extensions-agent-protocol`.
//!
//! Mirrors `io.agentscope.extensions.agentprotocol`:
//!
//! * [`AgentProtocolConstants`] / [`AgentProtocolProperties`] - configuration and namespacing
//! * [`AgentRequest`] - everything an [`AgentFactory`] needs to pick an agent
//! * [`RuntimeContextCustomizer`] - hook for shaping the run's `RuntimeContext`
//! * [`AgentProtocolEventBus`] - per-task monotonic event stream with replay
//! * [`ProtocolTaskRepository`] - control-plane `TaskRecord` persistence
//! * [`WorkspaceProtocolTaskRepository`] - workspace-backed persistence
//! * [`AgentProtocolTaskStore`] - task execution and control plane
//! * [`router`] - the `/tasks` HTTP endpoints
//!
//! Reuse of existing abstractions (no parallel types are invented):
//!
//! * task persistence is [`agentscope_harness::agent::subagent::task::TaskRecord`] /
//!   `TaskStatus` / `TaskRepository`;
//! * streaming events are [`agentscope_harness::agent::subagent::protocol::RemoteAgentEvent`],
//!   `RemoteEventType`, `RemoteConfirmDecision` and the harness codec;
//! * run context is [`agentscope_core::agent::RuntimeContext`];
//! * [`ProtocolAgent`] is implemented for [`HarnessAgent`] so a harness agent drives a task.

pub mod error;
pub mod workspace_repository;

pub use error::{AgentProtocolError, AgentProtocolResult};
pub use workspace_repository::WorkspaceProtocolTaskRepository;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::AgentEvent;
use agentscope_core::formatter::render_message_text;
use agentscope_core::message::{ContentBlock, Msg, MsgRole, ToolCallState, ToolUseBlock};
use agentscope_harness::agent::subagent::protocol::{
    from_agent_event, matches_detail_event, RemoteAgentEvent, RemoteConfirmDecision,
    RemoteEventType, RemotePendingConfirm, RemoteStreamDetail,
};
use agentscope_harness::agent::subagent::task::{TaskRecord, TaskStatus};
use agentscope_harness::agent::HarnessAgent;
use axum::response::IntoResponse;
use futures::StreamExt;
use serde_json::Value;

/// Namespacing constants and reserved-key rules.
///
/// Mirrors Java `io.agentscope.extensions.agentprotocol.AgentProtocolConstants`.
pub struct AgentProtocolConstants;

impl AgentProtocolConstants {
    /// Synthetic parent agent id for `agents/<id>/tasks/` paths.
    pub const PROTOCOL_AGENT_ID: &'static str = "_agentscope_protocol";
    /// Single session bucket file holding all protocol tasks.
    pub const PROTOCOL_SESSION_ID: &'static str = "_tasks";
    /// Field inside the submission `context` carrying caller-defined attributes.
    pub const CONTEXT_ATTRIBUTES_FIELD: &'static str = "attributes";
    /// [`RuntimeContext::metadata`] key holding the whole `context.attributes` map.
    pub const RUNTIME_CONTEXT_ATTRIBUTES_KEY: &'static str = "agentprotocol.context.attributes";

    /// Exact runtime-context keys the framework reads; never overwritten by flattening.
    pub const RESERVED_KEYS: [&'static str; 2] = ["agentId", "outboundAddress"];

    /// Runtime-context key prefixes owned by the framework.
    pub const RESERVED_PREFIXES: [&'static str; 3] = ["agentscope.", "agui.", "io.agentscope."];

    /// Whether `key` is read by the framework itself and therefore unsafe to overwrite.
    pub fn is_reserved_runtime_context_key(key: &str) -> bool {
        let key = key.trim();
        if key.is_empty() {
            return true;
        }
        if Self::RESERVED_KEYS.contains(&key) {
            return true;
        }
        Self::RESERVED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
    }
}

/// Sentinel returned when the agent paused for HITL confirmation.
///
/// Mirrors Java `AgentProtocolTaskStore.AWAITING_CONFIRM_SENTINEL`.
pub const AWAITING_CONFIRM_SENTINEL: &str = "__awaiting_confirm__";

/// Configuration for the AgentScope task HTTP protocol endpoints.
///
/// Mirrors Java `AgentProtocolProperties` (`agentscope.agent-protocol.*`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentProtocolProperties {
    /// When `true`, the `/tasks` REST endpoints are registered.
    pub enabled: bool,
    /// When `true`, `AgentEvent`s are pushed over the SSE endpoint.
    pub streaming_enabled: bool,
    /// When `true`, tasks may pause for tool confirmation (HITL).
    pub hitl_enabled: bool,
    /// Replay buffer size per task for late SSE subscribers.
    pub sse_replay_buffer_size: usize,
    /// Client idle timeout hint for SSE (ms).
    pub sse_timeout_ms: u64,
    /// Control-plane directory for task-record persistence.
    pub task_store_path: Option<String>,
}

impl Default for AgentProtocolProperties {
    fn default() -> Self {
        Self {
            enabled: false,
            streaming_enabled: true,
            hitl_enabled: true,
            sse_replay_buffer_size: 256,
            sse_timeout_ms: 10_800_000,
            task_store_path: None,
        }
    }
}

impl AgentProtocolProperties {
    /// The resolved control-plane path, defaulting to `<cwd>/.agentscope/agent-protocol`.
    pub fn resolved_task_store_path(&self) -> String {
        match self.task_store_path.as_deref() {
            Some(path) if !path.trim().is_empty() => path.to_string(),
            _ => std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join(".agentscope")
                .join("agent-protocol")
                .to_string_lossy()
                .to_string(),
        }
    }

    /// Returns the Java-style fluent builder.
    pub fn builder() -> AgentProtocolPropertiesBuilder {
        AgentProtocolPropertiesBuilder::default()
    }
}

/// Builder for [`AgentProtocolProperties`].
#[derive(Debug, Default, Clone)]
pub struct AgentProtocolPropertiesBuilder {
    props: AgentProtocolProperties,
}

impl AgentProtocolPropertiesBuilder {
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.props.enabled = enabled;
        self
    }

    pub fn streaming_enabled(mut self, enabled: bool) -> Self {
        self.props.streaming_enabled = enabled;
        self
    }

    pub fn hitl_enabled(mut self, enabled: bool) -> Self {
        self.props.hitl_enabled = enabled;
        self
    }

    pub fn sse_replay_buffer_size(mut self, size: usize) -> Self {
        self.props.sse_replay_buffer_size = size;
        self
    }

    pub fn sse_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.props.sse_timeout_ms = timeout_ms;
        self
    }

    pub fn task_store_path(mut self, path: impl Into<String>) -> Self {
        self.props.task_store_path = Some(path.into());
        self
    }

    pub fn build(self) -> AgentProtocolProperties {
        self.props
    }
}

/// Everything an [`AgentFactory`] needs to pick or build the agent for one task run.
///
/// Mirrors Java `AgentRequest`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRequest {
    /// Task identifier, also used as the agent session id.
    pub task_id: String,
    /// Requested agent id from `POST /tasks`.
    pub agent_id: String,
    /// User input; empty on a resume run, which carries confirmation decisions instead.
    pub input: String,
    /// Parsed from `context.user_id`.
    pub user_id: Option<String>,
    /// Parsed from `context.parent_session_id`.
    pub parent_session_id: Option<String>,
    /// Whether this run resumes a task that was awaiting tool confirmation.
    pub resume: bool,
    /// The submission `context` map exactly as received.
    #[serde(default)]
    pub context: BTreeMap<String, Value>,
}

impl AgentRequest {
    /// Builds a request, mirroring the Java compact constructor (null-safe copies).
    pub fn new(
        task_id: impl Into<String>,
        agent_id: impl Into<String>,
        input: impl Into<String>,
        resume: bool,
        context: Option<BTreeMap<String, Value>>,
    ) -> Self {
        let context = context.unwrap_or_default();
        Self {
            user_id: context_value(&context, "user_id").map(render_scalar),
            parent_session_id: context_value(&context, "parent_session_id").map(render_scalar),
            task_id: task_id.into(),
            agent_id: agent_id.into(),
            input: input.into(),
            resume,
            context,
        }
    }

    /// The raw `context` value for `key`.
    pub fn context_value(&self, key: &str) -> Option<&Value> {
        if key.is_empty() {
            return None;
        }
        self.context.get(key)
    }

    /// The `context` value for `key` as a string.
    pub fn context_string(&self, key: &str) -> Option<String> {
        self.context_value(key).map(render_scalar)
    }

    /// Caller-defined attributes from `context.attributes`, dropping null entries.
    pub fn attributes(&self) -> serde_json::Map<String, Value> {
        self.context
            .get(AgentProtocolConstants::CONTEXT_ATTRIBUTES_FIELD)
            .and_then(|raw| raw.as_object())
            .map(|map| {
                map.iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The `context.attributes` value for `key` as a string.
    pub fn attribute_string(&self, key: &str) -> Option<String> {
        if key.is_empty() {
            return None;
        }
        self.attributes().get(key).map(render_scalar)
    }
}

/// Reads a non-null value from a context map.
fn context_value<'a>(context: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a Value> {
    context.get(key).filter(|value| !value.is_null())
}

/// Renders a JSON scalar as a string, mirroring Java `String.valueOf`.
fn render_scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Hook for shaping the [`RuntimeContext`] of an Agent Protocol task run.
///
/// Mirrors Java `RuntimeContextCustomizer`. Note: Java's `RuntimeContext.Builder` supports
/// arbitrary typed keys; the Rust [`RuntimeContext`] exposes a JSON `metadata` map, so
/// customizers write there.
pub trait RuntimeContextCustomizer: Send + Sync {
    /// Adjusts the context being built for `request`. Called once per run.
    fn customize(&self, request: &AgentRequest, context: &mut RuntimeContext);

    /// Customizer copying the listed `context.attributes` entries to top-level metadata keys
    /// of the same name, skipping framework-reserved names.
    fn flatten(keys: &[&str]) -> FlattenCustomizer
    where
        Self: Sized,
    {
        let allowed: Vec<String> = keys
            .iter()
            .filter(|key| {
                let reserved = AgentProtocolConstants::is_reserved_runtime_context_key(key);
                if reserved {
                    tracing::warn!(
                        "ignoring reserved runtime context key '{key}' in RuntimeContextCustomizer::flatten"
                    );
                }
                !reserved
            })
            .map(|key| (*key).to_string())
            .collect();
        FlattenCustomizer { allowed }
    }
}

/// Customizer produced by [`RuntimeContextCustomizer::flatten`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlattenCustomizer {
    allowed: Vec<String>,
}

impl RuntimeContextCustomizer for FlattenCustomizer {
    fn customize(&self, request: &AgentRequest, context: &mut RuntimeContext) {
        let attributes = request.attributes();
        for key in &self.allowed {
            if let Some(value) = attributes.get(key) {
                context.metadata.insert(key.clone(), value.clone());
            }
        }
    }
}

/// An agent that can execute one Agent Protocol task run.
///
/// Mirrors the `HarnessAgent` role in Java `AgentFactory`. Java runs the agent with a
/// `RuntimeContext`; the Rust [`HarnessAgent::stream_events`] takes
/// [`agentscope_core::agent::StreamOptions`], so the trait keeps the context in its signature
/// for fidelity while [`ProtocolAgent for HarnessAgent`] adapts it.
#[async_trait::async_trait]
pub trait ProtocolAgent: Send + Sync {
    /// Streams the run's events, collected in order.
    async fn stream_events(
        &self,
        messages: Vec<Msg>,
        context: RuntimeContext,
    ) -> Result<Vec<AgentEvent>, AgentProtocolError>;
}

/// [`ProtocolAgent`] implementation backed by a harness agent.
///
/// This is the bridge the Java code gets from `HarnessAgent#streamEvents`: the harness agent
/// owns the model, toolkit and permission engine, while this crate owns the protocol.
/// `StreamOptions::defaults()` is used because the protocol's `detail` filtering is applied on
/// the emitted [`RemoteAgentEvent`]s, not on the agent's stream.
#[async_trait::async_trait]
impl ProtocolAgent for HarnessAgent {
    async fn stream_events(
        &self,
        messages: Vec<Msg>,
        _context: RuntimeContext,
    ) -> Result<Vec<AgentEvent>, AgentProtocolError> {
        use futures::StreamExt;

        let options = agentscope_core::agent::StreamOptions::defaults();
        let mut stream = self.stream_events(messages, options);
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            events.push(item?);
        }
        Ok(events)
    }
}

/// Resolves the [`ProtocolAgent`] that executes an Agent Protocol task.
///
/// Mirrors Java `AgentFactory`: called once per run, with the full [`AgentRequest`].
#[async_trait::async_trait]
pub trait AgentFactory: Send + Sync {
    /// Returns the agent for this run; `None` fails the task with an error status.
    async fn create(&self, request: &AgentRequest) -> Option<Arc<dyn ProtocolAgent>>;
}

/// [`AgentFactory`] that always returns the same agent, ignoring the request.
///
/// Mirrors Java `AgentFactory.fixed(HarnessAgent)`.
#[derive(Clone)]
pub struct FixedAgentFactory {
    agent: Arc<dyn ProtocolAgent>,
}

impl FixedAgentFactory {
    pub fn new(agent: Arc<dyn ProtocolAgent>) -> Self {
        Self { agent }
    }
}

impl std::fmt::Debug for FixedAgentFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FixedAgentFactory")
    }
}

#[async_trait::async_trait]
impl AgentFactory for FixedAgentFactory {
    async fn create(&self, _request: &AgentRequest) -> Option<Arc<dyn ProtocolAgent>> {
        Some(self.agent.clone())
    }
}

/// How much of a task's event stream a subscriber asked for.
///
/// Re-exported so callers need not reach into the harness for the protocol concept.
pub type StreamDetail = RemoteStreamDetail;

/// Event bus abstraction used by the Agent Protocol task SSE endpoints.
///
/// Mirrors Java `AgentProtocolEventBus`: implementations assign a monotonically increasing
/// sequence per task, publish events and replay them after `from_seq`.
pub trait AgentProtocolEventBus: Send + Sync {
    /// Publishes an event, assigning the protocol's `seq`/`taskId` fields.
    fn publish(&self, task_id: &str, event: RemoteAgentEvent) -> RemoteAgentEvent;

    /// Events with `seq > from_seq` already buffered for `task_id`.
    fn replay(&self, task_id: &str, from_seq: i64) -> Vec<RemoteAgentEvent>;

    /// Subscribes to a task's live events; the caller gets the replay plus a receiver.
    fn subscribe(&self, task_id: &str, from_seq: i64) -> EventSubscription;

    /// Completes and releases the event stream for a terminal task.
    fn complete(&self, task_id: &str);

    /// Whether the task's stream has been completed.
    fn is_completed(&self, task_id: &str) -> bool;
}

/// A replay snapshot plus a live receiver, returned by [`AgentProtocolEventBus::subscribe`].
pub struct EventSubscription {
    /// Buffered events with `seq > from_seq`.
    pub replayed: Vec<RemoteAgentEvent>,
    /// Live events published after the subscription.
    pub receiver: Option<tokio::sync::broadcast::Receiver<RemoteAgentEvent>>,
}

/// Per-task replay state, mirroring Java `AgentProtocolTaskEventBus.Channel`.
struct Channel {
    seq: i64,
    replay: VecDeque<RemoteAgentEvent>,
    completed: bool,
    sender: tokio::sync::broadcast::Sender<RemoteAgentEvent>,
}

/// In-process, replay-buffered event bus.
///
/// Mirrors Java `AgentProtocolTaskEventBus`. Applications needing cross-instance streaming can
/// provide a shared [`AgentProtocolEventBus`] implementation instead.
pub struct AgentProtocolTaskEventBus {
    replay_buffer_size: usize,
    channels: Mutex<HashMap<String, Channel>>,
}

impl std::fmt::Debug for AgentProtocolTaskEventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProtocolTaskEventBus")
            .field("replay_buffer_size", &self.replay_buffer_size)
            .finish_non_exhaustive()
    }
}

impl Default for AgentProtocolTaskEventBus {
    fn default() -> Self {
        Self::new(AgentProtocolProperties::default().sse_replay_buffer_size)
    }
}

impl AgentProtocolTaskEventBus {
    /// Creates a bus holding up to `replay_buffer_size` events per task.
    pub fn new(replay_buffer_size: usize) -> Self {
        Self {
            replay_buffer_size,
            channels: Mutex::new(HashMap::new()),
        }
    }

    /// Configured replay buffer size.
    pub fn replay_buffer_size(&self) -> usize {
        self.replay_buffer_size
    }
}

impl AgentProtocolEventBus for AgentProtocolTaskEventBus {
    fn publish(&self, task_id: &str, mut event: RemoteAgentEvent) -> RemoteAgentEvent {
        let mut channels = self.channels.lock().unwrap();
        let buffer_size = self.replay_buffer_size;
        let channel = channels
            .entry(task_id.to_string())
            .or_insert_with(|| Channel {
                seq: 0,
                replay: VecDeque::new(),
                completed: false,
                sender: tokio::sync::broadcast::channel(buffer_size.max(1)).0,
            });

        channel.seq += 1;
        event.seq = channel.seq;
        event.task_id = Some(task_id.to_string());
        if event.timestamp.is_none() {
            event.timestamp = Some(chrono::Utc::now().to_rfc3339());
        }

        if buffer_size > 0 {
            if channel.replay.len() >= buffer_size {
                channel.replay.pop_front();
            }
            channel.replay.push_back(event.clone());
        }
        // A send error only means "no live subscribers", which is normal.
        let _ = channel.sender.send(event.clone());
        event
    }

    fn replay(&self, task_id: &str, from_seq: i64) -> Vec<RemoteAgentEvent> {
        self.channels
            .lock()
            .unwrap()
            .get(task_id)
            .map(|channel| {
                channel
                    .replay
                    .iter()
                    .filter(|event| event.seq > from_seq)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn subscribe(&self, task_id: &str, from_seq: i64) -> EventSubscription {
        let mut channels = self.channels.lock().unwrap();
        let buffer_size = self.replay_buffer_size;
        let channel = channels
            .entry(task_id.to_string())
            .or_insert_with(|| Channel {
                seq: 0,
                replay: VecDeque::new(),
                completed: false,
                sender: tokio::sync::broadcast::channel(buffer_size.max(1)).0,
            });

        let replayed = channel
            .replay
            .iter()
            .filter(|event| event.seq > from_seq)
            .cloned()
            .collect();
        let receiver = if channel.completed {
            None
        } else {
            Some(channel.sender.subscribe())
        };
        EventSubscription { replayed, receiver }
    }

    fn complete(&self, task_id: &str) {
        let mut channels = self.channels.lock().unwrap();
        if let Some(channel) = channels.get_mut(task_id) {
            channel.completed = true;
            channel.replay.clear();
        }
    }

    fn is_completed(&self, task_id: &str) -> bool {
        self.channels
            .lock()
            .unwrap()
            .get(task_id)
            .map(|channel| channel.completed)
            .unwrap_or(false)
    }
}

/// Control-plane persistence for Agent Protocol [`TaskRecord`]s.
///
/// Mirrors Java `ProtocolTaskRepository`: a global `task_id` lookup, independent of which
/// agent ran the task.
///
/// Note: the harness `TaskRepository` trait is session-scoped and models *background subagent*
/// execution, whereas this protocol needs a global task-id lookup keyed only by id, so the two
/// are deliberately distinct; records are the same harness [`TaskRecord`] type either way.
pub trait ProtocolTaskRepository: Send + Sync {
    /// Upserts the given task record.
    fn save(&self, record: &TaskRecord);

    /// Returns the task with the given id, or `None` if unknown.
    fn find(&self, task_id: &str) -> Option<TaskRecord>;
}

/// In-memory [`ProtocolTaskRepository`], the default used by the task store.
#[derive(Debug, Default)]
pub struct InMemoryProtocolTaskRepository {
    records: Mutex<HashMap<String, TaskRecord>>,
}

impl InMemoryProtocolTaskRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored records.
    pub fn len(&self) -> usize {
        self.records.lock().unwrap().len()
    }

    /// Whether no records are stored.
    pub fn is_empty(&self) -> bool {
        self.records.lock().unwrap().is_empty()
    }

    /// All records with `status == Pending` or `Running`.
    pub fn active_records(&self) -> Vec<TaskRecord> {
        self.records
            .lock()
            .unwrap()
            .values()
            .filter(|record| !record.status.is_terminal())
            .cloned()
            .collect()
    }
}

impl ProtocolTaskRepository for InMemoryProtocolTaskRepository {
    fn save(&self, record: &TaskRecord) {
        self.records
            .lock()
            .unwrap()
            .insert(record.task_id.clone(), record.clone());
    }

    fn find(&self, task_id: &str) -> Option<TaskRecord> {
        self.records.lock().unwrap().get(task_id).cloned()
    }
}

/// In-memory execution handles with control-plane [`TaskRecord`] persistence.
///
/// Mirrors Java `AgentProtocolTaskStore`. Two deliberate adaptations, both recorded in the
/// module tracker:
///
/// 1. Java runs each task on a cached thread pool and returns immediately from `submit`;
///    Rust has no thread pool here, so the run is `tokio::spawn`ed instead.
/// 2. Java's `waitFor` blocks on a `CompletableFuture`; here it polls the persisted record,
///    which keeps the terminal-status contract while staying runtime-agnostic.
pub struct AgentProtocolTaskStore {
    agent_factory: Arc<dyn AgentFactory>,
    task_repository: Arc<dyn ProtocolTaskRepository>,
    event_bus: Arc<dyn AgentProtocolEventBus>,
    properties: AgentProtocolProperties,
    runtime_context_customizers: Vec<Arc<dyn RuntimeContextCustomizer>>,
    running: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for AgentProtocolTaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProtocolTaskStore")
            .field("properties", &self.properties)
            .finish_non_exhaustive()
    }
}

impl AgentProtocolTaskStore {
    /// Creates a store, falling back to the in-process event bus and default properties.
    pub fn new(
        agent_factory: Arc<dyn AgentFactory>,
        task_repository: Arc<dyn ProtocolTaskRepository>,
        event_bus: Option<Arc<dyn AgentProtocolEventBus>>,
        properties: Option<AgentProtocolProperties>,
        runtime_context_customizers: Vec<Arc<dyn RuntimeContextCustomizer>>,
    ) -> Self {
        let properties = properties.unwrap_or_default();
        let event_bus = event_bus.unwrap_or_else(|| {
            Arc::new(AgentProtocolTaskEventBus::new(
                properties.sse_replay_buffer_size,
            ))
        });
        Self {
            agent_factory,
            task_repository,
            event_bus,
            properties,
            runtime_context_customizers,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// Convenience constructor mirroring Java `AgentProtocolTaskStore(AgentFactory, repo)`.
    pub fn with_repository(
        agent_factory: Arc<dyn AgentFactory>,
        task_repository: Arc<dyn ProtocolTaskRepository>,
    ) -> Self {
        Self::new(agent_factory, task_repository, None, None, Vec::new())
    }

    /// Builds a store whose control-plane records persist under
    /// [`AgentProtocolProperties::resolved_task_store_path`].
    ///
    /// Mirrors Java's wiring of `new WorkspaceProtocolTaskRepository(taskStorePath)`: the
    /// `task_store_path` property only has an effect through this constructor, so a store built
    /// with [`new`](Self::new) keeps records in memory.
    pub fn with_workspace_task_store(
        agent_factory: Arc<dyn AgentFactory>,
        properties: AgentProtocolProperties,
        runtime_context_customizers: Vec<Arc<dyn RuntimeContextCustomizer>>,
    ) -> Self {
        let task_repository: Arc<dyn ProtocolTaskRepository> = Arc::new(
            WorkspaceProtocolTaskRepository::new(properties.resolved_task_store_path()),
        );
        Self::new(
            agent_factory,
            task_repository,
            None,
            Some(properties),
            runtime_context_customizers,
        )
    }

    /// The event bus backing the SSE endpoint.
    pub fn event_bus(&self) -> &Arc<dyn AgentProtocolEventBus> {
        &self.event_bus
    }

    /// The task repository.
    pub fn task_repository(&self) -> &Arc<dyn ProtocolTaskRepository> {
        &self.task_repository
    }

    /// The active configuration.
    pub fn properties(&self) -> &AgentProtocolProperties {
        &self.properties
    }

    /// Starts a background task, mirroring Java `submit`.
    ///
    /// Idempotent while running: if a non-terminal task with the same id already exists, this
    /// returns without starting a duplicate run. Fails when the task id already finished.
    pub fn submit(
        &self,
        task_id: &str,
        agent_id: &str,
        input: &str,
        context: Option<BTreeMap<String, Value>>,
    ) -> Result<(), AgentProtocolError> {
        if let Some(existing) = self.task_repository.find(task_id) {
            if existing.status.is_terminal() {
                return Err(AgentProtocolError::InvalidRequest(format!(
                    "task_id already finished: {task_id}"
                )));
            }
        }
        {
            let running = self.running.lock().unwrap();
            if let Some(handle) = running.get(task_id) {
                if !handle.is_finished() {
                    return Ok(());
                }
            }
        }

        let request = AgentRequest::new(
            task_id,
            if agent_id.is_empty() {
                "default"
            } else {
                agent_id
            },
            input,
            false,
            context,
        );

        let mut pending = TaskRecord::new(
            task_id,
            request.agent_id.clone(),
            AgentProtocolConstants::PROTOCOL_AGENT_ID,
            AgentProtocolConstants::PROTOCOL_SESSION_ID,
        );
        pending.status = TaskStatus::Pending;
        self.task_repository.save(&pending);

        self.spawn_run(request, None);
        Ok(())
    }

    /// Resumes a task paused for HITL confirmation with the given decisions.
    ///
    /// Mirrors Java `resume`.
    pub fn resume(
        &self,
        task_id: &str,
        decisions: Vec<RemoteConfirmDecision>,
    ) -> Result<(), AgentProtocolError> {
        if !self.properties.hitl_enabled {
            return Err(AgentProtocolError::InvalidRequest(
                "HITL is disabled on this server".to_string(),
            ));
        }
        let record = self
            .task_repository
            .find(task_id)
            .ok_or_else(|| AgentProtocolError::TaskNotFound(task_id.to_string()))?;
        if !record.awaiting_confirm {
            return Err(AgentProtocolError::InvalidRequest(format!(
                "task is not awaiting confirmation: {task_id}"
            )));
        }
        {
            let running = self.running.lock().unwrap();
            if let Some(handle) = running.get(task_id) {
                if !handle.is_finished() {
                    return Err(AgentProtocolError::InvalidRequest(format!(
                        "task is already running: {task_id}"
                    )));
                }
            }
        }

        let pending = record.pending_confirms.clone().unwrap_or_default();
        let confirm_results = to_confirm_results(&pending, &decisions);

        let mut updated = record.clone();
        updated.awaiting_confirm = false;
        updated.pending_confirms = None;
        updated.status = TaskStatus::Running;
        updated.touch();
        self.task_repository.save(&updated);

        let mut context = BTreeMap::new();
        // The submission context is not persisted by the in-memory repository; Java keeps it in
        // a side map. Rebuild the identity fields from the record instead.
        context.insert(
            "user_id".to_string(),
            Value::String(updated.parent_session_id.clone()),
        );

        let request = AgentRequest::new(
            task_id,
            updated.sub_agent_id.clone(),
            "",
            true,
            Some(context),
        );
        self.spawn_run(request, Some(confirm_results));
        Ok(())
    }

    /// Cancels a task, mirroring Java `cancel`.
    pub fn cancel(&self, task_id: &str) -> Result<(), AgentProtocolError> {
        let Some(mut record) = self.task_repository.find(task_id) else {
            return Err(AgentProtocolError::TaskNotFound(task_id.to_string()));
        };
        if record.status.is_terminal() {
            return Ok(());
        }
        record.status = TaskStatus::Cancelled;
        record.cancel_requested = true;
        record.awaiting_confirm = false;
        record.touch();
        self.task_repository.save(&record);
        self.event_bus.complete(task_id);
        Ok(())
    }

    /// A JSON snapshot of the task, mirroring Java `snapshot`.
    pub fn snapshot(&self, task_id: &str) -> Option<Value> {
        let record = self.task_repository.find(task_id)?;
        Some(snapshot_value(&record))
    }

    /// Waits for a task to reach a terminal status, then returns its snapshot.
    ///
    /// Mirrors Java `waitFor(taskId, timeoutMs)`, which blocks on the run's future.
    pub async fn wait_for(
        &self,
        task_id: &str,
        timeout_ms: u64,
    ) -> Result<Value, AgentProtocolError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let record = self
                .task_repository
                .find(task_id)
                .ok_or_else(|| AgentProtocolError::TaskNotFound(task_id.to_string()))?;
            if record.status.is_terminal() {
                return Ok(snapshot_value(&record));
            }
            if std::time::Instant::now() >= deadline {
                return Err(AgentProtocolError::Timeout(format!(
                    "task {task_id} did not finish within {timeout_ms}ms"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Spawns the run, tracking the handle so `submit`/`resume` stay idempotent.
    fn spawn_run(&self, request: AgentRequest, confirm_results: Option<Vec<ConfirmResult>>) {
        let factory = self.agent_factory.clone();
        let repository = self.task_repository.clone();
        let bus = self.event_bus.clone();
        let properties = self.properties.clone();
        let customizers = self.runtime_context_customizers.clone();
        let task_id = request.task_id.clone();

        let handle = tokio::spawn(async move {
            run_task(
                factory,
                repository,
                bus,
                properties,
                customizers,
                request,
                confirm_results,
            )
            .await;
        });
        self.running.lock().unwrap().insert(task_id, handle);
    }
}

/// Confirmation outcome for one pending tool call, mirroring Java `ConfirmResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmResult {
    /// Whether the user approved the tool call.
    pub approved: bool,
    /// The tool call the decision applies to.
    pub tool_call: Option<ToolUseBlock>,
    /// Optional message accompanying the decision.
    pub message: String,
}

/// Builds confirm results from a task's pending confirms and the user's decisions.
///
/// Mirrors Java `AgentProtocolTaskStore#toConfirmResults`: unlisted tool calls default to
/// denied.
pub fn to_confirm_results(
    pending: &[RemotePendingConfirm],
    decisions: &[RemoteConfirmDecision],
) -> Vec<ConfirmResult> {
    let mut by_id: HashMap<&str, bool> = HashMap::new();
    for decision in decisions {
        if let Some(id) = decision.tool_call_id.as_deref() {
            by_id.insert(id, decision.approved);
        }
    }

    pending
        .iter()
        .map(|confirm| {
            let tool_call_id = confirm.tool_call_id.clone().unwrap_or_default();
            let approved = by_id.get(tool_call_id.as_str()).copied().unwrap_or(false);
            let input = confirm
                .tool_input_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            let tool_call = ToolUseBlock {
                id: tool_call_id,
                name: confirm.tool_name.clone().unwrap_or_default(),
                input,
                content: None,
                metadata: Default::default(),
                state: ToolCallState::Asking,
            };
            ConfirmResult {
                approved,
                tool_call: Some(tool_call),
                message: String::new(),
            }
        })
        .collect()
}

/// One task run: build the context, execute the agent, project events and persist the outcome.
///
/// Mirrors Java `AgentProtocolTaskStore#runAgent`.
#[allow(clippy::too_many_arguments)]
async fn run_task(
    factory: Arc<dyn AgentFactory>,
    repository: Arc<dyn ProtocolTaskRepository>,
    bus: Arc<dyn AgentProtocolEventBus>,
    properties: AgentProtocolProperties,
    customizers: Vec<Arc<dyn RuntimeContextCustomizer>>,
    request: AgentRequest,
    confirm_results: Option<Vec<ConfirmResult>>,
) {
    let task_id = request.task_id.clone();
    let agent_id = request.agent_id.clone();

    if let Some(mut record) = repository.find(&task_id) {
        record.status = TaskStatus::Running;
        record.touch();
        repository.save(&record);
    }

    let context = build_runtime_context(&request, &customizers);

    let mut msg_builder = Msg::builder()
        .role(MsgRole::User)
        .text_content(request.input.clone());
    if let Some(results) = confirm_results.as_ref().filter(|r| !r.is_empty()) {
        msg_builder = msg_builder.metadata_entry(
            Msg::METADATA_CONFIRM_RESULTS,
            confirm_results_to_json(results),
        );
    }
    let msg = msg_builder.build();

    let Some(agent) = factory.create(&request).await else {
        fail_task(
            &repository,
            &bus,
            &task_id,
            &agent_id,
            format!("AgentFactory returned no agent for agent_id={agent_id}"),
        );
        return;
    };

    let detail = StreamDetail::parse(request.context_string("detail").as_deref());

    let events = match agent.stream_events(vec![msg], context).await {
        Ok(events) => events,
        Err(err) => {
            fail_task(&repository, &bus, &task_id, &agent_id, err.to_string());
            return;
        }
    };

    let mut result_text = String::new();
    let mut last_result: Option<Msg> = None;

    for event in &events {
        if let Some(msg) = agent_result_msg(event) {
            result_text = render_message_text(&msg, true);
            last_result = Some(msg);
        }
        if !properties.streaming_enabled {
            continue;
        }
        // Reuse the harness codec instead of inventing a parallel projection.
        if let Some(mut dto) = from_agent_event(event) {
            dto.agent_id = Some(agent_id.clone());
            if matches_detail_event(&dto, Some(detail.wire_value())) {
                bus.publish(&task_id, dto);
            }
        }
    }

    if let Some(reply) = last_result {
        if properties.hitl_enabled && is_permission_asking(&reply) {
            let pending = extract_pending_confirms(&reply);
            mark_awaiting_confirm(
                &repository,
                &bus,
                &task_id,
                &agent_id,
                record_agent_id(&repository, &task_id),
                pending,
            );
            return;
        }
    }

    let mut record = repository.find(&task_id).unwrap_or_else(|| {
        TaskRecord::new(
            &task_id,
            &agent_id,
            AgentProtocolConstants::PROTOCOL_AGENT_ID,
            AgentProtocolConstants::PROTOCOL_SESSION_ID,
        )
    });
    record.status = TaskStatus::Completed;
    record.result = Some(result_text);
    record.error_message = None;
    record.awaiting_confirm = false;
    record.pending_confirms = None;
    record.touch();
    repository.save(&record);
    publish_status(&bus, &task_id, &agent_id, "success", None);
    bus.complete(&task_id);
}

/// The record's agent id, falling back to the default.
fn record_agent_id(repository: &Arc<dyn ProtocolTaskRepository>, task_id: &str) -> String {
    repository
        .find(task_id)
        .map(|record| record.sub_agent_id)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Records the failure and publishes an error event.
fn fail_task(
    repository: &Arc<dyn ProtocolTaskRepository>,
    bus: &Arc<dyn AgentProtocolEventBus>,
    task_id: &str,
    agent_id: &str,
    error: String,
) {
    let mut record = repository.find(task_id).unwrap_or_else(|| {
        TaskRecord::new(
            task_id,
            agent_id,
            AgentProtocolConstants::PROTOCOL_AGENT_ID,
            AgentProtocolConstants::PROTOCOL_SESSION_ID,
        )
    });
    record.status = TaskStatus::Failed;
    record.error_message = Some(error.clone());
    record.awaiting_confirm = false;
    record.pending_confirms = None;
    record.touch();
    repository.save(&record);

    let mut event = RemoteAgentEvent {
        kind: Some(RemoteEventType::RunError),
        agent_id: Some(agent_id.to_string()),
        error: Some(error),
        status: Some("error".to_string()),
        ..Default::default()
    };
    event.task_id = Some(task_id.to_string());
    bus.publish(task_id, event);
    bus.complete(task_id);
}

/// Marks the task as paused awaiting confirmation, mirroring Java `markAwaitingConfirm`.
fn mark_awaiting_confirm(
    repository: &Arc<dyn ProtocolTaskRepository>,
    bus: &Arc<dyn AgentProtocolEventBus>,
    task_id: &str,
    agent_id: &str,
    record_agent: String,
    pending: Vec<RemotePendingConfirm>,
) {
    let mut record = repository.find(task_id).unwrap_or_else(|| {
        TaskRecord::new(
            task_id,
            &record_agent,
            AgentProtocolConstants::PROTOCOL_AGENT_ID,
            AgentProtocolConstants::PROTOCOL_SESSION_ID,
        )
    });
    record.status = TaskStatus::Running;
    record.awaiting_confirm = true;
    record.pending_confirms = Some(pending.clone());
    record.touch();
    repository.save(&record);

    let mut event = RemoteAgentEvent {
        kind: Some(RemoteEventType::RequireConfirm),
        agent_id: Some(agent_id.to_string()),
        pending_confirms: Some(pending),
        status: Some(AWAITING_CONFIRM_SENTINEL.trim_matches('_').to_string()),
        ..Default::default()
    };
    event.task_id = Some(task_id.to_string());
    bus.publish(task_id, event);
}

/// Publishes a `STATUS` event, mirroring Java `publishStatus`.
fn publish_status(
    bus: &Arc<dyn AgentProtocolEventBus>,
    task_id: &str,
    agent_id: &str,
    status: &str,
    error: Option<String>,
) {
    let mut event = RemoteAgentEvent {
        kind: Some(RemoteEventType::Status),
        agent_id: Some(agent_id.to_string()),
        status: Some(status.to_string()),
        error,
        ..Default::default()
    };
    event.task_id = Some(task_id.to_string());
    bus.publish(task_id, event);
}

/// Builds the run's [`RuntimeContext`], mirroring Java `buildRuntimeContext`.
fn build_runtime_context(
    request: &AgentRequest,
    customizers: &[Arc<dyn RuntimeContextCustomizer>],
) -> RuntimeContext {
    let mut context = RuntimeContext::new().with_session_id(request.task_id.clone());
    if let Some(user_id) = request.user_id.as_deref() {
        context = context.with_user_id(user_id);
    }
    context = context.with_agent_id(request.agent_id.clone());

    let attributes = request.attributes();
    if !attributes.is_empty() {
        context.metadata.insert(
            AgentProtocolConstants::RUNTIME_CONTEXT_ATTRIBUTES_KEY.to_string(),
            Value::Object(attributes),
        );
    }
    for customizer in customizers {
        customizer.customize(request, &mut context);
    }
    context
}

/// Extracts the source [`Msg`] from an `AGENT_RESULT` event.
///
/// The harness [`agentscope_core::event::AgentEvent`] carries the result as a serialized `Msg`
/// under the `result` field, matching the harness codec's expectations.
fn agent_result_msg(event: &AgentEvent) -> Option<Msg> {
    let AgentEvent::AgentResult(data) = event else {
        return None;
    };
    let value = data.fields.get("result")?;
    serde_json::from_value::<Msg>(value.clone()).ok()
}

/// Whether a reply signals that the permission engine is asking the user.
fn is_permission_asking(msg: &Msg) -> bool {
    matches!(
        msg.metadata
            .get(Msg::METADATA_GENERATE_REASON)
            .and_then(
                |value| serde_json::from_value::<agentscope_core::message::GenerateReason>(
                    value.clone()
                )
                .ok()
            ),
        Some(agentscope_core::message::GenerateReason::PermissionAsking)
    )
}

/// Collects the tool calls awaiting confirmation, mirroring Java `extractPendingConfirms`.
///
/// When no tool call carries the `Asking` state (an older agent that does not stamp it), every
/// tool-use block is reported instead.
pub fn extract_pending_confirms(msg: &Msg) -> Vec<RemotePendingConfirm> {
    let tool_uses: Vec<&ToolUseBlock> = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse(tool) => Some(tool),
            _ => None,
        })
        .collect();

    let asking: Vec<&ToolUseBlock> = tool_uses
        .iter()
        .copied()
        .filter(|tool| tool.state == ToolCallState::Asking)
        .collect();
    let selected = if asking.is_empty() { tool_uses } else { asking };

    selected
        .into_iter()
        .map(|tool| RemotePendingConfirm {
            tool_call_id: Some(tool.id.clone()),
            tool_name: Some(tool.name.clone()),
            tool_input_json: Some(Value::Object(tool.input.clone()).to_string()),
        })
        .collect()
}

/// Serializes confirm results for the `Msg` metadata, mirroring Java's metadata payload.
fn confirm_results_to_json(results: &[ConfirmResult]) -> Value {
    Value::Array(
        results
            .iter()
            .map(|result| {
                let mut object = serde_json::Map::new();
                object.insert("approved".to_string(), Value::Bool(result.approved));
                if !result.message.is_empty() {
                    object.insert("message".to_string(), Value::String(result.message.clone()));
                }
                if let Some(tool) = &result.tool_call {
                    object.insert(
                        "toolCall".to_string(),
                        serde_json::json!({
                            "id": tool.id,
                            "name": tool.name,
                            "input": Value::Object(tool.input.clone()),
                        }),
                    );
                }
                Value::Object(object)
            })
            .collect(),
    )
}

/// Maps a record onto the wire status vocabulary of Java `AgentProtocolTaskStore#mapStatus`.
///
/// `awaiting_confirm` wins over the underlying status: a parked run still carries
/// [`TaskStatus::Running`] internally, but both Java and this repository's
/// `RemoteTaskStatus` only recognize the pause through this word. The previous
/// `TaskStatus::as_str()` output (`Pending`/`Running`/`Completed`/`Failed`/`Cancelled`) was never
/// understood by the harness client, so `is_terminal_success()` could never become true.
fn map_status(record: &TaskRecord) -> &'static str {
    if record.awaiting_confirm {
        return AWAITING_CONFIRM_SENTINEL.trim_matches('_');
    }
    match record.status {
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Completed => "success",
        TaskStatus::Failed => "error",
        TaskStatus::Cancelled => "cancelled",
    }
}

/// Snapshot shape returned by [`AgentProtocolTaskStore::snapshot`].
///
/// Mirrors the map built by Java `AgentProtocolTaskStore#snapshot`. Java's snake-case keys are
/// emitted alongside the camelCase spellings this crate has always served, so a client ported from
/// either side finds the key it reads.
pub fn snapshot_value(record: &TaskRecord) -> Value {
    serde_json::json!({
        "task_id": record.task_id,
        "agent_id": record.sub_agent_id,
        "status": map_status(record),
        "result": record.result,
        "error": record.error_message,
        "pending_confirms": record.pending_confirms,
        "last_event_seq": record.last_event_seq,
        "cancel_requested": record.cancel_requested,
        "awaiting_confirm": record.awaiting_confirm,
        "taskId": record.task_id,
        "agentId": record.sub_agent_id,
        "awaitingConfirm": record.awaiting_confirm,
        "pendingConfirms": record.pending_confirms,
        "lastEventSeq": record.last_event_seq,
        "cancelRequested": record.cancel_requested,
        "version": record.version,
    })
}

/// Builds the `/tasks` router, mirroring `AgentProtocolController`.
///
/// | Method | Path | Controller method |
/// |---|---|---|
/// | `POST` | `/tasks` | `submit` |
/// | `GET` | `/tasks/{taskId}` | `get` |
/// | `GET` | `/tasks/{taskId}/wait` | `waitFor` |
/// | `POST` | `/tasks/{taskId}/cancel` | `cancel` |
/// | `GET` | `/tasks/{taskId}/events` | `events` (SSE) |
/// | `POST` | `/tasks/{taskId}/resume` | `resume` |
pub fn router(store: Arc<AgentProtocolTaskStore>) -> axum::Router {
    use axum::routing::{get, post};

    axum::Router::new()
        .route("/tasks", post(submit_task))
        .route("/tasks/{task_id}", get(get_task))
        .route("/tasks/{task_id}/wait", get(wait_task))
        .route("/tasks/{task_id}/cancel", post(cancel_task))
        .route("/tasks/{task_id}/events", get(task_events))
        .route("/tasks/{task_id}/resume", post(resume_task))
        .with_state(store)
}

/// JSON error body for the task endpoints.
fn api_error(
    status: axum::http::StatusCode,
    message: impl Into<String>,
) -> axum::response::Response {
    let body = serde_json::json!({ "error": message.into() });
    (status, axum::Json(body)).into_response()
}

fn status_for(err: &AgentProtocolError) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    match err {
        AgentProtocolError::TaskNotFound(_) => StatusCode::NOT_FOUND,
        AgentProtocolError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
        AgentProtocolError::InvalidRequest(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// `POST /tasks`.
async fn submit_task(
    axum::extract::State(store): axum::extract::State<Arc<AgentProtocolTaskStore>>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    let task_id = body
        .get("task_id")
        .or_else(|| body.get("taskId"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if task_id.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "task_id is required");
    }
    let agent_id = body
        .get("agent_id")
        .or_else(|| body.get("agentId"))
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let input = body
        .get("input")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let context = body
        .get("context")
        .and_then(|v| v.as_object())
        .map(|object| object.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

    match store.submit(task_id, agent_id, input, context) {
        Ok(()) => axum::Json(snapshot_for(&store, task_id)).into_response(),
        Err(err) => api_error(status_for(&err), err.to_string()),
    }
}

/// `GET /tasks/{taskId}`.
async fn get_task(
    axum::extract::State(store): axum::extract::State<Arc<AgentProtocolTaskStore>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> axum::response::Response {
    match store.snapshot(&task_id) {
        Some(snapshot) => axum::Json(snapshot).into_response(),
        None => api_error(axum::http::StatusCode::NOT_FOUND, "task not found"),
    }
}

/// `GET /tasks/{taskId}/wait?timeout_seconds=`.
async fn wait_task(
    axum::extract::State(store): axum::extract::State<Arc<AgentProtocolTaskStore>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> axum::response::Response {
    let timeout_ms = wait_timeout_ms(&params, store.properties().sse_timeout_ms);
    match store.wait_for(&task_id, timeout_ms).await {
        Ok(snapshot) => axum::Json(snapshot).into_response(),
        Err(err) => api_error(status_for(&err), err.to_string()),
    }
}

/// Resolves the `/wait` deadline in milliseconds.
///
/// Java `AgentProtocolController#waitFor` takes `timeout_seconds` (seconds, default 7200), which is
/// what this repository's harness client sends; `timeoutMs` stays supported as the legacy Rust
/// spelling. Before 2026-10-31 only `timeoutMs` was read, so a client's 30 s wait silently fell
/// back to `sseTimeoutMs` (3 h).
fn wait_timeout_ms(params: &HashMap<String, String>, default_ms: u64) -> u64 {
    if let Some(seconds) = params
        .get("timeout_seconds")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    {
        return seconds.saturating_mul(1_000);
    }
    params
        .get("timeoutMs")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(default_ms)
}

/// `POST /tasks/{taskId}/cancel`.
///
/// Java `AgentProtocolController#cancel` answers `200` with an empty body
/// (`ResponseEntity.ok().build()`); this server used to answer `202`, which some clients read as
/// "accepted but not applied yet".
async fn cancel_task(
    axum::extract::State(store): axum::extract::State<Arc<AgentProtocolTaskStore>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> axum::response::Response {
    match store.cancel(&task_id) {
        Ok(()) => axum::http::StatusCode::OK.into_response(),
        Err(err) => api_error(status_for(&err), err.to_string()),
    }
}

/// `POST /tasks/{taskId}/resume`.
async fn resume_task(
    axum::extract::State(store): axum::extract::State<Arc<AgentProtocolTaskStore>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    let decisions = parse_decisions(&body);
    match store.resume(&task_id, decisions) {
        Ok(()) => axum::Json(snapshot_for(&store, &task_id)).into_response(),
        Err(err) => api_error(status_for(&err), err.to_string()),
    }
}

/// One HITL decision as received on the wire.
///
/// Java `AgentProtocolController#parseDecisions` accepts the camelCase `toolCallId` and the snake
/// `tool_call_id` spelling; the harness `RemoteConfirmDecision` only knows the former, so the body
/// is decoded here and converted.
#[derive(serde::Deserialize)]
struct DecisionBody {
    #[serde(default, alias = "toolCallId")]
    tool_call_id: Option<String>,
    #[serde(default)]
    approved: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// Decodes the `decisions` field of a resume body, tolerating an unknown shape like Java does.
fn parse_decisions(body: &Value) -> Vec<RemoteConfirmDecision> {
    let decisions: Vec<DecisionBody> = body
        .get("decisions")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    decisions
        .into_iter()
        .map(|decision| RemoteConfirmDecision {
            tool_call_id: decision.tool_call_id,
            approved: decision.approved,
            reason: decision.reason,
        })
        .collect()
}

/// `GET /tasks/{taskId}/events`, an SSE stream of `RemoteAgentEvent` frames.
///
/// The frame format is Java's `ServerSentEvent` encoding:
/// `id: <seq>\nevent: <TYPE>\ndata: <json>\n\n`.
async fn task_events(
    axum::extract::State(store): axum::extract::State<Arc<AgentProtocolTaskStore>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    if store.snapshot(&task_id).is_none() {
        return api_error(axum::http::StatusCode::NOT_FOUND, "task not found");
    }
    let from_seq = replay_from_seq(
        &params,
        headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok()),
    );

    let body =
        axum::body::Body::from_stream(event_stream(store.event_bus().clone(), task_id, from_seq));
    axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Resolves the SSE replay cursor.
///
/// Mirrors Java `AgentProtocolController#events`: the `from_seq` query parameter wins when it is
/// `> 0`, then the `Last-Event-ID` header (sent by an `EventSource` reconnect). The legacy Rust
/// `fromSeq` query spelling stays supported as a last resort. Before 2026-10-31 only `fromSeq` was
/// read, so a reconnecting client always replayed from `0`.
fn replay_from_seq(params: &HashMap<String, String>, last_event_id: Option<&str>) -> i64 {
    if let Some(seq) = params
        .get("from_seq")
        .and_then(|raw| raw.trim().parse::<i64>().ok())
    {
        if seq > 0 {
            return seq;
        }
    }
    if let Some(raw) = last_event_id {
        let raw = raw.trim();
        if !raw.is_empty() {
            if let Ok(seq) = raw.parse::<i64>() {
                return seq;
            }
        }
    }
    params
        .get("fromSeq")
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// Builds the SSE byte stream: the buffered replay first, then the live events.
fn event_stream(
    bus: Arc<dyn AgentProtocolEventBus>,
    task_id: String,
    from_seq: i64,
) -> futures::stream::BoxStream<'static, Result<Vec<u8>, std::convert::Infallible>> {
    let subscription = bus.subscribe(&task_id, from_seq);
    let replayed = futures::stream::iter(
        subscription
            .replayed
            .into_iter()
            .map(|event| Ok::<Vec<u8>, std::convert::Infallible>(sse_frame(&event)))
            .collect::<Vec<_>>(),
    );

    let live = match subscription.receiver {
        Some(mut receiver) => futures::stream::unfold(receiver, |mut receiver| async move {
            match receiver.recv().await {
                Ok(event) => Some((Ok(sse_frame(&event)), receiver)),
                Err(_) => None,
            }
        })
        .boxed(),
        None => futures::stream::empty().boxed(),
    };

    replayed.chain(live).boxed()
}

/// Encodes one event as an SSE frame.
///
/// Java `toSse` writes `id`, `event` (`RemoteEventType.name()`, or `message` when the event has no
/// type) and `data`; the `event:` line lets an `EventSource` dispatch to a named listener.
fn sse_frame(event: &RemoteAgentEvent) -> Vec<u8> {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    let name = event
        .kind
        .and_then(|kind| serde_json::to_value(kind).ok())
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "message".to_string());
    format!("id: {}\nevent: {}\ndata: {}\n\n", event.seq, name, data).into_bytes()
}

/// Reads the current snapshot, or an empty object when the task vanished mid-request.
///
/// Java answers `submit`/`resume` with `{task_id, status}` (`AgentProtocolController#submit`
/// `:79-84`, `#resume` `:128-147`); this server keeps the richer snapshot, and
/// [`snapshot_value`] already carries Java's snake `task_id` next to the protocol's `taskId`. The
/// insert below only matters for the vanished-task fallback.
fn snapshot_for(store: &Arc<AgentProtocolTaskStore>, task_id: &str) -> Value {
    let mut snapshot = store
        .snapshot(task_id)
        .unwrap_or_else(|| serde_json::json!({ "taskId": task_id }));
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("task_id".to_string(), Value::String(task_id.to_string()));
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::EventData;
    use agentscope_core::message::{DocumentBlock, Source};
    use agentscope_harness::agent::subagent::protocol::RemoteEventType;
    use std::collections::BTreeMap;

    /// Runner emitting a single `AGENT_RESULT` whose `result` field is a serialized `Msg`.
    struct ResultAgent {
        reply: Msg,
    }

    #[async_trait::async_trait]
    impl ProtocolAgent for ResultAgent {
        async fn stream_events(
            &self,
            _messages: Vec<Msg>,
            _context: RuntimeContext,
        ) -> Result<Vec<AgentEvent>, AgentProtocolError> {
            let mut data = EventData::default();
            data.fields.insert(
                "result".to_string(),
                serde_json::to_value(&self.reply).unwrap(),
            );
            Ok(vec![AgentEvent::AgentResult(data)])
        }
    }

    /// Runner that pauses for HITL confirmation.
    struct AskingAgent;

    #[async_trait::async_trait]
    impl ProtocolAgent for AskingAgent {
        async fn stream_events(
            &self,
            _messages: Vec<Msg>,
            _context: RuntimeContext,
        ) -> Result<Vec<AgentEvent>, AgentProtocolError> {
            let mut reply = Msg::builder()
                .role(MsgRole::Assistant)
                .content(vec![ContentBlock::ToolUse(ToolUseBlock {
                    id: "call-1".to_string(),
                    name: "write_file".to_string(),
                    input: serde_json::Map::new(),
                    content: None,
                    metadata: Default::default(),
                    state: ToolCallState::Asking,
                })])
                .generate_reason(agentscope_core::message::GenerateReason::PermissionAsking)
                .build();
            reply.metadata.insert(
                Msg::METADATA_GENERATE_REASON.to_string(),
                Value::String("PERMISSION_ASKING".to_string()),
            );
            let mut data = EventData::default();
            data.fields
                .insert("result".to_string(), serde_json::to_value(&reply).unwrap());
            Ok(vec![AgentEvent::AgentResult(data)])
        }
    }

    fn reply_msg(text: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::Assistant)
            .text_content(text)
            .build()
    }

    fn store_for(agent: Arc<dyn ProtocolAgent>) -> Arc<AgentProtocolTaskStore> {
        Arc::new(AgentProtocolTaskStore::with_repository(
            Arc::new(FixedAgentFactory::new(agent)),
            Arc::new(InMemoryProtocolTaskRepository::new()),
        ))
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn reserved_runtime_context_keys_are_detected() {
        assert!(AgentProtocolConstants::is_reserved_runtime_context_key(
            "agentId"
        ));
        assert!(AgentProtocolConstants::is_reserved_runtime_context_key(
            "outboundAddress"
        ));
        assert!(AgentProtocolConstants::is_reserved_runtime_context_key(
            "agentscope.x"
        ));
        assert!(AgentProtocolConstants::is_reserved_runtime_context_key(
            "agui.thread"
        ));
        assert!(AgentProtocolConstants::is_reserved_runtime_context_key(
            "io.agentscope.y"
        ));
        assert!(AgentProtocolConstants::is_reserved_runtime_context_key(
            "  "
        ));
        assert!(!AgentProtocolConstants::is_reserved_runtime_context_key(
            "tenant"
        ));
    }

    #[test]
    fn agent_request_parses_identity_and_attributes() {
        let context: BTreeMap<String, Value> = serde_json::from_value(serde_json::json!({
            "user_id": "u-1",
            "parent_session_id": "s-1",
            "attributes": { "tenant": "acme", "blank": null }
        }))
        .unwrap();
        let request = AgentRequest::new("t-1", "agent-1", "hello", false, Some(context));

        assert_eq!(request.user_id.as_deref(), Some("u-1"));
        assert_eq!(request.parent_session_id.as_deref(), Some("s-1"));
        assert_eq!(request.context_string("user_id").as_deref(), Some("u-1"));
        assert!(request.context_value("").is_none());
        assert_eq!(request.attribute_string("tenant").as_deref(), Some("acme"));
        // Null attribute values are dropped, mirroring the Java LinkedHashMap filter.
        assert_eq!(request.attributes().len(), 1);
    }

    #[test]
    fn properties_default_and_resolve_task_store_path() {
        let props = AgentProtocolProperties::default();
        assert!(!props.enabled);
        assert!(props.streaming_enabled);
        assert!(props.hitl_enabled);
        assert_eq!(props.sse_replay_buffer_size, 256);
        assert!(props.resolved_task_store_path().ends_with("agent-protocol"));

        let configured = AgentProtocolProperties::builder()
            .task_store_path("C:/tmp/tasks")
            .hitl_enabled(false)
            .build();
        assert_eq!(configured.resolved_task_store_path(), "C:/tmp/tasks");
        assert!(!configured.hitl_enabled);
    }

    #[test]
    fn event_bus_assigns_monotonic_seq_and_replays_after_from_seq() {
        let bus = AgentProtocolTaskEventBus::new(4);
        let event = RemoteAgentEvent {
            kind: Some(RemoteEventType::TextDelta),
            text: Some("a".to_string()),
            ..Default::default()
        };
        let first = bus.publish("t-1", event.clone());
        let second = bus.publish("t-1", event.clone());
        let third = bus.publish("t-1", event);

        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(third.seq, 3);
        assert_eq!(first.task_id.as_deref(), Some("t-1"));
        assert!(first.timestamp.is_some());

        assert_eq!(bus.replay("t-1", 0).len(), 3);
        assert_eq!(bus.replay("t-1", 2).len(), 1);
        assert_eq!(bus.replay("t-1", 3).len(), 0);
        assert!(bus.replay("unknown", 0).is_empty());
    }

    #[test]
    fn event_bus_evicts_oldest_beyond_replay_buffer() {
        let bus = AgentProtocolTaskEventBus::new(2);
        for round in 0..3 {
            bus.publish(
                "t-1",
                RemoteAgentEvent {
                    kind: Some(RemoteEventType::Status),
                    status: Some(round.to_string()),
                    ..Default::default()
                },
            );
        }
        let replayed = bus.replay("t-1", 0);
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].seq, 2);
    }

    #[test]
    fn event_bus_complete_clears_replay_and_marks_completed() {
        let bus = AgentProtocolTaskEventBus::new(4);
        bus.publish("t-1", RemoteAgentEvent::default());
        assert!(!bus.is_completed("t-1"));
        bus.complete("t-1");
        assert!(bus.is_completed("t-1"));
        assert!(bus.replay("t-1", 0).is_empty());
    }

    #[test]
    fn event_bus_delivers_live_events_to_subscribers() {
        let bus = AgentProtocolTaskEventBus::new(4);
        let subscription = bus.subscribe("t-1", 0);
        assert!(subscription.replayed.is_empty());
        let mut receiver = subscription.receiver.unwrap();

        bus.publish(
            "t-1",
            RemoteAgentEvent {
                kind: Some(RemoteEventType::RunStarted),
                ..Default::default()
            },
        );
        let received = rt().block_on(receiver.recv()).unwrap();
        assert_eq!(received.kind, Some(RemoteEventType::RunStarted));
        assert_eq!(received.seq, 1);
    }

    #[test]
    fn flatten_customizer_skips_reserved_keys() {
        let context: BTreeMap<String, Value> = serde_json::from_value(serde_json::json!({
            "attributes": { "tenant": "acme", "agentId": "hijack" }
        }))
        .unwrap();
        let request = AgentRequest::new("t-1", "a-1", "", false, Some(context));

        let customizer =
            <FlattenCustomizer as RuntimeContextCustomizer>::flatten(&["tenant", "agentId"]);
        let mut runtime = RuntimeContext::new();
        customizer.customize(&request, &mut runtime);

        assert_eq!(
            runtime.metadata.get("tenant"),
            Some(&Value::String("acme".to_string()))
        );
        assert!(runtime.metadata.get("agentId").is_none());
    }

    #[test]
    fn submit_runs_to_completion_and_exposes_a_snapshot() {
        let store = store_for(Arc::new(ResultAgent {
            reply: reply_msg("done"),
        }));
        rt().block_on(async {
            store.submit("t-1", "agent-1", "ping", None).unwrap();
            let snapshot = store.wait_for("t-1", 5_000).await.unwrap();
            assert_eq!(snapshot["status"], "success");
            assert_eq!(snapshot["result"], "done");
            assert_eq!(snapshot["agentId"], "agent-1");
            assert_eq!(snapshot["awaitingConfirm"], false);
        });
    }

    /// `85fa9a602`: a task result is a string, so a delivered document is stored as a
    /// reference instead of an empty reply.
    #[test]
    fn a_document_only_reply_is_stored_as_a_reference() {
        let reply = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Document(
                DocumentBlock::builder()
                    .source(Source::Url(agentscope_core::message::URLSource::new(
                        "https://example.com/a.pdf",
                    )))
                    .name("a.pdf")
                    .build(),
            )])
            .build();
        let store = store_for(Arc::new(ResultAgent { reply }));
        rt().block_on(async {
            store.submit("t-1", "agent-1", "ping", None).unwrap();
            let snapshot = store.wait_for("t-1", 5_000).await.unwrap();
            assert_eq!(
                snapshot["result"],
                "[document: a.pdf, url: https://example.com/a.pdf]"
            );
        });
    }

    #[test]
    fn submit_persists_pending_record_and_rejects_finished_ids() {
        let store = store_for(Arc::new(ResultAgent {
            reply: reply_msg("done"),
        }));
        rt().block_on(async {
            store.submit("t-1", "agent-1", "ping", None).unwrap();
            store.wait_for("t-1", 5_000).await.unwrap();
            let err = store.submit("t-1", "agent-1", "again", None).unwrap_err();
            assert!(matches!(err, AgentProtocolError::InvalidRequest(_)));
        });
        assert_eq!(
            store.task_repository().find("t-1").unwrap().status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn wait_for_times_out_for_an_unknown_task() {
        let store = store_for(Arc::new(ResultAgent {
            reply: reply_msg("x"),
        }));
        let err = rt().block_on(store.wait_for("missing", 10)).unwrap_err();
        assert!(matches!(err, AgentProtocolError::TaskNotFound(_)));
    }

    #[test]
    fn cancel_marks_a_task_cancelled() {
        let store = store_for(Arc::new(ResultAgent {
            reply: reply_msg("x"),
        }));
        let mut record = harness_record("t-9");
        record.status = TaskStatus::Running;
        store.task_repository().save(&record);

        store.cancel("t-9").unwrap();
        let stored = store.task_repository().find("t-9").unwrap();
        assert_eq!(stored.status, TaskStatus::Cancelled);
        assert!(stored.cancel_requested);

        assert!(matches!(
            store.cancel("nope").unwrap_err(),
            AgentProtocolError::TaskNotFound(_)
        ));
    }

    #[test]
    fn hitl_pause_records_pending_confirms_and_resume_is_guarded() {
        let store = store_for(Arc::new(AskingAgent));
        rt().block_on(async {
            store.submit("t-1", "agent-1", "do it", None).unwrap();
            // Poll until the run parks awaiting confirmation.
            let mut record = store.task_repository().find("t-1").unwrap();
            for _ in 0..200 {
                record = store.task_repository().find("t-1").unwrap();
                if record.awaiting_confirm {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert!(record.awaiting_confirm);
            assert_eq!(record.status, TaskStatus::Running);
            let pending = record.pending_confirms.unwrap();
            assert_eq!(pending[0].tool_call_id.as_deref(), Some("call-1"));
            assert_eq!(pending[0].tool_name.as_deref(), Some("write_file"));

            // The waiter must not return while the task is parked.
            assert!(matches!(
                store.wait_for("t-1", 30).await.unwrap_err(),
                AgentProtocolError::Timeout(_)
            ));
            // Resume with the pending record cleared.
            store
                .resume("t-1", vec![RemoteConfirmDecision::new("call-1", true)])
                .unwrap();
        });
    }

    #[test]
    fn hitl_resume_rejects_tasks_that_are_not_awaiting() {
        let store = store_for(Arc::new(ResultAgent {
            reply: reply_msg("x"),
        }));
        store.task_repository().save(&harness_record("t-1"));
        let err = store.resume("t-1", Vec::new()).unwrap_err();
        assert!(matches!(err, AgentProtocolError::InvalidRequest(_)));
    }

    #[test]
    fn hitl_can_be_disabled() {
        let store = Arc::new(AgentProtocolTaskStore::new(
            Arc::new(FixedAgentFactory::new(Arc::new(ResultAgent {
                reply: reply_msg("x"),
            }))),
            Arc::new(InMemoryProtocolTaskRepository::new()),
            None,
            Some(
                AgentProtocolProperties::builder()
                    .hitl_enabled(false)
                    .build(),
            ),
            Vec::new(),
        ));
        store.task_repository().save(&harness_record("t-1"));
        let err = store.resume("t-1", Vec::new()).unwrap_err();
        assert!(matches!(err, AgentProtocolError::InvalidRequest(_)));
    }

    #[test]
    fn confirm_results_default_to_denied_and_rebuild_tool_calls() {
        let pending = vec![RemotePendingConfirm {
            tool_call_id: Some("c-1".to_string()),
            tool_name: Some("read_file".to_string()),
            tool_input_json: Some("{\"path\":\"a.txt\"}".to_string()),
        }];
        let approved = to_confirm_results(&pending, &[RemoteConfirmDecision::new("c-1", true)]);
        assert!(approved[0].approved);
        let tool = approved[0].tool_call.as_ref().unwrap();
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.input["path"], "a.txt");

        // A decision for a different call leaves this one denied.
        let denied = to_confirm_results(&pending, &[RemoteConfirmDecision::new("other", true)]);
        assert!(!denied[0].approved);
    }

    #[test]
    fn extract_pending_confirms_prefers_asking_blocks() {
        let mut asking = ToolUseBlock::new("c-1", "write_file", Default::default());
        asking.state = ToolCallState::Asking;
        let pending = ToolUseBlock::new("c-2", "read_file", Default::default());
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::ToolUse(asking),
                ContentBlock::ToolUse(pending),
            ])
            .build();

        let extracted = extract_pending_confirms(&msg);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].tool_call_id.as_deref(), Some("c-1"));
    }

    #[test]
    fn extract_pending_confirms_falls_back_to_all_tool_uses() {
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::ToolUse(ToolUseBlock::new("c-1", "a", Default::default())),
                ContentBlock::ToolUse(ToolUseBlock::new("c-2", "b", Default::default())),
            ])
            .build();
        assert_eq!(extract_pending_confirms(&msg).len(), 2);
    }

    #[test]
    fn build_runtime_context_namespaces_attributes() {
        let context: BTreeMap<String, Value> = serde_json::from_value(serde_json::json!({
            "user_id": "u-1",
            "attributes": { "tenant": "acme" }
        }))
        .unwrap();
        let request = AgentRequest::new("t-1", "a-1", "", false, Some(context));
        let runtime = build_runtime_context(&request, &[]);

        assert_eq!(runtime.session_id(), Some("t-1"));
        assert_eq!(runtime.user_id(), Some("u-1"));
        assert_eq!(runtime.agent_id(), Some("a-1"));
        assert_eq!(
            runtime.metadata[AgentProtocolConstants::RUNTIME_CONTEXT_ATTRIBUTES_KEY]["tenant"],
            "acme"
        );
    }

    // Java `#mapStatus` (`:585-596`) speaks `pending`/`running`/`success`/`error`/`cancelled`;
    // this crate used to emit `TaskStatus::as_str()`, so a Java client never saw `success`.
    #[test]
    fn status_words_follow_java_map_status() {
        let table = [
            (TaskStatus::Pending, "pending"),
            (TaskStatus::Running, "running"),
            (TaskStatus::Completed, "success"),
            (TaskStatus::Failed, "error"),
            (TaskStatus::Cancelled, "cancelled"),
        ];
        for (status, expected) in table {
            let mut record = harness_record("t-1");
            record.status = status;
            assert_eq!(snapshot_value(&record)["status"], expected, "{status:?}");
        }
    }

    // A parked run is still `Running` internally; Java reports the pause instead.
    #[test]
    fn awaiting_confirm_wins_over_the_running_status() {
        let mut record = harness_record("t-1");
        record.status = TaskStatus::Running;
        record.awaiting_confirm = true;
        let snapshot = snapshot_value(&record);
        assert_eq!(snapshot["status"], "awaiting_confirm");
        assert_eq!(snapshot["awaiting_confirm"], true);
    }

    #[test]
    fn snapshot_serves_both_java_and_camel_case_keys() {
        let mut record = harness_record("t-1");
        record.status = TaskStatus::Failed;
        record.error_message = Some("boom".to_string());
        let snapshot = snapshot_value(&record);
        assert_eq!(snapshot["task_id"], "t-1");
        assert_eq!(snapshot["taskId"], "t-1");
        assert_eq!(snapshot["agent_id"], record.sub_agent_id);
        assert_eq!(snapshot["agentId"], record.sub_agent_id);
        assert_eq!(snapshot["status"], "error");
        assert_eq!(snapshot["error"], "boom");
        assert_eq!(snapshot["awaiting_confirm"], false);
        assert_eq!(snapshot["awaitingConfirm"], false);
    }

    #[test]
    fn sse_frame_matches_java_wire_format() {
        let event = RemoteAgentEvent {
            seq: 7,
            kind: Some(RemoteEventType::TextDelta),
            text: Some("hi".to_string()),
            ..Default::default()
        };
        let frame = String::from_utf8(sse_frame(&event)).unwrap();
        assert!(frame.starts_with("id: 7\nevent: TEXT_DELTA\ndata: {"));
        assert!(frame.ends_with("}\n\n"));
        assert!(frame.contains("\"type\":\"TEXT_DELTA\""));

        // Java `toSse` falls back to `message` for an event without a type.
        let untyped = RemoteAgentEvent {
            seq: 8,
            ..Default::default()
        };
        let frame = String::from_utf8(sse_frame(&untyped)).unwrap();
        assert!(frame.starts_with("id: 8\nevent: message\ndata: {"));
    }

    // Java `AgentProtocolController#waitFor`: `timeout_seconds` (seconds) is the wire parameter;
    // `timeoutMs` remains as the legacy Rust spelling. Before 2026-10-31 only the latter was read.
    #[test]
    fn wait_timeout_seconds_wins_over_the_legacy_millisecond_parameter() {
        let seconds = HashMap::from([("timeout_seconds".to_string(), "30".to_string())]);
        assert_eq!(wait_timeout_ms(&seconds, 10_800_000), 30_000);

        let both = HashMap::from([
            ("timeout_seconds".to_string(), "30".to_string()),
            ("timeoutMs".to_string(), "500".to_string()),
        ]);
        assert_eq!(wait_timeout_ms(&both, 10_800_000), 30_000);

        let legacy = HashMap::from([("timeoutMs".to_string(), "1500".to_string())]);
        assert_eq!(wait_timeout_ms(&legacy, 10_800_000), 1_500);

        assert_eq!(wait_timeout_ms(&HashMap::new(), 10_800_000), 10_800_000);
    }

    // Java `#events`: `from_seq` first, then the `Last-Event-ID` reconnect header, then `fromSeq`.
    #[test]
    fn event_replay_cursor_prefers_from_seq_then_the_last_event_id_header() {
        let query = HashMap::from([("from_seq".to_string(), "4".to_string())]);
        assert_eq!(replay_from_seq(&query, Some("9")), 4);

        let zero = HashMap::from([("from_seq".to_string(), "0".to_string())]);
        assert_eq!(replay_from_seq(&zero, Some("9")), 9);

        assert_eq!(replay_from_seq(&HashMap::new(), Some(" 5 ")), 5);
        assert_eq!(replay_from_seq(&HashMap::new(), Some("not-a-seq")), 0);
        assert_eq!(replay_from_seq(&HashMap::new(), Some("  ")), 0);

        let legacy = HashMap::from([("fromSeq".to_string(), "2".to_string())]);
        assert_eq!(replay_from_seq(&legacy, None), 2);
        assert_eq!(replay_from_seq(&HashMap::new(), None), 0);
    }

    // The SSE stream must resume after the requested seq instead of replaying from 0.
    #[test]
    fn event_stream_replays_after_the_requested_seq() {
        let bus: Arc<dyn AgentProtocolEventBus> = Arc::new(AgentProtocolTaskEventBus::new(16));
        for _ in 0..3 {
            bus.publish(
                "t-1",
                RemoteAgentEvent {
                    kind: Some(RemoteEventType::TextDelta),
                    ..Default::default()
                },
            );
        }

        let frames: Vec<String> = rt().block_on(async {
            use futures::StreamExt as _;
            event_stream(bus, "t-1".to_string(), 1)
                .take(2)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|frame| String::from_utf8(frame.unwrap()).unwrap())
                .collect()
        });
        assert_eq!(frames.len(), 2);
        assert!(frames[0].starts_with("id: 2\nevent: TEXT_DELTA\n"));
        assert!(frames[1].starts_with("id: 3\nevent: TEXT_DELTA\n"));
    }

    #[test]
    fn in_memory_repository_upserts_and_lists_active_records() {
        let repository = InMemoryProtocolTaskRepository::new();
        assert!(repository.is_empty());

        let mut record = harness_record("t-1");
        record.status = TaskStatus::Running;
        repository.save(&record);
        assert_eq!(repository.len(), 1);
        assert_eq!(repository.active_records().len(), 1);

        record.status = TaskStatus::Completed;
        repository.save(&record);
        assert_eq!(repository.len(), 1);
        assert!(repository.active_records().is_empty());
    }

    fn harness_record(task_id: &str) -> TaskRecord {
        TaskRecord::new(
            task_id,
            "agent-1",
            AgentProtocolConstants::PROTOCOL_AGENT_ID,
            AgentProtocolConstants::PROTOCOL_SESSION_ID,
        )
    }

    /// Reads a handler response as `(status code, JSON body)`; an empty body reads as `Null`.
    async fn read_response(response: axum::response::Response) -> (u16, Value) {
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    /// Java's wire status words (`AgentProtocolTaskStore#mapStatus`).
    const JAVA_STATUS_WORDS: [&str; 6] = [
        "pending",
        "running",
        "success",
        "error",
        "cancelled",
        "awaiting_confirm",
    ];

    /// Asserts the body reports a status word a Java client can interpret.
    fn assert_java_status(body: &Value) {
        let status = body["status"].as_str().expect("status is reported");
        assert!(
            JAVA_STATUS_WORDS.contains(&status),
            "unexpected status word: {status} ({body})"
        );
    }

    // Java: `AgentProtocolController#submit` answers `200` + `{task_id, status}` (`:79-84`). This
    // handler keeps the richer snapshot, so `task_id` must join the protocol's `taskId`.
    #[test]
    fn submit_answers_with_the_java_task_keys() {
        let store = store_for(Arc::new(ResultAgent {
            reply: reply_msg("done"),
        }));
        let (status, body) = rt().block_on(async {
            let response = submit_task(
                axum::extract::State(store.clone()),
                axum::Json(serde_json::json!({
                    "task_id": "t-1",
                    "agent_id": "agent-1",
                    "input": "ping",
                })),
            )
            .await;
            read_response(response).await
        });
        assert_eq!(status, 200);
        assert_eq!(body["task_id"], "t-1");
        assert_eq!(body["taskId"], "t-1");
        assert_java_status(&body);
    }

    // Java: `#cancel` answers `200` with an empty body (`:106-110`).
    #[test]
    fn cancel_answers_ok_with_an_empty_body() {
        let store = store_for(Arc::new(ResultAgent {
            reply: reply_msg("x"),
        }));
        let mut record = harness_record("t-9");
        record.status = TaskStatus::Running;
        store.task_repository().save(&record);

        let (status, body) = rt().block_on(async {
            let response = cancel_task(
                axum::extract::State(store.clone()),
                axum::extract::Path("t-9".to_string()),
            )
            .await;
            read_response(response).await
        });
        assert_eq!(status, 200);
        assert_eq!(body, Value::Null, "Java sends no body");
        assert_eq!(
            store.task_repository().find("t-9").unwrap().status,
            TaskStatus::Cancelled
        );
    }

    // Java: `#resume` answers `{task_id, status}` (`:128-147`) and `#parseDecisions` (`:151-166`)
    // accepts the snake `tool_call_id` spelling.
    #[test]
    fn resume_answers_with_the_java_task_keys_and_accepts_snake_case_decisions() {
        let store = store_for(Arc::new(ResultAgent {
            reply: reply_msg("done"),
        }));
        let mut record = harness_record("t-1");
        record.status = TaskStatus::Running;
        record.awaiting_confirm = true;
        record.pending_confirms = Some(vec![RemotePendingConfirm::new(
            "call-1",
            "write_file",
            None,
        )]);
        store.task_repository().save(&record);

        let (status, body) = rt().block_on(async {
            let response = resume_task(
                axum::extract::State(store.clone()),
                axum::extract::Path("t-1".to_string()),
                axum::Json(serde_json::json!({
                    "decisions": [{ "tool_call_id": "call-1", "approved": true }],
                })),
            )
            .await;
            read_response(response).await
        });
        assert_eq!(status, 200);
        assert_eq!(body["task_id"], "t-1");
        assert_java_status(&body);
        // The snake-case decision was applied, so the task is no longer parked.
        assert!(
            !store
                .task_repository()
                .find("t-1")
                .unwrap()
                .awaiting_confirm
        );
    }

    // Java `#parseDecisions` reads `toolCallId`, then `tool_call_id`; both must land on one DTO.
    #[test]
    fn decisions_accept_both_the_camel_and_the_snake_tool_call_id() {
        let body = serde_json::json!({
            "decisions": [
                { "toolCallId": "c-1", "approved": true },
                { "tool_call_id": "c-2", "approved": false, "reason": "nope" },
            ],
        });
        let decisions = parse_decisions(&body);
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].tool_call_id.as_deref(), Some("c-1"));
        assert!(decisions[0].approved);
        assert_eq!(decisions[1].tool_call_id.as_deref(), Some("c-2"));
        assert!(!decisions[1].approved);
        assert_eq!(decisions[1].reason.as_deref(), Some("nope"));
    }
}
