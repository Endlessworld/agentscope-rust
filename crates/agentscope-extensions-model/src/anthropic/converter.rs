//! Conversion between AgentScope [`Msg`]/[`ToolSchema`] and the Anthropic
//! Messages wire format.
//!
//! Merges the responsibilities of the Java formatter classes:
//!
//! | Java | Rust |
//! |---|---|
//! | `AnthropicChatFormatter` / `AnthropicBaseFormatter` | [`build_request`] / [`ChatFormatOptions`] |
//! | `AnthropicMessageConverter` | [`to_anthropic_messages`] |
//! | `AnthropicToolsHelper` | [`to_anthropic_tools`], [`to_anthropic_tools_with_server`], [`to_tool_choice`], [`apply_extra_body_params`] |
//! | `AnthropicServerToolHelper` (declaration side) | [`crate::anthropic::server_tool`] |
//! | `AnthropicMediaConverter` | [`image_content_block`] |
//! | `AnthropicConversationMerger` | [`merge_conversation`] (used by the multi-agent formatter) |
//!
//! Anthropic has three requirements the OpenAI shape does not:
//! 1. The system prompt travels in the top-level `system` field, not as a message.
//! 2. Tool results must be `user` messages, one per [`ToolResultBlock`].
//! 3. A `user`/`assistant` message may not be emitted empty, so every branch
//!    drops messages whose converted block list came out empty.

use agentscope_core::{
    convert_tool_result_to_string, determine_media_type, extract_text_content, file_to_base64,
    is_local_file, resolve_mime_type, validate_image_extension, ContentBlock, DataBlock, Error,
    GenerateOptions, HintBlock, ImageBlock, Msg, MsgRole, Result, Source, ToolChoice,
    ToolResultBlock, ToolResultState, ToolSchema, ToolUseBlock,
};

use super::dto::{
    AnthropicCacheControl, AnthropicContentBlock, AnthropicImageBlock, AnthropicImageSource,
    AnthropicMessage, AnthropicRequest, AnthropicServerToolUseBlock, AnthropicTextBlock,
    AnthropicThinkingBlock, AnthropicTool, AnthropicToolChoice, AnthropicToolEntry,
    AnthropicToolResultBlock, AnthropicToolResultContent, AnthropicToolUseBlock, SystemPrompt,
};
use super::parser::{METADATA_SERVER_TOOL_RESULT, METADATA_THINKING_SIGNATURE};
use super::server_tool::AnthropicServerTool;
use crate::common::tool_search;

/// Per-request formatting switches derived from [`GenerateOptions`].
#[derive(Debug, Clone, Default)]
pub struct ChatFormatOptions {
    /// Whether `cache_control: {"type":"ephemeral"}` markers should be emitted.
    pub cache_control: bool,
    /// Optional TTL applied to those markers (e.g. `"1h"`).
    pub cache_ttl: Option<String>,
}

impl ChatFormatOptions {
    /// Derives the switches from the effective generation options.
    pub fn from_options(options: &GenerateOptions, cache_ttl: Option<&str>) -> Self {
        Self {
            cache_control: options.cache_control == Some(true),
            cache_ttl: cache_ttl.filter(|t| !t.is_empty()).map(str::to_string),
        }
    }

    fn marker(&self) -> AnthropicCacheControl {
        AnthropicCacheControl::ephemeral(self.cache_ttl.as_deref())
    }
}

/// Maps an AgentScope role to an Anthropic role string.
///
/// Anthropic only accepts `user`/`assistant`; `system` and `tool` collapse to
/// `user` (the same mapping as `AnthropicMessageConverter#convertRole`).
pub fn role_str(role: MsgRole) -> &'static str {
    match role {
        MsgRole::Assistant => "assistant",
        MsgRole::User | MsgRole::System | MsgRole::Tool => "user",
    }
}

/// Extracts the system prompt from a leading [`MsgRole::System`] message.
///
/// Mirrors `AnthropicMessageConverter#extractSystemMessage`: only the first
/// message is inspected and only its text blocks contribute.
pub fn extract_system_message(messages: &[Msg]) -> Option<String> {
    let first = messages.first()?;
    if first.role != MsgRole::System {
        return None;
    }
    let text = first
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Wraps the system prompt into the request `system` field, marking it for
/// prompt caching when requested.
pub fn system_prompt(text: &str, fmt: &ChatFormatOptions) -> SystemPrompt {
    if fmt.cache_control {
        SystemPrompt::Blocks(vec![AnthropicTextBlock {
            text: text.to_string(),
            cache_control: Some(fmt.marker()),
        }])
    } else {
        SystemPrompt::Text(text.to_string())
    }
}

/// Converts a media [`Source`] into an Anthropic image source.
///
/// Local files are inlined as base64 (Anthropic cannot read the client
/// filesystem); remote URLs are forwarded verbatim.
pub fn image_source(source: &Source) -> Result<AnthropicImageSource> {
    match source {
        Source::Base64(b64) => Ok(AnthropicImageSource::Base64 {
            media_type: if b64.media_type.trim().is_empty() {
                "image/png".to_string()
            } else {
                b64.media_type.clone()
            },
            data: b64.data.clone(),
        }),
        Source::Url(url_source) => {
            let url = &url_source.url;
            validate_image_extension(url)?;
            if is_local_file(url) {
                let path = url.strip_prefix("file://").unwrap_or(url);
                let media_type = determine_media_type(path);
                Ok(AnthropicImageSource::Base64 {
                    media_type: if media_type == "application/octet-stream" {
                        "image/png".to_string()
                    } else {
                        media_type
                    },
                    data: file_to_base64(path)?,
                })
            } else if let Some(resolved) = crate::common::remote_media_cache().get(url) {
                // Prefetched through `RemoteMediaCache::prefetch`: inline it as
                // base64 so the provider does not have to reach the origin (and
                // works for URLs Anthropic cannot fetch itself).
                Ok(AnthropicImageSource::Base64 {
                    media_type: resolved.mime_type,
                    data: resolved.base64,
                })
            } else {
                Ok(AnthropicImageSource::Url { url: url.clone() })
            }
        }
        Source::FileId(file) => Err(Error::Other(format!(
            "provider file id '{}' carries no image bytes; a document file id is not an image source",
            file.file_id
        ))),
    }
}

/// Converts an image block, or an `[Image - processing failed: ...]` text block
/// on failure (mirroring the Java converter's fallback).
pub fn image_content_block(block: &ImageBlock, fmt: &ChatFormatOptions) -> AnthropicContentBlock {
    match image_source(&block.source) {
        Ok(source) => AnthropicContentBlock::Image(AnthropicImageBlock {
            source,
            cache_control: fmt.cache_control.then(|| fmt.marker()),
        }),
        Err(e) => {
            crate::common::diagnostics::warn(
                "unsupported_media_type",
                "content.image",
                format!("image part omitted; replaced with text: {e}"),
            );
            AnthropicContentBlock::Text(AnthropicTextBlock::new(format!(
                "[Image - processing failed: {e}]"
            )))
        }
    }
}

/// Converts a data block, routing through the image source resolver.
pub fn data_content_block(block: &DataBlock, fmt: &ChatFormatOptions) -> AnthropicContentBlock {
    // Anthropic only exposes an image content block; non-image MIME types are
    // reported as a text fallback instead of failing the whole request.
    match resolve_mime_type(&block.source) {
        Ok(mime) if mime.starts_with("image/") => match image_source(&block.source) {
            Ok(source) => AnthropicContentBlock::Image(AnthropicImageBlock {
                source,
                cache_control: fmt.cache_control.then(|| fmt.marker()),
            }),
            Err(e) => AnthropicContentBlock::Text(AnthropicTextBlock::new(format!(
                "[Media - processing failed: {e}]"
            ))),
        },
        Ok(mime) => {
            crate::common::diagnostics::warn(
                "unsupported_media_type",
                "content.data",
                format!("Anthropic only supports image DataBlocks; got MIME type: {mime}"),
            );
            AnthropicContentBlock::Text(AnthropicTextBlock::new(format!(
                "[Media - processing failed: Anthropic only supports image DataBlocks; got MIME type: {mime}]"
            )))
        }
        Err(e) => AnthropicContentBlock::Text(AnthropicTextBlock::new(format!(
            "[Media - processing failed: {e}]"
        ))),
    }
}

fn tool_use_block(block: &ToolUseBlock, fmt: &ChatFormatOptions) -> AnthropicContentBlock {
    // Server-tool calls are echoed as `server_tool_use`, not `tool_use`.
    if block.is_server_tool() {
        return AnthropicContentBlock::ServerToolUse(AnthropicServerToolUseBlock {
            id: block.id.clone(),
            name: block.name.clone(),
            input: block.input.clone(),
        });
    }
    AnthropicContentBlock::ToolUse(AnthropicToolUseBlock {
        id: block.id.clone(),
        name: block.name.clone(),
        input: block.input.clone(),
        cache_control: fmt.cache_control.then(|| fmt.marker()),
    })
}

/// Extracts the extended-thinking signature kept in the block metadata.
fn thinking_signature(thinking: &agentscope_core::ThinkingBlock) -> Option<&str> {
    thinking
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(METADATA_THINKING_SIGNATURE))
        .and_then(|value| value.as_str())
        .filter(|signature| !signature.is_empty())
}

/// Converts non-tool-result blocks of one message into Anthropic content blocks.
///
/// Returns an empty list when the message carries nothing transferable, which
/// callers use to drop the message entirely.
pub fn message_content_blocks(
    blocks: &[ContentBlock],
    fmt: &ChatFormatOptions,
) -> Vec<AnthropicContentBlock> {
    let mut out: Vec<AnthropicContentBlock> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => {
                out.push(AnthropicContentBlock::Text(AnthropicTextBlock {
                    text: t.text.clone(),
                    cache_control: fmt.cache_control.then(|| fmt.marker()),
                }));
            }
            ContentBlock::Hint(h) => {
                out.push(AnthropicContentBlock::Text(AnthropicTextBlock {
                    text: h.hint.clone().unwrap_or_default(),
                    cache_control: fmt.cache_control.then(|| fmt.marker()),
                }));
            }
            ContentBlock::Thinking(t) => match thinking_signature(t) {
                // A signed block is replayed as a `thinking` input block: Anthropic
                // requires it once the history mixes thinking with tool use
                // (MODEL-18). Java replays thinking as plain text, which loses the
                // signature, so this is a hardening addition, not an alignment gap.
                Some(signature) => out.push(AnthropicContentBlock::Thinking(
                    AnthropicThinkingBlock {
                        thinking: t.thinking.clone(),
                        signature: Some(signature.to_string()),
                    },
                )),
                None => {
                    if !t.thinking.is_empty() {
                        out.push(AnthropicContentBlock::Text(AnthropicTextBlock::new(
                            t.thinking.clone(),
                        )));
                    }
                }
            },
            ContentBlock::Image(i) => out.push(image_content_block(i, fmt)),
            ContentBlock::Data(d) => out.push(data_content_block(d, fmt)),
            ContentBlock::ToolUse(t) => out.push(tool_use_block(t, fmt)),
            // Tool results are emitted as standalone user messages.
            ContentBlock::ToolResult(_) => {}
            // Audio/video have no Anthropic request representation.
            ContentBlock::Audio(_) | ContentBlock::Video(_) => {}
            // Documents are not accepted by Anthropic: report the dropped
            // content as a text placeholder instead of collapsing the message
            // to empty (which would drop it entirely).
            ContentBlock::Document(d) => {
                let label = d.name.clone().unwrap_or_else(|| d.id.clone());
                crate::common::diagnostics::warn(
                    "document_not_supported",
                    "content.document",
                    format!(
                        "DocumentBlock is not supported by Anthropic; sending a text placeholder instead of '{label}'"
                    ),
                );
                out.push(AnthropicContentBlock::Text(AnthropicTextBlock::new(
                    format!("[Document - not supported by Anthropic: {label}]"),
                )));
            }
        }
    }
    out
}

/// Converts one tool-result block into its standalone `user` message.
pub fn tool_result_message(block: &ToolResultBlock, fmt: &ChatFormatOptions) -> AnthropicMessage {
    // A provider-executed (server) tool result is echoed back verbatim: the
    // raw wire block was captured in metadata when the response was parsed.
    if block.is_server_tool() {
        if let Some(raw) = block.metadata.get(METADATA_SERVER_TOOL_RESULT) {
            if let Ok(restored) = serde_json::from_value::<AnthropicContentBlock>(raw.clone()) {
                return AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![restored],
                };
            }
        }
    }
    let mut content: Vec<AnthropicToolResultContent> = Vec::new();
    for output in &block.output {
        match output {
            ContentBlock::Text(t) => {
                content.push(AnthropicToolResultContent::Text {
                    text: t.text.clone(),
                });
            }
            ContentBlock::Image(i) => match image_source(&i.source) {
                Ok(source) => content.push(AnthropicToolResultContent::Image { source }),
                Err(_) => {}
            },
            ContentBlock::Data(d) => match image_source(&d.source) {
                Ok(source) => content.push(AnthropicToolResultContent::Image { source }),
                Err(_) => {}
            },
            _ => {}
        }
    }
    if content.is_empty() {
        // Java emits a text block with a null payload; use the canonical
        // textual rendering of the outputs instead of an empty list (Anthropic
        // rejects an empty tool_result content).
        content.push(AnthropicToolResultContent::Text {
            text: convert_tool_result_to_string(&block.output),
        });
    }
    // A successful client-executed tool search turns every match into a
    // `tool_reference` block, which is how Anthropic loads a deferred
    // definition (`AnthropicMessageConverter#convertToolResult`). A failed
    // search must not resurrect tools, hence the state check.
    if tool_search::mode_of(&block.metadata) == Some(tool_search::ToolSearchMode::Deferred)
        && block.state != ToolResultState::Error
    {
        for tool_name in tool_search::loaded_tool_names(&block.metadata) {
            content.push(AnthropicToolResultContent::ToolReference { tool_name });
        }
    }
    AnthropicMessage {
        role: "user".to_string(),
        content: vec![AnthropicContentBlock::ToolResult(
            AnthropicToolResultBlock {
                tool_use_id: block.id.clone().unwrap_or_default(),
                content,
                // ERROR / DENIED / INTERRUPTED results are explicitly marked as
                // failures so the model can react to them (Java maps the same
                // three states to `is_error: true`).
                is_error: matches!(
                    block.state,
                    ToolResultState::Error
                        | ToolResultState::Denied
                        | ToolResultState::Interrupted
                )
                .then_some(true),
                cache_control: fmt.cache_control.then(|| fmt.marker()),
            },
        )],
    }
}

/// Converts a [`Msg`] list into Anthropic messages.
///
/// Mirrors `AnthropicMessageConverter#convert`: a `system` message is rendered as
/// a `user` message (Anthropic has no `system` role inside `messages`). The
/// *leading* system prompt is normally peeled off by [`build_request`] and sent
/// through the top-level `system` field instead; this function converts whatever
/// it is given. Messages that convert to no blocks are dropped (Anthropic rejects
/// empty content).
pub fn to_anthropic_messages(messages: &[Msg], fmt: &ChatFormatOptions) -> Vec<AnthropicMessage> {
    let mut out: Vec<AnthropicMessage> = Vec::new();
    let mut index = 0usize;
    while index < messages.len() {
        let msg = &messages[index];

        // Parallel client tool calls: Anthropic requires a strict
        // assistant(tool_use) -> user(tool_result) alternation, so an assistant
        // message with several tool uses is expanded into one
        // assistant/user pair per call (`AnthropicMessageConverter#convert`).
        if msg.role == MsgRole::Assistant {
            let tool_uses = client_tool_uses(msg);
            if tool_uses.len() > 1 {
                let split = collect_split_tool_results(messages, index + 1);
                if should_split_parallel_tool_calls(&tool_uses, &split) {
                    out.extend(convert_parallel_tool_calls(msg, &tool_uses, &split, fmt));
                    index += split.consumed + 1;
                    continue;
                }
            }
        }

        let (tool_results, regular): (Vec<ContentBlock>, Vec<ContentBlock>) = msg
            .content
            .iter()
            .cloned()
            .partition(|b| matches!(b, ContentBlock::ToolResult(_)));

        if !regular.is_empty() {
            let blocks = message_content_blocks(&regular, fmt);
            if !blocks.is_empty() {
                out.push(AnthropicMessage {
                    role: role_str(msg.role).to_string(),
                    content: blocks,
                });
            }
        }

        for block in tool_results {
            if let ContentBlock::ToolResult(tr) = block {
                out.push(tool_result_message(&tr, fmt));
            }
        }
        index += 1;
    }
    out
}

/// The consecutive pure tool-result messages that follow an assistant message.
struct SplitToolResults {
    by_id: std::collections::BTreeMap<String, ToolResultBlock>,
    consumed: usize,
}

/// Client-executed tool calls (server tools are executed by the provider).
fn client_tool_uses(msg: &Msg) -> Vec<&ToolUseBlock> {
    msg.content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse(tool) if !tool.is_server_tool() => Some(tool),
            _ => None,
        })
        .collect()
}

/// Collects the consecutive messages made up **entirely** of tool results.
///
/// The scan stops at the first message with non-tool-result content, because
/// such a message must stay in its original position instead of being consumed
/// into the split sequence.
fn collect_split_tool_results(messages: &[Msg], start: usize) -> SplitToolResults {
    let mut by_id = std::collections::BTreeMap::new();
    let mut consumed = 0usize;
    for msg in &messages[start..] {
        let results: Vec<&ToolResultBlock> = msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        if results.is_empty() || results.len() != msg.content.len() {
            break;
        }
        consumed += 1;
        for result in results {
            if let Some(id) = result.id.as_ref().filter(|id| !id.is_empty()) {
                by_id.entry(id.clone()).or_insert_with(|| result.clone());
            }
        }
    }
    SplitToolResults { by_id, consumed }
}

/// Whether the assistant message should be expanded into the anthropic
/// tool-use/tool-result alternation (Java `shouldSplitParallelToolCalls`).
fn should_split_parallel_tool_calls(
    tool_uses: &[&ToolUseBlock],
    split: &SplitToolResults,
) -> bool {
    if split.consumed == 0 || tool_uses.len() <= 1 {
        return false;
    }
    if tool_uses.iter().any(|tool| tool.id.trim().is_empty()) {
        return false;
    }
    split.by_id.len() == tool_uses.len()
        && tool_uses
            .iter()
            .all(|tool| split.by_id.contains_key(&tool.id))
}

/// Expands one assistant message with parallel tool uses into alternating
/// messages; leading content is attached only to the first assistant message so
/// it is not duplicated.
fn convert_parallel_tool_calls(
    assistant: &Msg,
    tool_uses: &[&ToolUseBlock],
    split: &SplitToolResults,
    fmt: &ChatFormatOptions,
) -> Vec<AnthropicMessage> {
    let leading: Vec<ContentBlock> = assistant
        .content
        .iter()
        .filter(|block| match block {
            ContentBlock::ToolUse(tool) => tool.is_server_tool(),
            _ => true,
        })
        .cloned()
        .collect();

    let mut out = Vec::new();
    for (index, tool_use) in tool_uses.iter().enumerate() {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if index == 0 {
            blocks.extend(leading.iter().cloned());
        }
        blocks.push(ContentBlock::ToolUse((*tool_use).clone()));
        let converted = message_content_blocks(&blocks, fmt);
        if !converted.is_empty() {
            out.push(AnthropicMessage {
                role: role_str(assistant.role).to_string(),
                content: converted,
            });
        }
        if let Some(result) = split.by_id.get(&tool_use.id) {
            out.push(tool_result_message(result, fmt));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Converts tool schemas into Anthropic tool declarations. When prompt caching
/// is on, the **last eager** (non-deferred) tool carries the marker so the whole
/// tool prefix is cached (Anthropic caches everything up to and including the
/// marked block). Deferred definitions cannot carry `cache_control`
/// (`AnthropicToolsHelper#applyTools` skips them).
pub fn to_anthropic_tools(tools: &[ToolSchema], fmt: &ChatFormatOptions) -> Vec<AnthropicTool> {
    let last_eager = tools
        .iter()
        .rposition(|schema| schema.defer_loading != Some(true));
    tools
        .iter()
        .enumerate()
        .map(|(index, schema)| AnthropicTool {
            name: schema.name.clone(),
            description: schema.description.clone(),
            input_schema: serde_json::Value::Object(schema.parameters.clone()),
            defer_loading: schema.defer_loading,
            cache_control: (fmt.cache_control && Some(index) == last_eager)
                .then(|| fmt.marker()),
        })
        .collect()
}

/// Builds the Anthropic `tool_choice` object, translating AgentScope's
/// [`ToolChoice`] into Anthropic's `auto`/`none`/`any`/`tool` variants.
///
/// `parallel_tool_calls` has no top-level field in the Anthropic API: it is
/// expressed as `disable_parallel_tool_use = !parallel_tool_calls` nested in the
/// choice object. `none` has no such sub-field, so the flag is dropped there
/// (matching the Java helper's documented behaviour).
pub fn to_tool_choice(
    choice: &ToolChoice,
    parallel_tool_calls: Option<bool>,
) -> AnthropicToolChoice {
    let disable = parallel_tool_calls.map(|v| !v);
    match choice {
        ToolChoice::Auto => AnthropicToolChoice::Auto {
            disable_parallel_tool_use: disable,
        },
        ToolChoice::None => AnthropicToolChoice::None,
        // Anthropic has no "required"; `any` forces a tool call.
        ToolChoice::Required => AnthropicToolChoice::Any {
            disable_parallel_tool_use: disable,
        },
        ToolChoice::Specific { tool_name } => AnthropicToolChoice::Tool {
            name: tool_name.clone(),
            disable_parallel_tool_use: disable,
        },
    }
}

/// Merges extra headers declared on [`GenerateOptions`] into the request spec.
pub fn extra_headers(options: &[&GenerateOptions]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for opts in options {
        for (name, value) in &opts.additional_headers {
            out.retain(|(n, _)| n != name);
            out.push((name.clone(), value.clone()));
        }
    }
    out
}

/// Merges extra body parameters declared on [`GenerateOptions`] into the
/// request body (top-level keys), later option sets winning.
pub fn apply_extra_body_params(body: &mut serde_json::Value, options: &[&GenerateOptions]) {
    let Some(map) = body.as_object_mut() else {
        return;
    };
    for opts in options {
        for (key, value) in &opts.additional_body_params {
            map.insert(key.clone(), value.clone());
        }
    }
}

/// Merges client tools and Anthropic server tools into one `tools` array,
/// mirroring `AnthropicToolsHelper#applyTools(builder, tools, serverTools, ...)`.
///
/// Server tools are declared first (Java adds them before converting client
/// tools); when prompt caching is on, the **last client tool** carries the cache
/// marker so the whole tool prefix is cached. Tool names must be unique across
/// both sets, because Anthropic selects tools by name.
pub fn to_anthropic_tools_with_server(
    tools: &[ToolSchema],
    server_tools: &[AnthropicServerTool],
    fmt: &ChatFormatOptions,
) -> Result<Vec<AnthropicToolEntry>> {
    let mut names: Vec<&str> = Vec::with_capacity(tools.len() + server_tools.len());
    for tool in tools {
        if names.contains(&tool.name.as_str()) {
            return Err(duplicate_tool_name(&tool.name));
        }
        names.push(&tool.name);
    }
    for tool in server_tools {
        if names.contains(&tool.name()) {
            return Err(duplicate_tool_name(tool.name()));
        }
        names.push(tool.name());
    }

    let mut entries: Vec<AnthropicToolEntry> = server_tools
        .iter()
        .map(|tool| AnthropicToolEntry::Server(tool.definition().clone()))
        .collect();
    entries.extend(
        to_anthropic_tools(tools, fmt)
            .into_iter()
            .map(AnthropicToolEntry::Client),
    );
    Ok(entries)
}

fn duplicate_tool_name(name: &str) -> Error {
    Error::Model(format!(
        "Anthropic tool names must be unique; duplicate tool name: {name}"
    ))
}

/// Assembles the full Anthropic request body.
///
/// `options` overrides `default_options`, which in turn supplies the fallbacks
/// for temperature / top_p / top_k / max_tokens. Additional body params from
/// both option sets are merged last so callers can override anything.
pub fn build_request(
    model: &str,
    messages: &[Msg],
    tools: &[ToolSchema],
    options: &GenerateOptions,
    default_options: &GenerateOptions,
    fmt: &ChatFormatOptions,
    stream: bool,
) -> AnthropicRequest {
    let tools_field = (!tools.is_empty()).then(|| {
        to_anthropic_tools(tools, fmt)
            .into_iter()
            .map(AnthropicToolEntry::Client)
            .collect()
    });
    assemble_request(
        model,
        messages,
        tools_field,
        options,
        default_options,
        fmt,
        stream,
    )
}

/// [`build_request`] variant that also declares Anthropic built-in server tools.
pub fn build_request_with_server_tools(
    model: &str,
    messages: &[Msg],
    tools: &[ToolSchema],
    server_tools: &[AnthropicServerTool],
    options: &GenerateOptions,
    default_options: &GenerateOptions,
    fmt: &ChatFormatOptions,
    stream: bool,
) -> Result<AnthropicRequest> {
    let tools_field = if tools.is_empty() && server_tools.is_empty() {
        None
    } else {
        Some(to_anthropic_tools_with_server(tools, server_tools, fmt)?)
    };
    Ok(assemble_request(
        model,
        messages,
        tools_field,
        options,
        default_options,
        fmt,
        stream,
    ))
}

fn assemble_request(
    model: &str,
    messages: &[Msg],
    tools_field: Option<Vec<AnthropicToolEntry>>,
    options: &GenerateOptions,
    default_options: &GenerateOptions,
    fmt: &ChatFormatOptions,
    stream: bool,
) -> AnthropicRequest {
    let pick_f64 =
        |f: fn(&GenerateOptions) -> Option<f64>| f(options).or_else(|| f(default_options));
    let pick_u32 =
        |f: fn(&GenerateOptions) -> Option<u32>| f(options).or_else(|| f(default_options));

    // The leading system prompt is sent via the top-level `system` field, so it
    // is excluded from the conversation body (mirrors the Java model, which
    // slices it off before formatting).
    let system_text = extract_system_message(messages);
    let conversation = match (messages.first(), system_text.as_ref()) {
        (Some(first), Some(_)) if first.role == MsgRole::System => &messages[1..],
        _ => messages,
    };
    let anthropic_messages = to_anthropic_messages(conversation, fmt);

    let tool_choice = options
        .tool_choice
        .as_ref()
        .or(default_options.tool_choice.as_ref())
        .map(|c| {
            let parallel = options
                .parallel_tool_calls
                .or(default_options.parallel_tool_calls);
            to_tool_choice(c, parallel)
        })
        .or_else(|| {
            // No explicit choice but a parallel-tool-calls override: Anthropic
            // requires an implicit `auto` choice to carry the flag.
            let parallel = options
                .parallel_tool_calls
                .or(default_options.parallel_tool_calls);
            parallel.map(|p| AnthropicToolChoice::Auto {
                disable_parallel_tool_use: Some(!p),
            })
        });

    let thinking = pick_u32(|o| o.thinking_budget).map(super::dto::AnthropicThinking::enabled);

    AnthropicRequest {
        model: model.to_string(),
        max_tokens: pick_u32(|o| o.max_tokens).unwrap_or(super::credential::DEFAULT_MAX_TOKENS),
        messages: anthropic_messages,
        stream: stream.then_some(true),
        system: system_text.as_deref().map(|text| system_prompt(text, fmt)),
        tools: tools_field,
        tool_choice,
        temperature: pick_f64(|o| o.temperature),
        top_p: pick_f64(|o| o.top_p),
        top_k: pick_u32(|o| o.top_k),
        thinking,
    }
}

/// Adds `cache_control` to the last cacheable block of the last message.
///
/// Mirrors `AnthropicBaseFormatter#applyCacheControl`: anthropic caches the
/// whole prefix up to and including the marked block, so this caches the
/// conversation prefix across turns. Only text/image/tool_use/tool_result
/// blocks accept the marker.
pub fn apply_cache_control(messages: &mut [AnthropicMessage], fmt: &ChatFormatOptions) {
    if !fmt.cache_control {
        return;
    }
    let Some(last) = messages.last_mut() else {
        return;
    };
    let marker = fmt.marker();
    for block in last.content.iter_mut().rev() {
        match block {
            AnthropicContentBlock::Text(t) => {
                t.cache_control = Some(marker);
                return;
            }
            AnthropicContentBlock::Image(i) => {
                i.cache_control = Some(marker);
                return;
            }
            AnthropicContentBlock::ToolUse(t) => {
                t.cache_control = Some(marker);
                return;
            }
            AnthropicContentBlock::ToolResult(t) => {
                t.cache_control = Some(marker);
                return;
            }
            // Provider-executed blocks carry no cache marker, but keep scanning:
            // an earlier cacheable block may still precede them.
            AnthropicContentBlock::Thinking(_)
            | AnthropicContentBlock::ServerToolUse(_)
            | AnthropicContentBlock::WebSearchToolResult(_)
            | AnthropicContentBlock::WebFetchToolResult(_)
            | AnthropicContentBlock::CodeExecutionToolResult(_)
            | AnthropicContentBlock::BashCodeExecutionToolResult(_)
            | AnthropicContentBlock::TextEditorCodeExecutionToolResult(_)
            | AnthropicContentBlock::ToolSearchToolResult(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-agent conversation merging
// ---------------------------------------------------------------------------

/// A merged conversation element: either accumulated text or a media block.
#[derive(Debug, Clone, PartialEq)]
pub enum ConversationBlock {
    /// A run of accumulated `"<name>: <text>"` lines.
    Text(String),
    /// A media block to be converted later.
    Image(ImageBlock),
    /// A generic data block to be converted later.
    Data(DataBlock),
}

/// Merges multi-agent messages into a conversation transcript wrapped in
/// `<history>` tags, mirroring `AnthropicConversationMerger`.
///
/// Text and hint blocks accumulate into `"<name>: <content>"` lines; images and
/// data blocks flush the accumulated text and appear in place as their own
/// elements. Thinking blocks are in-memory only and are not merged.
pub fn merge_conversation(
    messages: &[Msg],
    conversation_history_prompt: &str,
) -> Vec<ConversationBlock> {
    let mut blocks: Vec<ConversationBlock> = Vec::new();
    let mut text: Vec<String> = Vec::new();

    let flush = |text: &mut Vec<String>, blocks: &mut Vec<ConversationBlock>| {
        if !text.is_empty() {
            blocks.push(ConversationBlock::Text(text.join("\n")));
            text.clear();
        }
    };

    for msg in messages {
        let name = msg.name.clone().unwrap_or_else(|| role_label(msg.role));
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => text.push(format!("{name}: {}", t.text)),
                ContentBlock::Hint(HintBlock { hint, .. }) => {
                    text.push(format!("{name}: {}", hint.clone().unwrap_or_default()))
                }
                ContentBlock::Image(i) => {
                    flush(&mut text, &mut blocks);
                    blocks.push(ConversationBlock::Image(i.clone()));
                }
                ContentBlock::Data(d) => {
                    flush(&mut text, &mut blocks);
                    blocks.push(ConversationBlock::Data(d.clone()));
                }
                _ => {}
            }
        }
    }
    flush(&mut text, &mut blocks);

    // Wrap with the history tags.
    if blocks.is_empty() {
        return blocks;
    }
    match blocks.first_mut() {
        Some(ConversationBlock::Text(first)) => {
            *first = format!("{conversation_history_prompt}<history>\n{first}");
        }
        _ => blocks.insert(
            0,
            ConversationBlock::Text(format!("{conversation_history_prompt}<history>\n")),
        ),
    }
    match blocks.last_mut() {
        Some(ConversationBlock::Text(last)) => last.push_str("\n</history>"),
        _ => blocks.push(ConversationBlock::Text("</history>".to_string())),
    }
    blocks
}

/// Full first name fallback: the message name, else the role's enum name.
fn role_label(role: MsgRole) -> String {
    match role {
        MsgRole::User => "USER",
        MsgRole::Assistant => "ASSISTANT",
        MsgRole::System => "SYSTEM",
        MsgRole::Tool => "TOOL",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Multi-agent formatting
// ---------------------------------------------------------------------------

/// Default conversation-history preamble (identical to the Java constant).
pub const DEFAULT_CONVERSATION_HISTORY_PROMPT: &str = "# Conversation History\nThe content between <history></history> tags contains your conversation history\n";

/// Message group kind used by the multi-agent formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupType {
    System,
    ToolSequence,
    AgentConversation,
}

/// Converts messages with multi-agent support, mirroring
/// `AnthropicMultiAgentFormatter#doFormat`: system messages and tool sequences
/// keep their normal conversion, while runs of plain agent messages are merged
/// into a single `user` message carrying `<history>` tags.
pub fn to_multi_agent_messages(
    messages: &[Msg],
    conversation_history_prompt: &str,
    fmt: &ChatFormatOptions,
) -> Vec<AnthropicMessage> {
    let mut result: Vec<AnthropicMessage> = Vec::new();
    let mut is_first_agent_group = true;
    let mut current: Vec<Msg> = Vec::new();
    let mut current_type: Option<GroupType> = None;

    let flush = |group_type: GroupType,
                 group: &[Msg],
                 is_first: &mut bool,
                 out: &mut Vec<AnthropicMessage>| {
        match group_type {
            GroupType::System => {
                if let Some(msg) = group.first() {
                    for m in to_anthropic_messages(std::slice::from_ref(msg), fmt) {
                        out.push(m);
                    }
                }
            }
            GroupType::ToolSequence => {
                out.extend(to_anthropic_messages(group, fmt));
            }
            GroupType::AgentConversation => {
                let prompt = if *is_first {
                    conversation_history_prompt
                } else {
                    ""
                };
                *is_first = false;
                if let Some(message) = merged_agent_message(group, prompt, fmt) {
                    out.push(message);
                }
            }
        }
    };

    for msg in messages {
        let msg_type = determine_group_type(msg);
        if msg_type == GroupType::System {
            if !current.is_empty() {
                flush(
                    current_type.unwrap_or(GroupType::AgentConversation),
                    &current,
                    &mut is_first_agent_group,
                    &mut result,
                );
                current.clear();
            }
            flush(
                GroupType::System,
                std::slice::from_ref(msg),
                &mut is_first_agent_group,
                &mut result,
            );
            current_type = None;
        } else if Some(msg_type) != current_type && !current.is_empty() {
            flush(
                current_type.unwrap_or(GroupType::AgentConversation),
                &current,
                &mut is_first_agent_group,
                &mut result,
            );
            current.clear();
            current_type = Some(msg_type);
            current.push(msg.clone());
        } else {
            current_type = Some(msg_type);
            current.push(msg.clone());
        }
    }
    if !current.is_empty() {
        flush(
            current_type.unwrap_or(GroupType::AgentConversation),
            &current,
            &mut is_first_agent_group,
            &mut result,
        );
    }
    result
}

fn determine_group_type(msg: &Msg) -> GroupType {
    if msg.role == MsgRole::System
        && !msg.content.is_empty()
        && !matches!(msg.content.first(), Some(ContentBlock::ToolResult(_)))
    {
        return GroupType::System;
    }
    let has_tool_use = msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse(_)));
    let has_tool_result = msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult(_)));
    if has_tool_use || has_tool_result {
        GroupType::ToolSequence
    } else {
        GroupType::AgentConversation
    }
}

/// Builds the single merged `user` message for an agent-conversation group.
fn merged_agent_message(
    group: &[Msg],
    prompt: &str,
    fmt: &ChatFormatOptions,
) -> Option<AnthropicMessage> {
    let merged = merge_conversation(group, prompt);
    let blocks: Vec<AnthropicContentBlock> = merged
        .into_iter()
        .map(|block| match block {
            ConversationBlock::Text(text) => {
                AnthropicContentBlock::Text(AnthropicTextBlock::new(text))
            }
            ConversationBlock::Image(image) => image_content_block(&image, fmt),
            ConversationBlock::Data(data) => data_content_block(&data, fmt),
        })
        .collect();
    if blocks.is_empty() {
        return None;
    }
    Some(AnthropicMessage {
        role: "user".to_string(),
        content: blocks,
    })
}

/// Text-only view of a message (used by `extract_text_content` consumers).
pub fn text_of(msg: &Msg) -> String {
    extract_text_content(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{
        Base64Source, HintBlock, JsonMap, TextBlock, ThinkingBlock, ToolCallState, URLSource,
    };

    fn plain() -> ChatFormatOptions {
        ChatFormatOptions::default()
    }

    fn tool_schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.to_string(),
            description: format!("{name} desc"),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string" } }
            })
            .as_object()
            .unwrap()
            .clone(),
            output_schema: None,
            strict: None,
            defer_loading: None,
        }
    }

    #[test]
    fn leading_system_message_becomes_a_user_message_in_conversion() {
        // `to_anthropic_messages` mirrors the Java converter, which renders a
        // system message as `user`; the model peels it off before calling this.
        let out = to_anthropic_messages(&[Msg::system("be brief"), Msg::user("hi")], &plain());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "user");
        match &out[0].content[0] {
            AnthropicContentBlock::Text(t) => assert_eq!(t.text, "be brief"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn system_message_is_extracted_not_sent_as_message() {
        let messages = vec![Msg::system("be brief"), Msg::user("hi")];
        let request = build_request(
            "claude-sonnet-4-5-20250929",
            &messages,
            &[],
            &GenerateOptions::default(),
            &GenerateOptions::default(),
            &plain(),
            false,
        );
        assert_eq!(request.system, Some(SystemPrompt::Text("be brief".into())));
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, "user");
    }

    #[test]
    fn cached_system_prompt_uses_text_blocks() {
        let messages = vec![Msg::system("be brief")];
        let fmt = ChatFormatOptions {
            cache_control: true,
            cache_ttl: Some("1h".into()),
        };
        let request = build_request(
            "m",
            &messages,
            &[],
            &GenerateOptions::default(),
            &GenerateOptions::default(),
            &fmt,
            false,
        );
        match request.system.unwrap() {
            SystemPrompt::Blocks(blocks) => {
                assert_eq!(blocks[0].text, "be brief");
                let marker = blocks[0].cache_control.clone().unwrap();
                assert_eq!(marker.kind, "ephemeral");
                assert_eq!(marker.ttl.as_deref(), Some("1h"));
            }
            other => panic!("expected blocks, got {other:?}"),
        }
    }

    #[test]
    fn tool_results_become_standalone_user_messages() {
        let messages = vec![
            Msg::assistant_blocks(vec![
                ContentBlock::Text(TextBlock::new("checking")),
                ContentBlock::ToolUse(ToolUseBlock::new("toolu_1", "get_weather", JsonMap::new())),
            ]),
            Msg::tool("toolu_1", "get_weather", "sunny"),
        ];
        let out = to_anthropic_messages(&messages, &plain());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "assistant");
        assert_eq!(out[1].role, "user");
        match &out[1].content[0] {
            AnthropicContentBlock::ToolResult(tr) => {
                assert_eq!(tr.tool_use_id, "toolu_1");
                assert_eq!(
                    tr.content,
                    vec![AnthropicToolResultContent::Text {
                        text: "sunny".into()
                    }]
                );
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn failed_tool_results_are_marked_is_error() {
        // Java `AnthropicMessageConverter` maps ERROR / DENIED / INTERRUPTED to
        // `is_error: true` so the model can react to the failure, while a
        // successful result stays unmarked.
        for (state, expected) in [
            (ToolResultState::Error, Some(true)),
            (ToolResultState::Denied, Some(true)),
            (ToolResultState::Interrupted, Some(true)),
            (ToolResultState::Success, None),
        ] {
            let mut block = ToolResultBlock::new(
                "toolu_1",
                "get_weather",
                vec![ContentBlock::Text(TextBlock::new("boom"))],
            );
            block.state = state;
            let message = tool_result_message(&block, &plain());
            match &message.content[0] {
                AnthropicContentBlock::ToolResult(tr) => {
                    assert_eq!(tr.is_error, expected, "state={state:?}");
                    assert_eq!(tr.tool_use_id, "toolu_1");
                    assert_eq!(
                        tr.content,
                        vec![AnthropicToolResultContent::Text {
                            text: "boom".into()
                        }]
                    );
                }
                other => panic!("expected tool result, got {other:?}"),
            }
        }
    }

    #[test]
    fn parallel_tool_uses_split_into_alternating_pairs() {
        // Anthropic rejects an assistant message whose tool_use blocks are not
        // answered one-by-one, so two calls become assistant/user pairs (Java
        // `convertParallelToolCalls`).
        let messages = vec![
            Msg::assistant_blocks(vec![
                ContentBlock::Text(TextBlock::new("checking")),
                ContentBlock::ToolUse(ToolUseBlock::new("t1", "a", JsonMap::new())),
                ContentBlock::ToolUse(ToolUseBlock::new("t2", "b", JsonMap::new())),
            ]),
            Msg::tool("t1", "a", "ra"),
            Msg::tool("t2", "b", "rb"),
        ];
        let out = to_anthropic_messages(&messages, &plain());
        assert_eq!(
            out.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
            vec!["assistant", "user", "assistant", "user"]
        );
        // The leading text is attached once, to the first assistant message.
        let first: Vec<&str> = out[0]
            .content
            .iter()
            .filter_map(|b| match b {
                AnthropicContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(first, vec!["checking"]);
        assert!(out[1].content.iter().all(|b| matches!(
            b,
            AnthropicContentBlock::ToolResult(tr) if tr.tool_use_id == "t1"
        )));
    }

    #[test]
    fn parallel_tool_uses_stay_unsplit_when_a_result_is_missing() {
        let messages = vec![
            Msg::assistant_blocks(vec![
                ContentBlock::ToolUse(ToolUseBlock::new("t1", "a", JsonMap::new())),
                ContentBlock::ToolUse(ToolUseBlock::new("t2", "b", JsonMap::new())),
            ]),
            Msg::tool("t1", "a", "ra"),
        ];
        let out = to_anthropic_messages(&messages, &plain());
        // No split: the assistant message is emitted as one message with both
        // calls, followed by the single result.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content.len(), 2);
    }

    #[test]
    fn deferred_search_results_become_tool_references() {
        let mut metadata = JsonMap::new();
        metadata.insert(
            tool_search::METADATA_MODE.to_string(),
            serde_json::json!("DEFERRED"),
        );
        metadata.insert(
            tool_search::METADATA_TOOLS.to_string(),
            serde_json::json!([{"name": "get_weather"}]),
        );
        let mut block = ToolResultBlock::new(
            "tu_1",
            "tool_search",
            vec![ContentBlock::Text(TextBlock::new("found 1 tool"))],
        );
        block.metadata = metadata.clone();
        let message = tool_result_message(&block, &plain());
        match &message.content[0] {
            AnthropicContentBlock::ToolResult(tr) => {
                assert_eq!(tr.content.len(), 2);
                assert!(tr.content.iter().any(|c| matches!(
                    c,
                    AnthropicToolResultContent::ToolReference { tool_name }
                        if tool_name == "get_weather"
                )));
            }
            other => panic!("expected tool result, got {other:?}"),
        }

        // A failed search must not resurrect the deferred tools.
        block.state = ToolResultState::Error;
        let message = tool_result_message(&block, &plain());
        match &message.content[0] {
            AnthropicContentBlock::ToolResult(tr) => assert_eq!(tr.content.len(), 1),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn cache_marker_skips_deferred_tool_definitions() {
        let fmt = ChatFormatOptions {
            cache_control: true,
            cache_ttl: None,
        };
        let mut deferred = tool_schema("searchable");
        deferred.defer_loading = Some(true);
        let converted = to_anthropic_tools(&[tool_schema("eager"), deferred], &fmt);
        assert!(converted[0].cache_control.is_some());
        assert!(converted[1].cache_control.is_none());
        assert_eq!(converted[1].defer_loading, Some(true));
    }

    #[test]
    fn empty_message_is_dropped() {
        // An empty-content message yields no transferable block and must not
        // produce an empty-content request (Anthropic rejects one).
        let messages = vec![Msg::assistant_blocks(vec![])];
        assert!(to_anthropic_messages(&messages, &plain()).is_empty());
    }

    #[test]
    fn image_falls_back_to_text_on_unsupported_extension() {
        let block = ImageBlock::new(Source::Url(URLSource::new("https://x/a.txt")));
        match image_content_block(&block, &plain()) {
            AnthropicContentBlock::Text(t) => {
                assert!(t.text.starts_with("[Image - processing failed:"))
            }
            other => panic!("expected text fallback, got {other:?}"),
        }
    }

    #[test]
    fn prefetched_remote_image_is_inlined_instead_of_passed_through() {
        let url = "https://example.invalid/anthropic-prefetched.png";
        crate::common::remote_media_cache().insert(
            url,
            crate::common::ResolvedMedia {
                base64: "QUJD".to_string(),
                mime_type: "image/png".to_string(),
            },
        );
        let source = image_source(&Source::Url(URLSource::new(url))).unwrap();
        match source {
            AnthropicImageSource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "QUJD");
            }
            other => panic!("expected inlined base64, got {other:?}"),
        }

        // Without a cache entry the URL is passed through as before.
        let source = image_source(&Source::Url(URLSource::new(
            "https://example.invalid/anthropic-unfetchable.png",
        )))
        .unwrap();
        assert!(matches!(source, AnthropicImageSource::Url { .. }));
    }

    #[test]
    fn base64_image_maps_to_base64_source() {
        let block = ImageBlock::new(Source::Base64(Base64Source::new("image/png", "AAAA")));
        match image_content_block(&block, &plain()) {
            AnthropicContentBlock::Image(i) => {
                assert_eq!(
                    i.source,
                    AnthropicImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into()
                    }
                );
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn data_block_non_image_mime_falls_back_to_text() {
        let block = DataBlock::new(Source::Base64(Base64Source::new("application/pdf", "AAAA")));
        match data_content_block(&block, &plain()) {
            AnthropicContentBlock::Text(t) => assert!(t.text.contains("only supports image")),
            other => panic!("expected text fallback, got {other:?}"),
        }
    }

    #[test]
    fn tools_are_converted_and_last_one_cached() {
        let tools = vec![tool_schema("a"), tool_schema("b")];
        let plain_tools = to_anthropic_tools(&tools, &plain());
        assert_eq!(plain_tools.len(), 2);
        assert!(plain_tools.iter().all(|t| t.cache_control.is_none()));
        assert_eq!(
            plain_tools[0].input_schema["type"],
            serde_json::json!("object")
        );

        let cached = to_anthropic_tools(
            &tools,
            &ChatFormatOptions {
                cache_control: true,
                cache_ttl: None,
            },
        );
        assert!(cached[0].cache_control.is_none());
        assert_eq!(cached[1].cache_control.as_ref().unwrap().kind, "ephemeral");
    }

    #[test]
    fn tool_choice_maps_all_variants_and_parallel_flag() {
        assert_eq!(
            to_tool_choice(&ToolChoice::None, Some(false)),
            AnthropicToolChoice::None
        );
        assert_eq!(
            to_tool_choice(&ToolChoice::Required, Some(false)),
            AnthropicToolChoice::Any {
                disable_parallel_tool_use: Some(true)
            }
        );
        assert_eq!(
            to_tool_choice(
                &ToolChoice::Specific {
                    tool_name: "f".into()
                },
                None
            ),
            AnthropicToolChoice::Tool {
                name: "f".into(),
                disable_parallel_tool_use: None
            }
        );
        assert_eq!(
            to_tool_choice(&ToolChoice::Auto, Some(true)),
            AnthropicToolChoice::Auto {
                disable_parallel_tool_use: Some(false)
            }
        );
    }

    #[test]
    fn implicit_auto_choice_when_only_parallel_flag_set() {
        let options = GenerateOptions {
            parallel_tool_calls: Some(false),
            ..Default::default()
        };
        let request = build_request(
            "m",
            &[Msg::user("hi")],
            &[tool_schema("a")],
            &options,
            &GenerateOptions::default(),
            &plain(),
            false,
        );
        assert_eq!(
            request.tool_choice,
            Some(AnthropicToolChoice::Auto {
                disable_parallel_tool_use: Some(true)
            })
        );
    }

    #[test]
    fn options_fall_back_to_defaults_and_thinking_is_wired() {
        let defaults = GenerateOptions {
            temperature: Some(0.2),
            top_k: Some(7),
            max_tokens: Some(2048),
            ..Default::default()
        };
        let options = GenerateOptions {
            temperature: Some(0.9),
            ..Default::default()
        };
        let request = build_request(
            "m",
            &[Msg::user("hi")],
            &[],
            &options,
            &defaults,
            &plain(),
            true,
        );
        assert_eq!(request.temperature, Some(0.9));
        assert_eq!(request.top_k, Some(7));
        assert_eq!(request.max_tokens, 2048);
        assert!(request.stream.is_some());

        let thinking = GenerateOptions {
            thinking_budget: Some(1024),
            ..Default::default()
        };
        let request = build_request(
            "m",
            &[Msg::user("hi")],
            &[],
            &thinking,
            &defaults,
            &plain(),
            false,
        );
        assert_eq!(request.thinking.unwrap().budget_tokens, 1024);
    }

    #[test]
    fn max_tokens_defaults_when_unset() {
        let request = build_request(
            "m",
            &[Msg::user("hi")],
            &[],
            &GenerateOptions::default(),
            &GenerateOptions::default(),
            &plain(),
            false,
        );
        assert_eq!(
            request.max_tokens,
            super::super::credential::DEFAULT_MAX_TOKENS
        );
    }

    #[test]
    fn extra_body_params_override_request_fields() {
        let mut body = serde_json::to_value(AnthropicRequest {
            model: "m".into(),
            max_tokens: 1,
            messages: vec![],
            stream: None,
            system: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking: None,
        })
        .unwrap();
        let extra = GenerateOptions {
            additional_body_params: [("metadata".to_string(), serde_json::json!({"user_id": "u"}))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        apply_extra_body_params(&mut body, &[&extra]);
        assert_eq!(body["metadata"]["user_id"], serde_json::json!("u"));
    }

    #[test]
    fn apply_cache_control_marks_last_cacheable_block() {
        let messages = vec![Msg::user("first"), Msg::user("second")];
        let mut converted = to_anthropic_messages(&messages, &plain());
        apply_cache_control(
            &mut converted,
            &ChatFormatOptions {
                cache_control: true,
                cache_ttl: None,
            },
        );
        match &converted[1].content[0] {
            AnthropicContentBlock::Text(t) => {
                assert_eq!(t.cache_control.as_ref().unwrap().kind, "ephemeral")
            }
            other => panic!("expected text, got {other:?}"),
        }
        // Earlier message stays untouched.
        match &converted[0].content[0] {
            AnthropicContentBlock::Text(t) => assert!(t.cache_control.is_none()),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn conversation_merge_wraps_history_tags() {
        let messages = vec![
            Msg::builder()
                .role(MsgRole::User)
                .name("Alice")
                .text_content("hi")
                .build(),
            Msg::builder()
                .role(MsgRole::Assistant)
                .name("Bob")
                .text_content("hello")
                .build(),
        ];
        let blocks = merge_conversation(&messages, "PROMPT\n");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ConversationBlock::Text(t) => {
                assert!(t.starts_with("PROMPT\n<history>\nAlice: hi\nBob: hello"));
                assert!(t.ends_with("\n</history>"));
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn conversation_merge_flushes_text_before_media() {
        let messages = vec![Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("look")),
            ContentBlock::Image(ImageBlock::new(Source::Base64(Base64Source::new(
                "image/png",
                "AAAA",
            )))),
        ])];
        let blocks = merge_conversation(&messages, "");
        // text, image, then the trailing `</history>` text element (the last
        // element is media, so the closing tag is appended separately).
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[0], ConversationBlock::Text(_)));
        assert!(matches!(blocks[1], ConversationBlock::Image(_)));
        assert_eq!(blocks[2], ConversationBlock::Text("</history>".to_string()));
    }

    #[test]
    fn multi_agent_groups_tool_sequences_separately() {
        let messages = vec![
            Msg::system("be brief"),
            Msg::builder()
                .role(MsgRole::User)
                .name("Alice")
                .text_content("hi")
                .build(),
            Msg::builder()
                .role(MsgRole::Assistant)
                .name("Bob")
                .text_content("hello")
                .build(),
            Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                "toolu_1",
                "f",
                JsonMap::new(),
            ))]),
            Msg::tool("toolu_1", "f", "done"),
        ];
        let out = to_multi_agent_messages(&messages, DEFAULT_CONVERSATION_HISTORY_PROMPT, &plain());
        // system + merged agent conversation + assistant tool_use + tool result
        assert_eq!(out.len(), 4);
        assert_eq!(out[1].role, "user");
        match &out[1].content[0] {
            AnthropicContentBlock::Text(t) => {
                assert!(t.text.contains("<history>"));
                assert!(t.text.contains("Alice: hi"));
            }
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(out[2].role, "assistant");
        assert_eq!(out[3].role, "user");
        assert!(matches!(
            &out[3].content[0],
            AnthropicContentBlock::ToolResult(_)
        ));
    }

    #[test]
    fn thinking_blocks_are_rendered_as_text() {
        let messages = vec![Msg::assistant_blocks(vec![ContentBlock::Thinking(
            ThinkingBlock::new("reasoning"),
        )])];
        let out = to_anthropic_messages(&messages, &plain());
        match &out[0].content[0] {
            AnthropicContentBlock::Text(t) => assert_eq!(t.text, "reasoning"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn signed_thinking_blocks_are_replayed_as_thinking_blocks() {
        // MODEL-18: with a signature (kept from the original response) the block
        // must go back as a `thinking` block, otherwise Anthropic rejects the
        // thinking + tool_use history.
        let parsed = crate::anthropic::parser::parse_message(
            &serde_json::from_value(serde_json::json!({
                "id": "msg_t",
                "content": [
                    { "type": "thinking", "thinking": "let me check", "signature": "sig-1" }
                ]
            }))
            .unwrap(),
        );
        let messages = vec![Msg::assistant_blocks(parsed.content.clone())];
        let out = to_anthropic_messages(&messages, &plain());
        match &out[0].content[0] {
            AnthropicContentBlock::Thinking(t) => {
                assert_eq!(t.thinking, "let me check");
                assert_eq!(t.signature.as_deref(), Some("sig-1"));
            }
            other => panic!("expected thinking block, got {other:?}"),
        }
    }

    #[test]
    fn server_tool_blocks_are_echoed_back_verbatim() {
        // Round trip: parse a server-tool exchange, then re-encode it for the
        // next request turn. The provider blocks must survive unchanged.
        let parsed = crate::anthropic::parser::parse_message(
            &serde_json::from_value(serde_json::json!({
                "id": "msg_s",
                "content": [
                    { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": { "query": "rust" } },
                    { "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                      "content": [{ "type": "web_search_result", "title": "Rust", "url": "https://rust-lang.org" }] }
                ]
            }))
            .unwrap(),
        );
        let messages = vec![Msg::assistant_blocks(parsed.content.clone())];
        let out = to_anthropic_messages(&messages, &plain());
        match &out[0].content[0] {
            AnthropicContentBlock::ServerToolUse(t) => {
                assert_eq!(t.id, "srvtoolu_1");
                assert_eq!(t.name, "web_search");
            }
            other => panic!("expected server_tool_use, got {other:?}"),
        }
        // The tool result travels as its own user message and is restored verbatim.
        let user = out
            .iter()
            .find(|m| m.role == "user")
            .expect("result message");
        match &user.content[0] {
            AnthropicContentBlock::WebSearchToolResult(r) => {
                assert_eq!(r.tool_use_id, "srvtoolu_1");
                assert!(r.content.is_some());
            }
            other => panic!("expected web_search_tool_result, got {other:?}"),
        }
    }

    #[test]
    fn hint_and_tool_state_do_not_break_conversion() {
        let mut tool_use = ToolUseBlock::new("toolu_1", "f", JsonMap::new());
        tool_use.state = ToolCallState::Finished;
        let messages = vec![Msg::assistant_blocks(vec![ContentBlock::ToolUse(tool_use)])];
        let out = to_anthropic_messages(&messages, &plain());
        assert!(matches!(
            &out[0].content[0],
            AnthropicContentBlock::ToolUse(_)
        ));

        let hint = Msg::builder()
            .role(MsgRole::Tool)
            .block(ContentBlock::Hint(HintBlock::new("hint", "hint")))
            .build();
        let out = to_anthropic_messages(&[hint], &plain());
        match &out[0].content[0] {
            AnthropicContentBlock::Text(t) => assert_eq!(t.text, "hint"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn role_mapping_collapses_system_and_tool_to_user() {
        assert_eq!(role_str(MsgRole::Assistant), "assistant");
        assert_eq!(role_str(MsgRole::User), "user");
        assert_eq!(role_str(MsgRole::System), "user");
        assert_eq!(role_str(MsgRole::Tool), "user");
    }

    #[test]
    fn extra_headers_deduplicate_with_last_winning() {
        let a = GenerateOptions {
            additional_headers: [("x-a".to_string(), "1".to_string())].into_iter().collect(),
            ..Default::default()
        };
        let b = GenerateOptions {
            additional_headers: [("x-a".to_string(), "2".to_string())].into_iter().collect(),
            ..Default::default()
        };
        let headers = extra_headers(&[&a, &b]);
        assert_eq!(headers, vec![("x-a".to_string(), "2".to_string())]);
    }

    // ── Server tools ───────────────────────────────────────────────────────

    #[test]
    fn server_tools_are_declared_before_client_tools() {
        let tools = vec![tool_schema("get_weather")];
        let server = vec![
            AnthropicServerTool::web_search(Some(2), None, None),
            AnthropicServerTool::code_execution(),
        ];
        let entries = to_anthropic_tools_with_server(&tools, &server, &plain()).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[0], AnthropicToolEntry::Server(v)
            if v["type"] == serde_json::json!("web_search_20250305")));
        assert!(matches!(&entries[1], AnthropicToolEntry::Server(v)
            if v["name"] == serde_json::json!("code_execution")));
        assert!(matches!(&entries[2], AnthropicToolEntry::Client(t) if t.name == "get_weather"));

        // Serialized shape keeps both variants side by side in `tools`.
        let request = build_request_with_server_tools(
            "claude",
            &[Msg::user("hi")],
            &tools,
            &server,
            &GenerateOptions::default(),
            &GenerateOptions::default(),
            &plain(),
            false,
        )
        .unwrap();
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value["tools"][0]["type"],
            serde_json::json!("web_search_20250305")
        );
        assert_eq!(
            value["tools"][2]["input_schema"]["type"],
            serde_json::json!("object")
        );
    }

    #[test]
    fn duplicate_tool_names_across_client_and_server_tools_are_rejected() {
        let tools = vec![tool_schema("web_search")];
        let server = vec![AnthropicServerTool::web_search(None, None, None)];
        let err = to_anthropic_tools_with_server(&tools, &server, &plain()).unwrap_err();
        assert!(
            err.to_string().contains("duplicate tool name: web_search"),
            "{err}"
        );

        let duplicated_client = vec![tool_schema("f"), tool_schema("f")];
        assert!(to_anthropic_tools_with_server(&duplicated_client, &[], &plain()).is_err());
    }

    #[test]
    fn server_tools_alone_produce_a_tools_array() {
        let request = build_request_with_server_tools(
            "claude",
            &[Msg::user("hi")],
            &[],
            &[AnthropicServerTool::web_fetch(None, None, None)],
            &GenerateOptions::default(),
            &GenerateOptions::default(),
            &plain(),
            false,
        )
        .unwrap();
        assert_eq!(request.tools.as_ref().unwrap().len(), 1);
    }
}
