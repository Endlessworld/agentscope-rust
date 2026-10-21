//! Core AG-UI request processor, mirroring `AguiRequestProcessor`.
//!
//! Encapsulates the request-level logic shared by HTTP transports: agent id resolution from four
//! sources, follow-up-message extraction for server-side-memory threads, and interrupt resume
//! coordination.
//!
//! **Design note (stream shape).** Java returns `Flux<AguiEvent>`; Rust returns a future that
//! resolves to the same ordered event vector the adapter produces. The processor therefore owns
//! the resume lifecycle (begin run, track interrupts, finish run) synchronously around the agent
//! run instead of through reactive `doOnComplete`/`doFinally` hooks.

use agentscope_core::RuntimeContext;

use crate::agui::adapter::{AguiAdapterConfig, AguiAgentAdapter};
use crate::agui::error::AguiError;
use crate::agui::event::AguiEvent;
use crate::agui::model::{AguiMessage, RunAgentInput};
use crate::agui::processor::resolver::AgentResolver;
use crate::agui::processor::resume_coordinator::AguiResumeCoordinator;
use crate::agui::runtime::{
    AguiRuntimeContextRequest, SharedRuntimeContextResolver,
};

/// The fallback agent id used when nothing else resolves one.
pub const DEFAULT_AGENT_ID: &str = "default";

/// The outcome of processing an AG-UI request.
#[derive(Debug, Clone)]
pub struct ProcessResult {
    /// The resolved agent id.
    pub agent_id: String,
    /// The resolved caller-provided runtime context (AG-UI metadata excluded until the adapter
    /// enriches it).
    pub runtime_context: RuntimeContext,
    /// The AG-UI event stream produced by the run.
    pub events: Vec<AguiEvent>,
}

impl ProcessResult {
    /// The wire event-type names, for assertions and logging.
    pub fn wire_types(&self) -> Vec<&'static str> {
        self.events
            .iter()
            .filter_map(|event| event.event_type().map(|t| t.as_str()))
            .collect()
    }

    /// Whether the run ended with an error.
    pub fn has_error(&self) -> bool {
        self.events
            .iter()
            .any(|event| matches!(event, AguiEvent::RunError { .. }))
    }
}

/// Core processor for AG-UI requests.
pub struct AguiRequestProcessor {
    agent_resolver: Box<dyn AgentResolver>,
    config: AguiAdapterConfig,
    resume_coordinator: AguiResumeCoordinator,
    runtime_context_resolver: Option<SharedRuntimeContextResolver>,
}

impl std::fmt::Debug for AguiRequestProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AguiRequestProcessor")
            .field("config", &self.config)
            .finish()
    }
}

impl AguiRequestProcessor {
    /// Creates a builder.
    pub fn builder() -> AguiRequestProcessorBuilder {
        AguiRequestProcessorBuilder::default()
    }

    /// The adapter configuration.
    pub fn config(&self) -> &AguiAdapterConfig {
        &self.config
    }

    /// The resume coordinator, exposed so callers can inspect open interrupts.
    pub fn resume_coordinator(&self) -> &AguiResumeCoordinator {
        &self.resume_coordinator
    }

    /// Processes an AG-UI request, producing the ordered AG-UI event stream.
    ///
    /// The caller-provided [`AguiRuntimeContextResolver`], when configured, is invoked with the
    /// request to obtain a [`RuntimeContext`]; the adapter then copies and enriches it. A failure
    /// to resolve the agent is reported through the AG-UI error lifecycle rather than as a Rust
    /// error, so transports can always stream a well-formed run.
    pub async fn process(&self, request: &AguiRuntimeContextRequest) -> ProcessResult {
        let input = &request.input;
        let thread_id = input.thread_id.clone();
        let run_id = input.run_id.clone();

        let resolved = self
            .runtime_context_resolver
            .as_ref()
            .and_then(|resolver| resolver.resolve(request));
        let mut runtime_context = resolved.unwrap_or_default();
        runtime_context.session_id = Some(thread_id.clone());

        let agent_id = resolve_agent_id(input, request.header_agent_id(), request.path_agent_id(), &self.config);

        // Serialize per-thread runs and validate the resume contract before anything else.
        if let crate::agui::processor::ResumeContractResult::Error(message) =
            self.resume_coordinator.begin_run(input)
        {
            return ProcessResult {
                agent_id,
                runtime_context,
                events: self.resume_coordinator.contract_error_events(
                    input,
                    &message,
                    self.config.emit_run_finished_after_error(),
                ),
            };
        }

        let agent = match self
            .agent_resolver
            .resolve_agent_for_user(&agent_id, &thread_id, runtime_context.user_id())
        {
            Ok(agent) => agent,
            Err(error) => {
                self.resume_coordinator.finish_run(&thread_id, &run_id);
                return ProcessResult {
                    agent_id,
                    runtime_context,
                    events: agent_resolution_error_events(input, &error, &self.config),
                };
            }
        };

        // When the thread already has server-side memory, only the follow-up messages after the
        // last assistant turn are appended; otherwise the frontend transcript is authoritative.
        let effective_input = if self.agent_resolver.has_memory(&runtime_context) {
            extract_latest_user_message(input)
        } else {
            input.clone()
        };

        let effective_runtime_context = self
            .resume_coordinator
            .add_resume_interrupts(input, runtime_context.clone());

        let adapter = AguiAgentAdapter::new(agent, self.config.clone());
        let mut events = Vec::new();
        let mut run_error_seen = false;
        let mut run = adapter.run_with_context(&effective_input, Some(effective_runtime_context));
        while let Some(event) = futures::StreamExt::next(run.events_mut()).await {
            if matches!(event, AguiEvent::RunError { .. }) {
                run_error_seen = true;
            }
            self.resume_coordinator
                .track_pending_interrupts(&thread_id, &run_id, &event, run_error_seen);
            events.push(event);
        }
        self.resume_coordinator.finish_run(&thread_id, &run_id);

        ProcessResult {
            agent_id,
            runtime_context,
            events,
        }
    }
}

/// Resolves the agent id from four sources, in priority order.
///
/// 1. the URL path variable, 2. the HTTP header, 3. `forwardedProps.agentId`, 4. the configured
/// default, and finally `"default"`.
pub fn resolve_agent_id(
    input: &RunAgentInput,
    header_agent_id: Option<&str>,
    path_agent_id: Option<&str>,
    config: &AguiAdapterConfig,
) -> String {
    if let Some(path_agent_id) = path_agent_id.filter(|id| !id.is_empty()) {
        return path_agent_id.to_string();
    }
    if let Some(header_agent_id) = header_agent_id.filter(|id| !id.is_empty()) {
        return header_agent_id.to_string();
    }
    if let Some(value) = input.forwarded_prop("agentId") {
        if let Some(text) = value.as_str() {
            return text.to_string();
        }
        // Java calls `toString()` on any non-null value, so scalars stringify naturally.
        if !value.is_null() {
            return value.to_string();
        }
    }
    config
        .default_agent_id()
        .map(str::to_string)
        .unwrap_or_else(|| DEFAULT_AGENT_ID.to_string())
}

/// Extracts the messages that arrived after the last assistant turn.
///
/// CopilotKit (and similar clients) always send the full transcript, including HITL tool results
/// that follow the last assistant message. When the transcript has no assistant message yet the
/// input is returned unchanged; when it ends on an assistant turn (regenerate/continue flows) the
/// last user message before that turn is returned so the agent has a prompt to regenerate from.
pub fn extract_latest_user_message(input: &RunAgentInput) -> RunAgentInput {
    if input.messages.is_empty() {
        return input.clone();
    }

    let Some(last_assistant_idx) = input
        .messages
        .iter()
        .rposition(|message| message.role.eq_ignore_ascii_case("assistant"))
    else {
        return input.clone();
    };

    let mut after: Vec<AguiMessage> = if last_assistant_idx + 1 < input.messages.len() {
        input.messages[last_assistant_idx + 1..].to_vec()
    } else {
        Vec::new()
    };

    if after.is_empty() {
        if let Some(message) = input.messages[..last_assistant_idx]
            .iter()
            .rev()
            .find(|message| message.role.eq_ignore_ascii_case("user"))
        {
            after = vec![message.clone()];
        }
    }

    RunAgentInput {
        messages: after,
        ..input.clone()
    }
}

/// Builds the AG-UI error lifecycle for an unresolvable agent.
fn agent_resolution_error_events(
    input: &RunAgentInput,
    error: &AguiError,
    config: &AguiAdapterConfig,
) -> Vec<AguiEvent> {
    let mut events = vec![AguiEvent::RunStarted {
        thread_id: input.thread_id.clone(),
        run_id: input.run_id.clone(),
        parent_run_id: None,
        input: serde_json::to_value(input).ok(),
        timestamp: None,
        raw_event: None,
    }];
    events.push(AguiEvent::RunError {
        thread_id: input.thread_id.clone(),
        run_id: input.run_id.clone(),
        message: error.to_string(),
        code: Some(match error {
            AguiError::AgentNotFound(_) => "AGENT_NOT_FOUND",
            AguiError::Encoding(_) => "ENCODING_ERROR",
            AguiError::InvalidInput(_) => "INVALID_INPUT_ERROR",
            AguiError::IllegalState(_) => "INTERNAL_ERROR",
        }
        .to_string()),
        timestamp: Some(chrono::Utc::now().timestamp_millis()),
        raw_event: None,
    });
    if config.emit_run_finished_after_error() {
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

/// Builder for [`AguiRequestProcessor`].
#[derive(Default)]
pub struct AguiRequestProcessorBuilder {
    agent_resolver: Option<Box<dyn AgentResolver>>,
    config: Option<AguiAdapterConfig>,
    runtime_context_resolver: Option<SharedRuntimeContextResolver>,
}

impl std::fmt::Debug for AguiRequestProcessorBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AguiRequestProcessorBuilder")
            .field("has_agent_resolver", &self.agent_resolver.is_some())
            .field("has_runtime_context_resolver", &self.runtime_context_resolver.is_some())
            .finish()
    }
}

impl AguiRequestProcessorBuilder {
    /// Sets the agent resolver (required).
    pub fn agent_resolver(mut self, resolver: Box<dyn AgentResolver>) -> Self {
        self.agent_resolver = Some(resolver);
        self
    }

    /// Sets the adapter configuration.
    pub fn config(mut self, config: AguiAdapterConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Sets the optional runtime context resolver.
    pub fn runtime_context_resolver(mut self, resolver: SharedRuntimeContextResolver) -> Self {
        self.runtime_context_resolver = Some(resolver);
        self
    }

    /// Builds the processor.
    ///
    /// # Panics
    ///
    /// Panics when no agent resolver was supplied, mirroring Java's
    /// `Objects.requireNonNull(builder.agentResolver)`.
    pub fn build(self) -> AguiRequestProcessor {
        AguiRequestProcessor {
            agent_resolver: self
                .agent_resolver
                .expect("agentResolver cannot be null"),
            config: self.config.unwrap_or_default(),
            resume_coordinator: AguiResumeCoordinator::new(),
            runtime_context_resolver: self.runtime_context_resolver,
        }
    }
}

/// Convenience alias for a callback-based runtime-context resolver.
pub type FnRuntimeContextResolver = fn(&AguiRuntimeContextRequest) -> Option<RuntimeContext>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::adapter::AguiStreamableAgent;
    use crate::agui::model::{AguiContext, AguiResume, AguiTool};
    use crate::agui::registry::AguiAgentRegistry;
    use crate::agui::runtime::AguiTransport;
    use agentscope_core::{Agent, AgentEvent, Msg, StreamOptions, Toolkit};
    use serde_json::{json, Map};
    use std::sync::{Arc, Mutex};

    struct StubAgent {
        id: String,
        toolkit: Toolkit,
        script: Vec<AgentEvent>,
    }

    impl StubAgent {
        fn new(id: &str, script: Vec<AgentEvent>) -> Self {
            Self {
                id: id.to_string(),
                toolkit: Toolkit::new(),
                script,
            }
        }
    }

    impl Agent for StubAgent {
        fn agent_id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.id
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn interrupt(&self) {}
        fn agent_state(&self) -> agentscope_core::AgentState {
            agentscope_core::AgentState::default()
        }
        fn toolkit(&self) -> &Toolkit {
            &self.toolkit
        }
    }

    impl AguiStreamableAgent for StubAgent {
        fn stream_events(
            &self,
            _messages: Vec<Msg>,
            _options: StreamOptions,
        ) -> futures::stream::BoxStream<'static, agentscope_core::Result<AgentEvent>> {
            Box::pin(futures::stream::iter(
                self.script.clone().into_iter().map(Ok),
            ))
        }
    }

    #[derive(Default)]
    struct TestResolver {
        registry: AguiAgentRegistry,
        memory_threads: Mutex<Vec<String>>,
    }

    impl AgentResolver for TestResolver {
        fn resolve_agent(
            &self,
            agent_id: &str,
            _thread_id: &str,
        ) -> Result<Arc<dyn AguiStreamableAgent>, AguiError> {
            self.registry
                .get_agent(agent_id)
                .ok_or_else(|| AguiError::AgentNotFound(agent_id.to_string()))
        }

        fn has_memory(&self, runtime_context: &RuntimeContext) -> bool {
            runtime_context.session_id().is_some_and(|session| {
                self.memory_threads
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|t| t == session)
            })
        }
    }

    fn start_event() -> AgentEvent {
        agentscope_core::AgentEvent::new(agentscope_core::AgentEventType::AgentStart)
    }

    fn request(input: RunAgentInput) -> AguiRuntimeContextRequest {
        AguiRuntimeContextRequest::builder()
            .input(input)
            .transport(AguiTransport::Custom)
            .build()
    }

    fn processor(resolver: TestResolver, config: AguiAdapterConfig) -> AguiRequestProcessor {
        AguiRequestProcessor::builder()
            .agent_resolver(Box::new(resolver))
            .config(config)
            .build()
    }

    #[test]
    fn agent_id_resolution_prefers_path_then_header_then_props_then_default() {
        let mut config = AguiAdapterConfig::builder().default_agent_id("from_config").build();
        let mut input = RunAgentInput::of("t", "r");
        input
            .forwarded_props
            .insert("agentId".to_string(), json!("from_props"));
        assert_eq!(resolve_agent_id(&input, Some("h"), Some("p"), &config), "p");
        assert_eq!(resolve_agent_id(&input, Some("h"), Some(""), &config), "h");
        assert_eq!(resolve_agent_id(&input, None, None, &config), "from_props");

        let mut without_props = RunAgentInput::of("t", "r");
        without_props.forwarded_props.clear();
        assert_eq!(
            resolve_agent_id(&without_props, None, None, &config),
            "from_config"
        );

        config = AguiAdapterConfig::default_config();
        assert_eq!(resolve_agent_id(&without_props, None, None, &config), "default");
    }

    #[test]
    fn non_string_forwarded_agent_ids_are_stringified() {
        let mut input = RunAgentInput::of("t", "r");
        input
            .forwarded_props
            .insert("agentId".to_string(), json!(7));
        assert_eq!(
            resolve_agent_id(&input, None, None, &AguiAdapterConfig::default_config()),
            "7"
        );
        // An explicit null is not a usable id.
        input.forwarded_props.insert("agentId".to_string(), json!(null));
        assert_eq!(
            resolve_agent_id(&input, None, None, &AguiAdapterConfig::default_config()),
            "default"
        );
    }

    #[test]
    fn follow_up_extraction_keeps_messages_after_the_last_assistant_turn() {
        let mut input = RunAgentInput::of("t", "r");
        input.messages = vec![
            AguiMessage::user_message("1", "first"),
            AguiMessage::new("2", "assistant", Some(crate::agui::model::MessageContent::Text("done".into())), Vec::new(), None),
            AguiMessage::new("3", "tool", Some(crate::agui::model::MessageContent::Text("result".into())), Vec::new(), Some("c1".into())),
        ];
        let extracted = extract_latest_user_message(&input);
        assert_eq!(extracted.messages.len(), 1);
        assert_eq!(extracted.messages[0].id, "3");
        // The rest of the input is carried over untouched.
        assert_eq!(extracted.thread_id, "t");
        assert_eq!(extracted.run_id, "r");
    }

    #[test]
    fn follow_up_extraction_regenerates_from_the_last_user_turn() {
        let mut input = RunAgentInput::of("t", "r");
        input.messages = vec![
            AguiMessage::user_message("1", "question"),
            AguiMessage::new("2", "assistant", Some(crate::agui::model::MessageContent::Text("answer".into())), Vec::new(), None),
        ];
        let extracted = extract_latest_user_message(&input);
        assert_eq!(extracted.messages.len(), 1);
        assert_eq!(extracted.messages[0].id, "1");
    }

    #[test]
    fn follow_up_extraction_is_identity_without_an_assistant_turn() {
        let mut input = RunAgentInput::of("t", "r");
        input.messages = vec![AguiMessage::user_message("1", "only user")];
        assert_eq!(extract_latest_user_message(&input), input);
        let empty = RunAgentInput::of("t", "r");
        assert_eq!(extract_latest_user_message(&empty), empty);
    }

    #[tokio::test]
    async fn a_request_runs_the_resolved_agent() {
        let mut resolver = TestResolver::default();
        resolver
            .registry
            .register("assistant", Arc::new(StubAgent::new("assistant", vec![start_event()])));
        let processor = processor(resolver, AguiAdapterConfig::default_config());
        let mut input = RunAgentInput::of("t", "r");
        input.messages.push(AguiMessage::user_message("m1", "hi"));
        // Agent id resolution falls back to "default" unless the request names one.
        input
            .forwarded_props
            .insert("agentId".to_string(), json!("assistant"));
        let result = processor.process(&request(input)).await;
        assert_eq!(result.agent_id, "assistant");
        assert_eq!(result.wire_types(), vec!["RUN_STARTED"]);
        assert!(!result.has_error());
    }

    #[tokio::test]
    async fn an_unknown_agent_reports_agent_not_found() {
        let processor = processor(TestResolver::default(), AguiAdapterConfig::default_config());
        let result = processor
            .process(&request(RunAgentInput::of("t", "r")))
            .await;
        assert!(result.has_error());
        assert_eq!(result.wire_types(), vec!["RUN_STARTED", "RUN_ERROR"]);
        match &result.events[1] {
            AguiEvent::RunError { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("AGENT_NOT_FOUND"));
                assert_eq!(message, "Agent not found: default");
            }
            other => panic!("expected RunError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_side_memory_reduces_the_input_to_follow_up_messages() {
        let mut resolver = TestResolver::default();
        resolver.memory_threads.lock().unwrap().push("t".to_string());
        let agent = Arc::new(StubAgent::new("default", vec![start_event()]));
        resolver.registry.register("default", agent);
        let processor = processor(resolver, AguiAdapterConfig::default_config());
        let mut input = RunAgentInput::of("t", "r");
        input.messages = vec![
            AguiMessage::user_message("1", "old"),
            AguiMessage::new("2", "assistant", Some(crate::agui::model::MessageContent::Text("a".into())), Vec::new(), None),
            AguiMessage::new("3", "tool", Some(crate::agui::model::MessageContent::Text("r".into())), Vec::new(), Some("c1".into())),
        ];
        let result = processor.process(&request(input)).await;
        assert!(!result.has_error());
    }

    #[tokio::test]
    async fn unserialized_threads_reject_a_second_concurrent_run() {
        let mut resolver = TestResolver::default();
        resolver
            .registry
            .register("default", Arc::new(StubAgent::new("default", vec![start_event()])));
        let processor = processor(resolver, AguiAdapterConfig::default_config());
        let first = processor
            .process(&request(RunAgentInput::of("t", "r1")))
            .await;
        assert!(!first.has_error());
        // `process` drains the whole event stream, so the active-run marker is cleared on return.
        assert!(!processor.resume_coordinator().has_active_run("t"));
        // A second run on the same thread is rejected while another run is still marked active.
        let in_flight = RunAgentInput::of("t", "r2");
        assert!(!processor
            .resume_coordinator()
            .begin_run(&in_flight)
            .is_error());
        let second = processor
            .process(&request(RunAgentInput::of("t", "r2")))
            .await;
        assert!(second.has_error());
        match second.events.last().unwrap() {
            AguiEvent::RunError { code, .. } => {
                assert_eq!(code.as_deref(), Some("AGUI_INTERRUPT_CONTRACT_ERROR"));
            }
            other => panic!("expected RunError, got {other:?}"),
        }
        processor.resume_coordinator().finish_run("t", "r2");
        assert!(processor.resume_coordinator().has_active_run("t") == false);
    }

    #[tokio::test]
    async fn a_resume_without_open_interrupts_is_rejected() {
        let mut resolver = TestResolver::default();
        resolver
            .registry
            .register("default", Arc::new(StubAgent::new("default", vec![start_event()])));
        let processor = processor(resolver, AguiAdapterConfig::default_config());
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "i1",
            AguiResume::STATUS_RESOLVED,
            None,
        )]);
        let result = processor.process(&request(input)).await;
        assert!(result.has_error());
        match &result.events[1] {
            AguiEvent::RunError { code, .. } => {
                assert_eq!(code.as_deref(), Some("AGUI_INTERRUPT_CONTRACT_ERROR"));
            }
            other => panic!("expected RunError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_runtime_context_resolver_values_reach_the_run() {
        let mut resolver = TestResolver::default();
        resolver
            .registry
            .register("default", Arc::new(StubAgent::new("default", vec![start_event()])));
        let mut input = RunAgentInput::of("tenant-thread", "r");
        input.messages.push(AguiMessage::user_message("m1", "hi"));
        let processor = AguiRequestProcessor::builder()
            .agent_resolver(Box::new(resolver))
            .runtime_context_resolver(Arc::new(|_request: &AguiRuntimeContextRequest| {
                Some(RuntimeContext::default().with_user_id("u-42"))
            }))
            .build();
        let result = processor.process(&request(input)).await;
        assert_eq!(result.runtime_context.user_id(), Some("u-42"));
        // The session id is always derived from the AG-UI thread id.
        assert_eq!(result.runtime_context.session_id(), Some("tenant-thread"));
    }

    #[test]
    fn agent_options_are_carried_into_the_request() {
        let input = RunAgentInput::new(
            "t",
            "r",
            Vec::new(),
            vec![AguiTool::new("search", "d", Map::new())],
            vec![AguiContext::new("k", "v")],
            Map::new(),
            Map::new(),
            Vec::new(),
        );
        let request = AguiRuntimeContextRequest::builder()
            .input(input)
            .header_agent_id("from-header")
            .path_agent_id("from-path")
            .build();
        assert_eq!(request.header_agent_id(), Some("from-header"));
        assert_eq!(request.path_agent_id(), Some("from-path"));
    }

    #[test]
    fn the_builder_requires_an_agent_resolver() {
        let result = std::panic::catch_unwind(|| AguiRequestProcessor::builder().build());
        assert!(result.is_err());
    }

    #[test]
    fn builder_debug_reports_configuration_presence() {
        let builder = AguiRequestProcessor::builder();
        let rendered = format!("{builder:?}");
        assert!(rendered.contains("has_agent_resolver: false"));
        assert!(rendered.contains("AguiRequestProcessorBuilder"));
    }

    #[test]
    fn process_results_expose_wire_helpers() {
        let result = ProcessResult {
            agent_id: "a".to_string(),
            runtime_context: RuntimeContext::default(),
            events: vec![],
        };
        assert!(result.wire_types().is_empty());
        assert!(!result.has_error());
        assert!(format!("{result:?}").contains("ProcessResult"));
        let processor = processor(TestResolver::default(), AguiAdapterConfig::default_config());
        assert_eq!(processor.config().tool_merge_mode(), crate::agui::model::ToolMergeMode::MergeExternalPriority);
        assert!(format!("{processor:?}").contains("AguiRequestProcessor"));
    }
}
