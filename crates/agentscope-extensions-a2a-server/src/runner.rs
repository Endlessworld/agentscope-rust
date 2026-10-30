//! Per-task agent runners, mirroring
//! `io.agentscope.core.a2a.server.executor.runner`:
//!
//! * [`TaskAgent`] / [`TaskAgentFactory`] - the Rust shape of what Java's runners obtain from a
//!   `ReActAgent` and its `ReActAgent.Builder` (`buildReActAgent()`);
//! * [`BaseReActAgentRunner`] - `BaseReActAgentRunner`: one agent per `taskId`, cached so that
//!   [`AgentRunner::stop`] can interrupt the run, and dropped when the run finishes, fails or is
//!   abandoned;
//! * [`ReActAgentWithBuilderRunner`] - `ReActAgentWithBuilderRunner`: builds the per-task agents
//!   from a caller-supplied builder.
//!
//! Java ties the cache to `ConcurrentHashMap<String, ReActAgent>` and the removal to
//! `doFinally(...)`. Rust keeps the same semantics with a [`CacheGuard`] whose `Drop` removes the
//! entry, so completion, stream errors and early `drop` of the event stream all release the slot.
//! Like Java's `agentCache.remove(taskId, agent)` the guard only removes the entry it owns, so a
//! stream that outlives a restart cannot evict the replacement agent.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::react_agent::ReActAgent;
use agentscope_core::event::AgentEvent;
use agentscope_core::message::Msg;
use agentscope_core::StreamOptions;
use agentscope_extensions_a2a_client::error::A2aError;
use futures::stream::BoxStream;
use futures::StreamExt;

use crate::{AgentRequestOptions, AgentRunner};

/// One agent instance driving a single A2A task.
///
/// Java's runners receive a `ReActAgent` and call `streamEvents(List<Msg>)` /
/// `interrupt()` / `getName()` / `getDescription()` on it. The trait keeps exactly that surface so
/// tests can drive the runner with a scripted agent, and so other agent kinds can be plugged in
/// without changing the runner.
pub trait TaskAgent: Send + Sync {
    /// The agent display name (Java `Agent#getName`).
    fn name(&self) -> String;

    /// The agent description (Java `Agent#getDescription`).
    fn description(&self) -> String;

    /// Runs one request, mirroring Java `ReActAgent#streamEvents(List<Msg>) -> Flux<AgentEvent>`.
    fn stream_events(&self, messages: Vec<Msg>) -> BoxStream<'static, Result<AgentEvent, A2aError>>;

    /// Interrupts the run, mirroring Java `ReActAgent#interrupt()`.
    fn interrupt(&self);
}

impl TaskAgent for ReActAgent {
    fn name(&self) -> String {
        ReActAgent::name(self).to_string()
    }

    fn description(&self) -> String {
        ReActAgent::description(self).to_string()
    }

    fn stream_events(&self, messages: Vec<Msg>) -> BoxStream<'static, Result<AgentEvent, A2aError>> {
        Box::pin(
            ReActAgent::stream_events(self, messages, StreamOptions::defaults())
                .map(|item| item.map_err(A2aError::Core)),
        )
    }

    fn interrupt(&self) {
        ReActAgent::interrupt(self);
    }
}

/// Builds one fresh [`TaskAgent`] per A2A task.
///
/// Mirrors the abstract `BaseReActAgentRunner#buildReActAgent()` plus the `getAgentName()` /
/// `getAgentDescription()` pair, which Java implements by building an agent and reading its name
/// and description. Implementations may override the two metadata methods to avoid building.
pub trait TaskAgentFactory: Send + Sync {
    /// Builds the agent for one task.
    fn build(&self) -> Result<Arc<dyn TaskAgent>, A2aError>;

    /// Agent name for the agent-card defaults (Java builds an agent and reads `getName()`).
    fn agent_name(&self) -> String {
        self.build().map(|agent| agent.name()).unwrap_or_default()
    }

    /// Agent description for the agent-card defaults (Java reads `getDescription()`).
    fn agent_description(&self) -> String {
        self.build()
            .map(|agent| agent.description())
            .unwrap_or_default()
    }
}

/// Removes the cached agent of one task when the run ends, mirroring Java
/// `doFinally(signal -> agentCache.remove(taskId, agent))`.
///
/// The guard is dropped on every exit path: normal completion, a stream error, and a caller that
/// drops the event stream before draining it. It only removes the entry when the cache still
/// holds **this** agent instance, so a stream that outlives a restart (the task was stopped and
/// started again with the same id) never evicts its replacement — the race fixed in Java
/// `431031a7a`.
struct CacheGuard {
    agents: Arc<Mutex<HashMap<String, Arc<dyn TaskAgent>>>>,
    task_id: String,
    agent: Arc<dyn TaskAgent>,
}

impl Drop for CacheGuard {
    fn drop(&mut self) {
        if let Ok(mut agents) = self.agents.lock() {
            let owns_entry = agents
                .get(&self.task_id)
                .map(|current| Arc::ptr_eq(current, &self.agent))
                .unwrap_or(false);
            if owns_entry {
                agents.remove(&self.task_id);
            }
        }
    }
}

/// Runner that keeps one agent per `taskId`, mirroring Java `BaseReActAgentRunner`.
pub struct BaseReActAgentRunner<F: TaskAgentFactory> {
    factory: F,
    agents: Arc<Mutex<HashMap<String, Arc<dyn TaskAgent>>>>,
}

impl<F: TaskAgentFactory> fmt::Debug for BaseReActAgentRunner<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cached = self.agents.lock().map(|agents| agents.len()).unwrap_or(0);
        f.debug_struct("BaseReActAgentRunner")
            .field("cached_agents", &cached)
            .finish()
    }
}

impl<F: TaskAgentFactory> BaseReActAgentRunner<F> {
    /// Creates a runner backed by `factory`.
    pub fn new(factory: F) -> Self {
        Self {
            factory,
            agents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The task ids that currently hold a cached agent, sorted for stable assertions.
    pub fn cached_task_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = match self.agents.lock() {
            Ok(agents) => agents.keys().cloned().collect(),
            Err(_) => Vec::new(),
        };
        ids.sort();
        ids
    }

    /// Whether `task_id` currently holds a cached agent.
    pub fn is_cached(&self, task_id: &str) -> bool {
        self.agents
            .lock()
            .map(|agents| agents.contains_key(task_id))
            .unwrap_or(false)
    }

    /// Builds and caches the agent of one task.
    ///
    /// Java checks `containsKey`, builds an agent, then inserts with `putIfAbsent` and reports the
    /// duplicate when another start won the race. Building outside the lock keeps a slow factory
    /// from serializing every task, and re-checking under the lock keeps the per-task slot
    /// exclusive (the fix from Java `431031a7a`).
    fn register(&self, task_id: &str) -> Result<Arc<dyn TaskAgent>, A2aError> {
        if self.is_cached(task_id) {
            return Err(already_started(task_id));
        }
        let agent = self.factory.build()?;
        let mut agents = self
            .agents
            .lock()
            .map_err(|_| A2aError::InvalidRequest("agent cache is poisoned".to_string()))?;
        if agents.contains_key(task_id) {
            return Err(already_started(task_id));
        }
        agents.insert(task_id.to_string(), agent.clone());
        Ok(agent)
    }

    fn guard(&self, task_id: &str, agent: &Arc<dyn TaskAgent>) -> CacheGuard {
        CacheGuard {
            agents: self.agents.clone(),
            task_id: task_id.to_string(),
            agent: agent.clone(),
        }
    }
}

#[async_trait::async_trait]
impl<F: TaskAgentFactory + 'static> AgentRunner for BaseReActAgentRunner<F> {
    fn agent_name(&self) -> String {
        self.factory.agent_name()
    }

    fn agent_description(&self) -> String {
        self.factory.agent_description()
    }

    async fn stream_events(
        &self,
        request_messages: Vec<Msg>,
        options: AgentRequestOptions,
    ) -> Result<Vec<AgentEvent>, A2aError> {
        let task_id = required_task_id(&options)?;
        let agent = self.register(&task_id)?;
        // Dropped on every exit path below, including an erroring event stream.
        let _guard = self.guard(&task_id, &agent);
        let mut stream = agent.stream_events(request_messages);
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            events.push(item?);
        }
        Ok(events)
    }

    fn stream_events_incremental(
        &self,
        request_messages: Vec<Msg>,
        options: AgentRequestOptions,
    ) -> Option<BoxStream<'static, Result<AgentEvent, A2aError>>> {
        // Without a task id there is nothing to cache, so let the server fall back to the
        // buffered contract, which reports the missing id as an error.
        let task_id = options.task_id().map(str::to_string)?;
        match self.register(&task_id) {
            Ok(agent) => {
                let guard = self.guard(&task_id, &agent);
                let stream = agent.stream_events(request_messages).map(move |item| {
                    // The closure owns the guard, so dropping the stream releases the cache slot.
                    let _ = &guard;
                    item
                });
                Some(Box::pin(stream))
            }
            // Java throws `IllegalStateException` out of `streamEvents`; a lazy stream cannot
            // return an error directly, so surface it as the first (and only) item.
            Err(error) => Some(Box::pin(futures::stream::once(async move { Err(error) }))),
        }
    }

    fn stop(&self, task_id: &str) {
        // Java removes before interrupting so a concurrent `stop` cannot interrupt twice.
        let agent = self
            .agents
            .lock()
            .ok()
            .and_then(|mut agents| agents.remove(task_id));
        if let Some(agent) = agent {
            agent.interrupt();
        }
    }
}

/// The task id of a request, or the error the buffered contract reports when it is missing.
///
/// Java keys the cache with `options.getTaskId()` and throws a `NullPointerException` on a
/// missing id; an explicit error keeps the same "a task id is required" intent.
fn required_task_id(options: &AgentRequestOptions) -> Result<String, A2aError> {
    options
        .task_id()
        .map(str::to_string)
        .ok_or_else(|| A2aError::InvalidRequest("AgentRunner requires a taskId".to_string()))
}

/// The error a duplicate start reports, mirroring Java `IllegalStateException`.
fn already_started(task_id: &str) -> A2aError {
    A2aError::InvalidRequest(format!("Agent already exists for taskId: {task_id}"))
}

/// Builds a fresh [`ReActAgent`] per task from a caller-supplied builder closure.
///
/// Java holds a reusable `ReActAgent.Builder` and calls `build()` per request. Rust's
/// `ReActAgentBuilder::build(self)` consumes the builder, so the factory takes a closure that
/// produces one agent instead (the caller rebuilds or clones its inputs inside the closure).
pub struct ReActAgentBuilderFactory {
    build: Arc<dyn Fn() -> Result<ReActAgent, A2aError> + Send + Sync>,
}

impl fmt::Debug for ReActAgentBuilderFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReActAgentBuilderFactory")
    }
}

impl ReActAgentBuilderFactory {
    /// Creates the factory from a closure producing one agent per call.
    pub fn new<B>(build: B) -> Self
    where
        B: Fn() -> Result<ReActAgent, A2aError> + Send + Sync + 'static,
    {
        Self {
            build: Arc::new(build),
        }
    }
}

impl TaskAgentFactory for ReActAgentBuilderFactory {
    fn build(&self) -> Result<Arc<dyn TaskAgent>, A2aError> {
        Ok(Arc::new((self.build)()?))
    }
}

/// Default runner, mirroring Java `ReActAgentWithBuilderRunner`:
/// `BaseReActAgentRunner` whose `buildReActAgent()` calls `agentBuilder.build()`.
pub type ReActAgentWithBuilderRunner = BaseReActAgentRunner<ReActAgentBuilderFactory>;

impl ReActAgentWithBuilderRunner {
    /// Builds a runner from a `ReActAgent` builder closure.
    ///
    /// Rust counterpart of Java `ReActAgentWithBuilderRunner#newInstance(ReActAgent.Builder)`.
    pub fn with_builder<B>(build: B) -> Self
    where
        B: Fn() -> Result<ReActAgent, A2aError> + Send + Sync + 'static,
    {
        Self::new(ReActAgentBuilderFactory::new(build))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agentscope_core::agent::react_agent::ReActAgent;
    use agentscope_core::event::EventData;
    use agentscope_core::message::{ChatUsage, ContentBlock, TextBlock};
    use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
    use agentscope_extensions_a2a_client::model::{
        methods, AgentCard, JsonRpcRequest, Message, MessageRole, MessageSendParams, Part,
    };

    use crate::{A2aRequestHandler, A2aTaskRegistry};

    /// How a [`FakeAgent`] behaves once it is asked to stream.
    #[derive(Debug, Clone, Copy)]
    enum FakeOutcome {
        /// Emits one text delta named after the agent, then ends.
        Complete,
        /// Emits one stream error.
        Fail,
        /// Emits one text delta, then never finishes (a run that can only end via `stop`).
        Pending,
    }

    /// What the fake agents observed, shared by every agent of one factory.
    #[derive(Default)]
    struct Recorder {
        built: Mutex<Vec<String>>,
        ran: Mutex<Vec<(String, String)>>,
        interrupted: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn built(&self) -> Vec<String> {
            self.built.lock().unwrap().clone()
        }

        fn ran(&self) -> Vec<(String, String)> {
            self.ran.lock().unwrap().clone()
        }

        fn interrupted(&self) -> Vec<String> {
            self.interrupted.lock().unwrap().clone()
        }
    }

    struct FakeAgent {
        id: String,
        recorder: Arc<Recorder>,
        outcome: FakeOutcome,
    }

    fn text_delta(text: &str) -> AgentEvent {
        let mut data = EventData::default();
        data.fields.insert(
            "delta".to_string(),
            serde_json::Value::String(text.to_string()),
        );
        AgentEvent::TextBlockDelta(data)
    }

    impl TaskAgent for FakeAgent {
        fn name(&self) -> String {
            format!("agent-{}", self.id)
        }

        fn description(&self) -> String {
            "scripted agent".to_string()
        }

        fn stream_events(
            &self,
            messages: Vec<Msg>,
        ) -> BoxStream<'static, Result<AgentEvent, A2aError>> {
            let text = messages
                .iter()
                .map(|message| message.text_content())
                .collect::<Vec<_>>()
                .join("");
            self.recorder
                .ran
                .lock()
                .unwrap()
                .push((self.id.clone(), text));
            match self.outcome {
                FakeOutcome::Complete => {
                    Box::pin(futures::stream::iter(vec![Ok(text_delta(&self.id))]))
                }
                FakeOutcome::Fail => Box::pin(futures::stream::iter(vec![Err(
                    A2aError::Transport("boom".to_string()),
                )])),
                FakeOutcome::Pending => Box::pin(
                    futures::stream::once(std::future::ready(Ok(text_delta(&self.id))))
                        .chain(futures::stream::pending()),
                ),
            }
        }

        fn interrupt(&self) {
            self.recorder
                .interrupted
                .lock()
                .unwrap()
                .push(self.id.clone());
        }
    }

    /// Factory handing out fresh [`FakeAgent`]s (`a0`, `a1`, ...) and recording every build.
    struct FakeFactory {
        recorder: Arc<Recorder>,
        outcome: FakeOutcome,
        built: AtomicUsize,
    }

    impl FakeFactory {
        fn new(recorder: Arc<Recorder>, outcome: FakeOutcome) -> Self {
            Self {
                recorder,
                outcome,
                built: AtomicUsize::new(0),
            }
        }
    }

    impl TaskAgentFactory for FakeFactory {
        fn build(&self) -> Result<Arc<dyn TaskAgent>, A2aError> {
            let index = self.built.fetch_add(1, Ordering::SeqCst);
            let id = format!("a{index}");
            self.recorder.built.lock().unwrap().push(id.clone());
            Ok(Arc::new(FakeAgent {
                id,
                recorder: self.recorder.clone(),
                outcome: self.outcome,
            }))
        }
    }

    fn options(task_id: &str) -> AgentRequestOptions {
        AgentRequestOptions::new().with_task_id(task_id)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    // Java: BaseReActAgentRunner#streamEvents — `buildReActAgent()` per request, keyed by
    // `options.getTaskId()`.
    #[test]
    fn every_task_gets_its_own_agent() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Complete,
        ));

        let first = rt()
            .block_on(runner.stream_events(vec![Msg::user("one")], options("t1")))
            .unwrap();
        let second = rt()
            .block_on(runner.stream_events(vec![Msg::user("two")], options("t2")))
            .unwrap();

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(recorder.built(), vec!["a0", "a1"]);
        assert_eq!(
            recorder.ran(),
            vec![
                ("a0".to_string(), "one".to_string()),
                ("a1".to_string(), "two".to_string())
            ]
        );
        assert!(runner.cached_task_ids().is_empty());
    }

    // Java: BaseReActAgentRunner#stop — `agentCache.remove(taskId)` then `agent.interrupt()`.
    #[test]
    fn stop_interrupts_only_the_targeted_task() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Pending,
        ));

        let first = rt().block_on(async {
            let mut stream = runner
                .stream_events_incremental(vec![Msg::user("one")], options("t1"))
                .expect("incremental stream");
            assert!(stream.next().await.is_some());
            stream
        });
        let second = rt().block_on(async {
            let mut stream = runner
                .stream_events_incremental(vec![Msg::user("two")], options("t2"))
                .expect("incremental stream");
            assert!(stream.next().await.is_some());
            stream
        });

        assert_eq!(runner.cached_task_ids(), vec!["t1", "t2"]);

        runner.stop("t1");

        assert_eq!(recorder.interrupted(), vec!["a0"]);
        assert!(!runner.is_cached("t1"));
        assert!(runner.is_cached("t2"));
        assert_eq!(recorder.ran().len(), 2, "both tasks ran their own agent");

        drop(first);
        drop(second);
        assert!(runner.cached_task_ids().is_empty());
    }

    // Java: `doFinally(signal -> agentCache.remove(taskId))` — runs on completion, error and
    // cancellation.
    #[test]
    fn cache_entry_is_dropped_when_the_run_finishes_errors_or_is_abandoned() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Complete,
        ));
        let events = rt()
            .block_on(runner.stream_events(vec![Msg::user("one")], options("t1")))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(!runner.is_cached("t1"));

        let failing = BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Fail,
        ));
        let error = rt()
            .block_on(failing.stream_events(vec![Msg::user("two")], options("t2")))
            .unwrap_err();
        assert!(matches!(error, A2aError::Transport(_)));
        assert!(!failing.is_cached("t2"));

        let pending = BaseReActAgentRunner::new(FakeFactory::new(
            recorder,
            FakeOutcome::Pending,
        ));
        let stream = pending
            .stream_events_incremental(vec![Msg::user("three")], options("t3"))
            .expect("incremental stream");
        assert!(pending.is_cached("t3"));
        drop(stream);
        assert!(!pending.is_cached("t3"));
    }

    // Java: `ConcurrentHashMap` keyed by taskId — two live runs never share an agent.
    #[test]
    fn concurrent_tasks_do_not_share_agents() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Complete,
        ));

        rt().block_on(async {
            let mut first = runner
                .stream_events_incremental(vec![Msg::user("one")], options("t1"))
                .expect("incremental stream");
            let mut second = runner
                .stream_events_incremental(vec![Msg::user("two")], options("t2"))
                .expect("incremental stream");
            assert_eq!(runner.cached_task_ids(), vec!["t1", "t2"]);

            let (left, right) = futures::join!(
                async {
                    let mut events = Vec::new();
                    while let Some(item) = first.next().await {
                        events.push(item.unwrap());
                    }
                    events
                },
                async {
                    let mut events = Vec::new();
                    while let Some(item) = second.next().await {
                        events.push(item.unwrap());
                    }
                    events
                }
            );

            assert_eq!(left.len(), 1);
            assert_eq!(right.len(), 1);
            assert_eq!(delta_of(&left[0]), "a0");
            assert_eq!(delta_of(&right[0]), "a1");
        });

        assert_eq!(recorder.built(), vec!["a0", "a1"]);
        assert_eq!(
            recorder.ran(),
            vec![
                ("a0".to_string(), "one".to_string()),
                ("a1".to_string(), "two".to_string())
            ]
        );
        assert!(runner.cached_task_ids().is_empty());
    }

    fn delta_of(event: &AgentEvent) -> String {
        match event {
            AgentEvent::TextBlockDelta(data) => data
                .fields
                .get("delta")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            other => panic!("expected a text delta, got {other:?}"),
        }
    }

    // Java: `IllegalStateException("Agent already exists for taskId: " + taskId)`.
    #[test]
    fn a_task_id_cannot_be_started_twice() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder,
            FakeOutcome::Pending,
        ));

        let _first = runner
            .stream_events_incremental(vec![Msg::user("one")], options("t1"))
            .expect("first start");
        let mut duplicate = runner
            .stream_events_incremental(vec![Msg::user("one")], options("t1"))
            .expect("a lazy start reports the clash through its stream");
        let item = rt().block_on(duplicate.next()).expect("one item");
        match item {
            Err(A2aError::InvalidRequest(message)) => {
                assert!(message.contains("Agent already exists for taskId: t1"));
            }
            other => panic!("expected the duplicate-task error, got {other:?}"),
        }
        assert!(runner.is_cached("t1"), "the running task keeps its slot");

        let buffered = rt().block_on(runner.stream_events(vec![Msg::user("one")], options("t1")));
        assert!(buffered.is_err());
    }

    #[test]
    fn a_missing_task_id_is_rejected_and_never_cached() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Complete,
        ));

        let error = rt()
            .block_on(runner.stream_events(vec![Msg::user("one")], AgentRequestOptions::new()))
            .unwrap_err();
        assert!(matches!(error, A2aError::InvalidRequest(_)));
        assert!(
            runner
                .stream_events_incremental(vec![Msg::user("one")], AgentRequestOptions::new())
                .is_none(),
            "without a task id the server falls back to the buffered contract"
        );
        assert!(recorder.built().is_empty());
    }

    // Java: `stop(String)` on an unknown task id is a no-op.
    #[test]
    fn stop_of_an_unknown_task_is_a_noop() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Complete,
        ));
        runner.stop("missing");
        assert!(recorder.interrupted().is_empty());
    }

    // Java: the runner plugs into the request handler, so `tasks/cancel` reaches only that task.
    #[test]
    fn tasks_cancel_through_the_handler_targets_only_the_running_task() {
        let recorder = Arc::new(Recorder::default());
        let runner = Arc::new(BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Pending,
        )));
        let handler = A2aRequestHandler::new(
            runner.clone(),
            AgentCard::new("scripted-agent", "http://localhost"),
            Arc::new(A2aTaskRegistry::new()),
        );

        rt().block_on(async {
            let mut first = handler
                .stream_responses(&stream_request("t1"))
                .expect("streaming request");
            let second = handler
                .stream_responses(&stream_request("t2"))
                .expect("streaming request");
            assert!(first.next().await.is_some(), "the working frame arrives first");
            let mut second = second;
            assert!(second.next().await.is_some());
            assert_eq!(runner.cached_task_ids(), vec!["t1", "t2"]);

            let cancel = JsonRpcRequest::new(
                methods::TASKS_CANCEL,
                Some(serde_json::json!({ "id": "t1" })),
                Some(serde_json::json!(1)),
            );
            let response = handler.handle(&cancel).await;
            assert!(response.error.is_none(), "cancel is served: {response:?}");

            assert_eq!(recorder.interrupted(), vec!["a0"]);
            assert!(!runner.is_cached("t1"));
            assert!(runner.is_cached("t2"));
            drop(first);
            drop(second);
        });
    }

    fn stream_request(task_id: &str) -> JsonRpcRequest {
        let mut message = Message::new("m1", MessageRole::User, vec![Part::text("ping")]);
        message.task_id = Some(task_id.to_string());
        let params = MessageSendParams {
            message,
            configuration: None,
            metadata: None,
        };
        JsonRpcRequest::new(
            methods::MESSAGE_STREAM,
            Some(serde_json::to_value(params).unwrap()),
            Some(serde_json::json!(1)),
        )
    }

    /// Minimal model returning one final text response, enough to run a real `ReActAgent`.
    struct StubModel {
        reply: String,
    }

    impl Model for StubModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, agentscope_core::Result<ChatResponse>> {
            let response = ChatResponse::builder()
                .id("stub-response")
                .content(vec![ContentBlock::Text(TextBlock::new(self.reply.clone()))])
                .usage(ChatUsage::default())
                .finish_reason("stop")
                .build();
            Box::pin(futures::stream::iter(vec![Ok(response)]))
        }

        fn model_name(&self) -> &str {
            "stub-model"
        }
    }

    // Java: ReActAgentWithBuilderRunner#buildReActAgent — `agentBuilder.build()` per request.
    #[test]
    fn react_agent_with_builder_runner_builds_one_agent_per_task() {
        let builds = Arc::new(AtomicUsize::new(0));
        let runner = ReActAgentWithBuilderRunner::with_builder({
            let builds = builds.clone();
            move || {
                builds.fetch_add(1, Ordering::SeqCst);
                ReActAgent::builder()
                    .name("remote-agent")
                    .description("built per task")
                    .model(Arc::new(StubModel {
                        reply: "hi from the stub".to_string(),
                    }))
                    .build()
                    .map_err(A2aError::Core)
            }
        });

        // Java's `getAgentName()` / `getAgentDescription()` build an agent and read it back.
        assert_eq!(AgentRunner::agent_name(&runner), "remote-agent");
        assert_eq!(AgentRunner::agent_description(&runner), "built per task");

        let events = rt()
            .block_on(runner.stream_events(vec![Msg::user("go")], options("t1")))
            .expect("the real agent streams");
        assert!(!events.is_empty(), "the run emitted agent events");
        assert!(!runner.is_cached("t1"));

        rt().block_on(runner.stream_events(vec![Msg::user("go")], options("t2")))
            .expect("the second task builds its own agent");
        assert_eq!(
            builds.load(Ordering::SeqCst),
            4,
            "two metadata lookups plus one agent per task"
        );
    }

    #[test]
    fn a_failing_builder_is_reported_and_never_cached() {
        let runner = ReActAgentWithBuilderRunner::with_builder(|| {
            Err(A2aError::Transport("no model".to_string()))
        });
        let error = rt()
            .block_on(runner.stream_events(vec![Msg::user("go")], options("t1")))
            .unwrap_err();
        assert!(matches!(error, A2aError::Transport(_)));
        assert!(runner.cached_task_ids().is_empty());
    }

    // Java: 431031a7a — an old stream that terminates after a restart must not remove the agent
    // that replaced it (`agentCache.remove(taskId, agent)` in `doFinally`).
    #[test]
    fn an_old_stream_never_evicts_the_replacement_agent() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Pending,
        ));

        let old_stream = rt().block_on(async {
            let mut stream = runner
                .stream_events_incremental(vec![Msg::user("one")], options("t1"))
                .expect("incremental stream");
            assert!(stream.next().await.is_some());
            stream
        });
        assert_eq!(runner.cached_task_ids(), vec!["t1".to_string()]);

        // Stop (cancel) the run and restart the same task id, as a client retry does.
        runner.stop("t1");
        assert!(runner.cached_task_ids().is_empty());
        assert_eq!(recorder.interrupted(), vec!["a0".to_string()]);

        let replacement = rt().block_on(async {
            let mut stream = runner
                .stream_events_incremental(vec![Msg::user("two")], options("t1"))
                .expect("the restarted task gets a fresh agent");
            assert!(stream.next().await.is_some());
            stream
        });
        assert_eq!(runner.cached_task_ids(), vec!["t1".to_string()]);

        // The old stream terminates only now: it must leave the replacement alone.
        drop(old_stream);
        assert_eq!(
            runner.cached_task_ids(),
            vec!["t1".to_string()],
            "the terminating old stream keeps the replacement agent cached"
        );

        drop(replacement);
        assert!(runner.cached_task_ids().is_empty());
    }

    // Java: 431031a7a — the per-task slot is exclusive, so concurrent starts of one task id get
    // exactly one agent; the others report "Agent already exists for taskId" without building or
    // evicting the running agent.
    #[test]
    fn concurrent_starts_of_one_task_are_rejected_without_evicting_the_owner() {
        let recorder = Arc::new(Recorder::default());
        let runner = Arc::new(BaseReActAgentRunner::new(FakeFactory::new(
            recorder.clone(),
            FakeOutcome::Pending,
        )));

        // The owner keeps its stream alive, so the slot stays taken for the whole test.
        let owner = rt().block_on(async {
            let mut stream = runner
                .stream_events_incremental(vec![Msg::user("owner")], options("t1"))
                .expect("incremental stream");
            assert!(stream.next().await.is_some());
            stream
        });
        assert_eq!(runner.cached_task_ids(), vec!["t1".to_string()]);

        let mut handles = Vec::new();
        for index in 0..4 {
            let runner = runner.clone();
            handles.push(std::thread::spawn(move || {
                rt().block_on(async move {
                    let mut stream = runner
                        .stream_events_incremental(
                            vec![Msg::user(format!("loser-{index}"))],
                            options("t1"),
                        )
                        .expect("the contract still yields a stream");
                    matches!(stream.next().await, Some(Err(_)))
                })
            }));
        }
        for handle in handles {
            assert!(
                handle.join().expect("worker thread"),
                "every concurrent start is rejected"
            );
        }

        assert_eq!(
            recorder.built(),
            vec!["a0".to_string()],
            "losers never build an agent"
        );
        assert_eq!(
            runner.cached_task_ids(),
            vec!["t1".to_string()],
            "the owner keeps the slot"
        );

        drop(owner);
        assert!(runner.cached_task_ids().is_empty());
    }

    // Java: 431031a7a also rolls the cache back when stream creation fails synchronously. Rust
    // creates the guard before asking the agent for its stream, so any early failure (panicking
    // factory or agent) drops the guard and releases the slot; a failing factory is covered by
    // `a_failing_builder_is_reported_and_never_cached`.
    #[test]
    fn a_stream_error_releases_the_cache_slot() {
        let recorder = Arc::new(Recorder::default());
        let runner = BaseReActAgentRunner::new(FakeFactory::new(
            recorder,
            FakeOutcome::Fail,
        ));

        let error = rt()
            .block_on(runner.stream_events(vec![Msg::user("go")], options("t1")))
            .unwrap_err();
        assert!(matches!(error, A2aError::Transport(_)));
        assert!(runner.cached_task_ids().is_empty());
    }
}
