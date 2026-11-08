//! `Msg` <-> AG-UI message conversion, mirroring `AguiMessageConverter`.
//!
//! The converter is stateless: every method is a pure function over its arguments, so
//! instances can be shared freely across requests.

use std::collections::{HashMap, HashSet};

use agentscope_core::{
    determine_media_type, resolve_mime_type, AudioBlock, Base64Source, ConfirmResult, ContentBlock,
    DocumentBlock, FileIdSource, ImageBlock, Msg, MsgRole, Source, TextBlock, ThinkingBlock,
    ToolResultBlock, ToolResultState, ToolUseBlock, URLSource, VideoBlock,
};
use serde_json::{Map, Value};

use crate::constants::{
    INTERRUPT_KIND_PERMISSION_CONFIRM, METADATA_AGENTSCOPE_INTERRUPT_KIND, METADATA_TOOL_CONTENT,
    METADATA_TOOL_INPUT, METADATA_TOOL_NAME, TOOL_CALL_INTERRUPT_REASON,
};
use crate::error::{AguiError, AguiResult};
use crate::event::Interrupt;
use crate::model::{
    AguiFunctionCall, AguiMessage, AguiResume, AguiToolCall, InputContent, InputContentSource,
    MessageContent, RunAgentInput,
};

/// Metadata key carried on a resume-produced `ToolResultBlock`: the originating interrupt id.
pub const METADATA_INTERRUPT_ID: &str = "agui.interruptId";

/// Metadata key carried on a resume-produced `ToolResultBlock`: the resume status.
pub const METADATA_RESUME_STATUS: &str = "agui.resumeStatus";

/// Provider name for a protocol file handle that arrived without one.
const AGUI_FILE_PROVIDER: &str = "agui";

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
    /// content the frontend also reported. A tool message may also answer through structured
    /// parts, which become the result output. A `reasoning` message becomes an assistant
    /// message carrying a `ThinkingBlock`.
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
                let mut content_blocks: Vec<ContentBlock> = parts
                    .iter()
                    .map(to_content_block)
                    .collect::<AguiResult<Vec<_>>>()?;
                // A tool message's parts are its result output; the error text, when reported,
                // comes first and turns the result into an error, exactly like the text path.
                match agui_message
                    .tool_call_id
                    .as_deref()
                    .filter(|_| agui_message.is_tool_message())
                {
                    Some(tool_call_id) => {
                        let error = agui_message
                            .error
                            .as_deref()
                            .filter(|error| !error.trim().is_empty());
                        if let Some(error) = error {
                            // The outbound tool message keeps the same text in `content` and
                            // `error`, so a client that echoes the history back reports it as a
                            // part too. Inserting it again would double the text on every round
                            // trip; the error state is what carries the meaning.
                            let duplicated = content_blocks
                                .iter()
                                .any(|block| matches!(block, ContentBlock::Text(text) if text.text == error));
                            if !duplicated {
                                content_blocks.insert(0, ContentBlock::Text(TextBlock::new(error)));
                            }
                        }
                        blocks.push(ContentBlock::ToolResult(
                            ToolResultBlock::builder()
                                .id(tool_call_id)
                                .outputs(content_blocks)
                                .state(if error.is_some() {
                                    ToolResultState::Error
                                } else {
                                    ToolResultState::Success
                                })
                                .build(),
                        ));
                    }
                    None => blocks.extend(content_blocks),
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

    /// Converts an AgentScope [`Msg`] into the standard AG-UI message.
    ///
    /// This is the last message produced by [`to_agui_messages`](Self::to_agui_messages): a
    /// message that expands into several AG-UI messages — reasoning mixed with content, or a
    /// TOOL message carrying tool results — has to go through that method, which is what
    /// [`to_agui_message_list`](Self::to_agui_message_list) uses.
    pub fn to_agui_message(&self, msg: &Msg) -> AguiMessage {
        self.to_agui_messages(msg)
            .pop()
            .expect("a message always converts to at least one AG-UI message")
    }

    /// Converts an AgentScope [`Msg`] into one or more AG-UI messages.
    ///
    /// Thinking, text, tool calls/results and media are collected in one traversal, so a message
    /// carrying reasoning next to media keeps both. A message that only reasoned becomes a single
    /// reasoning message; otherwise the reasoning (if any) is emitted first, followed by the
    /// standard message. A TOOL message carrying tool results is expanded into one AG-UI tool
    /// message per result, each keeping its own `toolCallId`, content and error. Expanded messages
    /// get a distinct, deterministic id derived from the source id (`<sourceId>-reasoning`,
    /// `<sourceId>-<index>`), so the split is always observable.
    pub fn to_agui_messages(&self, msg: &Msg) -> Vec<AguiMessage> {
        if msg.role() == MsgRole::Tool {
            let tool_results: Vec<&ToolResultBlock> = msg.tool_result_blocks().collect();
            if !tool_results.is_empty() {
                return tool_results
                    .iter()
                    .enumerate()
                    .map(|(index, result)| tool_result_message(msg, result, index))
                    .collect();
            }
        }

        let role = convert_role_out(msg.role());
        let tool_message = msg.role() == MsgRole::Tool;
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut input_contents: Vec<InputContent> = Vec::new();
        let mut tool_calls: Vec<AguiToolCall> = Vec::new();
        let mut tool_call_id: Option<String> = None;
        let mut error: Option<String> = None;

        for block in msg.content() {
            match block {
                ContentBlock::Thinking(tb) => append_content(&mut reasoning, &tb.thinking),
                ContentBlock::Text(tb) => append_content(&mut content, &tb.text),
                ContentBlock::ToolUse(tub) => tool_calls.push(to_agui_tool_call(tub)),
                ContentBlock::ToolResult(trb) => {
                    tool_call_id = trb.id.clone();
                    let text = tool_result_text(trb);
                    // `error` belongs to the protocol's tool message; the same text also stays in
                    // `content` because clients render tool results from there.
                    if tool_message && trb.state == ToolResultState::Error {
                        error = Some(text.clone());
                    }
                    append_content(&mut content, &text);
                }
                block => input_contents.extend(media_input_content(block)),
            }
        }

        // Reasoning alone has no content to carry it, so it becomes the whole message — but media
        // collected next to the reasoning must still be emitted, so any part blocks the shortcut.
        let has_content = !content.is_empty()
            || !input_contents.is_empty()
            || !tool_calls.is_empty()
            || tool_call_id.is_some();
        let mut messages = Vec::new();
        if !reasoning.is_empty() {
            let id = if has_content {
                reasoning_message_id(msg)
            } else {
                msg.id().to_string()
            };
            messages.push(AguiMessage::reasoning_message(id, reasoning));
            if !has_content {
                return messages;
            }
        }

        // The protocol lists `content` as required on a tool message and does not accept null.
        let wire_content = if input_contents.is_empty() {
            (!content.is_empty() || tool_message).then_some(MessageContent::Text(content))
        } else {
            if !content.is_empty() || tool_message {
                input_contents.push(InputContent::Text { text: content });
            }
            Some(MessageContent::Blocks(input_contents))
        };

        let mut message = AguiMessage::new(msg.id(), role, wire_content, tool_calls, tool_call_id);
        message.error = error;
        messages.push(message);
        messages
    }

    /// Converts a list of AG-UI messages into AgentScope messages.
    pub fn to_msg_list(&self, agui_messages: &[AguiMessage]) -> AguiResult<Vec<Msg>> {
        agui_messages.iter().map(|m| self.to_msg(m)).collect()
    }

    /// Converts a list of AgentScope messages into AG-UI messages.
    ///
    /// The output can be longer than the input: a message that mixes reasoning with other
    /// content, or a TOOL message carrying tool results, expands into one AG-UI message per
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
                    // An error result keeps the text in `content` too, so a payload identical to
                    // the error must not be repeated when converting back.
                    Some(text) if !text.is_empty() && text != error => format!("{error}\n{text}"),
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
/// The result text stays in `content` — clients render tool results from there — and an error
/// result repeats it in `error`. Documents keep their structured part. The first expanded message
/// keeps the source message id; later ones get an index suffix so every emitted AG-UI message has
/// a distinct id.
fn tool_result_message(msg: &Msg, result: &ToolResultBlock, index: usize) -> AguiMessage {
    let id = if index == 0 {
        msg.id().to_string()
    } else {
        format!("{}-{}", msg.id(), index)
    };
    let mut text = String::new();
    let mut parts: Vec<InputContent> = Vec::new();
    for output in &result.output {
        if let ContentBlock::Text(block) = output {
            append_content(&mut text, &block.text);
        }
        if let ContentBlock::Document(document) = output {
            parts.push(document_input_content(document));
        }
    }
    let content = if parts.is_empty() {
        MessageContent::Text(text.clone())
    } else {
        if !text.is_empty() {
            parts.push(InputContent::Text { text: text.clone() });
        }
        MessageContent::Blocks(parts)
    };
    let error = result.state == ToolResultState::Error;
    let mut message = AguiMessage::new(
        id,
        convert_role_out(msg.role()),
        Some(content),
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

/// Maps a media or document content block onto an AG-UI content part.
///
/// Only URL media is converted: an inline Base64 image/audio/video has no outbound
/// representation, mirroring the Java converter. A `DataBlock` is routed by its resolved MIME
/// type, and a document keeps whichever source the protocol can express.
fn media_input_content(block: &ContentBlock) -> Option<InputContent> {
    if let ContentBlock::Document(document) = block {
        return Some(document_input_content(document));
    }
    if let ContentBlock::Data(data) = block {
        return data_input_content(&data.source);
    }
    let media_source = match block {
        ContentBlock::Image(image) => &image.source,
        ContentBlock::Audio(audio) => &audio.source,
        ContentBlock::Video(video) => &video.source,
        _ => return None,
    };
    let Source::Url(url) = media_source else {
        return None;
    };
    let source = InputContentSource::Url {
        value: url.url.clone(),
        mime_type: url.mime_type.clone(),
    };
    Some(match block {
        ContentBlock::Image(_) => InputContent::Image {
            source,
            metadata: None,
        },
        ContentBlock::Audio(_) => InputContent::Audio {
            source,
            metadata: None,
        },
        ContentBlock::Video(_) => InputContent::Video {
            source,
            metadata: None,
        },
        _ => return None,
    })
}

/// Routes a `DataBlock` by its resolved MIME type, mirroring Java `dataBlockInputContent`.
///
/// A URL whose MIME type cannot be resolved is skipped rather than guessed.
fn data_input_content(source: &Source) -> Option<InputContent> {
    let Source::Url(url) = source else {
        return None;
    };
    let mime_type = resolve_mime_type(source).ok()?;
    let source = InputContentSource::Url {
        value: url.url.clone(),
        mime_type: Some(mime_type.clone()),
    };
    Some(if mime_type.starts_with("image/") {
        InputContent::Image {
            source,
            metadata: None,
        }
    } else if mime_type.starts_with("audio/") {
        InputContent::Audio {
            source,
            metadata: None,
        }
    } else if mime_type.starts_with("video/") {
        InputContent::Video {
            source,
            metadata: None,
        }
    } else {
        InputContent::Document {
            id: None,
            source,
            metadata: None,
        }
    })
}

/// Maps a document block onto the AG-UI content model.
///
/// Every [`Source`] variant has a protocol counterpart (URL, inline data, provider file handle),
/// so a document always travels structurally; the block id and metadata ride along.
fn document_input_content(block: &DocumentBlock) -> InputContent {
    let source = match &block.source {
        Source::Url(url) => InputContentSource::Url {
            value: url.url.clone(),
            mime_type: block.mime_type.clone().or_else(|| url.mime_type.clone()),
        },
        Source::Base64(base64) => InputContentSource::Data {
            value: base64.data.clone(),
            mime_type: block
                .mime_type
                .clone()
                .unwrap_or_else(|| base64.media_type.clone()),
        },
        Source::FileId(file_id) => InputContentSource::File {
            value: file_id.file_id.clone(),
            // The protocol's file source carries an opaque provider handle, which is exactly what
            // a file-id source is; the peer resolves it against the named provider.
            provider: Some(file_id.provider.clone()),
            mime_type: file_handle_mime_type(block),
        },
    };
    InputContent::Document {
        id: Some(block.id.clone()),
        source,
        metadata: block.metadata.clone(),
    }
}

/// Resolves the MIME type of a provider file handle: the declared one, else the file name's.
fn file_handle_mime_type(block: &DocumentBlock) -> Option<String> {
    if let Some(mime_type) = block.mime_type.as_deref().filter(|m| !m.trim().is_empty()) {
        return Some(mime_type.to_string());
    }
    block
        .name
        .as_deref()
        .map(determine_media_type)
        .filter(|mime_type| mime_type != "application/octet-stream")
}

/// The MIME type an AG-UI content source declares, if any.
fn mime_type_of(source: &InputContentSource) -> Option<String> {
    match source {
        InputContentSource::Url { mime_type, .. } | InputContentSource::File { mime_type, .. } => {
            mime_type.clone()
        }
        InputContentSource::Data { mime_type, .. } => Some(mime_type.clone()),
    }
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
        InputContent::Document {
            id,
            source,
            metadata,
        } => {
            let mut builder = DocumentBlock::builder().source(to_source(source)?);
            if let Some(id) = id {
                builder = builder.id(id.clone());
            }
            if let Some(mime_type) = mime_type_of(source) {
                builder = builder.mime_type(mime_type);
            }
            if let Some(metadata) = metadata {
                builder = builder.metadata(metadata.clone());
            }
            ContentBlock::Document(builder.build())
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
        // A handle without a provider keeps the round trip with the neutral provider name,
        // because a core file-id source requires one.
        InputContentSource::File {
            value, provider, ..
        } => Source::FileId(FileIdSource::new(
            provider
                .clone()
                .unwrap_or_else(|| AGUI_FILE_PROVIDER.to_string()),
            value.clone(),
        )),
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
        AguiFunctionCall::new(
            tub.name.clone(),
            Some(serialize_arguments(Some(&tub.input))),
        ),
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
    use crate::constants::RESUME_PAYLOAD_APPROVED;
    use agentscope_core::DataBlock;
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
            let agui = AguiMessage::new(
                "m",
                role,
                Some(MessageContent::Text("x".into())),
                Vec::new(),
                None,
            );
            assert_eq!(converter.to_msg(&agui).unwrap().role(), expected);
            assert_eq!(
                converter.to_agui_message(&Msg::text(expected, "x")).role,
                role
            );
        }
        // Unknown roles fall back to USER, mirroring the Java switch default.
        let unknown = AguiMessage::new(
            "m",
            "developer",
            Some(MessageContent::Text("x".into())),
            Vec::new(),
            None,
        );
        assert_eq!(converter.to_msg(&unknown).unwrap().role(), MsgRole::User);
        // Role matching is case-insensitive.
        let upper = AguiMessage::new(
            "m",
            "ASSISTANT",
            Some(MessageContent::Text("x".into())),
            Vec::new(),
            None,
        );
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
        // The protocol requires `content` on a tool message, so an empty result still carries it.
        assert_eq!(back.text_content(), Some(""));
        assert!(back.error.is_none());
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
        // An error result keeps its output in `content` as well, so clients can render it.
        assert_eq!(results[1].text_content(), Some("second"));
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
        // carries both rather than dropping the payload, and the same text stays in `content`.
        assert_eq!(
            round_tripped.error.as_deref(),
            Some("sandbox unavailable\npartial output")
        );
        assert_eq!(
            round_tripped.text_content(),
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
        // Identical content and error text are only included once.
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
    fn structured_blocks_on_an_assistant_message_are_read_back() {
        // Outbound messages carry parts for every role (an attachment on a `returnDirect` tool
        // delivery lands on an assistant turn), so rejecting them on the way back made an echoed
        // history fail the whole next run.
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
        let msg = converter
            .to_msg(&agui)
            .expect("assistant parts are readable");
        assert_eq!(msg.role(), MsgRole::Assistant);
        assert_eq!(msg.text_content(), "x");
    }

    fn url_source(url: &str, mime_type: Option<&str>) -> Source {
        Source::Url(URLSource {
            url: url.to_string(),
            mime_type: mime_type.map(str::to_string),
        })
    }

    fn metadata(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn first_document(msg: &Msg) -> &DocumentBlock {
        msg.content()
            .iter()
            .find_map(|block| match block {
                ContentBlock::Document(document) => Some(document),
                _ => None,
            })
            .expect("expected a document block")
    }

    fn blocks(agui: &AguiMessage) -> &[InputContent] {
        match &agui.content {
            Some(MessageContent::Blocks(parts)) => parts.as_slice(),
            other => panic!("expected structured blocks, got {other:?}"),
        }
    }

    /// The URL or provider handle a media/document part points at.
    fn url_of(part: &InputContent) -> Option<&str> {
        match part {
            InputContent::Image { source, .. }
            | InputContent::Audio { source, .. }
            | InputContent::Video { source, .. }
            | InputContent::Document { source, .. } => match source {
                InputContentSource::Url { value, .. } | InputContentSource::File { value, .. } => {
                    Some(value.as_str())
                }
                InputContentSource::Data { .. } => None,
            },
            InputContent::Text { .. } => None,
        }
    }

    /// The MIME type a media/document part declares.
    fn mime_of(part: &InputContent) -> Option<&str> {
        match part {
            InputContent::Image { source, .. }
            | InputContent::Audio { source, .. }
            | InputContent::Video { source, .. }
            | InputContent::Document { source, .. } => match source {
                InputContentSource::Url { mime_type, .. }
                | InputContentSource::File { mime_type, .. } => mime_type.as_deref(),
                InputContentSource::Data { mime_type, .. } => Some(mime_type.as_str()),
            },
            InputContent::Text { .. } => None,
        }
    }

    #[test]
    fn url_media_blocks_become_structured_parts_with_the_text_last() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-media")
            .role(MsgRole::User)
            .content(vec![
                ContentBlock::Text(TextBlock::new("Describe these")),
                ContentBlock::Image(ImageBlock::new(url_source(
                    "https://x/img.png",
                    Some("image/png"),
                ))),
                ContentBlock::Audio(AudioBlock::new(url_source(
                    "https://x/a.mp3",
                    Some("audio/mpeg"),
                ))),
                ContentBlock::Video(VideoBlock::new(url_source(
                    "https://x/v.mp4",
                    Some("video/mp4"),
                ))),
                ContentBlock::Document(
                    DocumentBlock::builder()
                        .source(url_source("https://x/doc.pdf", Some("application/pdf")))
                        .metadata(metadata(json!({"sourcePage": 3})))
                        .build(),
                ),
            ])
            .build();

        let messages = converter.to_agui_messages(&msg);

        assert_eq!(messages.len(), 1);
        let parts = blocks(&messages[0]);
        assert_eq!(parts.len(), 5);
        assert!(matches!(parts[0], InputContent::Image { .. }));
        assert_eq!(url_of(&parts[0]), Some("https://x/img.png"));
        assert_eq!(mime_of(&parts[0]), Some("image/png"));
        assert!(matches!(parts[1], InputContent::Audio { .. }));
        assert_eq!(url_of(&parts[1]), Some("https://x/a.mp3"));
        assert_eq!(mime_of(&parts[1]), Some("audio/mpeg"));
        assert!(matches!(parts[2], InputContent::Video { .. }));
        assert_eq!(url_of(&parts[2]), Some("https://x/v.mp4"));
        assert_eq!(mime_of(&parts[2]), Some("video/mp4"));
        assert!(matches!(
            &parts[3],
            InputContent::Document { metadata: Some(meta), .. }
                if meta.get("sourcePage") == Some(&json!(3))
        ));
        assert_eq!(url_of(&parts[3]), Some("https://x/doc.pdf"));
        assert_eq!(mime_of(&parts[3]), Some("application/pdf"));
        assert!(matches!(&parts[4], InputContent::Text { text } if text == "Describe these"));
    }

    #[test]
    fn inline_media_blocks_are_skipped_outbound() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-b64")
            .role(MsgRole::User)
            .content(vec![
                ContentBlock::Text(TextBlock::new("caption")),
                ContentBlock::Image(ImageBlock::new(Source::Base64(Base64Source::new(
                    "image/png",
                    "iVBORw0KGgo=",
                )))),
                ContentBlock::Audio(AudioBlock::new(Source::Base64(Base64Source::new(
                    "audio/wav",
                    "UklGRg==",
                )))),
                ContentBlock::Video(VideoBlock::new(Source::Base64(Base64Source::new(
                    "video/mp4",
                    "AAAA",
                )))),
            ])
            .build();

        let messages = converter.to_agui_messages(&msg);

        assert!(!messages[0].has_blocks());
        assert_eq!(messages[0].text_content(), Some("caption"));
    }

    #[test]
    fn a_data_block_is_routed_by_its_mime_type() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-data")
            .role(MsgRole::User)
            .content(vec![
                ContentBlock::Data(DataBlock::new(url_source(
                    "https://x/generated.png",
                    Some("image/png"),
                ))),
                ContentBlock::Data(DataBlock::new(url_source(
                    "https://x/report.pdf",
                    Some("application/pdf"),
                ))),
            ])
            .build();

        let messages = converter.to_agui_messages(&msg);
        let parts = blocks(&messages[0]);

        assert!(matches!(parts[0], InputContent::Image { .. }));
        assert_eq!(mime_of(&parts[0]), Some("image/png"));
        assert!(matches!(parts[1], InputContent::Document { .. }));
        assert_eq!(mime_of(&parts[1]), Some("application/pdf"));
    }

    #[test]
    fn reasoning_next_to_url_media_keeps_both_messages() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-reasoning-media")
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Thinking(ThinkingBlock::new("Generate the output")),
                ContentBlock::Image(ImageBlock::new(url_source(
                    "https://x/out.png",
                    Some("image/png"),
                ))),
            ])
            .build();

        let results = converter.to_agui_messages(&msg);

        assert_eq!(results.len(), 2);
        assert!(results[0].is_reasoning_message());
        assert_eq!(results[0].text_content(), Some("Generate the output"));
        assert_ne!(results[0].id, results[1].id);
        assert!(results[1].is_assistant_message());
        let parts = blocks(&results[1]);
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], InputContent::Image { .. }));

        // Reasoning alone still maps to exactly one reasoning message.
        let reasoning_only = Msg::builder()
            .id("msg-reasoning")
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Thinking(ThinkingBlock::new("only"))])
            .build();
        let results = converter.to_agui_messages(&reasoning_only);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "msg-reasoning");
    }

    #[test]
    fn a_document_travels_as_a_structured_part_and_round_trips() {
        let converter = converter();
        let block_metadata = metadata(json!({"file_parsing_strategy": "text_only"}));
        let original = Msg::builder()
            .id("msg-doc")
            .role(MsgRole::User)
            .content(vec![
                ContentBlock::Text(TextBlock::new("read this")),
                ContentBlock::Document(
                    DocumentBlock::builder()
                        .id("doc-1")
                        .name("report.pdf")
                        .mime_type("application/pdf")
                        .source(url_source("https://x/report.pdf", Some("application/pdf")))
                        .metadata(block_metadata.clone())
                        .build(),
                ),
            ])
            .build();

        let mut messages = converter.to_agui_messages(&original);
        let wire = messages.remove(0);

        let parts = blocks(&wire);
        assert_eq!(parts.len(), 2);
        assert!(matches!(
            &parts[0],
            InputContent::Document { id: Some(id), metadata: Some(meta), .. }
                if id == "doc-1" && meta.get("file_parsing_strategy") == Some(&json!("text_only"))
        ));
        assert_eq!(url_of(&parts[0]), Some("https://x/report.pdf"));
        assert!(matches!(&parts[1], InputContent::Text { text } if text == "read this"));

        let restored = converter.to_msg(&wire).unwrap();
        let document = first_document(&restored);
        assert_eq!(document.id, "doc-1");
        // AG-UI has no document display-name field.
        assert_eq!(document.name, None);
        assert_eq!(document.mime_type.as_deref(), Some("application/pdf"));
        assert_eq!(document.metadata, Some(block_metadata));
        let Source::Url(url) = &document.source else {
            panic!("expected a url source, got {:?}", document.source)
        };
        assert_eq!(url.url, "https://x/report.pdf");
        assert_eq!(url.mime_type.as_deref(), Some("application/pdf"));
    }

    #[test]
    fn a_provider_file_document_travels_as_a_file_source() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-file")
            .role(MsgRole::User)
            .content(vec![ContentBlock::Document(
                DocumentBlock::builder()
                    .name("report.pdf")
                    .source(Source::FileId(FileIdSource::new("dashscope", "file-1")))
                    .build(),
            )])
            .build();

        let messages = converter.to_agui_messages(&msg);
        let parts = blocks(&messages[0]);

        assert!(matches!(parts[0], InputContent::Document { .. }));
        assert_eq!(url_of(&parts[0]), Some("file-1"));
        assert_eq!(mime_of(&parts[0]), Some("application/pdf"));
        assert!(matches!(
            &parts[0],
            InputContent::Document {
                source: InputContentSource::File {
                    provider: Some(provider),
                    ..
                },
                ..
            } if provider == "dashscope"
        ));
    }

    #[test]
    fn a_file_source_without_a_provider_gets_the_neutral_one() {
        let converter = converter();
        let agui = AguiMessage::user_message_blocks(
            "m-file",
            vec![InputContent::Document {
                id: Some("doc-1".into()),
                source: InputContentSource::File {
                    value: "file-abc".into(),
                    provider: None,
                    mime_type: Some("application/pdf".into()),
                },
                metadata: None,
            }],
        );

        let msg = converter.to_msg(&agui).unwrap();

        let document = first_document(&msg);
        assert_eq!(document.id, "doc-1");
        assert_eq!(
            document.source,
            Source::FileId(FileIdSource::new("agui", "file-abc"))
        );
        assert_eq!(document.mime_type.as_deref(), Some("application/pdf"));
    }

    #[test]
    fn document_sources_round_trip_when_they_carry_inline_data() {
        let converter = converter();
        let agui = AguiMessage::user_message_blocks(
            "m-data",
            vec![InputContent::Document {
                id: None,
                source: InputContentSource::Data {
                    value: "dGVzdA==".into(),
                    mime_type: "application/pdf".into(),
                },
                metadata: None,
            }],
        );

        let msg = converter.to_msg(&agui).unwrap();

        let document = first_document(&msg);
        assert_eq!(document.mime_type.as_deref(), Some("application/pdf"));
        assert_eq!(
            document.source,
            Source::Base64(Base64Source::new("application/pdf", "dGVzdA=="))
        );
    }

    #[test]
    fn absent_document_fields_are_omitted_from_the_wire_json() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-no-mime")
            .role(MsgRole::User)
            .content(vec![ContentBlock::Document(
                DocumentBlock::builder()
                    .source(url_source("https://x/report", None))
                    .build(),
            )])
            .build();

        let json = serde_json::to_string(&converter.to_agui_messages(&msg)[0]).unwrap();

        assert!(!json.contains("\"mimeType\""), "{json}");
    }

    #[test]
    fn a_file_source_without_a_provider_omits_it_from_the_wire_json() {
        let part = InputContent::Document {
            id: None,
            source: InputContentSource::File {
                value: "file-abc".to_string(),
                provider: None,
                mime_type: None,
            },
            metadata: None,
        };

        let json = serde_json::to_value(part).unwrap();

        assert_eq!(json["source"]["type"], json!("file"));
        assert!(json["source"].get("provider").is_none(), "{json}");
        assert!(json["source"].get("mimeType").is_none(), "{json}");
    }

    #[test]
    fn a_tool_result_document_is_serialized_as_blocks_and_reads_back() {
        let converter = converter();
        let msg = Msg::builder()
            .id("msg-t21")
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(
                ToolResultBlock::builder()
                    .id("tc-1")
                    .outputs(vec![
                        ContentBlock::Text(TextBlock::new("generated report")),
                        ContentBlock::Document(
                            DocumentBlock::builder()
                                .id("doc-1")
                                .mime_type("application/pdf")
                                .source(url_source("https://x/report.pdf", Some("application/pdf")))
                                .build(),
                        ),
                    ])
                    .build(),
            )])
            .build();

        let mut messages = converter.to_agui_messages(&msg);
        let wire = messages.remove(0);

        assert_eq!(wire.tool_call_id.as_deref(), Some("tc-1"));
        assert!(wire.error.is_none());
        let parts = blocks(&wire);
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], InputContent::Document { .. }));
        assert!(matches!(&parts[1], InputContent::Text { .. }));

        let restored = converter.to_msg(&wire).unwrap();
        let result = restored.tool_result_blocks().next().unwrap();
        assert_eq!(result.id.as_deref(), Some("tc-1"));
        assert_eq!(result.state, ToolResultState::Success);
        assert_eq!(result.output.len(), 2);
        assert!(matches!(&result.output[0], ContentBlock::Document(_)));
        assert_eq!(result.text_content(), "generated report");
    }

    #[test]
    fn an_error_tool_result_keeps_its_output_in_content_across_repeated_round_trips() {
        let converter = converter();
        let mut msg = Msg::builder()
            .id("msg-err")
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(
                ToolResultBlock::builder()
                    .id("tc-1")
                    .outputs(vec![
                        ContentBlock::Text(TextBlock::new("sandbox unavailable")),
                        ContentBlock::Text(TextBlock::new("partial output")),
                    ])
                    .state(ToolResultState::Error)
                    .build(),
            )])
            .build();

        for _ in 0..3 {
            let mut messages = converter.to_agui_messages(&msg);
            let wire = messages.remove(0);
            assert_eq!(
                wire.text_content(),
                Some("sandbox unavailable\npartial output")
            );
            assert_eq!(wire.error.as_deref(), wire.text_content());

            msg = converter.to_msg(&wire).unwrap();
            let result = msg.tool_result_blocks().next().unwrap();
            assert_eq!(result.id.as_deref(), Some("tc-1"));
            assert_eq!(result.state, ToolResultState::Error);
            assert_eq!(result.text_content(), "sandbox unavailable\npartial output");
        }
    }

    #[test]
    fn an_error_echoed_as_a_part_is_not_duplicated() {
        // The outbound tool message keeps the same text in `content` and `error`, so a client
        // echoes it back as a part. Inserting it again would double the text on every round trip.
        let converter = converter();
        let agui = AguiMessage::new(
            "msg-t1",
            "tool",
            Some(MessageContent::Blocks(vec![
                InputContent::Text {
                    text: "sandbox unavailable".into(),
                },
                InputContent::Text {
                    text: "partial output".into(),
                },
            ])),
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
    fn an_assistant_attachment_round_trips_through_both_directions() {
        let converter = converter();
        let msg = Msg::builder()
            .id("m1")
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Document(
                DocumentBlock::builder()
                    .source(Source::Url(URLSource::new(
                        "https://example.com/report.pdf",
                    )))
                    .name("report.pdf")
                    .build(),
            )])
            .build();

        let wire = converter.to_agui_message(&msg);
        let back = converter
            .to_msg(&wire)
            .expect("the history must be readable back");
        assert_eq!(back.role(), MsgRole::Assistant);
        // AG-UI's document part carries `id`/`source`/`metadata` only, so the URL is what has to
        // survive the round trip (the display name is not part of the wire shape, same as Java).
        match &first_document(&back).source {
            Source::Url(url) => assert_eq!(url.url, "https://example.com/report.pdf"),
            other => panic!("expected url source, got {other:?}"),
        }
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
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "",
            AguiResume::STATUS_RESOLVED,
            None,
        )]);
        assert!(converter.to_input_msg_list(&input).unwrap().is_empty());

        // A known interrupt whose reason is not `tool_call` and which carries no tool-call id
        // cannot be correlated to a tool call.
        let mut interrupts = HashMap::new();
        interrupts.insert("i9".to_string(), interrupt_with_metadata("i9", None, None));
        interrupts.get_mut("i9").unwrap().reason = "input_required".to_string();
        let input = RunAgentInput::of("t", "r").with_resume(vec![AguiResume::new(
            "i9",
            AguiResume::STATUS_RESOLVED,
            None,
        )]);
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
        assert_eq!(
            resolve_tool_call_id("reply:c2", None).as_deref(),
            Some("c2")
        );
        assert_eq!(
            resolve_tool_call_id("plain", None).as_deref(),
            Some("plain")
        );
        // A trailing separator with no suffix keeps the whole id.
        assert_eq!(
            resolve_tool_call_id("reply:", None).as_deref(),
            Some("reply:")
        );
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
        assert_eq!(
            resolve_tool_call_args_json(&with_content),
            "{\"partial\":true}"
        );
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
