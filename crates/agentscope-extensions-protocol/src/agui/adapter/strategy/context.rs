//! Stream conversion context, mirroring `AguiStreamContext`.
//!
//! The context owns the per-run state that keeps the AG-UI event stream well-formed:
//! it starts/ends text, reasoning and tool-call messages exactly once, buffers tool-result
//! text until the result ends, and collects pending interrupts for `RUN_FINISHED.outcome`.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use agentscope_core::{AgentEvent, AgentEventType, ChatUsage, ContentBlock, Toolkit};
use serde_json::{Map, Value};

use crate::agui::adapter::config::AguiAdapterConfig;
use crate::agui::event::{AguiEvent, Interrupt};
use crate::agui::model::RunAgentInput;

/// Per-run stream conversion context.
pub struct AguiStreamContext {
    thread_id: String,
    run_id: String,
    config: AguiAdapterConfig,
    run_input: Option<RunAgentInput>,
    pending_events: Vec<AguiEvent>,
    started_text_messages: BTreeSet<String>,
    ended_text_messages: BTreeSet<String>,
    started_reasoning_messages: BTreeSet<String>,
    ended_reasoning_messages: BTreeSet<String>,
    started_tool_calls: BTreeSet<String>,
    ended_tool_calls: BTreeSet<String>,
    adopted_tool_calls: BTreeSet<String>,
    current_text_message_id: Option<String>,
    current_reasoning_message_id: Option<String>,
    tool_result_content: BTreeMap<String, String>,
    pending_interrupts: BTreeMap<String, Interrupt>,
    warned_missing_tool_call_id_operations: HashSet<&'static str>,
    token_usage_accumulator: TokenUsageAccumulator,
    started_tool_call_names: BTreeMap<String, String>,
    external_tool_names: BTreeSet<String>,
    toolkit: Option<Toolkit>,
}

impl std::fmt::Debug for AguiStreamContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AguiStreamContext")
            .field("thread_id", &self.thread_id)
            .field("run_id", &self.run_id)
            .field("pending_events", &self.pending_events.len())
            .field("started_tool_calls", &self.started_tool_calls)
            .finish()
    }
}

impl AguiStreamContext {
    /// Creates a context whose external-tool detection matches frontend tool names from `input`.
    pub fn new(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        config: AguiAdapterConfig,
        run_input: Option<RunAgentInput>,
    ) -> Self {
        let run_input = run_input;
        let external_tool_names = frontend_tool_names(run_input.as_ref());
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            config,
            run_input,
            pending_events: Vec::new(),
            started_text_messages: BTreeSet::new(),
            ended_text_messages: BTreeSet::new(),
            started_reasoning_messages: BTreeSet::new(),
            ended_reasoning_messages: BTreeSet::new(),
            started_tool_calls: BTreeSet::new(),
            ended_tool_calls: BTreeSet::new(),
            adopted_tool_calls: BTreeSet::new(),
            current_text_message_id: None,
            current_reasoning_message_id: None,
            tool_result_content: BTreeMap::new(),
            pending_interrupts: BTreeMap::new(),
            warned_missing_tool_call_id_operations: HashSet::new(),
            token_usage_accumulator: TokenUsageAccumulator::default(),
            started_tool_call_names: BTreeMap::new(),
            external_tool_names,
            toolkit: None,
        }
    }

    /// Creates a context whose external-tool detection uses the live agent toolkit.
    ///
    /// External tools execute outside the framework (frontend-provided or schema-only), so they
    /// need `TOOL_CALL_ARGS` delivered to the client even when `emit_tool_call_args` is disabled.
    /// A tool not present in the toolkit falls back to frontend-name matching.
    pub fn with_toolkit(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        config: AguiAdapterConfig,
        run_input: Option<RunAgentInput>,
        toolkit: Option<Toolkit>,
    ) -> Self {
        let mut context = Self::new(thread_id, run_id, config, run_input);
        context.toolkit = toolkit;
        context
    }

    /// The AG-UI thread id.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The AG-UI run id.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The adapter configuration.
    pub fn config(&self) -> &AguiAdapterConfig {
        &self.config
    }

    /// The originating run input, when available.
    pub fn run_input(&self) -> Option<&RunAgentInput> {
        self.run_input.as_ref()
    }

    /// Clears the pending-event buffer before converting one source event.
    pub fn begin_event(&mut self) {
        self.pending_events.clear();
    }

    /// Takes the events emitted since [`Self::begin_event`].
    pub fn drain_events(&mut self) -> Vec<AguiEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Buffers an event for the caller to drain.
    pub fn emit(&mut self, event: AguiEvent) {
        self.pending_events.push(event);
    }

    /// The token usage accumulator.
    pub fn token_usage_accumulator(&self) -> &TokenUsageAccumulator {
        &self.token_usage_accumulator
    }

    /// Mutable access to the token usage accumulator.
    pub fn token_usage_accumulator_mut(&mut self) -> &mut TokenUsageAccumulator {
        &mut self.token_usage_accumulator
    }

    /// Starts a text message, emitting `TEXT_MESSAGE_START` on first use.
    pub fn start_text_message(&mut self, message_id: &str) {
        if self.started_text_messages.insert(message_id.to_string()) {
            self.emit(AguiEvent::TextMessageStart {
                thread_id: self.thread_id.clone(),
                run_id: self.run_id.clone(),
                message_id: message_id.to_string(),
                role: "assistant".to_string(),
                timestamp: None,
                raw_event: None,
            });
        }
        self.current_text_message_id = Some(message_id.to_string());
    }

    /// Appends a text delta, starting the message lazily.
    pub fn append_text_delta(&mut self, message_id: &str, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.start_text_message(message_id);
        self.emit(AguiEvent::TextMessageContent {
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            message_id: message_id.to_string(),
            delta: delta.to_string(),
            timestamp: None,
            raw_event: None,
        });
    }

    /// Closes the message currently receiving text deltas.
    pub fn close_active_text_message(&mut self) {
        if let Some(message_id) = self.current_text_message_id.clone() {
            self.close_text_message(&message_id);
        }
    }

    /// Closes a text message, emitting `TEXT_MESSAGE_END` at most once.
    pub fn close_text_message(&mut self, message_id: &str) {
        if !self.started_text_messages.contains(message_id)
            || self.ended_text_messages.contains(message_id)
        {
            return;
        }
        self.ended_text_messages.insert(message_id.to_string());
        if self.current_text_message_id.as_deref() == Some(message_id) {
            self.current_text_message_id = None;
        }
        self.emit(AguiEvent::TextMessageEnd {
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            message_id: message_id.to_string(),
            timestamp: None,
            raw_event: None,
        });
    }

    /// Starts a reasoning message, emitting `REASONING_MESSAGE_START` on first use.
    pub fn start_reasoning_message(&mut self, message_id: &str) {
        if self
            .started_reasoning_messages
            .insert(message_id.to_string())
        {
            self.emit(AguiEvent::ReasoningMessageStart {
                thread_id: self.thread_id.clone(),
                run_id: self.run_id.clone(),
                message_id: message_id.to_string(),
                role: "reasoning".to_string(),
                timestamp: None,
                raw_event: None,
            });
        }
        self.current_reasoning_message_id = Some(message_id.to_string());
    }

    /// Appends a reasoning delta, starting the message lazily.
    pub fn append_reasoning_delta(&mut self, message_id: &str, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.start_reasoning_message(message_id);
        self.emit(AguiEvent::ReasoningMessageContent {
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            message_id: message_id.to_string(),
            delta: delta.to_string(),
            timestamp: None,
            raw_event: None,
        });
    }

    /// Closes the message currently receiving reasoning deltas.
    pub fn close_active_reasoning_message(&mut self) {
        if let Some(message_id) = self.current_reasoning_message_id.clone() {
            self.close_reasoning_message(&message_id);
        }
    }

    /// Closes a reasoning message, emitting `REASONING_MESSAGE_END` at most once.
    pub fn close_reasoning_message(&mut self, message_id: &str) {
        if !self.started_reasoning_messages.contains(message_id)
            || self.ended_reasoning_messages.contains(message_id)
        {
            return;
        }
        self.ended_reasoning_messages.insert(message_id.to_string());
        if self.current_reasoning_message_id.as_deref() == Some(message_id) {
            self.current_reasoning_message_id = None;
        }
        self.emit(AguiEvent::ReasoningMessageEnd {
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            message_id: message_id.to_string(),
            timestamp: None,
            raw_event: None,
        });
    }

    /// Starts a tool call, defaulting a blank name to `unknown`.
    pub fn start_tool_call(&mut self, tool_call_id: &str, tool_call_name: &str) {
        if tool_call_id.trim().is_empty() {
            self.warn_missing_tool_call_id("ToolCallStartEvent");
            return;
        }
        if self.started_tool_calls.insert(tool_call_id.to_string()) {
            if !tool_call_name.trim().is_empty() {
                self.started_tool_call_names
                    .insert(tool_call_id.to_string(), tool_call_name.to_string());
            }
            self.emit(AguiEvent::ToolCallStart {
                thread_id: self.thread_id.clone(),
                run_id: self.run_id.clone(),
                tool_call_id: tool_call_id.to_string(),
                tool_call_name: normalize_tool_call_name(tool_call_name),
                timestamp: None,
                raw_event: None,
            });
        }
    }

    /// Appends tool-call arguments for a tool call started in this run.
    pub fn append_tool_call_args(&mut self, tool_call_id: &str, delta: &str) {
        if !self.has_started_tool_call(tool_call_id, "ToolCallDeltaEvent") {
            return;
        }
        if delta.is_empty() {
            return;
        }
        self.emit(AguiEvent::ToolCallArgs {
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            delta: delta.to_string(),
            timestamp: None,
            raw_event: None,
        });
    }

    /// Ends a tool call, emitting `TOOL_CALL_END` at most once.
    pub fn end_tool_call(&mut self, tool_call_id: &str) {
        if !self.has_started_tool_call(tool_call_id, "ToolCallEndEvent") {
            return;
        }
        if self.ended_tool_calls.insert(tool_call_id.to_string()) {
            self.emit(AguiEvent::ToolCallEnd {
                thread_id: self.thread_id.clone(),
                run_id: self.run_id.clone(),
                tool_call_id: tool_call_id.to_string(),
                timestamp: None,
                raw_event: None,
            });
            self.started_tool_call_names.remove(tool_call_id);
        }
    }

    /// Accepts results for a tool call whose invocation was emitted in a previous run.
    ///
    /// Adoption is idempotent and emits no events. It does not start a tool call in this run,
    /// enable argument events, or clear an existing result buffer.
    pub fn adopt_tool_call(&mut self, tool_call_id: &str) {
        if tool_call_id.trim().is_empty() {
            self.warn_missing_tool_call_id("adoptToolCall");
            return;
        }
        self.adopted_tool_calls.insert(tool_call_id.to_string());
    }

    /// Begins collecting tool-result content for a known tool call.
    pub fn begin_tool_result(&mut self, tool_call_id: &str) {
        if !self.has_known_tool_call(tool_call_id, "ToolResultStartEvent") {
            return;
        }
        self.tool_result_content
            .entry(tool_call_id.to_string())
            .or_default();
    }

    /// Appends tool-result text to the buffer for a known tool call.
    pub fn append_tool_result_text(&mut self, tool_call_id: &str, delta: &str) {
        if !self.has_known_tool_call(tool_call_id, "ToolResultTextDeltaEvent") {
            return;
        }
        if delta.is_empty() {
            return;
        }
        self.tool_result_buffer(tool_call_id).push_str(delta);
    }

    /// Appends a structured data block to the tool-result buffer, newline-separated.
    pub fn append_tool_result_data(&mut self, tool_call_id: &str, data: &ContentBlock) {
        if !self.has_known_tool_call(tool_call_id, "ToolResultDataDeltaEvent") {
            return;
        }
        let rendered = serialize_block(data);
        let buffer = self.tool_result_buffer(tool_call_id);
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(&rendered);
    }

    /// Ends a tool result, emitting `TOOL_CALL_END` (if needed) and `TOOL_CALL_RESULT`.
    pub fn end_tool_result(&mut self, reply_id: &str, tool_call_id: &str) {
        if !self.has_known_tool_call(tool_call_id, "ToolResultEndEvent") {
            return;
        }
        if self.started_tool_calls.contains(tool_call_id)
            && self.ended_tool_calls.insert(tool_call_id.to_string())
        {
            self.emit(AguiEvent::ToolCallEnd {
                thread_id: self.thread_id.clone(),
                run_id: self.run_id.clone(),
                tool_call_id: tool_call_id.to_string(),
                timestamp: None,
                raw_event: None,
            });
        }
        let content = self.tool_result_content.remove(tool_call_id);
        self.emit(AguiEvent::ToolCallResult {
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            content: content.filter(|c| !c.is_empty()),
            role: Some("tool".to_string()),
            message_id: Some(format!("{reply_id}:{tool_call_id}")),
            timestamp: None,
            raw_event: None,
        });
    }

    /// Marks a tool call as suspended, discarding its partial result buffer.
    pub fn mark_tool_call_suspended(&mut self, tool_call_id: &str) {
        if !self.has_known_tool_call(tool_call_id, "ToolResultEndEvent") {
            return;
        }
        self.tool_result_content.remove(tool_call_id);
    }

    /// Registers a pending interrupt, keyed by its id.
    pub fn add_interrupt(&mut self, interrupt: Interrupt) {
        self.pending_interrupts.insert(interrupt.id.clone(), interrupt);
    }

    /// The pending interrupts, in id order.
    pub fn pending_interrupts(&self) -> Vec<Interrupt> {
        self.pending_interrupts.values().cloned().collect()
    }

    /// Flushes every open text/reasoning/tool-call block, returning the emitted events.
    pub fn finish_pending_events(&mut self) -> Vec<AguiEvent> {
        self.pending_events.clear();
        self.close_active_text_message();
        self.close_active_reasoning_message();
        let open: Vec<String> = self
            .started_tool_calls
            .iter()
            .filter(|id| !self.ended_tool_calls.contains(*id))
            .cloned()
            .collect();
        for tool_call_id in open {
            self.end_tool_call(&tool_call_id);
        }
        self.drain_events()
    }

    /// Whether the started tool call resolves to an external tool.
    ///
    /// The live toolkit decides first when one is attached; otherwise the tool name is matched
    /// against the frontend tools of the originating run input.
    pub fn is_external_tool_call(&self, tool_call_id: &str) -> bool {
        let Some(name) = self.started_tool_call_names.get(tool_call_id) else {
            return false;
        };
        if let Some(toolkit) = &self.toolkit {
            if toolkit.get_tool(name).is_some() {
                return toolkit.is_external_tool(name);
            }
        }
        self.external_tool_names.contains(name)
    }

    fn has_started_tool_call(&mut self, tool_call_id: &str, event_name: &'static str) -> bool {
        if tool_call_id.trim().is_empty() {
            self.warn_missing_tool_call_id(event_name);
            return false;
        }
        self.started_tool_calls.contains(tool_call_id)
    }

    fn has_known_tool_call(&mut self, tool_call_id: &str, event_name: &'static str) -> bool {
        self.has_started_tool_call(tool_call_id, event_name)
            || self.adopted_tool_calls.contains(tool_call_id)
    }

    fn tool_result_buffer(&mut self, tool_call_id: &str) -> &mut String {
        self.tool_result_content
            .entry(tool_call_id.to_string())
            .or_default()
    }

    fn warn_missing_tool_call_id(&mut self, event_name: &'static str) {
        if !self.warned_missing_tool_call_id_operations.insert(event_name) {
            return;
        }
        tracing::warn!(
            event = event_name,
            "Ignoring event: null/blank toolCallId. Upstream must supply a stable toolCallId per \
             AG-UI protocol."
        );
    }
}

/// Names of the frontend-provided tools in `run_input`.
fn frontend_tool_names(run_input: Option<&RunAgentInput>) -> BTreeSet<String> {
    run_input
        .map(|input| input.tools.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default()
}

/// Normalizes a tool-call name, mirroring the Java `normalizeToolCallName` fallback.
fn normalize_tool_call_name(tool_call_name: &str) -> String {
    if tool_call_name.trim().is_empty() {
        "unknown".to_string()
    } else {
        tool_call_name.to_string()
    }
}

/// Renders a content block for a tool-result buffer: text verbatim, anything else as JSON.
fn serialize_block(data: &ContentBlock) -> String {
    match data {
        ContentBlock::Text(text) => text.text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| format!("{other:?}")),
    }
}

/// A single token-usage delta plus its cumulative totals.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TokenUsageSnapshot {
    /// Usage of the current model call.
    pub delta: TokenUsage,
    /// Accumulated usage across the run.
    pub cumulative: TokenUsage,
}

/// Token counters mirroring the Java `TokenUsage` record.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TokenUsage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Cached prompt tokens.
    pub cached_tokens: u64,
    /// Wall-clock time in seconds.
    pub time: f64,
}

impl TokenUsage {
    /// Input plus output tokens.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Builds a usage value from a core [`ChatUsage`].
    pub fn from_chat_usage(usage: ChatUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_tokens: usage.cached_tokens,
            time: usage.time,
        }
    }

    /// Renders the usage as the `token_usage` custom-event value.
    pub fn to_value(self) -> Value {
        let mut map = Map::new();
        map.insert("inputTokens".to_string(), Value::from(self.input_tokens));
        map.insert("outputTokens".to_string(), Value::from(self.output_tokens));
        map.insert("cachedTokens".to_string(), Value::from(self.cached_tokens));
        map.insert("totalTokens".to_string(), Value::from(self.total_tokens()));
        map.insert("time".to_string(), Value::from(self.time));
        Value::Object(map)
    }
}

/// Accumulates model-call token usage across a run.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TokenUsageAccumulator {
    cumulative_input_tokens: u64,
    cumulative_output_tokens: u64,
    cumulative_cached_tokens: u64,
    cumulative_time: f64,
}

impl TokenUsageAccumulator {
    /// Adds one model call's usage and returns the delta plus the running totals.
    pub fn add(&mut self, usage: ChatUsage) -> TokenUsageSnapshot {
        let delta = TokenUsage::from_chat_usage(usage);
        self.cumulative_input_tokens += delta.input_tokens;
        self.cumulative_output_tokens += delta.output_tokens;
        self.cumulative_cached_tokens += delta.cached_tokens;
        self.cumulative_time += delta.time;
        TokenUsageSnapshot {
            delta,
            cumulative: TokenUsage {
                input_tokens: self.cumulative_input_tokens,
                output_tokens: self.cumulative_output_tokens,
                cached_tokens: self.cumulative_cached_tokens,
                time: self.cumulative_time,
            },
        }
    }
}

/// Reads a flat string field from an agent event.
pub fn event_string(event: &AgentEvent, key: &str) -> Option<String> {
    event
        .data()
        .fields
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Reads a flat string field from an agent event, defaulting to an empty string.
pub fn event_string_or_default(event: &AgentEvent, key: &str) -> String {
    event_string(event, key).unwrap_or_default()
}

/// Derives the AG-UI message id for a block lifecycle event.
///
/// Java correlates blocks by `replyId` + `blockId`. Core's `AgentEvent` carries `reply_id` and
/// `block_type` but no block id (the ReAct loop emits at most one text/thinking block per reply),
/// so the block type stands in for the block id; an explicit `block_id` field is preferred when a
/// producer supplies one.
pub fn block_message_id(event: &AgentEvent) -> String {
    let reply_id = event_string_or_default(event, "reply_id");
    let block_id = event_string(event, "block_id")
        .or_else(|| event_string(event, "block_type"))
        .or_else(|| default_block_type(event.event_type()).map(str::to_string))
        .unwrap_or_else(|| "block".to_string());
    format!("{reply_id}-{block_id}")
}

/// The block type implied by a block lifecycle event type.
///
/// Core's delta events carry only `reply_id` (no `block_id`/`block_type`), whereas the matching
/// start/end events carry `block_type`. Deriving the type from the event keeps a delta and its end
/// event correlated on the same AG-UI message id, which is what Java achieves with the explicit
/// `blockId` both events carry.
fn default_block_type(event_type: AgentEventType) -> Option<&'static str> {
    match event_type {
        AgentEventType::TextBlockStart
        | AgentEventType::TextBlockDelta
        | AgentEventType::TextBlockEnd => Some("text"),
        AgentEventType::ThinkingBlockStart
        | AgentEventType::ThinkingBlockDelta
        | AgentEventType::ThinkingBlockEnd => Some("thinking"),
        _ => None,
    }
}

/// Event types the built-in converters handle, used by registry tests.
pub(crate) const BUILT_IN_EVENT_TYPES: &[AgentEventType] = &[
    AgentEventType::TextBlockStart,
    AgentEventType::TextBlockDelta,
    AgentEventType::TextBlockEnd,
    AgentEventType::ThinkingBlockStart,
    AgentEventType::ThinkingBlockDelta,
    AgentEventType::ThinkingBlockEnd,
    AgentEventType::ToolCallStart,
    AgentEventType::ToolCallDelta,
    AgentEventType::ToolCallEnd,
    AgentEventType::ToolResultStart,
    AgentEventType::ToolProgress,
    AgentEventType::ToolResultTextDelta,
    AgentEventType::ToolResultDataDelta,
    AgentEventType::ToolResultEnd,
    AgentEventType::AgentStart,
    AgentEventType::AgentEnd,
    AgentEventType::AgentResult,
    AgentEventType::ModelCallStart,
    AgentEventType::ModelCallEnd,
    AgentEventType::RequireUserConfirm,
    AgentEventType::UserConfirmResult,
    AgentEventType::RequireExternalExecution,
    AgentEventType::ExternalExecutionResult,
    AgentEventType::Custom,
];
