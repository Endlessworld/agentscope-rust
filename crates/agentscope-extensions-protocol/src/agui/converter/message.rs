//! `Msg` <-> AG-UI message conversion, mirroring `AguiMessageConverter`.
//!
//! The converter is stateless: every method is a pure function over its arguments, so
//! instances can be shared freely across requests.

use std::collections::{HashMap, HashSet};

use agentscope_core::{
    AudioBlock, Base64Source, ConfirmResult, ContentBlock, ImageBlock, Msg, MsgRole, Source,
    TextBlock, ThinkingBlock, ToolResultBlock, ToolResultState, ToolUseBlock, URLSource, VideoBlock,
};
use serde_json::{Map, Value};

use crate::agui::constants::{
    INTERRUPT_KIND_PERMISSION_CONFIRM, METADATA_AGENTSCOPE_INTERRUPT_KIND, METADATA_TOOL_CONTENT,
    METADATA_TOOL_INPUT, METADATA_TOOL_NAME, TOOL_CALL_INTERRUPT_REASON,
};
use crate::agui::error::{AguiError, AguiResult};
use crate::agui::event::Interrupt;
use crate::agui::model::{
    AguiFunctionCall, AguiMessage, AguiResume, AguiToolCall, InputContent, InputContentSource,
    MessageContent, RunAgentInput,
};

/// Metadata key carried on a resume-produced `ToolResultBlock`: the originating interrupt id.
pub const METADATA_INTERRUPT_ID: &str = "agui.interruptId";

/// Metadata key carried on a resume-produced `ToolResultBlock`: the resume status.
pub const METADATA_RESUME_STATUS: &str = "agui.resumeStatus";

/// Bidirectional converter between AG-UI messages and AgentScope [`Msg`] values.
#[derive(Debug, Clone, Copy, Default)]
pub struct AguiMessageConverter;

impl AguiMessageConverter {
    /// Creates a converter.
    pub fn new() -> Self {
        Self
    }

    /// Converts an AG-UI message into an AgentScope [`Msg`].
    ///
    /// Tool messages always yield a `ToolResultBlock`, even when the frontend returned no
    /// content, so a pending tool call is resolved downstream. A non-blank `error` on a tool
    /// message makes that block an error result, with the error text kept alongside any
    /// content the frontend also reported. A `reasoning` message becomes an assistant message
    /// carrying a `ThinkingBlock`.
    pub fn to_msg(&self, agui_message: &AguiMessage) -> AguiResult<Msg> {
        let role = convert_role_in(&agui_message.role);
        let mut blocks: Vec<ContentBlock> = Vec::new();

        match &agui_message.content {
            Some(MessageContent::Text(text)) => {
                if agui_message.is_reasoning_message() {
                    add_thinking_block(&mut blocks, text);
                } else {
                    add_text_block(&mut blocks, Some(text.as_str()), agui_message);
                }
            }
            Some(MessageContent::Blocks(parts)) => {
                if !agui_message.is_user_message() {
                    return Err(AguiError::invalid_input(
                        "Structured content blocks are only supported for AG-UI user messages",
                    ));
                }
                for input in parts {
                    blocks.push(to_content_block(input)?);
                }
            }
            None => {
                if agui_message.is_tool_message() && agui_message.tool_call_id.is_some() {
                    add_text_block(&mut blocks, Some(""), agui_message);
                }
            }
        }

        for tc in &agui_message.tool_calls {
            blocks.push(to_tool_use_block(tc));
        }

        Ok(Msg::builder()
            .id(agui_message.id.clone())
            .role(role)
            .content(blocks)
            .build())
    }

    /// Converts an AgentScope [`Msg`] into a single AG-UI message.
    ///
    /// Text blocks and tool-result text are joined with `\n`; tool-use blocks become
    /// `toolCalls`; a tool-result block also sets `toolCallId`. An assistant message that
    /// carries only reasoning becomes a `reasoning` message. A message that expands into
    /// several AG-UI messages — reasoning mixed with text or tool calls, or a TOOL message
    /// with several tool results — has to go through
    /// [`to_agui_messages`](Self::to_agui_messages), which is what
    /// [`to_agui_message_list`](Self::to_agui_message_list) uses.
    pub fn to_agui_message(&self, msg: &Msg) -> AguiMessage {
        self.to_single_agui_message(msg)
    }

    /// Converts an AgentScope [`Msg`] into one or more AG-UI messages.
    ///
    /// A message mixing reasoning with text or tool calls is split into a reasoning message
    /// followed by the standard message. A TOOL message carrying several tool results is
    /// expanded into one AG-UI tool message per result, each keeping its own `toolCallId`,
    /// content and error. Expanded messages get a distinct, deterministic id derived from the
    /// source id (`<sourceId>-reasoning`, `<sourceId>-<index>`), so the split is always
    /// observable.
    pub fn to_agui_messages(&self, msg: &Msg) -> Vec<AguiMessage> {
        if msg.role() == MsgRole::Tool {
            let tool_results: Vec<&ToolResultBlock> = msg.tool_result_blocks().collect();
            if tool_results.len() > 1 {
                return tool_results
                    .iter()
                    .enumerate()
                    .map(|(index, result)| tool_result_message(msg, result, index))
                    .collect();
            }
        }

        let standard = self.to_single_agui_message(msg);
        let reasoning = thinking_content(msg);
        if reasoning.is_empty() || standard.is_reasoning_message() {
            return vec![standard];
        }
        vec![
            AguiMessage::reasoning_message(reasoning_message_id(msg), reasoning),
            standard,
        ]
    }

    /// Converts an AgentScope [`Msg`] that maps to exactly one AG-UI message.
    fn to_single_agui_message(&self, msg: &Msg) -> AguiMessage {
        let role = convert_role_out(msg.role());
        let tool_message = msg.role() == MsgRole::Tool;
        let mut content = String::new();
        let mut tool_calls: Vec<AguiToolCall> = Vec::new();
        let mut tool_call_id: Option<String> = None;
        let mut error: Option<String> = None;

        for block in msg.content() {
            match block {
                ContentBlock::Text(tb) => append_content(&mut content, &tb.text),
                ContentBlock::ToolUse(tub) => tool_calls.push(to_agui_tool_call(tub)),
                ContentBlock::ToolResult(trb) => {
                    tool_call_id = trb.id.clone();
                    // `error` belongs to the protocol's tool message; for any other role the
                    // result text has to stay in `content`. Mirrors `toSingleAguiMessage`.
                    if tool_message && trb.state == ToolResultState::Error {
                        error = Some(tool_result_text(trb));
                    } else {
                        for output in &trb.output {
                            if let ContentBlock::Text(tb) = output {
                                append_content(&mut content, &tb.text);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // A message that only reasoned has no assistant content to carry the reasoning, so it
        // becomes a reasoning message instead of losing it.
        let reasoning = thinking_content(msg);
        if !reasoning.is_empty()
            && content.is_empty()
            && tool_calls.is_empty()
            && tool_call_id.is_none()
        {
            return AguiMessage::reasoning_message(msg.id(), reasoning);
        }

        let mut message = AguiMessage::new(
            msg.id(),
            role,
            (!content.is_empty()).then(|| MessageContent::Text(content)),
            tool_calls,
            tool_call_id,
        );
        message.error = error;
        message
    }

    /// Converts a list of AG-UI messages into AgentScope messages.
    pub fn to_msg_list(&self, agui_messages: &[AguiMessage]) -> AguiResult<Vec<Msg>> {
        agui_messages.iter().map(|m| self.to_msg(m)).collect()
    }

    /// Converts a list of AgentScope messages into AG-UI messages.
    ///
    /// The output can be longer than the input: a message that mixes reasoning with other
    /// content, or a TOOL message with several results, expands into one AG-UI message per
    /// part.
    pub fn to_agui_message_list(&self, msgs: &[Msg]) -> Vec<AguiMessage> {
        msgs.iter().flat_map(|m| self.to_agui_messages(m)).collect()
    }

    /// Converts a run input (messages plus official resume entries) into AgentScope messages.
    pub fn to_input_msg_list(&self, input: &RunAgentInput) -> AguiResult<Vec<Msg>> {
        self.to_input_msg_list_with_interrupts(input, &HashMap::new())
    }

    /// Converts a run input, resolving resume entries through known originating interrupts.
    ///
    /// Some clients (notably CopilotKit `useInterrupt.resolve()`) send both a `role:"tool"`
    /// message and a matching `resume[]` entry for the same tool-call id. Ordinary tool
    /// resumes that would duplicate an already-present `ToolResultBlock` id are skipped so
    /// the ReAct loop does not observe a duplicate tool-result id. Permission-confirm resumes
    /// still produce a USER `ConfirmResult` message and are never skipped on that basis.
    pub fn to_input_msg_list_with_interrupts(
        &self,
        input: &RunAgentInput,
        resume_interrupts: &HashMap<String, Interrupt>,
    ) -> AguiResult<Vec<Msg>> {
        let mut msgs = self.to_msg_list(&input.messages)?;
        let mut existing_tool_result_ids = collect_tool_result_ids(&msgs);
        for resume in &input.resume {
            let interrupt = resume_interrupts.get(&resume.interrupt_id);
            let Some(tool_call_id) = resolve_tool_call_id(&resume.interrupt_id, interrupt) else {
                continue;
            };
            if is_permission_confirm_interrupt(interrupt) {
                msgs.push(self.to_confirm_result_msg(resume, &tool_call_id, interrupt));
            } else if !existing_tool_result_ids.contains(&tool_call_id) {
                msgs.push(to_tool_result_msg(resume, &tool_call_id));
                existing_tool_result_ids.insert(tool_call_id);
            }
        }
        Ok(msgs)
    }

    /// Builds the USER-role confirmation message consumed by the ReAct loop after a
    /// permission-mode HITL resume.
    fn to_confirm_result_msg(
        &self,
        resume: &AguiResume,
        tool_call_id: &str,
        interrupt: Option<&Interrupt>,
    ) -> Msg {
        let approved = is_approved(resume);
        let metadata = interrupt.and_then(|i| i.metadata.as_ref());

        let tool_name = metadata
            .and_then(|m| m.get(METADATA_TOOL_NAME))
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut tool_input = metadata
            .and_then(|m| m.get(METADATA_TOOL_INPUT))
            .and_then(Value::as_object)
            .cloned();
        let mut tool_content = metadata
            .and_then(|m| m.get(METADATA_TOOL_CONTENT))
            .and_then(Value::as_str)
            .map(str::to_string);

        // `editedArgs` fully replaces the original input and re-serializes the content so
        // the downstream tool-input validation always sees a non-null JSON object string.
        if let Some(edited) = resume.payload_edited_args() {
            tool_input = Some(edited.clone());
            tool_content = Some(serialize_arguments(Some(edited)));
        }

        let mut tool_use = ToolUseBlock::builder()
            .id(tool_call_id)
            .name(tool_name.unwrap_or_default())
            .build();
        if let Some(input) = tool_input {
            tool_use.input = input;
        }
        tool_use.content = tool_content;

        let confirm_result = {
            let mut result = ConfirmResult::new(approved, Some(tool_use), Vec::new());
            // Mirrors Java `c0d03ccea`: `resume.payload.reason` becomes `ConfirmResult.reason`
            // and, on denial, the text of the DENIED tool result. Core carries that text in
            // `ConfirmResult::message`.
            if let Some(reason) = resume.reason() {
                result.message = reason.to_string();
            }
            result
        };
        let mut msg_metadata = Map::new();
        msg_metadata.insert(
            Msg::METADATA_CONFIRM_RESULTS.to_string(),
            serde_json::to_value(vec![confirm_result]).unwrap_or(Value::Array(Vec::new())),
        );
        Msg::builder()
            .id(format!("agui-confirm-{}", resume.interrupt_id))
            .role(MsgRole::User)
            .text_content(if approved { "approved" } else { "denied" })
            .metadata(msg_metadata)
            .build()
    }
}

/// Builds a TOOL-role message carrying the resume payload as a `ToolResultBlock`.
fn to_tool_result_msg(resume: &AguiResume, tool_call_id: &str) -> Msg {
    let state = if resume.is_cancelled() {
        ToolResultState::Interrupted
    } else {
        ToolResultState::Success
    };
    let mut metadata = Map::new();
    metadata.insert(
        METADATA_INTERRUPT_ID.to_string(),
        Value::String(resume.interrupt_id.clone()),
    );
    metadata.insert(
        METADATA_RESUME_STATUS.to_string(),
        Value::String(resume.status.clone()),
    );
    let result = ToolResultBlock::builder()
        .id(tool_call_id)
        .output(ContentBlock::Text(TextBlock::new(resume_content(resume))))
        .metadata(metadata)
        .state(state)
        .build();
    Msg::builder()
        .id(format!("agui-resume-{}", resume.interrupt_id))
        .role(MsgRole::Tool)
        .content(vec![ContentBlock::ToolResult(result)])
        .build()
}

/// Appends a text block, wrapping tool messages in a `ToolResultBlock`.
///
/// A non-blank `error` on the tool message turns the block into an error result: the error
/// text comes first and any content the frontend also sent is appended after it. Blank or
/// absent errors keep the previous success behaviour.
fn add_text_block(blocks: &mut Vec<ContentBlock>, text: Option<&str>, agui_message: &AguiMessage) {
    if agui_message.is_tool_message() {
        if let Some(tool_call_id) = &agui_message.tool_call_id {
            if let Some(error) = agui_message
                .error
                .as_deref()
                .filter(|error| !error.trim().is_empty())
            {
                let output = match text {
                    Some(text) if !text.is_empty() => format!("{error}\n{text}"),
                    _ => error.to_string(),
                };
                blocks.push(ContentBlock::ToolResult(
                    ToolResultBlock::builder()
                        .id(tool_call_id)
                        .output(ContentBlock::Text(TextBlock::new(output)))
                        .state(ToolResultState::Error)
                        .build(),
                ));
                return;
            }
            blocks.push(ContentBlock::ToolResult(
                ToolResultBlock::builder()
                    .id(tool_call_id)
                    .output(ContentBlock::Text(TextBlock::new(text.unwrap_or(""))))
                    .state(ToolResultState::Success)
                    .build(),
            ));
            return;
        }
    }
    if let Some(text) = text {
        if !text.is_empty() {
            blocks.push(ContentBlock::Text(TextBlock::new(text)));
        }
    }
}

/// Appends a `ThinkingBlock` for a reasoning payload; empty reasoning is dropped.
fn add_thinking_block(blocks: &mut Vec<ContentBlock>, text: &str) {
    if text.is_empty() {
        return;
    }
    blocks.push(ContentBlock::Thinking(ThinkingBlock::new(text)));
}

/// Joins the text blocks of one tool result into a single string.
fn tool_result_text(result: &ToolResultBlock) -> String {
    let mut text = String::new();
    for output in &result.output {
        if let ContentBlock::Text(tb) = output {
            append_content(&mut text, &tb.text);
        }
    }
    text
}

/// Builds one AG-UI tool message from a single AgentScope tool result.
///
/// The first expanded message keeps the source message id; later ones get an index suffix so
/// every emitted AG-UI message has a distinct id.
fn tool_result_message(msg: &Msg, result: &ToolResultBlock, index: usize) -> AguiMessage {
    let id = if index == 0 {
        msg.id().to_string()
    } else {
        format!("{}-{}", msg.id(), index)
    };
    let text = tool_result_text(result);
    let error = result.state == ToolResultState::Error;
    let mut message = AguiMessage::new(
        id,
        convert_role_out(msg.role()),
        (!error && !text.is_empty()).then(|| MessageContent::Text(text.clone())),
        Vec::new(),
        result.id.clone(),
    );
    message.error = error.then_some(text);
    message
}

/// The id of the reasoning message split out of a source message.
fn reasoning_message_id(msg: &Msg) -> String {
    format!("{}-reasoning", msg.id())
}

/// Joins the `ThinkingBlock` texts of a message into a single string.
fn thinking_content(msg: &Msg) -> String {
    let mut reasoning = String::new();
    for block in msg.content() {
        if let ContentBlock::Thinking(tb) = block {
            append_content(&mut reasoning, &tb.thinking);
        }
    }
    reasoning
}

/// Converts an AG-UI [`InputContent`] into a core [`ContentBlock`].
fn to_content_block(input: &InputContent) -> AguiResult<ContentBlock> {
    Ok(match input {
        InputContent::Text { text } => ContentBlock::Text(TextBlock::new(text)),
        InputContent::Image { source, .. } => {
            ContentBlock::Image(ImageBlock::new(to_source(source)?))
        }
        InputContent::Audio { source, .. } => {
            ContentBlock::Audio(AudioBlock::new(to_source(source)?))
        }
        InputContent::Video { source, .. } => {
            ContentBlock::Video(VideoBlock::new(to_source(source)?))
        }
        // Mirrors the Java stub: AgentScope has no native DocumentBlock, and the Java
        // `toContentBlock` throws for unhandled InputContent subtypes.
        InputContent::Document { .. } => {
            return Err(AguiError::IllegalState(
                "Unhandled InputContent type: document".to_string(),
            ))
        }
    })
}

/// Converts an AG-UI [`InputContentSource`] into a core [`Source`].
fn to_source(input_source: &InputContentSource) -> AguiResult<Source> {
    Ok(match input_source {
        InputContentSource::Url { value, mime_type } => {
            let mut builder = URLSource::builder().url(value.clone());
            if let Some(mime_type) = mime_type {
                builder = builder.mime_type(mime_type.clone());
            }
            Source::Url(builder.build())
        }
        InputContentSource::Data { value, mime_type } => {
            Source::Base64(Base64Source::new(mime_type.clone(), value.clone()))
        }
    })
}

/// Converts an AG-UI tool call into a core `ToolUseBlock`.
fn to_tool_use_block(tc: &AguiToolCall) -> ContentBlock {
    ContentBlock::ToolUse(
        ToolUseBlock::builder()
            .id(tc.id.clone())
            .name(tc.function.name.clone())
            .input(parse_json_arguments(&tc.function.arguments))
            .build(),
    )
}

/// Converts a core `ToolUseBlock` into an AG-UI tool call.
fn to_agui_tool_call(tub: &ToolUseBlock) -> AguiToolCall {
    AguiToolCall::new(
        tub.id.clone(),
        AguiFunctionCall::new(tub.name.clone(), Some(serialize_arguments(Some(&tub.input)))),
    )
}

/// Converts an AG-UI role string into a [`MsgRole`]; unknown roles fall back to `USER`.
fn convert_role_in(role: &str) -> MsgRole {
    match role.to_ascii_lowercase().as_str() {
        "assistant" => MsgRole::Assistant,
        "system" => MsgRole::System,
        "tool" => MsgRole::Tool,
        // A reasoning message is assistant-authored thinking content.
        "reasoning" => MsgRole::Assistant,
        _ => MsgRole::User,
    }
}

/// Converts a [`MsgRole`] into its AG-UI role string.
fn convert_role_out(role: MsgRole) -> &'static str {
    match role {
        MsgRole::User => "user",
        MsgRole::Assistant => "assistant",
        MsgRole::System => "system",
        MsgRole::Tool => "tool",
    }
}

/// Appends a newline-separated segment to the accumulated content; empty segments are
/// skipped so they cannot introduce a blank line.
fn append_content(content: &mut String, segment: &str) {
    if segment.is_empty() {
        return;
    }
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(segment);
}

/// Parses a JSON-object argument string; malformed input yields an empty map.
pub(crate) fn parse_json_arguments(arguments: &str) -> Map<String, Value> {
    if arguments.is_empty() {
        return Map::new();
    }
    serde_json::from_str::<Map<String, Value>>(arguments).unwrap_or_default()
}

/// Serializes an argument map to a JSON-object string; empty input yields `{}`.
pub(crate) fn serialize_arguments(arguments: Option<&Map<String, Value>>) -> String {
    match arguments {
        Some(map) if !map.is_empty() => {
            serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string())
        }
        _ => "{}".to_string(),
    }
}

/// Resolves the JSON-object argument string for a tool call, preferring raw streaming content.
pub(crate) fn resolve_tool_call_args_json(tool_use: &ToolUseBlock) -> String {
    match tool_use.content.as_deref().filter(|c| !c.is_empty()) {
        Some(content) => content.to_string(),
        None => serialize_arguments(Some(&tool_use.input)),
    }
}

/// Collects every non-blank tool-result id present in the given messages.
fn collect_tool_result_ids(msgs: &[Msg]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for msg in msgs {
        for block in msg.content() {
            if let ContentBlock::ToolResult(result) = block {
                if let Some(id) = result.id.as_deref().filter(|id| !id.trim().is_empty()) {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    ids
}

/// Whether the interrupt is a permission-mode tool confirmation.
fn is_permission_confirm_interrupt(interrupt: Option<&Interrupt>) -> bool {
    let Some(interrupt) = interrupt else {
        return false;
    };
    if interrupt.reason != TOOL_CALL_INTERRUPT_REASON {
        return false;
    }
    interrupt
        .metadata
        .as_ref()
        .and_then(|m| m.get(METADATA_AGENTSCOPE_INTERRUPT_KIND))
        .and_then(Value::as_str)
        == Some(INTERRUPT_KIND_PERMISSION_CONFIRM)
}

/// Whether the user approved the tool: `cancelled` denies, otherwise only `approved: true` allows.
fn is_approved(resume: &AguiResume) -> bool {
    if resume.is_cancelled() {
        return false;
    }
    resume.payload_approved() == Some(true)
}

/// Resolves the tool-call id a resume entry answers, if any.
fn resolve_tool_call_id(interrupt_id: &str, interrupt: Option<&Interrupt>) -> Option<String> {
    if let Some(interrupt) = interrupt {
        if let Some(tool_call_id) = interrupt.tool_call_id.as_deref() {
            if !tool_call_id.trim().is_empty() {
                return Some(tool_call_id.to_string());
            }
        }
        if interrupt.reason != TOOL_CALL_INTERRUPT_REASON {
            return None;
        }
    }
    if interrupt_id.trim().is_empty() {
        return None;
    }
    match interrupt_id.rfind(':') {
        Some(separator) if separator < interrupt_id.len() - 1 => {
            Some(interrupt_id[separator + 1..].to_string())
        }
        _ => Some(interrupt_id.to_string()),
    }
}

/// Renders the resume payload as tool-result text.
///
/// A cancelled interrupt with an explicit reason reports that reason; without one it keeps
/// the historical default text. This is where an inbound `resume.reason` (or
/// `resume.payload.reason`) turns into the tool-result text the agent sees.
fn resume_content(resume: &AguiResume) -> String {
    if resume.is_cancelled() {
        return match resume.reason() {
            Some(reason) => reason.to_string(),
            None => "Interrupt cancelled by user".to_string(),
        };
    }
    match &resume.payload {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::constants::RESUME_PAYLOAD_APPROVED;
    use serde_json::json;

    fn converter() -> AguiMessageConverter {
        AguiMessageConverter::new()
    }

    fn interrupt_with_metadata(
        id: &str,
        tool_call_id: Option<&str>,
        metadata: Option<Map<String, Value>>,
    ) -> Interrupt {
        let mut interrupt = Interrupt::new(id, TOOL_CALL_INTERRUPT_REASON);
        interrupt.tool_call_id = tool_call_id.map(str::to_string);
        interrupt.metadata = metadata;
        interrupt
    }

    #[test]
    fn text_user_message_round_trips() {
        let converter = converter();
        let agui = AguiMessage::user_message("m1", "hello world");
        let msg = converter.to_msg(&agui).unwrap();
        assert_eq!(msg.role(), MsgRole::User);
        assert_eq!(msg.id(), "m1");
        assert_eq!(msg.text_content(), "hello world");

        let back = converter.to_agui_message(&msg);
        assert_eq!(back.id, "m1");
        assert_eq!(back.role, "user");
        assert_eq!(back.text_content(), Some("hello world"));
        assert!(back.tool_calls.is_empty());
        assert_eq!(back.tool_call_id, None);
    }

    #[test]
    fn all_four_roles_are_mapped_both_ways() {
        let converter = converter();
        for (role, expected) in [
            ("user", MsgRole::User),
            ("assistant", MsgRole::Assistant),
            ("system", MsgRole::System),
            ("tool", MsgRole::Tool),
        ] {
            let agui =
                AguiMessage::new("m", role, Some(MessageContent::Text("x".into())), Vec::new(), None);
            assert_eq!(converter.to_msg(&agui).unwrap().role(), expected);
            assert_eq!(converter.to_agui_message(&Msg::text(expected, "x")).role, role);
        }
        // Unknown roles fall back to USER, mirroring the Java switch default.
        let unknown =
            AguiMessage::new("m", "developer", Some(MessageContent::Text("x".into())), Vec::new(), None);
        assert_eq!(converter.to_msg(&unknown).unwrap().role(), MsgRole::User);
        // Role matching is case-insensitive.
        let upper =
            AguiMessage::new("m", "ASSISTANT", Some(MessageContent::Text("x".into())), Vec::new(), None);
        assert_eq!(converter.to_msg(&upper).unwrap().role(), MsgRole::Assistant);
    }

    #[test]
    fn assistant_tool_call_round_trips_through_json_arguments() {
        let converter = converter();
        let agui = AguiMessage::new(
            "m2",
            "assistant",
            None,
            vec![AguiToolCall::new(
                "c1",
                AguiFunctionCall::new("search", Some(r#"{"q":"rust"}"#.into())),
            )],
            None,
        );
        let msg = converter.to_msg(&agui).unwrap();
        assert_eq!(msg.role(), MsgRole::Assistant);
        let tool_use = msg.tool_use_blocks().next().unwrap();
        assert_eq!(tool_use.id, "c1");
        assert_eq!(tool_use.name, "search");
        assert_eq!(tool_use.input.get("q"), Some(&json!("rust")));

        let back = converter.to_agui_message(&msg);
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].id, "c1");
        assert_eq!(back.tool_calls[0].function.name, "search");
        assert_eq!(
            parse_json_arguments(&back.tool_calls[0].function.arguments).get("q"),
            Some(&json!("rust"))
        );
        // No text content, so `content` stays absent.
        assert!(back.content.is_none());
    }

    #[test]
    fn malformed_tool_arguments_degrade_to_an_empty_object() {
        assert!(parse_json_arguments("not json").is_empty());
        assert!(parse_json_arguments("").is_empty());
        assert_eq!(serialize_arguments(None), "{}");
        assert_eq!(serialize_arguments(Some(&Map::new())), "{}");
    }

    #[test]
    fn tool_message_without_content_still_yields_a_tool_result_block() {
        let converter = converter();
        let agui = AguiMessage::new("m3", "tool", None, Vec::new(), Some("c9".to_string()));
        let msg = converter.to_msg(&agui).unwrap();
        assert_eq!(msg.role(), MsgRole::Tool);
        let result = msg.tool_result_blocks().next().unwrap();
        assert_eq!(result.id.as_deref(), Some("c9"));
        assert_eq!(result.text_content(), "");
        assert_eq!(result.state, ToolResultState::Success);

        let back = converter.to_agui_message(&msg);
        assert_eq!(back.role, "tool");
        assert_eq!(back.tool_call_id.as_deref(), Some("c9"));
        assert!(back.content.is_none());
    }

    #[test]
    fn tool_result_text_is_flattened_into_content() {
        let converter = converter();
        let msg = Msg::builder()
            .id("m4")
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(
                ToolResultBlock::builder()
                    .id("c1")
                    .output(ContentBlock::Text(TextBlock::new("42")))
                    .build(),
            )])
            .build();
        let back = converter.to_agui_message(&msg);
        assert_eq!(back.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(back.text_content(), Some("42"));
    }

    #[test]
    fn text_blocks_are_joined_with_newlines() {
        let converter = converter();
        let msg = Msg::builder()
            .id("m5")
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Text(TextBlock::new("a")),
                ContentBlock::Text(TextBlock::new("b")),
            ])
            .build();
        assert_eq!(converter.to_agui_message(&msg).text_content(), Some("a\nb"));
    }

    #[test]
    fn reasoning_message_converts_to_a_thinking_block() {
        let converter = converter();
        let agui = AguiMessage::reasoning_message("m-r1", "Inspect the tool result");

        let msg = converter.to_msg(&agui).unwrap();

        assert_eq!(msg.role(), MsgRole::Assistant);
        assert_eq!(msg.content().len(), 1);
        match &msg.content()[0] {
            ContentBlock::Thinking(thinking) => {
                assert_eq!(thinking.thinking, "Inspect the tool result")
            }
            other => panic!("expected a thinking block, got {other:?}"),
        }
        assert!(msg.tool_result_blocks().next().is_none());
    }

    #[test]
    fn thinking_block_is_converted_into_a_reasoning_message() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-thinking")
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Thinking(ThinkingBlock::new(
                "Need to inspect the weather",
            ))])
            .build();

        let result = converter.to_agui_message(&msg);

        assert_eq!(result.id, "msg-thinking");
        assert_eq!(result.role, "reasoning");
        assert!(result.is_reasoning_message());
        assert_eq!(result.text_content(), Some("Need to inspect the weather"));
        assert!(result.tool_calls.is_empty());
        assert!(result.error.is_none());
    }

    #[test]
    fn mixed_reasoning_and_text_splits_into_two_distinct_messages() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-mixed")
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Thinking(ThinkingBlock::new("Need a tool")),
                ContentBlock::Text(TextBlock::new("Let me check")),
                ContentBlock::ToolUse(
                    ToolUseBlock::builder()
                        .id("tc-weather")
                        .name("get_weather")
                        .input(parse_json_arguments(r#"{"city":"Beijing"}"#))
                        .build(),
                ),
            ])
            .build();

        let results = converter.to_agui_messages(&msg);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "msg-mixed-reasoning");
        assert!(results[0].is_reasoning_message());
        assert_eq!(results[0].text_content(), Some("Need a tool"));
        assert_ne!(results[0].id, results[1].id);
        assert!(results[1].is_assistant_message());
        assert_eq!(results[1].id, "msg-mixed");
        assert_eq!(results[1].text_content(), Some("Let me check"));
        assert_eq!(results[1].tool_calls.len(), 1);
        assert_eq!(results[1].tool_calls[0].id, "tc-weather");

        // Reasoning-only messages still map to exactly one AG-UI message.
        let reasoning_only = Msg::builder()
            .id("msg-reasoning")
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Thinking(ThinkingBlock::new("only"))])
            .build();
        assert_eq!(converter.to_agui_messages(&reasoning_only).len(), 1);
    }

    #[test]
    fn message_list_expands_reasoning_messages() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-mixed")
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Thinking(ThinkingBlock::new("Thinking")),
                ContentBlock::Text(TextBlock::new("Answer")),
            ])
            .build();

        let results = converter.to_agui_message_list(&[msg]);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].role, "reasoning");
        assert_eq!(results[0].text_content(), Some("Thinking"));
        assert_eq!(results[1].role, "assistant");
        assert_eq!(results[1].text_content(), Some("Answer"));
    }

    #[test]
    fn tool_message_with_several_results_expands_one_message_per_result() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-multi")
            .role(MsgRole::Tool)
            .content(vec![
                ContentBlock::ToolResult(
                    ToolResultBlock::builder()
                        .id("tc-1")
                        .output(ContentBlock::Text(TextBlock::new("first")))
                        .state(ToolResultState::Success)
                        .build(),
                ),
                ContentBlock::ToolResult(
                    ToolResultBlock::builder()
                        .id("tc-2")
                        .output(ContentBlock::Text(TextBlock::new("second")))
                        .state(ToolResultState::Error)
                        .build(),
                ),
            ])
            .build();

        let results = converter.to_agui_messages(&msg);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "msg-multi");
        assert_eq!(results[1].id, "msg-multi-1");
        assert_eq!(results[0].tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(results[0].text_content(), Some("first"));
        assert!(results[0].error.is_none());
        assert_eq!(results[1].tool_call_id.as_deref(), Some("tc-2"));
        assert_eq!(results[1].error.as_deref(), Some("second"));
        assert_eq!(results[1].text_content(), None);
    }

    #[test]
    fn frontend_tool_error_becomes_an_error_tool_result() {
        let converter = converter();
        let agui = AguiMessage::new("msg-t1", "tool", None, Vec::new(), Some("tc-1".into()))
            .with_error("sandbox unavailable");

        let msg = converter.to_msg(&agui).unwrap();

        assert_eq!(msg.role(), MsgRole::Tool);
        let result = msg.tool_result_blocks().next().unwrap();
        assert_eq!(result.id.as_deref(), Some("tc-1"));
        assert_eq!(result.state, ToolResultState::Error);
        assert_eq!(result.text_content(), "sandbox unavailable");

        // The error text comes first and the payload the frontend also sent is kept.
        let agui = AguiMessage::new(
            "msg-t1",
            "tool",
            Some(MessageContent::Text("partial output".into())),
            Vec::new(),
            Some("tc-1".into()),
        )
        .with_error("sandbox unavailable");
        let result = converter
            .to_msg(&agui)
            .unwrap()
            .tool_result_blocks()
            .next()
            .unwrap()
            .clone();
        assert_eq!(result.state, ToolResultState::Error);
        assert_eq!(result.text_content(), "sandbox unavailable\npartial output");
    }

    #[test]
    fn blank_frontend_tool_error_and_non_tool_error_stay_success() {
        let converter = converter();
        let blank = AguiMessage::new(
            "msg-t1",
            "tool",
            Some(MessageContent::Text("Tool result here".into())),
            Vec::new(),
            Some("tc-1".into()),
        )
        .with_error("   ");
        let result = converter
            .to_msg(&blank)
            .unwrap()
            .tool_result_blocks()
            .next()
            .unwrap()
            .clone();
        assert_eq!(result.state, ToolResultState::Success);
        assert_eq!(result.text_content(), "Tool result here");

        // `error` is only read for tool messages, so on an assistant message it is a no-op.
        let assistant = AguiMessage::new(
            "msg-t5",
            "assistant",
            Some(MessageContent::Text("Calling tool...".into())),
            Vec::new(),
            None,
        )
        .with_error("sandbox unavailable");
        let msg = converter.to_msg(&assistant).unwrap();
        assert_eq!(msg.role(), MsgRole::Assistant);
        assert!(msg.tool_result_blocks().next().is_none());
        assert_eq!(msg.text_content(), "Calling tool...");
    }

    #[test]
    fn error_tool_result_round_trips_through_both_directions() {
        let converter = converter();
        let original = AguiMessage::new(
            "msg-t4",
            "tool",
            Some(MessageContent::Text("partial output".into())),
            Vec::new(),
            Some("tc-1".into()),
        )
        .with_error("sandbox unavailable");

        let msg = converter.to_msg(&original).unwrap();
        let round_tripped = converter.to_agui_message(&msg);
        assert_eq!(round_tripped.id, "msg-t4");
        assert_eq!(round_tripped.role, "tool");
        assert_eq!(round_tripped.tool_call_id.as_deref(), Some("tc-1"));

        // The payload sent alongside the error stays part of the error text, so the round trip
        // carries both rather than dropping the payload.
        assert_eq!(
            round_tripped.error.as_deref(),
            Some("sandbox unavailable\npartial output")
        );

        let result = converter
            .to_msg(&round_tripped)
            .unwrap()
            .tool_result_blocks()
            .next()
            .unwrap()
            .clone();
        assert_eq!(result.state, ToolResultState::Error);
        assert_eq!(result.text_content(), "sandbox unavailable\npartial output");
    }

    #[test]
    fn error_tool_result_on_an_assistant_role_keeps_its_text_in_content() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-t20")
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::ToolResult(
                ToolResultBlock::builder()
                    .id("srv-1")
                    .output(ContentBlock::Text(TextBlock::new(
                        "web search failed: too_many_requests",
                    )))
                    .state(ToolResultState::Error)
                    .build(),
            )])
            .build();

        let agui = converter.to_agui_message(&msg);

        assert_eq!(agui.role, "assistant");
        assert!(agui.error.is_none());
        assert_eq!(
            agui.text_content(),
            Some("web search failed: too_many_requests")
        );
    }

    #[test]
    fn user_blocks_convert_to_media_blocks() {
        let converter = converter();
        let agui = AguiMessage::user_message_blocks(
            "m6",
            vec![
                InputContent::Text {
                    text: "look".into(),
                },
                InputContent::Image {
                    source: InputContentSource::Url {
                        value: "https://x/a.png".into(),
                        mime_type: Some("image/png".into()),
                    },
                    metadata: None,
                },
                InputContent::Audio {
                    source: InputContentSource::Data {
                        value: "QUJD".into(),
                        mime_type: "audio/mpeg".into(),
                    },
                    metadata: None,
                },
                InputContent::Video {
                    source: InputContentSource::Url {
                        value: "https://x/v.mp4".into(),
                        mime_type: None,
                    },
                    metadata: None,
                },
            ],
        );
        let msg = converter.to_msg(&agui).unwrap();
        assert_eq!(msg.content().len(), 4);
        assert!(matches!(msg.content()[0], ContentBlock::Text(_)));
        match &msg.content()[1] {
            ContentBlock::Image(image) => match &image.source {
                Source::Url(url) => {
                    assert_eq!(url.url, "https://x/a.png");
                    assert_eq!(url.mime_type.as_deref(), Some("image/png"));
                }
                other => panic!("expected url source, got {other:?}"),
            },
            other => panic!("expected image block, got {other:?}"),
        }
        match &msg.content()[2] {
            ContentBlock::Audio(audio) => match &audio.source {
                Source::Base64(data) => {
                    assert_eq!(data.media_type, "audio/mpeg");
                    assert_eq!(data.data, "QUJD");
                }
                other => panic!("expected base64 source, got {other:?}"),
            },
            other => panic!("expected audio block, got {other:?}"),
        }
        match &msg.content()[3] {
            ContentBlock::Video(video) => match &video.source {
                Source::Url(url) => {
                    assert_eq!(url.url, "https://x/v.mp4");
                    assert_eq!(url.mime_type, None);
                }
                other => panic!("expected url source, got {other:?}"),
            },
            other => panic!("expected video block, got {other:?}"),
        }
    }

    #[test]
    fn structured_blocks_are_rejected_for_non_user_messages() {
        let converter = converter();
        let agui = AguiMessage::new(
            "m7",
            "assistant",
            Some(MessageContent::Blocks(vec![InputContent::Text {
                text: "x".into(),
            }])),
            Vec::new(),
            None,
        );
        let err = converter.to_msg(&agui).unwrap_err();
        assert!(matches!(err, AguiError::InvalidInput(_)));
        assert!(err
            .to_string()
            .contains("only supported for AG-UI user messages"));
    }

    #[test]
    fn document_input_content_is_unhandled_like_java() {
        let converter = converter();
        let agui = AguiMessage::user_message_blocks(
            "m8",
            vec![InputContent::Document {
                source: InputContentSource::Url {
                    value: "https://x/d.pdf".into(),
                    mime_type: None,
                },
                metadata: None,
            }],
        );
        assert!(matches!(
            converter.to_msg(&agui).unwrap_err(),
            AguiError::IllegalState(_)
        ));
    }

    #[test]
    fn message_lists_convert_in_order() {
        let converter = converter();
        let msgs = converter
            .to_msg_list(&[
                AguiMessage::user_message("1", "a"),
                AguiMessage::new(
                    "2",
                    "assistant",
                    Some(MessageContent::Text("b".into())),
                    Vec::new(),
                    None,
                ),
            ])
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id(), "1");
        assert_eq!(msgs[1].role(), MsgRole::Assistant);
        let back = converter.to_agui_message_list(&msgs);
        assert_eq!(back[0].text_content(), Some("a"));
        assert_eq!(back[1].text_content(), Some("b"));
        assert!(converter.to_msg_list(&[]).unwrap().is_empty());
        assert!(converter.to_agui_message_list(&[]).is_empty());
    }

    #[test]
    fn plain_tool_resume_becomes_a_tool_result_message() {
        let converter = converter();
        let input = RunAgentInput::of("t", "r")
            .with_messages(vec![AguiMessage::user_message("m1", "hi")])
            .with_resume(vec![AguiResume::new(
                "reply-1:c1",
                AguiResume::STATUS_RESOLVED,
                Some(json!("the answer")),
            )]);
        let msgs = converter.to_input_msg_list(&input).unwrap();
        assert_eq!(msgs.len(), 2);
        let result = msgs[1].tool_result_blocks().next().unwrap();
        assert_eq!(result.id.as_deref(), Some("c1"));
        assert_eq!(result.text_content(), "the answer");
        assert_eq!(result.state, ToolResultState::Success);
        assert_eq!(msgs[1].id(), "agui-resume-reply-1:c1");
    }

    #[test]
    fn cancelled_tool_resume_is_marked_interrupted() {
        let converter = converter();
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c2",
            AguiResume::STATUS_CANCELLED,
            None,
        )]);
        let msgs = converter.to_input_msg_list(&input).unwrap();
        let result = msgs[0].tool_result_blocks().next().unwrap();
        assert_eq!(result.state, ToolResultState::Interrupted);
        assert_eq!(result.text_content(), "Interrupt cancelled by user");
    }

    #[test]
    fn resume_duplicating_an_existing_tool_result_is_skipped() {
        let converter = converter();
        let tool_msg = AguiMessage::new(
            "m2",
            "tool",
            Some(MessageContent::Text("done".into())),
            Vec::new(),
            Some("c1".into()),
        );
        let input = RunAgentInput::of("t", "r")
            .with_messages(vec![tool_msg])
            .with_resume(vec![AguiResume::new(
                "c1",
                AguiResume::STATUS_RESOLVED,
                Some(json!("dup")),
            )]);
        let msgs = converter.to_input_msg_list(&input).unwrap();
        assert_eq!(msgs.len(), 1, "duplicate resume must be skipped");
        assert_eq!(
            msgs[0].tool_result_blocks().next().unwrap().text_content(),
            "done"
        );
    }

    #[test]
    fn resume_without_a_resolvable_tool_call_id_is_skipped() {
        let converter = converter();
        // No originating interrupt and a blank interrupt id: nothing to correlate.
        let input = RunAgentInput::of("t", "r")
            .with_resume(vec![AguiResume::new("", AguiResume::STATUS_RESOLVED, None)]);
        assert!(converter.to_input_msg_list(&input).unwrap().is_empty());

        // A known interrupt whose reason is not `tool_call` and which carries no tool-call id
        // cannot be correlated to a tool call.
        let mut interrupts = HashMap::new();
        interrupts.insert(
            "i9".to_string(),
            interrupt_with_metadata("i9", None, None),
        );
        interrupts.get_mut("i9").unwrap().reason = "input_required".to_string();
        let input = RunAgentInput::of("t", "r")
            .with_resume(vec![AguiResume::new("i9", AguiResume::STATUS_RESOLVED, None)]);
        let msgs = converter
            .to_input_msg_list_with_interrupts(&input, &interrupts)
            .unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn permission_confirm_resume_produces_a_user_confirm_result() {
        let converter = converter();
        let mut metadata = Map::new();
        metadata.insert(
            METADATA_AGENTSCOPE_INTERRUPT_KIND.to_string(),
            json!(INTERRUPT_KIND_PERMISSION_CONFIRM),
        );
        metadata.insert(METADATA_TOOL_NAME.to_string(), json!("shell"));
        metadata.insert(METADATA_TOOL_INPUT.to_string(), json!({"cmd": "ls"}));
        metadata.insert(METADATA_TOOL_CONTENT.to_string(), json!(r#"{"cmd":"ls"}"#));
        let mut interrupts = HashMap::new();
        interrupts.insert(
            "reply-1:c7".to_string(),
            interrupt_with_metadata("reply-1:c7", Some("c7"), Some(metadata)),
        );

        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "reply-1:c7",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"approved": true})),
        )]);
        let msgs = converter
            .to_input_msg_list_with_interrupts(&input, &interrupts)
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role(), MsgRole::User);
        assert_eq!(msgs[0].text_content(), "approved");
        assert_eq!(msgs[0].id(), "agui-confirm-reply-1:c7");
        let confirmations = msgs[0]
            .metadata()
            .get(Msg::METADATA_CONFIRM_RESULTS)
            .and_then(Value::as_array)
            .expect("confirm results metadata");
        assert_eq!(confirmations.len(), 1);
        let result: ConfirmResult = serde_json::from_value(confirmations[0].clone()).unwrap();
        assert!(result.confirmed);
        let tool_call = result
            .tool_call
            .expect("tool call is carried for content validation");
        assert_eq!(tool_call.id, "c7");
        assert_eq!(tool_call.name, "shell");
        assert_eq!(tool_call.input.get("cmd"), Some(&json!("ls")));
        assert_eq!(tool_call.content.as_deref(), Some(r#"{"cmd":"ls"}"#));
    }

    #[test]
    fn permission_confirm_denial_and_edited_args_are_honoured() {
        let converter = converter();
        let mut metadata = Map::new();
        metadata.insert(
            METADATA_AGENTSCOPE_INTERRUPT_KIND.to_string(),
            json!(INTERRUPT_KIND_PERMISSION_CONFIRM),
        );
        metadata.insert(METADATA_TOOL_NAME.to_string(), json!("shell"));
        metadata.insert(METADATA_TOOL_INPUT.to_string(), json!({"cmd": "rm -rf /"}));
        let mut interrupts = HashMap::new();
        interrupts.insert(
            "c8".to_string(),
            interrupt_with_metadata("c8", Some("c8"), Some(metadata)),
        );

        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c8",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"approved": false, "editedArgs": {"cmd": "ls"}})),
        )]);
        let msgs = converter
            .to_input_msg_list_with_interrupts(&input, &interrupts)
            .unwrap();
        assert_eq!(msgs[0].text_content(), "denied");
        let confirmations = msgs[0]
            .metadata()
            .get(Msg::METADATA_CONFIRM_RESULTS)
            .unwrap();
        let result: ConfirmResult = serde_json::from_value(confirmations[0].clone()).unwrap();
        assert!(!result.confirmed);
        let tool_call = result.tool_call.unwrap();
        // `editedArgs` fully replaces the original input and re-serializes the content.
        assert_eq!(tool_call.input.get("cmd"), Some(&json!("ls")));
        assert_eq!(
            parse_json_arguments(tool_call.content.as_deref().unwrap()).get("cmd"),
            Some(&json!("ls"))
        );

        // `cancelled` denies even without an explicit payload.
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c8",
            AguiResume::STATUS_CANCELLED,
            None,
        )]);
        let msgs = converter
            .to_input_msg_list_with_interrupts(&input, &interrupts)
            .unwrap();
        assert_eq!(msgs[0].text_content(), "denied");
    }

    #[test]
    fn permission_confirm_with_out_originating_interrupt_metadata_yields_an_empty_tool_name() {
        let converter = converter();
        // Matches the Java behaviour: the interrupt is only recognised through its metadata,
        // so a plain `tool_call` interrupt takes the ordinary tool-result path.
        let mut interrupts = HashMap::new();
        interrupts.insert(
            "c1".to_string(),
            interrupt_with_metadata("c1", Some("c1"), None),
        );
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c1",
            AguiResume::STATUS_RESOLVED,
            Some(json!("ok")),
        )]);
        let msgs = converter
            .to_input_msg_list_with_interrupts(&input, &interrupts)
            .unwrap();
        assert_eq!(msgs[0].role(), MsgRole::Tool);
    }

    #[test]
    fn tool_call_id_is_resolved_from_interrupt_then_from_the_id_suffix() {
        let interrupt = interrupt_with_metadata("i1", Some("c1"), None);
        assert_eq!(
            resolve_tool_call_id("i1", Some(&interrupt)).as_deref(),
            Some("c1")
        );
        assert_eq!(resolve_tool_call_id("reply:c2", None).as_deref(), Some("c2"));
        assert_eq!(resolve_tool_call_id("plain", None).as_deref(), Some("plain"));
        // A trailing separator with no suffix keeps the whole id.
        assert_eq!(resolve_tool_call_id("reply:", None).as_deref(), Some("reply:"));
        assert_eq!(resolve_tool_call_id("  ", None), None);
        // A blank tool-call id on a non-tool_call interrupt falls through to `None`.
        let mut blank = interrupt_with_metadata("i2", None, None);
        blank.reason = "input_required".to_string();
        assert_eq!(resolve_tool_call_id("i2", Some(&blank)), None);
        // The interrupt tool-call id wins over the id suffix when both exist.
        let explicit = interrupt_with_metadata("reply:x", Some("c9"), None);
        assert_eq!(
            resolve_tool_call_id("reply:x", Some(&explicit)).as_deref(),
            Some("c9")
        );
    }

    #[test]
    fn resume_content_renders_strings_objects_and_empty_payloads() {
        assert_eq!(
            resume_content(&AguiResume::new(
                "i",
                AguiResume::STATUS_RESOLVED,
                Some(json!("ok"))
            )),
            "ok"
        );
        assert_eq!(
            resume_content(&AguiResume::new(
                "i",
                AguiResume::STATUS_RESOLVED,
                Some(json!({"a": 1}))
            )),
            r#"{"a":1}"#
        );
        assert_eq!(
            resume_content(&AguiResume::new("i", AguiResume::STATUS_RESOLVED, None)),
            ""
        );
    }

    #[test]
    fn cancelled_resume_reason_becomes_the_tool_result_text() {
        let converter = converter();
        // No reason: the historical default text still applies.
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c2",
            AguiResume::STATUS_CANCELLED,
            None,
        )]);
        let msgs = converter.to_input_msg_list(&input).unwrap();
        let result = msgs[0].tool_result_blocks().next().unwrap();
        assert_eq!(result.text_content(), "Interrupt cancelled by user");
        assert_eq!(result.state, ToolResultState::Interrupted);

        // Top-level `reason` is carried through to the cancelled tool result.
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c2",
            AguiResume::STATUS_CANCELLED,
            None,
        )
        .with_reason("user closed the dialog")]);
        let msgs = converter.to_input_msg_list(&input).unwrap();
        let result = msgs[0].tool_result_blocks().next().unwrap();
        assert_eq!(result.text_content(), "user closed the dialog");
        assert_eq!(result.state, ToolResultState::Interrupted);

        // `payload.reason` works too, matching the Java wire shape.
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c2",
            AguiResume::STATUS_CANCELLED,
            Some(json!({"reason": "policy blocked it"})),
        )]);
        let msgs = converter.to_input_msg_list(&input).unwrap();
        assert_eq!(
            msgs[0].tool_result_blocks().next().unwrap().text_content(),
            "policy blocked it"
        );
    }

    #[test]
    fn permission_confirm_resume_reason_reaches_the_confirm_result() {
        let converter = converter();
        let mut metadata = Map::new();
        metadata.insert(
            METADATA_AGENTSCOPE_INTERRUPT_KIND.to_string(),
            json!(INTERRUPT_KIND_PERMISSION_CONFIRM),
        );
        metadata.insert(METADATA_TOOL_NAME.to_string(), json!("shell"));
        metadata.insert(METADATA_TOOL_INPUT.to_string(), json!({"cmd": "rm -rf /"}));
        let mut interrupts = HashMap::new();
        interrupts.insert(
            "c8".to_string(),
            interrupt_with_metadata("c8", Some("c8"), Some(metadata)),
        );

        // Java `c0d03ccea`: `payload.reason` becomes `ConfirmResult.reason`, which core
        // carries as `ConfirmResult::message` and uses as the DENIED tool-result text.
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c8",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"approved": false, "reason": "too destructive"})),
        )]);
        let msgs = converter
            .to_input_msg_list_with_interrupts(&input, &interrupts)
            .unwrap();
        let confirmations = msgs[0]
            .metadata()
            .get(Msg::METADATA_CONFIRM_RESULTS)
            .unwrap();
        let result: ConfirmResult = serde_json::from_value(confirmations[0].clone()).unwrap();
        assert!(!result.confirmed);
        assert_eq!(result.message, "too destructive");

        // Without a reason the confirm result keeps an empty message so core falls back to
        // its default denial text.
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "c8",
            AguiResume::STATUS_RESOLVED,
            Some(json!({"approved": false})),
        )]);
        let msgs = converter
            .to_input_msg_list_with_interrupts(&input, &interrupts)
            .unwrap();
        let confirmations = msgs[0]
            .metadata()
            .get(Msg::METADATA_CONFIRM_RESULTS)
            .unwrap();
        let result: ConfirmResult = serde_json::from_value(confirmations[0].clone()).unwrap();
        assert!(result.message.is_empty());
    }

    #[test]
    fn tool_call_args_json_prefers_raw_streaming_content() {
        let with_content = ToolUseBlock::builder()
            .id("c")
            .name("t")
            .input(parse_json_arguments(r#"{"full":true}"#))
            .content("{\"partial\":true}")
            .build();
        assert_eq!(resolve_tool_call_args_json(&with_content), "{\"partial\":true}");
        let without_content = ToolUseBlock::builder()
            .id("c")
            .name("t")
            .input(parse_json_arguments(r#"{"full":true}"#))
            .build();
        assert_eq!(
            parse_json_arguments(&resolve_tool_call_args_json(&without_content)).get("full"),
            Some(&json!(true))
        );
    }

    #[test]
    fn resume_metadata_records_the_interrupt_id_and_status() {
        let converter = converter();
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "i1",
            AguiResume::STATUS_RESOLVED,
            Some(json!("v")),
        )]);
        let msgs = converter.to_input_msg_list(&input).unwrap();
        let result = msgs[0].tool_result_blocks().next().unwrap();
        assert_eq!(
            result.metadata.get(METADATA_INTERRUPT_ID),
            Some(&json!("i1"))
        );
        assert_eq!(
            result.metadata.get(METADATA_RESUME_STATUS),
            Some(&json!("resolved"))
        );
        assert!(result.metadata.contains_key(RESUME_PAYLOAD_APPROVED) == false);
    }
}
