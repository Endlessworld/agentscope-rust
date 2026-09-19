//! Built-in AgentScope -> AG-UI event conversion strategies, mirroring
//! `io.agentscope.core.agui.adapter.strategy.*EventConverter`.
//!
//! Java models `AgentEvent` as a class hierarchy and dispatches by concrete class; the Rust
//! core models it as a single tagged enum, so dispatch is by [`AgentEventType`] instead.

use std::collections::HashMap;

use agentscope_core::{
    AgentEvent, AgentEventType, ChatUsage, ConfirmResult, ContentBlock, GenerateReason, Msg,
    ToolResultState, ToolUseBlock,
};
use serde_json::{Map, Value};

use crate::agui::adapter::strategy::context::{block_message_id, event_string, event_string_or_default};
use crate::agui::adapter::strategy::context::AguiStreamContext;
use crate::agui::constants::{
    INTERRUPT_KIND_PERMISSION_CONFIRM, METADATA_AGENTSCOPE_INTERRUPT_KIND, METADATA_REPLY_ID,
    METADATA_TOOL_CONTENT, METADATA_TOOL_INPUT, METADATA_TOOL_NAME, TOOL_CALL_INTERRUPT_REASON,
};
use crate::agui::converter::message::resolve_tool_call_args_json;
use crate::agui::event::{AguiEvent, Interrupt};

/// Extension point for converting AgentScope events into AG-UI protocol events.
///
/// Implementations declare the [`AgentEventType`]s they handle and push AG-UI events into the
/// provided [`AguiStreamContext`]. Custom converters can be registered through
/// `AguiAdapterConfig` to extend or override the built-in mappings.
pub trait AgentEventConverter: Send + Sync {
    /// The AgentScope event types handled by this converter.
    fn event_types(&self) -> Vec<AgentEventType>;

    /// Converts a source event by emitting AG-UI events to the stream context.
    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext);
}

/// Converts run lifecycle events into `RUN_STARTED` / `RUN_FINISHED` and collects interrupts.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentLifecycleEventConverter;

impl AgentEventConverter for AgentLifecycleEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![
            AgentEventType::AgentStart,
            AgentEventType::AgentEnd,
            AgentEventType::AgentResult,
            AgentEventType::ModelCallStart,
        ]
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        match event.event_type() {
            AgentEventType::AgentStart => {
                context.emit(AguiEvent::RunStarted {
                    thread_id: context.thread_id().to_string(),
                    run_id: context.run_id().to_string(),
                    parent_run_id: None,
                    input: context
                        .run_input()
                        .and_then(|input| serde_json::to_value(input).ok()),
                    timestamp: None,
                    raw_event: None,
                });
            }
            AgentEventType::AgentResult => collect_suspended_tool_interrupts(event, context),
            AgentEventType::AgentEnd => {
                let pending = context.finish_pending_events();
                for pending_event in pending {
                    context.emit(pending_event);
                }
                let interrupts = context.pending_interrupts();
                let outcome = (!interrupts.is_empty())
                    .then(|| crate::agui::event::RunFinishedOutcome::Interrupt { interrupts });
                context.emit(AguiEvent::RunFinished {
                    thread_id: context.thread_id().to_string(),
                    run_id: context.run_id().to_string(),
                    result: None,
                    outcome,
                    timestamp: None,
                    raw_event: None,
                });
            }
            _ => {}
        }
    }
}

/// Collects suspended tool results from an `AGENT_RESULT` event as pending interrupts.
///
/// Only results whose message carries `GenerateReason::ToolSuspended` are considered, and
/// frontend-provided tools are skipped because the client executes them itself.
fn collect_suspended_tool_interrupts(event: &AgentEvent, context: &mut AguiStreamContext) {
    let Some(result) = event_result_msg(event) else {
        return;
    };
    if result.generate_reason() != GenerateReason::ToolSuspended {
        return;
    }
    let frontend_tools = frontend_tool_names(context);

    let mut tool_uses: HashMap<String, ToolUseBlock> = HashMap::new();
    for block in result.content() {
        if let ContentBlock::ToolUse(tool_use) = block {
            if !tool_use.id.trim().is_empty() {
                tool_uses.insert(tool_use.id.clone(), tool_use.clone());
            }
        }
    }

    for block in result.content() {
        let ContentBlock::ToolResult(tool_result) = block else {
            continue;
        };
        if !tool_result.is_suspended() {
            continue;
        }
        let Some(tool_call_id) = tool_result.id.as_deref() else {
            continue;
        };
        if tool_call_id.trim().is_empty() {
            continue;
        }
        let Some(tool_use) = tool_uses.get(tool_call_id) else {
            continue;
        };
        if frontend_tools.contains(&tool_use.name) {
            continue;
        }
        context.add_interrupt(build_tool_call_interrupt(
            &result,
            tool_use,
            tool_call_id,
            None,
        ));
    }
}

/// Builds the interrupt for a suspended or permission-gated tool call.
fn build_tool_call_interrupt(
    result: &Msg,
    tool_use: &ToolUseBlock,
    tool_call_id: &str,
    extra_metadata: Option<(&str, Value)>,
) -> Interrupt {
    let mut metadata = Map::new();
    if !tool_use.name.trim().is_empty() {
        metadata.insert(METADATA_TOOL_NAME.to_string(), Value::String(tool_use.name.clone()));
    }
    if !tool_use.input.is_empty() {
        metadata.insert(METADATA_TOOL_INPUT.to_string(), Value::Object(tool_use.input.clone()));
    }
    metadata.insert(
        METADATA_TOOL_CONTENT.to_string(),
        Value::String(resolve_tool_call_args_json(tool_use)),
    );
    if !result.id().trim().is_empty() {
        metadata.insert(METADATA_REPLY_ID.to_string(), Value::String(result.id().to_string()));
    }
    if let Some((key, value)) = extra_metadata {
        metadata.insert(key.to_string(), value);
    }
    Interrupt {
        id: interrupt_id(result.id(), tool_call_id),
        reason: TOOL_CALL_INTERRUPT_REASON.to_string(),
        message: None,
        tool_call_id: Some(tool_call_id.to_string()),
        response_schema: None,
        expires_at: None,
        metadata: Some(metadata),
    }
}

/// The `replyId:toolCallId` interrupt id, or just the tool call id when no reply id is known.
fn interrupt_id(reply_id: &str, tool_call_id: &str) -> String {
    if reply_id.trim().is_empty() {
        tool_call_id.to_string()
    } else {
        format!("{reply_id}:{tool_call_id}")
    }
}

/// Names of the frontend-provided tools from the originating run input.
fn frontend_tool_names(context: &AguiStreamContext) -> Vec<String> {
    context
        .run_input()
        .map(|input| input.tools.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default()
}

/// Reads the `reply` field of an `AGENT_RESULT` event back into a [`Msg`].
fn event_result_msg(event: &AgentEvent) -> Option<Msg> {
    event
        .data()
        .fields
        .get("reply")
        .and_then(|value| serde_json::from_value::<Msg>(value.clone()).ok())
}

/// Converts permission-mode HITL events into pending interrupts and tool-call adoption.
#[derive(Debug, Clone, Copy, Default)]
pub struct PermissionConfirmEventConverter;

impl PermissionConfirmEventConverter {
    /// The JSON schema describing the expected confirmation payload.
    pub fn confirm_response_schema() -> Map<String, Value> {
        let mut approved = Map::new();
        approved.insert("type".to_string(), Value::String("boolean".to_string()));
        let mut edited_args = Map::new();
        edited_args.insert("type".to_string(), Value::String("object".to_string()));
        edited_args.insert(
            "description".to_string(),
            Value::String("Full replacement of the tool args. Not merged.".to_string()),
        );
        let mut properties = Map::new();
        properties.insert("approved".to_string(), Value::Object(approved));
        properties.insert("editedArgs".to_string(), Value::Object(edited_args));
        let mut schema = Map::new();
        schema.insert("type".to_string(), Value::String("object".to_string()));
        schema.insert("properties".to_string(), Value::Object(properties));
        schema.insert(
            "required".to_string(),
            Value::Array(vec![Value::String("approved".to_string())]),
        );
        schema
    }
}

impl AgentEventConverter for PermissionConfirmEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![
            AgentEventType::RequireUserConfirm,
            AgentEventType::UserConfirmResult,
        ]
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        if event.event_type() == AgentEventType::UserConfirmResult {
            for tool_call_id in confirmed_tool_call_ids(event) {
                context.adopt_tool_call(&tool_call_id);
            }
            return;
        }

        let reply_id = event_string_or_default(event, "reply_id");
        for tool_use in pending_tool_calls(event) {
            if tool_use.id.trim().is_empty() {
                tracing::warn!(
                    "RequireUserConfirm event contains a tool call without a stable id; skipping"
                );
                continue;
            }
            let tool_call_id = tool_use.id.clone();
            let mut interrupt = build_tool_call_interrupt(
                &Msg::builder().id(reply_id.clone()).build(),
                &tool_use,
                &tool_call_id,
                Some((
                    METADATA_AGENTSCOPE_INTERRUPT_KIND,
                    Value::String(INTERRUPT_KIND_PERMISSION_CONFIRM.to_string()),
                )),
            );
            interrupt.message = Some(confirm_message(&tool_use));
            interrupt.response_schema = Some(Self::confirm_response_schema());
            context.add_interrupt(interrupt);
        }
    }
}

/// Reads the tool calls awaiting confirmation from a `REQUIRE_USER_CONFIRM` event.
///
/// Core's ReAct loop produces this event through `AgentEvent::with_fields`; the harness
/// subagent codec uses a `tool_calls` field holding the same shape. Both are accepted.
fn pending_tool_calls(event: &AgentEvent) -> Vec<ToolUseBlock> {
    let fields = &event.data().fields;
    if let Some(Value::Array(blocks)) = fields.get("tool_calls") {
        return blocks
            .iter()
            .filter_map(|block| serde_json::from_value::<ToolUseBlock>(block.clone()).ok())
            .collect();
    }
    Vec::new()
}

/// The tool-call ids adopted by a `USER_CONFIRM_RESULT` event.
fn confirmed_tool_call_ids(event: &AgentEvent) -> Vec<String> {
    let fields = &event.data().fields;
    let Some(value) = fields
        .get("confirm_results")
        .or_else(|| fields.get(Msg::METADATA_CONFIRM_RESULTS))
    else {
        return Vec::new();
    };
    serde_json::from_value::<Vec<ConfirmResult>>(value.clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|result| result.tool_call.map(|call| call.id))
        .collect()
}

/// The human-readable confirmation prompt shown for a tool call.
fn confirm_message(tool_use: &ToolUseBlock) -> String {
    let name = if tool_use.name.trim().is_empty() {
        "tool"
    } else {
        tool_use.name.as_str()
    };
    format!("Tool '{name}' requires user confirmation before execution")
}

/// Suppresses the external-execution handshake events in the default AG-UI stream.
///
/// AG-UI surfaces externally suspended tool calls through `RUN_FINISHED.outcome`; emitting the
/// typed AgentScope events as well would duplicate the native interrupt contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExternalExecutionEventConverter;

impl AgentEventConverter for ExternalExecutionEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![
            AgentEventType::RequireExternalExecution,
            AgentEventType::ExternalExecutionResult,
        ]
    }

    fn convert(&self, _event: &AgentEvent, _context: &mut AguiStreamContext) {
        // Intentionally no-op.
    }
}

/// Converts text-block lifecycle events into `TEXT_MESSAGE_*` events.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextBlockEventConverter;

impl AgentEventConverter for TextBlockEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![
            AgentEventType::TextBlockStart,
            AgentEventType::TextBlockDelta,
            AgentEventType::TextBlockEnd,
        ]
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        match event.event_type() {
            AgentEventType::TextBlockDelta => {
                let message_id = block_message_id(event);
                let delta = event_string_or_default(event, "delta");
                context.append_text_delta(&message_id, &delta);
            }
            AgentEventType::TextBlockEnd => {
                let message_id = block_message_id(event);
                context.close_text_message(&message_id);
            }
            _ => {}
        }
    }
}

/// Converts reasoning-block lifecycle events into `REASONING_MESSAGE_*` events.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThinkingBlockEventConverter;

impl AgentEventConverter for ThinkingBlockEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![
            AgentEventType::ThinkingBlockStart,
            AgentEventType::ThinkingBlockDelta,
            AgentEventType::ThinkingBlockEnd,
        ]
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        // Reasoning output is opt-in, mirroring `AguiAdapterConfig.enableReasoning`.
        if !context.config().enable_reasoning() {
            return;
        }
        match event.event_type() {
            AgentEventType::ThinkingBlockDelta => {
                let message_id = block_message_id(event);
                let delta = event_string_or_default(event, "delta");
                context.append_reasoning_delta(&message_id, &delta);
            }
            AgentEventType::ThinkingBlockEnd => {
                let message_id = block_message_id(event);
                context.close_reasoning_message(&message_id);
            }
            _ => {}
        }
    }
}

/// Converts tool-call lifecycle events into `TOOL_CALL_*` events.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCallEventConverter;

impl AgentEventConverter for ToolCallEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![
            AgentEventType::ToolCallStart,
            AgentEventType::ToolCallDelta,
            AgentEventType::ToolCallEnd,
        ]
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        match event.event_type() {
            AgentEventType::ToolCallDelta => {
                let tool_call_id = event_string_or_default(event, "tool_call_id");
                // External tools always need args so the client can invoke them; streaming deltas
                // often use a placeholder name, so resolution happens by toolCallId.
                if context.config().emit_tool_call_args()
                    || context.is_external_tool_call(&tool_call_id)
                {
                    let delta = event_string_or_default(event, "delta");
                    context.append_tool_call_args(&tool_call_id, &delta);
                }
            }
            AgentEventType::ToolCallStart => {
                let tool_call_id = event_string_or_default(event, "tool_call_id");
                let tool_call_name = event_string_or_default(event, "tool_name");
                context.start_tool_call(&tool_call_id, &tool_call_name);
            }
            AgentEventType::ToolCallEnd => {
                let tool_call_id = event_string_or_default(event, "tool_call_id");
                context.end_tool_call(&tool_call_id);
            }
            _ => {}
        }
    }
}

/// Converts tool-result lifecycle events into `TOOL_CALL_RESULT` events.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolResultEventConverter;

impl AgentEventConverter for ToolResultEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![
            AgentEventType::ToolResultStart,
            AgentEventType::ToolResultTextDelta,
            AgentEventType::ToolResultDataDelta,
            AgentEventType::ToolResultEnd,
        ]
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        match event.event_type() {
            AgentEventType::ToolResultStart => {
                let tool_call_id = event_string_or_default(event, "tool_call_id");
                context.begin_tool_result(&tool_call_id);
            }
            AgentEventType::ToolResultTextDelta => {
                let tool_call_id = event_string_or_default(event, "tool_call_id");
                let delta = event_string_or_default(event, "delta");
                context.append_tool_result_text(&tool_call_id, &delta);
            }
            AgentEventType::ToolResultDataDelta => {
                let tool_call_id = event_string_or_default(event, "tool_call_id");
                if let Some(data) = event
                    .data()
                    .fields
                    .get("data")
                    .and_then(|value| serde_json::from_value::<ContentBlock>(value.clone()).ok())
                {
                    context.append_tool_result_data(&tool_call_id, &data);
                }
            }
            AgentEventType::ToolResultEnd => {
                let tool_call_id = event_string_or_default(event, "tool_call_id");
                // A `RUNNING` state means the tool is suspended for a later run: keep the
                // tool call open and discard the partial buffer.
                if tool_result_state(event) == Some(ToolResultState::Running) {
                    context.mark_tool_call_suspended(&tool_call_id);
                    return;
                }
                let reply_id = event_string_or_default(event, "reply_id");
                context.end_tool_result(&reply_id, &tool_call_id);
            }
            _ => {}
        }
    }
}

/// Reads the `state` field of a `TOOL_RESULT_END` event.
fn tool_result_state(event: &AgentEvent) -> Option<ToolResultState> {
    match event_string(event, "state")?.to_ascii_uppercase().as_str() {
        "SUCCESS" => Some(ToolResultState::Success),
        "ERROR" => Some(ToolResultState::Error),
        "INTERRUPTED" => Some(ToolResultState::Interrupted),
        "DENIED" => Some(ToolResultState::Denied),
        "RUNNING" => Some(ToolResultState::Running),
        _ => None,
    }
}

/// Converts `MODEL_CALL_END` usage into AG-UI `token_usage` custom events.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelCallUsageEventConverter;

impl AgentEventConverter for ModelCallUsageEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![AgentEventType::ModelCallEnd]
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        if !context.config().emit_token_usage() {
            return;
        }
        let Some(usage) = event
            .data()
            .fields
            .get("usage")
            .and_then(|value| serde_json::from_value::<ChatUsage>(value.clone()).ok())
        else {
            return;
        };
        let snapshot = context.token_usage_accumulator_mut().add(usage);
        let mut value = Map::new();
        value.insert("delta".to_string(), snapshot.delta.to_value());
        value.insert("cumulative".to_string(), snapshot.cumulative.to_value());
        let mut model_call = Map::new();
        model_call.insert(
            "replyId".to_string(),
            Value::String(event_string_or_default(event, "reply_id")),
        );
        value.insert("modelCall".to_string(), Value::Object(model_call));
        context.emit(AguiEvent::Custom {
            thread_id: context.thread_id().to_string(),
            run_id: context.run_id().to_string(),
            name: "token_usage".to_string(),
            value: Some(Value::Object(value)),
            timestamp: None,
            raw_event: None,
        });
    }
}

/// Converts `CUSTOM` AgentScope events into AG-UI `CUSTOM` events.
#[derive(Debug, Clone, Copy, Default)]
pub struct CustomAgentEventConverter;

impl AgentEventConverter for CustomAgentEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        vec![AgentEventType::Custom]
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        let name = event_string_or_default(event, "name");
        let value = event.data().fields.get("value").cloned();
        context.emit(AguiEvent::Custom {
            thread_id: context.thread_id().to_string(),
            run_id: context.run_id().to_string(),
            name,
            value,
            timestamp: None,
            raw_event: None,
        });
    }
}

/// Fallback converter emitting official AG-UI `RAW` events for unmapped AgentScope events.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawAgentEventConverter;

impl AgentEventConverter for RawAgentEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        Vec::new()
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        context.emit(AguiEvent::Raw {
            thread_id: context.thread_id().to_string(),
            run_id: context.run_id().to_string(),
            event: serde_json::to_value(event).ok(),
            source: event.source().map(str::to_string),
            timestamp: None,
            raw_event: None,
        });
    }
}

/// Converts subagent-sourced events into `subagent.*` `CUSTOM` events.
///
/// Child events must not pollute the parent run lifecycle or text stream, so they are namespaced
/// under `subagent.*` instead of being mapped to native AG-UI types.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubagentEventConverter;

impl SubagentEventConverter {
    /// Lifecycle custom-event name.
    pub const NAME_LIFECYCLE: &'static str = "subagent.lifecycle";
    /// Text custom-event name.
    pub const NAME_TEXT: &'static str = "subagent.text";
    /// Thinking custom-event name.
    pub const NAME_THINKING: &'static str = "subagent.thinking";
    /// Tool-call custom-event name.
    pub const NAME_TOOL_CALL: &'static str = "subagent.tool_call";
    /// Tool-result custom-event name.
    pub const NAME_TOOL_RESULT: &'static str = "subagent.tool_result";
    /// Confirmation custom-event name.
    pub const NAME_CONFIRM: &'static str = "subagent.require_confirm";
}

impl AgentEventConverter for SubagentEventConverter {
    fn event_types(&self) -> Vec<AgentEventType> {
        // Invoked by the registry when `source` is set, not by event-type lookup.
        Vec::new()
    }

    fn convert(&self, event: &AgentEvent, context: &mut AguiStreamContext) {
        let source = event.source().unwrap_or_default().to_string();
        let thread_id = context.thread_id().to_string();
        let run_id = context.run_id().to_string();
        let mut emit = |name: &str, mut extra: Map<String, Value>| {
            let mut value = Map::new();
            value.insert("source".to_string(), Value::String(source.clone()));
            value.insert(
                "type".to_string(),
                Value::String(event.event_type().wire_name().to_string()),
            );
            value.append(&mut extra);
            context.emit(AguiEvent::Custom {
                thread_id: thread_id.clone(),
                run_id: run_id.clone(),
                name: name.to_string(),
                value: Some(Value::Object(value)),
                timestamp: None,
                raw_event: None,
            });
        };

        match event.event_type() {
            AgentEventType::AgentStart => emit(Self::NAME_LIFECYCLE, Map::new()),
            AgentEventType::AgentEnd => emit(Self::NAME_LIFECYCLE, Map::new()),
            AgentEventType::TextBlockDelta => {
                let mut extra = Map::new();
                extra.insert(
                    "delta".to_string(),
                    Value::String(event_string_or_default(event, "delta")),
                );
                emit(Self::NAME_TEXT, extra);
            }
            AgentEventType::ThinkingBlockDelta => {
                let mut extra = Map::new();
                extra.insert(
                    "delta".to_string(),
                    Value::String(event_string_or_default(event, "delta")),
                );
                emit(Self::NAME_THINKING, extra);
            }
            AgentEventType::ToolCallStart | AgentEventType::ToolCallEnd => {
                let mut extra = Map::new();
                extra.insert(
                    "toolCallId".to_string(),
                    Value::String(event_string_or_default(event, "tool_call_id")),
                );
                extra.insert(
                    "toolName".to_string(),
                    Value::String(event_string_or_default(event, "tool_name")),
                );
                emit(Self::NAME_TOOL_CALL, extra);
            }
            AgentEventType::ToolResultEnd => {
                let mut extra = Map::new();
                extra.insert(
                    "toolCallId".to_string(),
                    Value::String(event_string_or_default(event, "tool_call_id")),
                );
                extra.insert(
                    "toolName".to_string(),
                    Value::String(event_string_or_default(event, "tool_name")),
                );
                if let Some(state) = event_string(event, "state") {
                    extra.insert("state".to_string(), Value::String(state));
                }
                emit(Self::NAME_TOOL_RESULT, extra);
            }
            AgentEventType::RequireUserConfirm => {
                let mut extra = Map::new();
                extra.insert(
                    "toolCallCount".to_string(),
                    Value::from(pending_tool_calls(event).len() as u64),
                );
                emit(Self::NAME_CONFIRM, extra);
            }
            // Unknown typed events fall back to RAW, which already carries the source.
            _ => RawAgentEventConverter.convert(event, context),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::adapter::config::AguiAdapterConfig;
    use crate::agui::event::AguiEventType;
    use crate::agui::model::{AguiTool, RunAgentInput};
    use agentscope_core::{MsgRole, TextBlock, ToolResultBlock};
    use serde_json::json;

    fn context() -> AguiStreamContext {
        AguiStreamContext::new("t", "r", AguiAdapterConfig::default_config(), None)
    }

    fn context_with_config(config: AguiAdapterConfig) -> AguiStreamContext {
        AguiStreamContext::new("t", "r", config, None)
    }

    fn delta_event(event_type: AgentEventType, fields: &[(&str, Value)]) -> AgentEvent {
        let mut map = Map::new();
        for (key, value) in fields {
            map.insert((*key).to_string(), value.clone());
        }
        AgentEvent::with_fields(event_type, map)
    }

    fn render(event: &AguiEvent) -> String {
        serde_json::to_string(event).unwrap()
    }

    #[test]
    fn text_delta_starts_the_message_lazily_and_end_closes_it() {
        let mut context = context();
        let converter = TextBlockEventConverter;
        let delta = delta_event(
            AgentEventType::TextBlockDelta,
            &[
                ("reply_id", json!("rep1")),
                ("block_type", json!("text")),
                ("delta", json!("hello ")),
            ],
        );
        context.begin_event();
        converter.convert(&delta, &mut context);
        let events = context.drain_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type(), Some(AguiEventType::TextMessageStart));
        assert_eq!(events[1].event_type(), Some(AguiEventType::TextMessageContent));
        assert_eq!(render(&events[0]), render(&AguiEvent::TextMessageStart {
            thread_id: "t".into(),
            run_id: "r".into(),
            message_id: "rep1-text".into(),
            role: "assistant".into(),
            timestamp: None,
            raw_event: None,
        }));

        // A second delta reuses the started message.
        context.begin_event();
        converter.convert(&delta, &mut context);
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::TextMessageContent));

        context.begin_event();
        converter.convert(
            &delta_event(
                AgentEventType::TextBlockEnd,
                &[("reply_id", json!("rep1")), ("block_type", json!("text"))],
            ),
            &mut context,
        );
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::TextMessageEnd));
        // Closing twice is idempotent.
        context.begin_event();
        converter.convert(
            &delta_event(
                AgentEventType::TextBlockEnd,
                &[("reply_id", json!("rep1")), ("block_type", json!("text"))],
            ),
            &mut context,
        );
        assert!(context.drain_events().is_empty());
    }

    #[test]
    fn empty_text_deltas_are_ignored() {
        let mut context = context();
        TextBlockEventConverter.convert(
            &delta_event(
                AgentEventType::TextBlockDelta,
                &[("reply_id", json!("r")), ("delta", json!(""))],
            ),
            &mut context,
        );
        assert!(context.drain_events().is_empty());
    }

    #[test]
    fn thinking_events_are_ignored_unless_reasoning_is_enabled() {
        let delta = delta_event(
            AgentEventType::ThinkingBlockDelta,
            &[("reply_id", json!("rep")), ("delta", json!("hmm"))],
        );
        let mut disabled = context();
        ThinkingBlockEventConverter.convert(&delta, &mut disabled);
        assert!(disabled.drain_events().is_empty());

        let mut enabled = context_with_config(
            AguiAdapterConfig::builder().enable_reasoning(true).build(),
        );
        ThinkingBlockEventConverter.convert(&delta, &mut enabled);
        let events = enabled.drain_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type(), Some(AguiEventType::ReasoningMessageStart));
        assert_eq!(events[1].event_type(), Some(AguiEventType::ReasoningMessageContent));
        assert!(render(&events[0]).contains("\"role\":\"reasoning\""));

        ThinkingBlockEventConverter.convert(
            &delta_event(
                AgentEventType::ThinkingBlockEnd,
                &[("reply_id", json!("rep")), ("block_type", json!("thinking"))],
            ),
            &mut enabled,
        );
        let events = enabled.drain_events();
        assert_eq!(events[0].event_type(), Some(AguiEventType::ReasoningMessageEnd));
    }

    #[test]
    fn tool_call_lifecycle_emits_start_args_and_end() {
        let mut context = context();
        let converter = ToolCallEventConverter;
        context.begin_event();
        converter.convert(
            &delta_event(
                AgentEventType::ToolCallStart,
                &[("tool_call_id", json!("c1")), ("tool_name", json!("search"))],
            ),
            &mut context,
        );
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert!(render(&events[0]).contains("\"toolCallId\":\"c1\""));
        assert!(render(&events[0]).contains("\"toolCallName\":\"search\""));

        context.begin_event();
        converter.convert(
            &delta_event(
                AgentEventType::ToolCallDelta,
                &[("tool_call_id", json!("c1")), ("delta", json!("{\"q\":"))],
            ),
            &mut context,
        );
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::ToolCallArgs));

        context.begin_event();
        converter.convert(
            &delta_event(AgentEventType::ToolCallEnd, &[("tool_call_id", json!("c1"))]),
            &mut context,
        );
        let events = context.drain_events();
        assert_eq!(events[0].event_type(), Some(AguiEventType::ToolCallEnd));
        // Ending twice is idempotent.
        context.begin_event();
        converter.convert(
            &delta_event(AgentEventType::ToolCallEnd, &[("tool_call_id", json!("c1"))]),
            &mut context,
        );
        assert!(context.drain_events().is_empty());
    }

    #[test]
    fn tool_call_args_are_suppressed_when_disabled_unless_the_tool_is_external() {
        let input = RunAgentInput::new(
            "t",
            "r",
            Vec::new(),
            vec![AguiTool::new("frontend_tool", "d", Map::new())],
            Vec::new(),
            Map::new(),
            Map::new(),
            Vec::new(),
        );
        let config = AguiAdapterConfig::builder().emit_tool_call_args(false).build();
        let mut context = AguiStreamContext::new("t", "r", config, Some(input));
        let converter = ToolCallEventConverter;
        converter.convert(
            &delta_event(
                AgentEventType::ToolCallStart,
                &[("tool_call_id", json!("c1")), ("tool_name", json!("internal"))],
            ),
            &mut context,
        );
        context.drain_events();
        converter.convert(
            &delta_event(
                AgentEventType::ToolCallDelta,
                &[("tool_call_id", json!("c1")), ("delta", json!("x"))],
            ),
            &mut context,
        );
        assert!(context.drain_events().is_empty(), "internal tool args are suppressed");

        // A frontend tool still receives its arguments.
        context.begin_event();
        converter.convert(
            &delta_event(
                AgentEventType::ToolCallStart,
                &[("tool_call_id", json!("c2")), ("tool_name", json!("frontend_tool"))],
            ),
            &mut context,
        );
        context.drain_events();
        converter.convert(
            &delta_event(
                AgentEventType::ToolCallDelta,
                &[("tool_call_id", json!("c2")), ("delta", json!("y"))],
            ),
            &mut context,
        );
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::ToolCallArgs));
    }

    #[test]
    fn unknown_tool_call_names_fall_back_to_placeholder() {
        let mut context = context();
        ToolCallEventConverter.convert(
            &delta_event(
                AgentEventType::ToolCallStart,
                &[("tool_call_id", json!("c1")), ("tool_name", json!(""))],
            ),
            &mut context,
        );
        let events = context.drain_events();
        assert!(render(&events[0]).contains("\"toolCallName\":\"unknown\""));
        // The name is not recorded, so the call cannot resolve as external.
        assert!(!context.is_external_tool_call("c1"));
    }

    #[test]
    fn blank_tool_call_ids_are_ignored() {
        let mut context = context();
        let converter = ToolCallEventConverter;
        converter.convert(
            &delta_event(
                AgentEventType::ToolCallStart,
                &[("tool_call_id", json!("")), ("tool_name", json!("x"))],
            ),
            &mut context,
        );
        converter.convert(
            &delta_event(
                AgentEventType::ToolCallDelta,
                &[("tool_call_id", json!("  ")), ("delta", json!("x"))],
            ),
            &mut context,
        );
        converter.convert(
            &delta_event(AgentEventType::ToolCallEnd, &[("tool_call_id", json!(""))]),
            &mut context,
        );
        assert!(context.drain_events().is_empty());
    }

    #[test]
    fn tool_result_text_is_buffered_until_the_result_ends() {
        let mut context = context();
        let converter = ToolCallEventConverter;
        ToolCallEventConverter.convert(
            &delta_event(
                AgentEventType::ToolCallStart,
                &[("tool_call_id", json!("c1")), ("tool_name", json!("t"))],
            ),
            &mut context,
        );
        context.drain_events();

        let result_converter = ToolResultEventConverter;
        context.begin_event();
        result_converter.convert(
            &delta_event(AgentEventType::ToolResultStart, &[("tool_call_id", json!("c1"))]),
            &mut context,
        );
        assert!(context.drain_events().is_empty(), "start only buffers");

        result_converter.convert(
            &delta_event(
                AgentEventType::ToolResultTextDelta,
                &[("tool_call_id", json!("c1")), ("delta", json!("par"))],
            ),
            &mut context,
        );
        result_converter.convert(
            &delta_event(
                AgentEventType::ToolResultTextDelta,
                &[("tool_call_id", json!("c1")), ("delta", json!("tial"))],
            ),
            &mut context,
        );
        assert!(context.drain_events().is_empty());

        context.begin_event();
        result_converter.convert(
            &delta_event(
                AgentEventType::ToolResultEnd,
                &[
                    ("reply_id", json!("rep1")),
                    ("tool_call_id", json!("c1")),
                    ("state", json!("SUCCESS")),
                ],
            ),
            &mut context,
        );
        let events = context.drain_events();
        // The tool call was ended implicitly, then the result was emitted.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type(), Some(AguiEventType::ToolCallEnd));
        assert_eq!(events[1].event_type(), Some(AguiEventType::ToolCallResult));
        assert!(render(&events[1]).contains("\"content\":\"partial\""));
        assert!(render(&events[1]).contains("\"messageId\":\"rep1:c1\""));
        assert!(render(&events[1]).contains("\"role\":\"tool\""));
        let _ = converter;
    }

    #[test]
    fn a_running_tool_result_suspends_the_call_without_emitting_a_result() {
        let mut context = context();
        ToolCallEventConverter.convert(
            &delta_event(
                AgentEventType::ToolCallStart,
                &[("tool_call_id", json!("c1")), ("tool_name", json!("t"))],
            ),
            &mut context,
        );
        context.drain_events();
        let converter = ToolResultEventConverter;
        converter.convert(
            &delta_event(AgentEventType::ToolResultStart, &[("tool_call_id", json!("c1"))]),
            &mut context,
        );
        converter.convert(
            &delta_event(
                AgentEventType::ToolResultTextDelta,
                &[("tool_call_id", json!("c1")), ("delta", json!("discarded"))],
            ),
            &mut context,
        );
        context.drain_events();
        converter.convert(
            &delta_event(
                AgentEventType::ToolResultEnd,
                &[("tool_call_id", json!("c1")), ("state", json!("RUNNING"))],
            ),
            &mut context,
        );
        assert!(context.drain_events().is_empty());
        // The tool call is still open, so finishing the run closes it.
        let events = context.finish_pending_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::ToolCallEnd));
    }

    #[test]
    fn tool_result_data_blocks_are_json_rendered_and_newline_separated() {
        let mut context = context();
        ToolCallEventConverter.convert(
            &delta_event(
                AgentEventType::ToolCallStart,
                &[("tool_call_id", json!("c1")), ("tool_name", json!("t"))],
            ),
            &mut context,
        );
        context.drain_events();
        let converter = ToolResultEventConverter;
        converter.convert(
            &delta_event(AgentEventType::ToolResultStart, &[("tool_call_id", json!("c1"))]),
            &mut context,
        );
        converter.convert(
            &delta_event(
                AgentEventType::ToolResultDataDelta,
                &[
                    ("tool_call_id", json!("c1")),
                    ("data", json!({"type": "text", "text": "body"})),
                ],
            ),
            &mut context,
        );
        // Consecutive data blocks are newline-separated (Java
        // `AguiStreamContext.appendToolResultData`); plain text deltas just concatenate.
        converter.convert(
            &delta_event(
                AgentEventType::ToolResultDataDelta,
                &[
                    ("tool_call_id", json!("c1")),
                    ("data", json!({"type": "text", "text": "tail"})),
                ],
            ),
            &mut context,
        );
        context.drain_events();
        converter.convert(
            &delta_event(
                AgentEventType::ToolResultEnd,
                &[("reply_id", json!("r")), ("tool_call_id", json!("c1"))],
            ),
            &mut context,
        );
        let events = context.drain_events();
        let result = events.last().unwrap();
        assert!(render(result).contains("body\\ntail"), "got {}", render(result));
    }

    #[test]
    fn adopted_tool_calls_accept_results_without_a_start_event() {
        let mut context = context();
        context.adopt_tool_call("c1");
        let converter = ToolResultEventConverter;
        converter.convert(
            &delta_event(AgentEventType::ToolResultStart, &[("tool_call_id", json!("c1"))]),
            &mut context,
        );
        converter.convert(
            &delta_event(
                AgentEventType::ToolResultTextDelta,
                &[("tool_call_id", json!("c1")), ("delta", json!("adopted"))],
            ),
            &mut context,
        );
        context.drain_events();
        converter.convert(
            &delta_event(
                AgentEventType::ToolResultEnd,
                &[("reply_id", json!("r")), ("tool_call_id", json!("c1"))],
            ),
            &mut context,
        );
        let events = context.drain_events();
        // No TOOL_CALL_END: the call was emitted in a previous run.
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::ToolCallResult));
        assert!(render(&events[0]).contains("adopted"));
    }

    #[test]
    fn agent_start_and_end_bracket_the_run() {
        let mut context = context();
        let converter = AgentLifecycleEventConverter;
        converter.convert(&AgentEvent::new(AgentEventType::AgentStart), &mut context);
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::RunStarted));

        converter.convert(&AgentEvent::new(AgentEventType::AgentEnd), &mut context);
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::RunFinished));
        // Java annotates `RunFinished` with `@JsonInclude(NON_NULL)`, so a null outcome is
        // omitted from the JSON payload rather than rendered as `"outcome":null`.
        assert!(
            !render(&events[0]).contains("outcome"),
            "got {}",
            render(&events[0])
        );
    }

    #[test]
    fn agent_end_reports_pending_interrupts_as_the_run_finished_outcome() {
        let mut context = context();
        context.add_interrupt(Interrupt::new("i1", TOOL_CALL_INTERRUPT_REASON));
        AgentLifecycleEventConverter.convert(&AgentEvent::new(AgentEventType::AgentEnd), &mut context);
        let events = context.drain_events();
        let rendered = render(&events[0]);
        assert!(rendered.contains("\"outcome\":{\"type\":\"interrupt\""), "got {rendered}");
        assert!(rendered.contains("\"id\":\"i1\""));
    }

    #[test]
    fn suspended_tool_results_become_pending_interrupts() {
        let mut context = context();
        let tool_use = ToolUseBlock::builder()
            .id("c1")
            .name("shell")
            .input(serde_json::from_value(json!({"cmd": "ls"})).unwrap())
            .build();
        let mut result = ToolResultBlock::builder()
            .id("c1")
            .output(ContentBlock::Text(TextBlock::new("needs approval")))
            .build();
        result.metadata.insert(
            ToolResultBlock::METADATA_SUSPENDED.to_string(),
            json!(true),
        );
        let msg = Msg::builder()
            .id("rep1")
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::ToolUse(tool_use),
                ContentBlock::ToolResult(result),
            ])
            .generate_reason(GenerateReason::ToolSuspended)
            .build();
        let event = delta_event(
            AgentEventType::AgentResult,
            &[("reply", serde_json::to_value(&msg).unwrap())],
        );
        AgentLifecycleEventConverter.convert(&event, &mut context);
        let interrupts = context.pending_interrupts();
        assert_eq!(interrupts.len(), 1);
        assert_eq!(interrupts[0].id, "rep1:c1");
        assert_eq!(interrupts[0].reason, TOOL_CALL_INTERRUPT_REASON);
        assert_eq!(interrupts[0].tool_call_id.as_deref(), Some("c1"));
        let metadata = interrupts[0].metadata.as_ref().unwrap();
        assert_eq!(metadata[METADATA_TOOL_NAME], json!("shell"));
        assert_eq!(metadata[METADATA_TOOL_INPUT], json!({"cmd": "ls"}));
        assert_eq!(metadata[METADATA_TOOL_CONTENT], json!(r#"{"cmd":"ls"}"#));
        assert_eq!(metadata[METADATA_REPLY_ID], json!("rep1"));
    }

    #[test]
    fn non_suspended_results_do_not_create_interrupts() {
        let mut context = context();
        let msg = Msg::builder()
            .id("rep1")
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::ToolUse(
                    ToolUseBlock::builder().id("c1").name("t").build(),
                ),
                ContentBlock::ToolResult(ToolResultBlock::builder().id("c1").build()),
            ])
            .generate_reason(GenerateReason::ToolCalls)
            .build();
        let event = delta_event(
            AgentEventType::AgentResult,
            &[("reply", serde_json::to_value(&msg).unwrap())],
        );
        AgentLifecycleEventConverter.convert(&event, &mut context);
        assert!(context.pending_interrupts().is_empty());
    }

    #[test]
    fn frontend_tool_suspensions_are_not_surfaced_as_interrupts() {
        let input = RunAgentInput::new(
            "t",
            "r",
            Vec::new(),
            vec![AguiTool::new("frontend_tool", "d", Map::new())],
            Vec::new(),
            Map::new(),
            Map::new(),
            Vec::new(),
        );
        let mut context = AguiStreamContext::new(
            "t",
            "r",
            AguiAdapterConfig::default_config(),
            Some(input),
        );
        let mut result = ToolResultBlock::builder().id("c1").build();
        result
            .metadata
            .insert(ToolResultBlock::METADATA_SUSPENDED.to_string(), json!(true));
        let msg = Msg::builder()
            .id("rep1")
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::ToolUse(
                    ToolUseBlock::builder().id("c1").name("frontend_tool").build(),
                ),
                ContentBlock::ToolResult(result),
            ])
            .generate_reason(GenerateReason::ToolSuspended)
            .build();
        let event = delta_event(
            AgentEventType::AgentResult,
            &[("reply", serde_json::to_value(&msg).unwrap())],
        );
        AgentLifecycleEventConverter.convert(&event, &mut context);
        assert!(context.pending_interrupts().is_empty());
    }

    #[test]
    fn permission_confirm_events_become_interrupts_with_a_response_schema() {
        let mut context = context();
        let event = delta_event(
            AgentEventType::RequireUserConfirm,
            &[
                ("reply_id", json!("rep1")),
                (
                    "tool_calls",
                    json!([{
                        "id": "c1",
                        "name": "shell",
                        "input": {"cmd": "ls"},
                        "state": "pending"
                    }]),
                ),
            ],
        );
        PermissionConfirmEventConverter.convert(&event, &mut context);
        let interrupts = context.pending_interrupts();
        assert_eq!(interrupts.len(), 1);
        assert_eq!(interrupts[0].id, "rep1:c1");
        assert_eq!(
            interrupts[0].message.as_deref(),
            Some("Tool 'shell' requires user confirmation before execution")
        );
        let schema = interrupts[0].response_schema.as_ref().unwrap();
        assert_eq!(schema["required"], json!(["approved"]));
        assert_eq!(schema["properties"]["approved"]["type"], json!("boolean"));
        let metadata = interrupts[0].metadata.as_ref().unwrap();
        assert_eq!(metadata[METADATA_AGENTSCOPE_INTERRUPT_KIND], json!(INTERRUPT_KIND_PERMISSION_CONFIRM));
    }

    #[test]
    fn user_confirm_results_adopt_the_validated_tool_calls() {
        let mut context = context();
        let confirm = ConfirmResult::new(
            true,
            Some(ToolUseBlock::builder().id("c1").name("shell").build()),
            Vec::new(),
        );
        let event = delta_event(
            AgentEventType::UserConfirmResult,
            &[(
                "confirm_results",
                serde_json::to_value(vec![confirm]).unwrap(),
            )],
        );
        PermissionConfirmEventConverter.convert(&event, &mut context);
        assert!(context.drain_events().is_empty());
        // Adoption lets the next run deliver this call's result.
        ToolResultEventConverter.convert(
            &delta_event(
                AgentEventType::ToolResultEnd,
                &[("reply_id", json!("r")), ("tool_call_id", json!("c1"))],
            ),
            &mut context,
        );
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::ToolCallResult));
    }

    #[test]
    fn external_execution_handshake_events_are_suppressed() {
        let mut context = context();
        ExternalExecutionEventConverter
            .convert(&AgentEvent::new(AgentEventType::RequireExternalExecution), &mut context);
        ExternalExecutionEventConverter
            .convert(&AgentEvent::new(AgentEventType::ExternalExecutionResult), &mut context);
        assert!(context.drain_events().is_empty());
        assert_eq!(
            ExternalExecutionEventConverter.event_types(),
            vec![
                AgentEventType::RequireExternalExecution,
                AgentEventType::ExternalExecutionResult
            ]
        );
    }

    #[test]
    fn token_usage_events_are_opt_in_and_accumulate() {
        let usage = ChatUsage {
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 2,
            time: 1.5,
        };
        let event = delta_event(
            AgentEventType::ModelCallEnd,
            &[
                ("reply_id", json!("rep1")),
                ("usage", serde_json::to_value(usage).unwrap()),
            ],
        );

        let mut disabled = context();
        ModelCallUsageEventConverter.convert(&event, &mut disabled);
        assert!(disabled.drain_events().is_empty());

        let mut enabled =
            context_with_config(AguiAdapterConfig::builder().emit_token_usage(true).build());
        ModelCallUsageEventConverter.convert(&event, &mut enabled);
        let events = enabled.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::Custom));
        let rendered = render(&events[0]);
        assert!(rendered.contains("\"name\":\"token_usage\""));
        assert!(rendered.contains("\"inputTokens\":10"));
        assert!(rendered.contains("\"totalTokens\":15"));

        // A second call accumulates the cumulative counters.
        ModelCallUsageEventConverter.convert(&event, &mut enabled);
        let events = enabled.drain_events();
        let rendered = render(&events[0]);
        assert!(rendered.contains("\"cumulative\":{\"inputTokens\":20"));
    }

    #[test]
    fn custom_agent_events_are_forwarded_verbatim() {
        let mut context = context();
        let event = delta_event(
            AgentEventType::Custom,
            &[("name", json!("my_event")), ("value", json!({"k": 1}))],
        );
        CustomAgentEventConverter.convert(&event, &mut context);
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        let rendered = render(&events[0]);
        assert!(rendered.contains("\"name\":\"my_event\""));
        assert!(rendered.contains("\"value\":{\"k\":1}"));
    }

    #[test]
    fn raw_converter_carries_the_source_event() {
        let mut context = context();
        let event = AgentEvent::new(AgentEventType::ExceedMaxIters);
        RawAgentEventConverter.convert(&event, &mut context);
        let events = context.drain_events();
        assert_eq!(events[0].event_type(), Some(AguiEventType::Raw));
        assert!(render(&events[0]).contains("\"type\":\"EXCEED_MAX_ITERS\""));
    }

    #[test]
    fn subagent_events_are_namespaced_as_custom_events() {
        let mut context = context();
        let mut event = delta_event(
            AgentEventType::TextBlockDelta,
            &[("reply_id", json!("rep")), ("delta", json!("child"))],
        );
        event = event.with_source("main/researcher");
        SubagentEventConverter.convert(&event, &mut context);
        let events = context.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type(), Some(AguiEventType::Custom));
        let rendered = render(&events[0]);
        assert!(rendered.contains("subagent.text"), "got {rendered}");
        assert!(rendered.contains("\"type\":\"TEXT_BLOCK_DELTA\""));
        assert!(rendered.contains("\"delta\":\"child\""));
        assert!(rendered.contains("main/researcher"));
    }

    #[test]
    fn subagent_tool_and_confirm_events_use_their_namespaces() {
        let mut context = context();
        let call = delta_event(
            AgentEventType::ToolCallStart,
            &[("tool_call_id", json!("c1")), ("tool_name", json!("t"))],
        )
        .with_source("main/child");
        SubagentEventConverter.convert(&call, &mut context);
        assert!(render(&context.drain_events()[0]).contains("subagent.tool_call"));

        let result = delta_event(
            AgentEventType::ToolResultEnd,
            &[
                ("tool_call_id", json!("c1")),
                ("tool_name", json!("t")),
                ("state", json!("SUCCESS")),
            ],
        )
        .with_source("main/child");
        SubagentEventConverter.convert(&result, &mut context);
        let rendered = render(&context.drain_events()[0]);
        assert!(rendered.contains("subagent.tool_result"));
        assert!(rendered.contains("\"state\":\"SUCCESS\""));

        let confirm = delta_event(
            AgentEventType::RequireUserConfirm,
            &[("tool_calls", json!([{"id": "c1", "name": "t"}]))],
        )
        .with_source("main/child");
        SubagentEventConverter.convert(&confirm, &mut context);
        let rendered = render(&context.drain_events()[0]);
        assert!(rendered.contains("subagent.require_confirm"));
        assert!(rendered.contains("\"toolCallCount\":1"));
    }

    #[test]
    fn subagent_unknown_events_fall_back_to_raw() {
        let mut context = context();
        let event = AgentEvent::new(AgentEventType::ExceedMaxIters).with_source("main/child");
        SubagentEventConverter.convert(&event, &mut context);
        let events = context.drain_events();
        assert_eq!(events[0].event_type(), Some(AguiEventType::Raw));
        let rendered = render(&events[0]);
        assert!(rendered.contains("main/child"));
    }

    #[test]
    fn lifecycle_subagent_events_are_custom_not_native() {
        let mut context = context();
        let start = AgentEvent::new(AgentEventType::AgentStart).with_source("main/child");
        SubagentEventConverter.convert(&start, &mut context);
        let events = context.drain_events();
        assert_eq!(events[0].event_type(), Some(AguiEventType::Custom));
        assert!(render(&events[0]).contains("subagent.lifecycle"));
        assert!(render(&events[0]).contains("\"type\":\"AGENT_START\""));
    }

    #[test]
    fn block_message_id_prefers_an_explicit_block_id() {
        let derived = delta_event(
            AgentEventType::TextBlockDelta,
            &[("reply_id", json!("r")), ("block_type", json!("text"))],
        );
        assert_eq!(block_message_id(&derived), "r-text");
        let explicit = delta_event(
            AgentEventType::TextBlockDelta,
            &[
                ("reply_id", json!("r")),
                ("block_id", json!("b9")),
                ("block_type", json!("text")),
            ],
        );
        assert_eq!(block_message_id(&explicit), "r-b9");
        // Without an explicit id the block type is derived from the event kind, so a delta and
        // its matching end event stay correlated on the same AG-UI message id.
        let bare = delta_event(AgentEventType::TextBlockDelta, &[]);
        assert_eq!(block_message_id(&bare), "-text");
    }
}
