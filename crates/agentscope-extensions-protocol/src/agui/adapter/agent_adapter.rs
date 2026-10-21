//! AG-UI agent adapter, mirroring `AguiAgentAdapter` and `AguiAgentAdapterFactory`.
//!
//! The adapter converts a [`RunAgentInput`] into AgentScope messages, builds the run-scoped tool
//! view, runs the agent's fine-grained event stream and converts every AgentScope event into
//! AG-UI events.
//!
//! **Adapter layer (design note).** Java's adapter reaches into `ReActAgent` and `HarnessAgent`
//! through reflection (`AguiUtil.isHarnessAgent` / `getDelegate`) to avoid a compile-time harness
//! dependency. Rust needs no reflection: both types expose the same
//! `stream_events(Vec<Msg>, StreamOptions)` signature, so the adapter targets the local
//! [`AguiStreamableAgent`] trait, implemented for both.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::Extensions;
use agentscope_core::tool::{
    ToolMergeMode, ToolRequestConfig, TOOL_REQUEST_CONFIG_CONTEXT_KEY,
};
use agentscope_core::{Agent, AgentEvent, EventCategory, Msg, RuntimeContext, StreamOptions};
use futures::stream::{self, StreamExt};
use serde_json::{Map, Value};

use crate::agui::adapter::config::AguiAdapterConfig;
use crate::agui::adapter::strategy::{AgentEventConverterRegistry, AguiStreamContext};
use crate::agui::converter::message::AguiMessageConverter;
use crate::agui::converter::tool::AguiToolConverter;
use crate::agui::event::{AguiEvent, Interrupt};
use crate::agui::model::RunAgentInput;

/// Runtime-context metadata key: the AG-UI thread id.
pub const RUNTIME_CONTEXT_THREAD_ID_KEY: &str = "agui.threadId";
/// Runtime-context metadata key: the AG-UI run id.
pub const RUNTIME_CONTEXT_RUN_ID_KEY: &str = "agui.runId";
/// Runtime-context metadata key: the AG-UI messages.
pub const RUNTIME_CONTEXT_MESSAGES_KEY: &str = "agui.messages";
/// Runtime-context metadata key: the frontend-provided tools.
pub const RUNTIME_CONTEXT_TOOLS_KEY: &str = "agui.tools";
/// Runtime-context metadata key: the AG-UI context items.
pub const RUNTIME_CONTEXT_CONTEXT_KEY: &str = "agui.context";
/// Runtime-context metadata key: the initial state map.
pub const RUNTIME_CONTEXT_STATE_KEY: &str = "agui.state";
/// Runtime-context metadata key: the forwarded properties.
pub const RUNTIME_CONTEXT_FORWARDED_PROPS_KEY: &str = "agui.forwardedProps";
/// Runtime-context metadata key: the resume entries.
pub const RUNTIME_CONTEXT_RESUME_KEY: &str = "agui.resume";
/// Runtime-context metadata key: known originating interrupts, keyed by interrupt id.
pub const RUNTIME_CONTEXT_RESUME_INTERRUPTS_KEY: &str = "agui.resume.interrupts";

/// An [`Agent`] whose fine-grained event stream can be driven by the AG-UI adapter.
///
/// Implemented for `ReActAgent` and `HarnessAgent`. Custom agents can implement this trait and
/// then be adapted with [`AguiAgentAdapter`].
pub trait AguiStreamableAgent: Agent {
    /// Streams fine-grained agent events for the given messages.
    fn stream_events(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>>;

    /// Streams fine-grained agent events with per-run context extensions.
    ///
    /// The adapter carries a per-call [`ToolRequestConfig`] this way, so external tools never
    /// touch the shared toolkit (mirroring Java's `RuntimeContext`-carried config). Agents that
    /// cannot accept extensions keep the default, which drops them.
    fn stream_events_with_extensions(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
        _extensions: Extensions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
        self.stream_events(messages, options)
    }
}

impl AguiStreamableAgent for agentscope_core::ReActAgent {
    fn stream_events(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
        agentscope_core::ReActAgent::stream_events(self, messages, options)
    }

    fn stream_events_with_extensions(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
        extensions: Extensions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
        agentscope_core::ReActAgent::stream_events_with_extensions(self, messages, options, extensions)
    }
}

impl AguiStreamableAgent for agentscope_harness::agent::HarnessAgent {
    fn stream_events(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
        agentscope_harness::agent::HarnessAgent::stream_events(self, messages, options)
    }

    fn stream_events_with_extensions(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
        extensions: Extensions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
        agentscope_harness::agent::HarnessAgent::stream_events_with_extensions(
            self, messages, options, extensions,
        )
    }
}

/// A boxed stream of AG-UI events.
///
/// Failures are reported as `RUN_ERROR` events, mirroring the AG-UI contract where the run
/// lifecycle owns error reporting.
pub type AguiEventStream = futures::stream::BoxStream<'static, AguiEvent>;

/// The result of running the adapter: the AG-UI event stream plus the conversion context.
pub struct AguiRunStream {
    events: AguiEventStream,
    stream_context: Arc<Mutex<AguiStreamContext>>,
}

impl AguiRunStream {
    /// The underlying AG-UI event stream.
    pub fn events_mut(&mut self) -> &mut AguiEventStream {
        &mut self.events
    }

    /// Takes the event stream, leaving an empty one behind.
    pub fn take_events(&mut self) -> AguiEventStream {
        std::mem::replace(&mut self.events, Box::pin(stream::empty()))
    }

    /// The conversion context, for inspecting pending interrupts after the run.
    pub fn stream_context(&self) -> Arc<Mutex<AguiStreamContext>> {
        Arc::clone(&self.stream_context)
    }

    /// Drains the run into the ordered list of AG-UI events.
    pub async fn collect_events(mut self) -> Vec<AguiEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.events.next().await {
            events.push(event);
        }
        events
    }
}

impl std::fmt::Debug for AguiRunStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AguiRunStream").finish_non_exhaustive()
    }
}

/// Adapts an agent to the AG-UI protocol.
pub struct AguiAgentAdapter {
    agent: Arc<dyn AguiStreamableAgent>,
    config: AguiAdapterConfig,
    message_converter: AguiMessageConverter,
    registry: Arc<AgentEventConverterRegistry>,
}

impl std::fmt::Debug for AguiAgentAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AguiAgentAdapter")
            .field("agent_id", &self.agent.agent_id())
            .field("config", &self.config)
            .finish()
    }
}

impl AguiAgentAdapter {
    /// Creates an adapter for `agent`.
    pub fn new(agent: Arc<dyn AguiStreamableAgent>, config: AguiAdapterConfig) -> Self {
        let registry = Arc::new(AgentEventConverterRegistry::new(
            config.event_converters().to_vec(),
            config.event_enrichers().to_vec(),
            config.emit_subagent_events_as_native(),
        ));
        Self {
            agent,
            config,
            message_converter: AguiMessageConverter::new(),
            registry,
        }
    }

    /// The adapted agent.
    pub fn agent(&self) -> &Arc<dyn AguiStreamableAgent> {
        &self.agent
    }

    /// The adapter configuration.
    pub fn config(&self) -> &AguiAdapterConfig {
        &self.config
    }

    /// The event converter registry used by this adapter.
    pub fn registry(&self) -> &Arc<AgentEventConverterRegistry> {
        &self.registry
    }

    /// Runs the agent and returns the AG-UI event stream.
    pub fn run(&self, input: &RunAgentInput) -> AguiRunStream {
        self.run_with_context(input, None)
    }

    /// Runs the agent with a caller-provided runtime context.
    pub fn run_with_context(
        &self,
        input: &RunAgentInput,
        runtime_context: Option<RuntimeContext>,
    ) -> AguiRunStream {
        let effective_runtime_context = build_runtime_context(input, runtime_context);

        // 1. Convert messages (including official resume entries).
        let interrupts = resume_interrupts(&effective_runtime_context);
        let msgs = match self
            .message_converter
            .to_input_msg_list_with_interrupts(input, &interrupts)
        {
            Ok(msgs) => msgs,
            Err(error) => return self.failed_run(input, &error.to_string(), true),
        };

        // 2. Build the run-scoped tool view. It travels as a runtime-context extension, so the
        // shared toolkit is never mutated and concurrent runs each keep their own tool visibility.
        let toolkit = self.agent.toolkit().clone();
        let tool_config = match self.tool_request_config(input) {
            Ok(config) => config,
            Err(error) => return self.failed_run(input, &error, true),
        };
        let mut extensions = Extensions::new();
        if let Some(config) = tool_config {
            extensions.insert(TOOL_REQUEST_CONFIG_CONTEXT_KEY, config);
        }

        // 3. Drive the agent and convert every event.
        //
        // `eventTypes(ALL) + incremental(true)` mirrors Java's `StreamOptions` for this path.
        let options = StreamOptions::builder()
            .event_types(&[EventCategory::All])
            .incremental(true)
            .build();
        let agent_stream = self
            .agent
            .stream_events_with_extensions(msgs, options, extensions);
        let context = Arc::new(Mutex::new(AguiStreamContext::with_toolkit(
            input.thread_id.clone(),
            input.run_id.clone(),
            self.config.clone(),
            Some(input.clone()),
            Some(toolkit),
        )));

        let registry = Arc::clone(&self.registry);
        let per_event_context = Arc::clone(&context);
        // Mid-stream failures must not re-emit `RUN_STARTED`: the run is already live, and a
        // leading `RUN_STARTED` here would duplicate the one produced by the `AgentStart`
        // event (or surface even when the agent failed before emitting any event).
        let error_events = self.error_events_with(input, false);

        let events: AguiEventStream = Box::pin(
            agent_stream
                .then(move |result| {
                    let registry = Arc::clone(&registry);
                    let context = Arc::clone(&per_event_context);
                    let error_events = error_events.clone();
                    async move {
                        let mut guard = match context.lock() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        let mut emitted: Vec<AguiEvent> = Vec::new();
                        match result {
                            Ok(event) => emitted.extend(registry.convert(&event, &mut guard)),
                            Err(error) => {
                                // Flush open blocks, then report the failure, mirroring Java's
                                // `onErrorResume` which finishes pending events before the error.
                                emitted.extend(guard.finish_pending_events());
                                let events = registry.enrich(None, error_events.clone(), &guard);
                                emitted.extend(decorate_error(events, &error.to_string()));
                            }
                        }
                        stream::iter(emitted)
                    }
                })
                .flatten(),
        );

        AguiRunStream {
            events,
            stream_context: Arc::clone(&context),
        }
    }

    /// Builds the run-scoped tool view from the request.
    ///
    /// Returns `None` when the request carries nothing to inject (`AGENT_ONLY`, or no frontend
    /// tools): the shared toolkit is then used as-is, exactly like `ToolRequestConfig.NONE`.
    fn tool_request_config(&self, input: &RunAgentInput) -> Result<Option<ToolRequestConfig>, String> {
        let mode = self.config.tool_merge_mode();
        if mode == ToolMergeMode::AgentOnly || !input.has_tools() {
            return Ok(None);
        }
        let schemas = AguiToolConverter::new().to_tool_schema_list(&input.tools);
        ToolRequestConfig::from_schemas(schemas, mode)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    /// Builds the AG-UI error lifecycle events describing a run failure.
    ///
    /// The events carry the generic failure message; [`Self::error_events_with_message`] lets a
    /// caller supply the originating error text.
    pub fn error_events_with(&self, input: &RunAgentInput, include_run_started: bool) -> Vec<AguiEvent> {
        self.error_events_with_message(input, "AG-UI adapter run failed", include_run_started)
    }

    /// Builds the AG-UI error lifecycle events carrying `message`.
    ///
    /// `include_run_started` mirrors Java's distinction between a failure before any event was
    /// observed (emit `RUN_STARTED` first) and a mid-stream failure (the run had already started).
    pub fn error_events_with_message(
        &self,
        input: &RunAgentInput,
        message: &str,
        include_run_started: bool,
    ) -> Vec<AguiEvent> {
        let mut events = Vec::new();
        if include_run_started {
            events.push(run_started(input));
        }
        events.push(run_error(input, message));
        if self.config.emit_run_finished_after_error() {
            events.push(AguiEvent::RunFinished {
                thread_id: input.thread_id.clone(),
                run_id: input.run_id.clone(),
                result: None,
                outcome: None,
                timestamp: None,
                raw_event: None,
            });
        }
        events
    }

    /// Builds a complete error run for a failure before streaming begins.
    fn failed_run(
        &self,
        input: &RunAgentInput,
        message: &str,
        include_run_started: bool,
    ) -> AguiRunStream {
        let events = self.error_events_with_message(input, message, include_run_started);
        let context = Arc::new(Mutex::new(AguiStreamContext::new(
            input.thread_id.clone(),
            input.run_id.clone(),
            self.config.clone(),
            Some(input.clone()),
        )));
        AguiRunStream {
            events: Box::pin(stream::iter(events)),
            stream_context: context,
        }
    }
}

/// Replaces a generic `RUN_ERROR` message with the real failure text.
fn decorate_error(events: Vec<AguiEvent>, message: &str) -> Vec<AguiEvent> {
    events
        .into_iter()
        .map(|event| match event {
            AguiEvent::RunError {
                thread_id,
                run_id,
                timestamp,
                raw_event,
                ..
            } => run_error_with(
                &thread_id,
                &run_id,
                message,
                timestamp,
                raw_event,
            ),
            other => other,
        })
        .collect()
}

/// Builds the `RUN_STARTED` event for a run input.
fn run_started(input: &RunAgentInput) -> AguiEvent {
    AguiEvent::RunStarted {
        thread_id: input.thread_id.clone(),
        run_id: input.run_id.clone(),
        parent_run_id: None,
        input: serde_json::to_value(input).ok(),
        timestamp: None,
        raw_event: None,
    }
}

/// Builds a `RUN_ERROR` event with a timestamp taken now.
fn run_error(input: &RunAgentInput, message: &str) -> AguiEvent {
    run_error_with(
        &input.thread_id,
        &input.run_id,
        message,
        Some(chrono::Utc::now().timestamp_millis()),
        None,
    )
}

/// Builds a `RUN_ERROR` event from explicit parts.
fn run_error_with(
    thread_id: &str,
    run_id: &str,
    message: &str,
    timestamp: Option<i64>,
    raw_event: Option<Value>,
) -> AguiEvent {
    AguiEvent::RunError {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        message: message.to_string(),
        code: Some(map_error_code(message)),
        timestamp,
        raw_event,
    }
}

/// A factory for per-request [`AguiAgentAdapter`] instances.
pub trait AguiAgentAdapterFactory: Send + Sync {
    /// Creates an adapter for a resolved agent.
    fn create(
        &self,
        agent: Arc<dyn AguiStreamableAgent>,
        config: AguiAdapterConfig,
    ) -> AguiAgentAdapter;
}

/// The default factory: creates a standard [`AguiAgentAdapter`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultAguiAgentAdapterFactory;

impl AguiAgentAdapterFactory for DefaultAguiAgentAdapterFactory {
    fn create(
        &self,
        agent: Arc<dyn AguiStreamableAgent>,
        config: AguiAdapterConfig,
    ) -> AguiAgentAdapter {
        AguiAgentAdapter::new(agent, config)
    }
}

/// Builds the effective runtime context for a run, stamping AG-UI protocol metadata.
///
/// The caller-provided context is copied and enriched so callers can add custom attributes
/// without losing the protocol defaults; the session id always comes from `input.threadId`.
///
/// Java stores the `RunAgentInput` itself under the `RunAgentInput.class` key; Rust keeps typed
/// metadata under stable string keys instead, which is the idiomatic equivalent and avoids
/// coupling the runtime context to a specific payload type.
pub fn build_runtime_context(
    input: &RunAgentInput,
    runtime_context: Option<RuntimeContext>,
) -> RuntimeContext {
    let mut context = runtime_context.unwrap_or_default();
    context.session_id = Some(input.thread_id.clone());
    let metadata = &mut context.metadata;
    metadata.insert(
        RUNTIME_CONTEXT_THREAD_ID_KEY.to_string(),
        Value::String(input.thread_id.clone()),
    );
    metadata.insert(
        RUNTIME_CONTEXT_RUN_ID_KEY.to_string(),
        Value::String(input.run_id.clone()),
    );
    metadata.insert(
        RUNTIME_CONTEXT_MESSAGES_KEY.to_string(),
        serde_json::to_value(&input.messages).unwrap_or(Value::Array(Vec::new())),
    );
    metadata.insert(
        RUNTIME_CONTEXT_TOOLS_KEY.to_string(),
        serde_json::to_value(&input.tools).unwrap_or(Value::Array(Vec::new())),
    );
    metadata.insert(
        RUNTIME_CONTEXT_CONTEXT_KEY.to_string(),
        serde_json::to_value(&input.context).unwrap_or(Value::Array(Vec::new())),
    );
    metadata.insert(
        RUNTIME_CONTEXT_STATE_KEY.to_string(),
        Value::Object(input.state.clone()),
    );
    metadata.insert(
        RUNTIME_CONTEXT_FORWARDED_PROPS_KEY.to_string(),
        Value::Object(input.forwarded_props.clone()),
    );
    metadata.insert(
        RUNTIME_CONTEXT_RESUME_KEY.to_string(),
        serde_json::to_value(&input.resume).unwrap_or(Value::Array(Vec::new())),
    );
    context
}

/// Reads the known originating interrupts stored on a runtime context.
pub fn resume_interrupts(runtime_context: &RuntimeContext) -> HashMap<String, Interrupt> {
    let Some(Value::Object(map)) = runtime_context
        .metadata
        .get(RUNTIME_CONTEXT_RESUME_INTERRUPTS_KEY)
    else {
        return HashMap::new();
    };
    map.iter()
        .filter_map(|(key, value)| {
            serde_json::from_value::<Interrupt>(value.clone())
                .ok()
                .map(|interrupt| (key.clone(), interrupt))
        })
        .collect()
}

/// Stores known originating interrupts on a runtime context.
pub fn put_resume_interrupts(
    runtime_context: &mut RuntimeContext,
    interrupts: &HashMap<String, Interrupt>,
) {
    let mut map = Map::new();
    for (key, interrupt) in interrupts {
        map.insert(
            key.clone(),
            serde_json::to_value(interrupt).unwrap_or(Value::Null),
        );
    }
    runtime_context.metadata.insert(
        RUNTIME_CONTEXT_RESUME_INTERRUPTS_KEY.to_string(),
        Value::Object(map),
    );
}

/// Maps a failure message to the AG-UI error code used by Java's `mapErrorCode`.
///
/// Java branches on the exception class; Rust carries failures as strings, so the same four codes
/// are derived from the message text.
pub fn map_error_code(message: &str) -> String {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("timed out") || lowered.contains("timeout") {
        "TIMEOUT_ERROR".to_string()
    } else if lowered.contains("interrupt") {
        "INTERRUPTED_ERROR".to_string()
    } else if lowered.contains("invalid") || lowered.contains("illegal") {
        "INVALID_INPUT_ERROR".to_string()
    } else {
        "INTERNAL_ERROR".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::event::AguiEventType as Wire;
    use crate::agui::model::{
        AguiContext, AguiMessage, AguiResume, AguiTool, InputContent, MessageContent, ToolMergeMode,
    };
    use agentscope_core::{AgentEventType as CoreEventType, JsonMap, MsgRole, ToolSchema, Toolkit};
    use serde_json::json;

    /// A model-free agent used to exercise the adapter's message/tool/stream plumbing.
    struct ScriptedAgent {
        id: String,
        name: String,
        toolkit: Toolkit,
        script: Mutex<Vec<AgentEvent>>,
        fail_with: Mutex<Option<String>>,
        seen_messages: Mutex<Vec<Msg>>,
        seen_extensions: Mutex<Vec<Extensions>>,
    }

    impl ScriptedAgent {
        fn new(script: Vec<AgentEvent>) -> Self {
            Self {
                id: "scripted".to_string(),
                name: "scripted".to_string(),
                toolkit: Toolkit::new(),
                script: Mutex::new(script),
                fail_with: Mutex::new(None),
                seen_messages: Mutex::new(Vec::new()),
                seen_extensions: Mutex::new(Vec::new()),
            }
        }

        fn failing(message: &str) -> Self {
            let agent = Self::new(Vec::new());
            *agent.fail_with.lock().unwrap() = Some(message.to_string());
            agent
        }

        fn captured_messages(&self) -> Vec<Msg> {
            self.seen_messages.lock().unwrap().clone()
        }

        /// The tool config the adapter handed this run, if any.
        fn captured_tool_config(&self) -> Option<Arc<ToolRequestConfig>> {
            self.seen_extensions
                .lock()
                .unwrap()
                .last()
                .and_then(|extensions| {
                    extensions.get::<ToolRequestConfig>(TOOL_REQUEST_CONFIG_CONTEXT_KEY)
                })
        }
    }

    impl Agent for ScriptedAgent {
        fn agent_id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "scripted agent"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> agentscope_core::AgentState {
            agentscope_core::AgentState::default()
        }
        fn toolkit(&self) -> &Toolkit {
            &self.toolkit
        }
    }

    impl AguiStreamableAgent for ScriptedAgent {
        fn stream_events(
            &self,
            messages: Vec<Msg>,
            _options: StreamOptions,
        ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
            self.seen_messages.lock().unwrap().extend(messages);
            if let Some(message) = self.fail_with.lock().unwrap().clone() {
                return Box::pin(stream::iter(vec![Err(agentscope_core::Error::Agent(
                    message,
                ))]));
            }
            let scripted = self.script.lock().unwrap().clone();
            Box::pin(stream::iter(scripted.into_iter().map(Ok)))
        }

        fn stream_events_with_extensions(
            &self,
            messages: Vec<Msg>,
            options: StreamOptions,
            extensions: Extensions,
        ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
            self.seen_extensions.lock().unwrap().push(extensions);
            self.stream_events(messages, options)
        }
    }

    fn delta(type_: CoreEventType, fields: &[(&str, Value)]) -> AgentEvent {
        let mut map = JsonMap::new();
        for (key, value) in fields {
            map.insert((*key).to_string(), value.clone());
        }
        AgentEvent::with_fields(type_, map)
    }

    fn text_run() -> Vec<AgentEvent> {
        vec![
            AgentEvent::new(CoreEventType::AgentStart),
            delta(
                CoreEventType::TextBlockDelta,
                &[
                    ("reply_id", json!("rep1")),
                    ("block_type", json!("text")),
                    ("delta", json!("hello")),
                ],
            ),
            delta(
                CoreEventType::TextBlockEnd,
                &[("reply_id", json!("rep1")), ("block_type", json!("text"))],
            ),
            AgentEvent::new(CoreEventType::AgentEnd),
        ]
    }

    fn adapter_for(agent: Arc<ScriptedAgent>, config: AguiAdapterConfig) -> AguiAgentAdapter {
        AguiAgentAdapter::new(agent, config)
    }

    fn wire_types(events: &[AguiEvent]) -> Vec<&'static str> {
        events
            .iter()
            .filter_map(|event| event.event_type().map(|t| t.as_str()))
            .collect()
    }

    #[test]
    fn runtime_context_carries_every_agui_metadata_key() {
        let input = RunAgentInput::new(
            "t1",
            "r1",
            vec![AguiMessage::user_message("m1", "hi")],
            vec![AguiTool::new("search", "d", serde_json::Map::new())],
            vec![AguiContext::new("k", "v")],
            serde_json::from_value(json!({"count": 1})).unwrap(),
            serde_json::from_value(json!({"agentId": "assistant"})).unwrap(),
            vec![AguiResume::new(
                "i1",
                AguiResume::STATUS_RESOLVED,
                None,
            )],
        );
        let context = build_runtime_context(&input, None);
        assert_eq!(context.session_id(), Some("t1"));
        assert_eq!(context.metadata[RUNTIME_CONTEXT_THREAD_ID_KEY], json!("t1"));
        assert_eq!(context.metadata[RUNTIME_CONTEXT_RUN_ID_KEY], json!("r1"));
        assert_eq!(context.metadata[RUNTIME_CONTEXT_MESSAGES_KEY][0]["id"], json!("m1"));
        assert_eq!(context.metadata[RUNTIME_CONTEXT_TOOLS_KEY][0]["name"], json!("search"));
        assert_eq!(context.metadata[RUNTIME_CONTEXT_CONTEXT_KEY][0]["value"], json!("v"));
        assert_eq!(context.metadata[RUNTIME_CONTEXT_STATE_KEY], json!({"count": 1}));
        assert_eq!(
            context.metadata[RUNTIME_CONTEXT_FORWARDED_PROPS_KEY],
            json!({"agentId": "assistant"})
        );
        assert_eq!(
            context.metadata[RUNTIME_CONTEXT_RESUME_KEY][0]["interruptId"],
            json!("i1")
        );
    }

    #[test]
    fn runtime_context_preserves_caller_values_but_overrides_the_session() {
        let input = RunAgentInput::of("thread", "run");
        let mut caller = RuntimeContext::default().with_user_id("u1");
        caller.session_id = Some("other".to_string());
        caller
            .metadata
            .insert("custom".to_string(), json!("kept"));
        let context = build_runtime_context(&input, Some(caller));
        assert_eq!(context.session_id(), Some("thread"));
        assert_eq!(context.user_id(), Some("u1"));
        assert_eq!(context.metadata["custom"], json!("kept"));
    }

    #[test]
    fn resume_interrupts_round_trip_through_the_runtime_context() {
        let mut interrupts = HashMap::new();
        let mut interrupt = Interrupt::new("i1", "tool_call");
        interrupt.tool_call_id = Some("c1".to_string());
        interrupts.insert("i1".to_string(), interrupt);

        let mut context = RuntimeContext::default();
        assert!(resume_interrupts(&context).is_empty());
        put_resume_interrupts(&mut context, &interrupts);
        let read = resume_interrupts(&context);
        assert_eq!(read.len(), 1);
        assert_eq!(read["i1"].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(read["i1"].reason, "tool_call");
    }

    #[test]
    fn error_codes_are_derived_from_the_message() {
        assert_eq!(map_error_code("request timed out"), "TIMEOUT_ERROR");
        assert_eq!(map_error_code("operation was interrupted"), "INTERRUPTED_ERROR");
        assert_eq!(map_error_code("invalid tool input"), "INVALID_INPUT_ERROR");
        assert_eq!(map_error_code("boom"), "INTERNAL_ERROR");
    }

    #[tokio::test]
    async fn a_text_run_emits_the_full_lifecycle_in_order() {
        let agent = Arc::new(ScriptedAgent::new(text_run()));
        let adapter = adapter_for(agent.clone(), AguiAdapterConfig::default_config());
        let input = RunAgentInput::of("t", "r").with_messages(vec![AguiMessage::user_message("m1", "hi")]);
        let events = adapter.run(&input).collect_events().await;
        assert_eq!(
            wire_types(&events),
            vec![
                "RUN_STARTED",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "RUN_FINISHED",
            ]
        );
        // The converted messages reach the agent.
        assert_eq!(agent.captured_messages().len(), 1);
        assert_eq!(agent.captured_messages()[0].text_content(), "hi");
    }

    #[tokio::test]
    async fn run_started_carries_the_serialized_input() {
        let agent = Arc::new(ScriptedAgent::new(vec![AgentEvent::new(CoreEventType::AgentStart)]));
        let adapter = adapter_for(agent, AguiAdapterConfig::default_config());
        let events = adapter
            .run(&RunAgentInput::of("t", "r"))
            .collect_events()
            .await;
        match &events[0] {
            AguiEvent::RunStarted { input, .. } => {
                let payload = input.as_ref().expect("run input is attached");
                assert_eq!(payload["threadId"], json!("t"));
            }
            other => panic!("expected RunStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn frontend_tools_travel_as_a_per_call_config_without_touching_the_toolkit() {
        let agent = Arc::new(ScriptedAgent::new(vec![AgentEvent::new(CoreEventType::AgentStart)]));
        agent
            .toolkit
            .register_schema(ToolSchema::new("agent_tool", "agent"));
        let adapter = adapter_for(agent.clone(), AguiAdapterConfig::default_config());
        let mut input = RunAgentInput::of("t", "r");
        input
            .tools
            .push(AguiTool::new("frontend", "d", serde_json::Map::new()));

        let events = adapter.run(&input).collect_events().await;
        assert!(!events.is_empty());

        // The shared registry is never mutated: the frontend tool was never registered.
        assert!(agent.toolkit.get_tool("frontend").is_none());
        assert!(agent.toolkit.get_tool("agent_tool").is_some());

        // Instead the run received an immutable per-call view.
        let config = agent.captured_tool_config().expect("tool config passed to the run");
        assert_eq!(config.merge_mode(), ToolMergeMode::MergeExternalPriority);
        assert!(config.overrides("frontend"));
        assert!(!config.hides_backend());
        assert_eq!(
            config.resolve_tool("agent_tool", &agent.toolkit).map(|t| t.name().to_string()),
            Some("agent_tool".to_string())
        );
    }

    #[tokio::test]
    async fn external_only_hides_the_backend_without_mutating_the_toolkit() {
        let agent = Arc::new(ScriptedAgent::new(vec![AgentEvent::new(CoreEventType::AgentStart)]));
        agent
            .toolkit
            .register_schema(ToolSchema::new("agent_tool", "agent"));
        let adapter = adapter_for(
            agent.clone(),
            AguiAdapterConfig::builder()
                .tool_merge_mode(ToolMergeMode::ExternalOnly)
                .build(),
        );
        let mut input = RunAgentInput::of("t", "r");
        input
            .tools
            .push(AguiTool::new("frontend", "d", serde_json::Map::new()));
        adapter.run(&input).collect_events().await;

        let config = agent.captured_tool_config().expect("tool config passed to the run");
        assert!(config.hides_backend());
        assert!(config.resolve_tool("agent_tool", &agent.toolkit).is_none());
        assert!(config.resolve_tool("frontend", &agent.toolkit).is_some());
        // Hidden, not unregistered.
        assert!(agent.toolkit.get_tool("agent_tool").is_some());
    }

    #[tokio::test]
    async fn agent_only_mode_passes_no_tool_config() {
        let agent = Arc::new(ScriptedAgent::new(vec![AgentEvent::new(CoreEventType::AgentStart)]));
        let adapter = adapter_for(
            agent.clone(),
            AguiAdapterConfig::builder()
                .tool_merge_mode(ToolMergeMode::AgentOnly)
                .build(),
        );
        let mut input = RunAgentInput::of("t", "r");
        input
            .tools
            .push(AguiTool::new("frontend", "d", serde_json::Map::new()));
        adapter.run(&input).collect_events().await;
        assert!(agent.captured_tool_config().is_none());
        assert!(agent.toolkit.get_tool("frontend").is_none());
    }

    #[tokio::test]
    async fn run_errors_surface_as_run_error_events() {
        let agent = Arc::new(ScriptedAgent::failing("invalid tool input"));
        let adapter = adapter_for(agent, AguiAdapterConfig::default_config());
        let events = adapter
            .run(&RunAgentInput::of("t", "r"))
            .collect_events()
            .await;
        assert_eq!(wire_types(&events), vec!["RUN_ERROR"]);
        match &events[0] {
            AguiEvent::RunError { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("INVALID_INPUT_ERROR"));
                assert!(message.contains("invalid tool input"));
            }
            other => panic!("expected RunError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_clients_receive_run_finished_after_an_error() {
        let agent = Arc::new(ScriptedAgent::failing("boom"));
        let adapter = adapter_for(
            agent,
            AguiAdapterConfig::builder()
                .emit_run_finished_after_error(true)
                .build(),
        );
        let events = adapter
            .run(&RunAgentInput::of("t", "r"))
            .collect_events()
            .await;
        assert_eq!(wire_types(&events), vec!["RUN_ERROR", "RUN_FINISHED"]);
    }

    #[tokio::test]
    async fn structured_content_on_a_non_user_message_fails_before_streaming() {
        let agent = Arc::new(ScriptedAgent::new(text_run()));
        let adapter = adapter_for(agent, AguiAdapterConfig::default_config());
        let mut input = RunAgentInput::of("t", "r");
        input.messages.push(AguiMessage::new(
            "m1",
            "assistant",
            Some(MessageContent::Blocks(vec![InputContent::Text {
                text: "x".to_string(),
            }])),
            Vec::new(),
            None,
        ));
        let events = adapter.run(&input).collect_events().await;
        assert_eq!(wire_types(&events), vec!["RUN_STARTED", "RUN_ERROR"]);
        match &events[1] {
            AguiEvent::RunError { message, .. } => {
                assert!(message.contains("only supported for AG-UI user messages"));
            }
            other => panic!("expected RunError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resume_entries_are_resolved_into_messages_for_the_agent() {
        let agent = Arc::new(ScriptedAgent::new(vec![AgentEvent::new(CoreEventType::AgentStart)]));
        let adapter = adapter_for(agent.clone(), AguiAdapterConfig::default_config());
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "reply:c1",
            AguiResume::STATUS_RESOLVED,
            Some(json!("result")),
        )]);
        adapter.run(&input).collect_events().await;
        let captured = agent.captured_messages();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].role(), MsgRole::Tool);
        assert_eq!(
            captured[0].tool_result_blocks().next().unwrap().text_content(),
            "result"
        );
    }

    #[tokio::test]
    async fn pending_blocks_are_flushed_before_a_run_error() {
        /// An agent whose stream yields a text delta and then fails.
        struct HalfStreamAgent;

        impl Agent for HalfStreamAgent {
            fn agent_id(&self) -> &str {
                "half"
            }
            fn name(&self) -> &str {
                "half"
            }
            fn description(&self) -> &str {
                "half"
            }
            fn interrupt(&self) {}
            fn agent_state(&self) -> agentscope_core::AgentState {
                agentscope_core::AgentState::default()
            }
            fn toolkit(&self) -> &Toolkit {
                static TOOLKIT: std::sync::OnceLock<Toolkit> = std::sync::OnceLock::new();
                TOOLKIT.get_or_init(Toolkit::new)
            }
        }

        impl AguiStreamableAgent for HalfStreamAgent {
            fn stream_events(
                &self,
                _messages: Vec<Msg>,
                _options: StreamOptions,
            ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
                let events = vec![
                    Ok(AgentEvent::new(CoreEventType::AgentStart)),
                    Ok(delta(
                        CoreEventType::TextBlockDelta,
                        &[
                            ("reply_id", json!("rep")),
                            ("block_type", json!("text")),
                            ("delta", json!("partial")),
                        ],
                    )),
                    Err(agentscope_core::Error::Agent("stream broke".to_string())),
                ];
                Box::pin(stream::iter(events))
            }
        }

        let adapter = AguiAgentAdapter::new(
            Arc::new(HalfStreamAgent),
            AguiAdapterConfig::default_config(),
        );
        let events = adapter
            .run(&RunAgentInput::of("t", "r"))
            .collect_events()
            .await;
        assert_eq!(
            wire_types(&events),
            vec![
                "RUN_STARTED",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                // The open text message is closed before the error is reported.
                "TEXT_MESSAGE_END",
                "RUN_ERROR",
            ]
        );
        match events.last().unwrap() {
            AguiEvent::RunError { message, .. } => assert!(message.contains("stream broke")),
            other => panic!("expected RunError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_adapter_exposes_its_config_registry_and_context() {
        let agent = Arc::new(ScriptedAgent::new(Vec::new()));
        let config = AguiAdapterConfig::builder()
            .default_agent_id("assistant")
            .build();
        let adapter = adapter_for(agent, config);
        assert_eq!(adapter.config().default_agent_id(), Some("assistant"));
        assert_eq!(adapter.agent().agent_id(), "scripted");
        assert!(adapter.registry().registered_event_types().len() > 20);
        assert!(format!("{adapter:?}").contains("AguiAgentAdapter"));

        let mut run = adapter.run(&RunAgentInput::of("t", "r"));
        assert!(format!("{run:?}").contains("AguiRunStream"));
        let context = run.stream_context();
        let _events = run.take_events();
        assert!(context.lock().unwrap().pending_interrupts().is_empty());
    }

    #[test]
    fn the_default_factory_builds_a_standard_adapter() {
        let agent = Arc::new(ScriptedAgent::new(Vec::new()));
        let factory = DefaultAguiAgentAdapterFactory;
        let adapter = factory.create(agent, AguiAdapterConfig::default_config());
        assert_eq!(adapter.agent().agent_id(), "scripted");
        let _ = Wire::RunStarted;
    }
}
