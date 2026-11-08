//! Conversion between AgentScope [`Msg`]/[`ToolSchema`] and DashScope DTOs.
//!
//! Mirrors `DashScopeChatFormatter` + `DashScopeMessageConverter` +
//! `DashScopeToolsHelper` + `DashScopeMediaConverter`.
//!
//! DashScope has two wire shapes:
//! * the *generation* API sends `content` as a plain string;
//! * the *multimodal* API sends `content` as a list of content parts, using
//!   `data:` URLs for local/base64 media (DashScope cannot read local files).

use agentscope_core::{
    convert_tool_result_to_string, determine_media_type, extract_text_content, file_to_base64,
    has_media_content, is_local_file, resolve_mime_type, validate_audio_extension,
    validate_image_extension, validate_video_extension, ContentBlock, DataBlock, GenerateOptions,
    ImageBlock, Msg, MsgRole, Result, Source, ToolChoice, ToolSchema, VideoBlock,
};

use super::dto::{
    CacheControl, DashScopeContent, DashScopeContentPart, DashScopeFunction, DashScopeInput,
    DashScopeMessage, DashScopeParameters, DashScopeRequest, DashScopeTool, DashScopeToolCall,
    DashScopeToolFunction,
};
use crate::common::multi_agent::{
    agent_name, group_messages, GroupType, HISTORY_END_TAG, HISTORY_START_TAG,
};

/// Maximum number of `cache_control` markers DashScope accepts per request.
pub const MAX_CACHE_MARKERS: usize = 4;

/// The `{"type": "ephemeral"}` marker used for automatic cache marking.
pub fn ephemeral_cache_control() -> CacheControl {
    let mut map = CacheControl::new();
    map.insert("type".to_string(), "ephemeral".to_string());
    map
}

/// Places a `cache_control` marker on the final content part of `message`,
/// converting string content into a single text part when required.
///
/// Mirrors `DashScopeChatFormatter#setCacheControlOnContent`: an existing marker
/// on the target part is preserved (never overwritten).
fn set_cache_control_on_content(message: &mut DashScopeMessage) {
    let marker = ephemeral_cache_control();
    match message.content.as_mut() {
        Some(DashScopeContent::Text(text)) => {
            let mut part = DashScopeContentPart::text(text.clone());
            part.cache_control = Some(marker);
            message.content = Some(DashScopeContent::Parts(vec![part]));
        }
        Some(DashScopeContent::Parts(parts)) => {
            if let Some(last) = parts.last_mut() {
                if last.cache_control.is_none() {
                    last.cache_control = Some(marker);
                }
            }
        }
        None => {}
    }
}

/// Whether `message` already carries at least one content-level cache marker.
fn has_cache_marker(message: &DashScopeMessage) -> bool {
    match &message.content {
        Some(DashScopeContent::Parts(parts)) => parts.iter().any(|p| p.cache_control.is_some()),
        _ => false,
    }
}

/// Applies the automatic cache policy: when `options.cache_control == Some(true)`
/// the final message and every system message (that is not already explicitly
/// marked) receive a marker, capped at [`MAX_CACHE_MARKERS`] total markers.
///
/// Mirrors `DashScopeChatFormatter#applyAutomaticCacheControl`.
pub fn apply_automatic_cache_control(messages: &mut [DashScopeMessage], options: &GenerateOptions) {
    apply_automatic_cache_control_excluding(messages, options, &[]);
}

/// Same as [`apply_automatic_cache_control`], but `skip[i] == true` marks a
/// message that must never receive an automatic marker (the `fileid://`
/// directives injected for document blocks). Java keeps the injected messages out
/// of automatic placement with a parallel directive list, so the remaining
/// boundaries stay aligned.
pub fn apply_automatic_cache_control_excluding(
    messages: &mut [DashScopeMessage],
    options: &GenerateOptions,
    skip: &[bool],
) {
    if messages.is_empty() || options.cache_control != Some(true) {
        return;
    }
    let skipped = |index: usize| skip.get(index).copied().unwrap_or(false);
    let mut marker_count = messages
        .iter()
        .enumerate()
        .filter(|(index, m)| !skipped(*index) && has_cache_marker(m))
        .count();
    if marker_count > MAX_CACHE_MARKERS {
        return;
    }
    // Candidate indices: the final message, then each system message, in order.
    let mut candidates: Vec<usize> = Vec::new();
    let last = messages.len() - 1;
    if !skipped(last) {
        candidates.push(last);
    }
    for (i, m) in messages.iter().enumerate() {
        if !skipped(i) && m.role.as_deref() == Some("system") {
            candidates.push(i);
        }
    }
    for index in candidates {
        if marker_count >= MAX_CACHE_MARKERS {
            break;
        }
        if has_cache_marker(&messages[index]) {
            continue;
        }
        set_cache_control_on_content(&mut messages[index]);
        marker_count += 1;
    }
}

/// Whether `msg` explicitly requests caching via its `CACHE_CONTROL` metadata.
fn metadata_cache_directive(msg: &Msg) -> Option<bool> {
    msg.metadata
        .get(agentscope_core::metadata_keys::CACHE_CONTROL)
        .and_then(|v| v.as_bool())
}

/// Provider name a DashScope document file id must belong to.
const DASHSCOPE_PROVIDER: &str = "dashscope";
/// Scheme DashScope's `qwen-long` document contract uses.
const FILE_ID_SCHEME: &str = "fileid://";

/// Builds the system messages DashScope's document contract expects.
///
/// `qwen-long` reads documents from a `fileid://` reference carried by a system
/// message placed before the turn that asks about them, not from a content part.
/// Each DashScope file-id document therefore contributes one system message
/// ahead of its own message (Java `DashScopeChatFormatter#appendDocumentMessages`).
///
/// URL documents need no injection: `qwen-doc-turbo` reads them from a `doc_url`
/// content part inside the user turn itself, which [`multimodal_parts`] emits.
/// Base64 payloads and file ids issued by another provider have no DashScope
/// channel: they become a text reference, because a formatter that throws on a
/// source it cannot deliver would break every later turn of the session.
fn document_messages(msg: &Msg) -> Result<Vec<DashScopeMessage>> {
    if msg.role != MsgRole::User {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for block in &msg.content {
        let ContentBlock::Document(document) = block else {
            continue;
        };
        let Source::FileId(file) = &document.source else {
            continue;
        };
        if file.provider != DASHSCOPE_PROVIDER {
            crate::common::diagnostics::warn(
                "unsupported_document_source",
                "content.document",
                &format!(
                    "file id issued by '{}' cannot reach DashScope; rendered as a text reference",
                    file.provider
                ),
            );
            continue;
        }
        out.push(DashScopeMessage {
            role: Some("system".to_string()),
            content: Some(DashScopeContent::Text(format!(
                "{FILE_ID_SCHEME}{}",
                file.file_id
            ))),
            ..Default::default()
        });
    }
    Ok(out)
}

/// Metadata key selecting the parsing strategy of a URL document.
const FILE_PARSING_STRATEGY_KEY: &str = "file_parsing_strategy";
/// Parsing strategies the DashScope API accepts.
const FILE_PARSING_STRATEGIES: [&str; 3] = ["auto", "text_only", "text_and_images"];
/// `qwen-doc-turbo` accepts at most 10 document URLs per request.
const MAX_DOCUMENT_URLS: usize = 10;

/// Resolves the `file_parsing_strategy` configured on a document block.
fn file_parsing_strategy(document: &agentscope_core::DocumentBlock) -> Option<String> {
    let value = document.metadata.as_ref()?.get(FILE_PARSING_STRATEGY_KEY)?;
    let strategy = match value {
        serde_json::Value::String(strategy) => strategy.clone(),
        other => other.to_string(),
    };
    if FILE_PARSING_STRATEGIES.contains(&strategy.as_str()) {
        Some(strategy)
    } else {
        crate::common::diagnostics::warn(
            "unsupported_document_parsing_strategy",
            "content.document",
            &format!(
                "unknown file_parsing_strategy '{strategy}'; expected one of {FILE_PARSING_STRATEGIES:?}"
            ),
        );
        None
    }
}

/// Resolves the public URL of a document block for a `doc_url` part.
///
/// DashScope fetches the document itself, so the URL must be publicly reachable
/// over http(s); a local `file://` reference would fail server-side.
fn document_url(document: &agentscope_core::DocumentBlock) -> Result<String> {
    let Source::Url(url) = &document.source else {
        return Err(agentscope_core::Error::Other(
            "DashScope doc_url requires a URL source".to_string(),
        ));
    };
    let value = url.url.trim();
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(value.to_string())
    } else {
        Err(agentscope_core::Error::Other(format!(
            "DashScope doc_url requires a publicly reachable http(s) URL, got '{value}'"
        )))
    }
}

/// Converts a document block into the content part DashScope can consume for the
/// given role: a `doc_url` part on a user turn, a text reference otherwise.
fn document_part(document: &agentscope_core::DocumentBlock) -> DashScopeContentPart {
    match document_url(document) {
        Ok(url) => DashScopeContentPart::doc_url(vec![url], file_parsing_strategy(document)),
        Err(e) => {
            crate::common::diagnostics::warn(
                "unsupported_document_source",
                "content.document",
                &format!("document part omitted; replaced with a reference: {e}"),
            );
            DashScopeContentPart::text(crate::common::document::render(document))
        }
    }
}

/// DashScope native text-generation endpoint.
pub const TEXT_GENERATION_ENDPOINT: &str = "/api/v1/services/aigc/text-generation/generation";
/// DashScope native multimodal-generation endpoint.
pub const MULTIMODAL_GENERATION_ENDPOINT: &str =
    "/api/v1/services/aigc/multimodal-generation/generation";
/// Default DashScope base URL (native protocol, **not** the `compatible-mode` gateway).
pub const DEFAULT_BASE_URL: &str = "https://dashscope.aliyuncs.com";

/// Maps an AgentScope role to a DashScope role string (`lowercase` of the enum name).
pub fn role_str(role: MsgRole) -> &'static str {
    match role {
        MsgRole::System => "system",
        MsgRole::User => "user",
        MsgRole::Assistant => "assistant",
        MsgRole::Tool => "tool",
    }
}

/// Detects whether a model requires the multimodal-generation endpoint
/// (mirrors `DashScopeHttpClient#isMultimodalModel`).
pub fn is_multimodal_model(model_name: &str) -> bool {
    let lower = model_name.to_lowercase();
    // Reverse exclusions: text-only models whose prefix would otherwise match.
    if lower == "qwen3.6-max-preview"
        || lower.starts_with("qwen3.7-max")
        || lower.contains("kimi-k2-thinking")
        || lower.contains("moonshot-kimi-k2-instruct")
        || lower.starts_with("qwen3.8-2.4t-a95b")
    {
        return false;
    }
    lower.starts_with("qvq")
        || lower.contains("-vl")
        || lower.contains("-asr")
        || lower.starts_with("qwen3.5")
        || lower.starts_with("qwen3.6")
        || lower.starts_with("qwen3.7")
        || lower.starts_with("qwen3.8")
        || lower.contains("kimi-k")
}

fn base64_data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

/// Resolves a media [`Source`] to the URL DashScope expects.
///
/// Local files are inlined as `data:` URLs because the HTTP API cannot read the
/// client filesystem; remote URLs are forwarded verbatim.
fn source_url(source: &Source, validate: fn(&str) -> Result<()>) -> Result<String> {
    match source {
        Source::Base64(b) => Ok(base64_data_url(&b.media_type, &b.data)),
        Source::Url(u) => {
            validate(&u.url)?;
            if is_local_file(&u.url) || u.url.starts_with("file:") {
                let path = u.url.strip_prefix("file://").unwrap_or(&u.url);
                let mime = determine_media_type(path);
                let data = file_to_base64(path)?;
                Ok(base64_data_url(&mime, &data))
            } else {
                Ok(u.url.clone())
            }
        }
        Source::FileId(file) => Err(agentscope_core::Error::Other(format!(
            "provider file id '{}' cannot be used as a media URL; upload the document and use a DocumentBlock",
            file.file_id
        ))),
    }
}

fn image_part(block: &ImageBlock) -> DashScopeContentPart {
    match source_url(&block.source, validate_image_extension) {
        Ok(url) => DashScopeContentPart {
            image: Some(url),
            min_pixels: block.min_pixels,
            max_pixels: block.max_pixels,
            ..Default::default()
        },
        Err(e) => DashScopeContentPart::text(format!("[Image - processing failed: {e}]")),
    }
}

fn audio_part(block: &agentscope_core::AudioBlock) -> DashScopeContentPart {
    match source_url(&block.source, validate_audio_extension) {
        Ok(url) => DashScopeContentPart::audio(url),
        Err(e) => DashScopeContentPart::text(format!("[Audio - processing failed: {e}]")),
    }
}

fn video_part(block: &VideoBlock) -> DashScopeContentPart {
    match source_url(&block.source, validate_video_extension) {
        Ok(url) => DashScopeContentPart {
            video: Some(serde_json::Value::String(url)),
            fps: block.fps,
            max_frames: block.max_frames,
            min_pixels: block.min_pixels,
            max_pixels: block.max_pixels,
            total_pixels: block.total_pixels,
            ..Default::default()
        },
        Err(e) => DashScopeContentPart::text(format!("[Video - processing failed: {e}]")),
    }
}

/// Maps a [`DataBlock`] onto the closest DashScope content part, using the MIME
/// type to pick image / audio / video.
fn data_part(block: &DataBlock) -> DashScopeContentPart {
    let mime = resolve_mime_type(&block.source).unwrap_or_default();
    if mime.starts_with("image/") {
        match source_url(&block.source, validate_image_extension) {
            Ok(url) => DashScopeContentPart::image(url),
            Err(e) => DashScopeContentPart::text(format!("[Media - processing failed: {e}]")),
        }
    } else if mime.starts_with("video/") {
        match source_url(&block.source, validate_video_extension) {
            Ok(url) => DashScopeContentPart::video(url),
            Err(e) => DashScopeContentPart::text(format!("[Media - processing failed: {e}]")),
        }
    } else if mime.starts_with("audio/") {
        match source_url(&block.source, validate_audio_extension) {
            Ok(url) => DashScopeContentPart::audio(url),
            Err(e) => DashScopeContentPart::text(format!("[Media - processing failed: {e}]")),
        }
    } else {
        DashScopeContentPart::text(format!("[Media - unsupported MIME type: {mime}]"))
    }
}

fn tool_call_args(tool_use: &agentscope_core::ToolUseBlock) -> String {
    if let Some(raw) = &tool_use.content {
        if !raw.trim().is_empty() {
            return raw.clone();
        }
    }
    serde_json::Value::Object(tool_use.input.clone()).to_string()
}

/// Converts `ToolUseBlock`s into DashScope tool calls.
pub fn to_dashscope_tool_calls(msg: &Msg) -> Vec<DashScopeToolCall> {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse(t) => Some(DashScopeToolCall {
                id: Some(t.id.clone()),
                tool_type: Some("function".to_string()),
                function: Some(DashScopeFunction::of(t.name.clone(), tool_call_args(t))),
                index: None,
            }),
            _ => None,
        })
        .collect()
}

/// Concatenates the message text (text/hint blocks plus tool-result text) and appends one
/// reference per document the message carries, so an attachment that DashScope cannot deliver
/// still stays visible in the history (Java `DashScopeMessageConverter#extractTextContent`).
fn simple_text(msg: &Msg) -> String {
    crate::common::document::append(&extract_text_content(msg), msg)
}

/// Whether the message carries a URL document on a user turn.
///
/// DashScope sends URL documents as `doc_url` parts inside the user message, so such a turn needs
/// the content-part shape even when it holds no other media. File-id documents are excluded: they
/// ride on the injected `fileid://` system message (Java `DashScopeChatFormatter#hasMediaContent`).
fn has_url_document_on_user_turn(msg: &Msg) -> bool {
    msg.role == MsgRole::User
        && msg.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Document(document) if matches!(document.source, Source::Url(_))
            )
        })
}

fn multimodal_parts(blocks: &[ContentBlock], role: MsgRole) -> Vec<DashScopeContentPart> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(DashScopeContentPart::text(t.text.clone())),
            ContentBlock::Hint(h) => parts.push(DashScopeContentPart::text(
                h.hint.clone().unwrap_or_default(),
            )),
            ContentBlock::Image(i) => parts.push(image_part(i)),
            ContentBlock::Audio(a) => parts.push(audio_part(a)),
            ContentBlock::Video(v) => parts.push(video_part(v)),
            ContentBlock::Data(d) => parts.push(data_part(d)),
            ContentBlock::ToolResult(tr) => {
                let text = convert_tool_result_to_string(&tr.output);
                if !text.is_empty() {
                    parts.push(DashScopeContentPart::text(text));
                }
            }
            // Thinking blocks are never sent back to the model.
            ContentBlock::Thinking(_) => {}
            ContentBlock::ToolUse(_) => {}
            // Documents travel as a `doc_url` part on a user turn (qwen-doc-turbo
            // reads them there) and as a text reference everywhere else.
            ContentBlock::Document(document) => parts.push(if role == MsgRole::User {
                document_part(document)
            } else {
                DashScopeContentPart::text(crate::common::document::render(document))
            }),
        }
    }
    if parts.is_empty() {
        parts.push(DashScopeContentPart::text(""));
    }
    // Document-capable models validate the type discriminator of every part of a
    // turn that carries `doc_url` parts ("User content type must be one of
    // [doc_url,text]"). Only document turns are touched so other models keep
    // their exact request shape.
    if parts.iter().any(|part| part.doc_url.is_some()) {
        for part in parts.iter_mut() {
            if part.doc_url.is_none() && part.text.is_some() && part.part_type.is_none() {
                part.part_type = Some(super::dto::PART_TYPE_TEXT.to_string());
            }
        }
    }
    parts
}

fn simple_message(msg: &Msg) -> DashScopeMessage {
    // A tool-role (or system) message carrying a tool result is sent as `tool`.
    if let Some(tr) = msg.content.iter().find_map(|b| match b {
        ContentBlock::ToolResult(tr) => Some(tr),
        _ => None,
    }) {
        if matches!(msg.role, MsgRole::Tool | MsgRole::System) {
            return DashScopeMessage {
                role: Some("tool".to_string()),
                tool_call_id: tr.id.clone(),
                name: tr.name.clone(),
                content: Some(DashScopeContent::Text(convert_tool_result_to_string(
                    &tr.output,
                ))),
                ..Default::default()
            };
        }
    }

    let mut out = DashScopeMessage {
        role: Some(role_str(msg.role).to_string()),
        ..Default::default()
    };
    if msg.role == MsgRole::Assistant {
        let calls = to_dashscope_tool_calls(msg);
        if !calls.is_empty() {
            let text = simple_text(msg);
            out.content = Some(DashScopeContent::Text(text));
            out.tool_calls = Some(calls);
            return out;
        }
    }
    out.content = Some(DashScopeContent::Text(simple_text(msg)));
    out
}

fn tool_role_multimodal_message(msg: &Msg) -> DashScopeMessage {
    if let Some(tr) = msg.content.iter().find_map(|b| match b {
        ContentBlock::ToolResult(tr) => Some(tr),
        _ => None,
    }) {
        let content = if tr.output.iter().any(|b| {
            matches!(
                b,
                ContentBlock::Image(_)
                    | ContentBlock::Audio(_)
                    | ContentBlock::Video(_)
                    | ContentBlock::Data(_)
            )
        }) {
            multimodal_parts(&tr.output, MsgRole::Tool)
        } else {
            vec![DashScopeContentPart::text(convert_tool_result_to_string(
                &tr.output,
            ))]
        };
        return DashScopeMessage {
            role: Some("tool".to_string()),
            tool_call_id: tr.id.clone(),
            name: tr.name.clone(),
            content: Some(DashScopeContent::Parts(content)),
            ..Default::default()
        };
    }
    DashScopeMessage {
        role: Some("tool".to_string()),
        content: Some(DashScopeContent::Parts(vec![DashScopeContentPart::text(
            extract_text_content(msg),
        )])),
        ..Default::default()
    }
}

fn multimodal_message(msg: &Msg) -> DashScopeMessage {
    if msg.role == MsgRole::Tool {
        return tool_role_multimodal_message(msg);
    }
    let mut out = DashScopeMessage {
        role: Some(role_str(msg.role).to_string()),
        content: Some(DashScopeContent::Parts(multimodal_parts(
            &msg.content,
            msg.role,
        ))),
        ..Default::default()
    };
    if msg.role == MsgRole::Assistant {
        let calls = to_dashscope_tool_calls(msg);
        if !calls.is_empty() {
            out.tool_calls = Some(calls);
        }
    }
    out
}

/// Converts messages into DashScope DTOs.
///
/// `force_multimodal` mirrors "this model only speaks the multimodal API"; when
/// set, every message uses the content-part list shape regardless of whether it
/// carries media.
pub fn to_dashscope_messages(
    messages: Vec<Msg>,
    force_multimodal: bool,
) -> Result<Vec<DashScopeMessage>> {
    Ok(to_dashscope_messages_flagged(messages, force_multimodal)?.0)
}

/// Same as [`to_dashscope_messages`], additionally reporting which output entries
/// were injected `fileid://` directives (so automatic cache placement can skip
/// them and keep the directive list aligned).
fn to_dashscope_messages_flagged(
    messages: Vec<Msg>,
    force_multimodal: bool,
) -> Result<(Vec<DashScopeMessage>, Vec<bool>)> {
    if force_multimodal {
        reject_document_urls_on_multimodal_endpoint(&messages)?;
    }
    let mut out: Vec<DashScopeMessage> = Vec::new();
    let mut injected: Vec<bool> = Vec::new();
    for msg in messages {
        let documents = document_messages(&msg)?;
        injected.extend(std::iter::repeat(true).take(documents.len()));
        out.extend(documents);
        let explicit = metadata_cache_directive(&msg) == Some(true);
        let mut converted =
            if force_multimodal || has_media_content(&msg) || has_url_document_on_user_turn(&msg) {
                multimodal_message(&msg)
            } else {
                simple_message(&msg)
            };
        // Explicit `Msg` metadata markers are emitted as supplied; the
        // automatic pass (if enabled) only fills in unmarked boundaries.
        if explicit {
            set_cache_control_on_content(&mut converted);
        }
        out.push(converted);
        injected.push(false);
    }
    validate_document_url_limit(&out)?;
    Ok((out, injected))
}

/// Rejects URL documents on the multimodal endpoint: `doc_url` parts are only
/// accepted by the text-generation endpoint.
fn reject_document_urls_on_multimodal_endpoint(messages: &[Msg]) -> Result<()> {
    for msg in messages {
        if msg.role != MsgRole::User {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::Document(document) = block {
                if matches!(document.source, Source::Url(_)) {
                    return Err(agentscope_core::Error::Other(
                        "DashScope doc_url documents are only accepted by the text-generation \
                         endpoint, but this model routes to the multimodal endpoint; use a \
                         document-capable text model such as qwen-doc-turbo, or upload the \
                         document and pass a DashScope file id"
                            .to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Rejects requests that exceed DashScope's document URL budget (10 per request).
fn validate_document_url_limit(messages: &[DashScopeMessage]) -> Result<()> {
    let count: usize = messages
        .iter()
        .filter_map(|message| message.content.as_ref())
        .map(|content| match content {
            DashScopeContent::Text(_) => 0,
            DashScopeContent::Parts(parts) => parts
                .iter()
                .map(|part| part.doc_url.as_ref().map(Vec::len).unwrap_or(0))
                .sum(),
        })
        .sum();
    if count > MAX_DOCUMENT_URLS {
        return Err(agentscope_core::Error::Other(format!(
            "DashScope accepts at most {MAX_DOCUMENT_URLS} document URLs per request, got {count}; \
             split the documents across requests or upload them and pass DashScope file ids"
        )));
    }
    Ok(())
}

/// Converts messages and then applies the automatic cache policy from
/// `options` (mirrors the request-scoped `format(msgs, options)` overload).
pub fn to_dashscope_messages_with_options(
    messages: Vec<Msg>,
    force_multimodal: bool,
    options: &GenerateOptions,
) -> Result<Vec<DashScopeMessage>> {
    let (mut converted, injected) = to_dashscope_messages_flagged(messages, force_multimodal)?;
    apply_automatic_cache_control_excluding(&mut converted, options, &injected);
    Ok(converted)
}

/// Converts tool schemas into DashScope tool definitions.
pub fn to_dashscope_tools(tools: Vec<ToolSchema>) -> Vec<DashScopeTool> {
    tools
        .into_iter()
        .map(|t| {
            DashScopeTool::function(DashScopeToolFunction {
                name: t.name,
                description: if t.description.is_empty() {
                    None
                } else {
                    Some(t.description)
                },
                parameters: if t.parameters.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Object(t.parameters))
                },
            })
        })
        .collect()
}

/// Converts a [`ToolChoice`] to the DashScope `tool_choice` value.
///
/// `Required` degrades to `"auto"` because DashScope has no generic
/// "must call a tool" mode (same fallback as Java).
pub fn to_tool_choice_value(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!("auto"),
        ToolChoice::None => serde_json::json!("none"),
        ToolChoice::Required => {
            crate::common::diagnostics::warn(
                "tool_choice_downgraded",
                "tool_choice",
                "DashScope has no 'required' tool-choice mode; downgraded to 'auto'",
            );
            serde_json::json!("auto")
        }
        ToolChoice::Specific { tool_name } => serde_json::json!({
            "type": "function",
            "function": { "name": tool_name }
        }),
    }
}

/// Builds a complete [`DashScopeRequest`], merging `defaults` under `runtime`
/// (runtime wins) and applying tools/tool-choice/thinking.
pub fn build_request(
    model_name: &str,
    messages: Vec<DashScopeMessage>,
    stream: bool,
    runtime: &GenerateOptions,
    defaults: &GenerateOptions,
    tools: Vec<ToolSchema>,
    tool_choice: Option<&ToolChoice>,
    enable_thinking: bool,
) -> DashScopeRequest {
    let merged = GenerateOptions::merge_options(runtime, defaults);

    let mut params = DashScopeParameters {
        result_format: "message".to_string(),
        incremental_output: Some(stream),
        temperature: merged.temperature,
        top_p: merged.top_p,
        top_k: merged.top_k,
        max_tokens: merged.max_tokens,
        seed: merged.seed,
        frequency_penalty: merged.frequency_penalty,
        presence_penalty: merged.presence_penalty,
        parallel_tool_calls: merged.parallel_tool_calls,
        // DashScope accepts the effort string verbatim (low/medium/high).
        reasoning_effort: merged.reasoning_effort.clone(),
        response_format: merged
            .response_format
            .as_ref()
            .and_then(|rf| serde_json::to_value(rf).ok()),
        ..Default::default()
    };

    if merged.thinking_budget.is_some() {
        params.thinking_budget = merged.thinking_budget;
        params.enable_thinking = Some(true);
    } else if enable_thinking {
        params.enable_thinking = Some(true);
    }

    let dashscope_tools = to_dashscope_tools(tools);
    if !dashscope_tools.is_empty() {
        params.tools = Some(dashscope_tools);
    }
    if let Some(choice) = tool_choice {
        params.tool_choice = Some(to_tool_choice_value(choice));
    }

    DashScopeRequest {
        model: Some(model_name.to_string()),
        input: Some(DashScopeInput::of(messages)),
        parameters: Some(params),
    }
}

// ---------------------------------------------------------------------------
// Multi-agent formatting
// ---------------------------------------------------------------------------

/// Merges a run of agent messages into one `user` message wrapped in
/// `<history>` tags, mirroring `DashScopeConversationMerger#mergeToMessage`
/// (string-content mode).
///
/// Text/hint blocks render as `"<name>: <content>"`, tool results as
/// `"<name> (<tool>): <output>"` (empty outputs become `[Empty tool result]`).
/// Images/data blocks are preserved as extra content parts and referenced in the
/// text; videos only contribute their URL as text, matching Java.
fn merge_history_text_and_parts(messages: &[Msg]) -> (String, Vec<DashScopeContentPart>) {
    let mut text = String::new();
    let mut parts: Vec<DashScopeContentPart> = Vec::new();

    for msg in messages {
        let name = agent_name(msg);
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => text.push_str(&format!("{name}: {}\n", t.text)),
                ContentBlock::Hint(h) => {
                    text.push_str(&format!("{name}: {}\n", h.hint.clone().unwrap_or_default()));
                }
                ContentBlock::Image(img) => {
                    parts.push(image_part(img));
                    text.push_str(&format!("{name}: [Image]\n"));
                }
                ContentBlock::Video(v) => {
                    let rendered = match source_url(&v.source, validate_video_extension) {
                        Ok(url) => format!("{name}: [Video: {url}]\n"),
                        Err(e) => {
                            crate::common::diagnostics::warn(
                                "unsupported_media_type",
                                "conversation.video",
                                &format!("failed to process VideoBlock: {e}"),
                            );
                            format!("{name}: [Video - processing failed]\n")
                        }
                    };
                    text.push_str(&rendered);
                }
                ContentBlock::Data(d) => {
                    parts.push(data_part(d));
                    text.push_str(&format!("{name}: [Data]\n"));
                }
                // Document-capable models read a URL from a `doc_url` part on the
                // merged user message; the reference alone would leave the
                // document unread. Any other source becomes a reference.
                ContentBlock::Document(document) => match document_url(document) {
                    Ok(url) => {
                        parts.push(DashScopeContentPart::doc_url(
                            vec![url],
                            file_parsing_strategy(document),
                        ));
                        text.push_str(&format!("{name}: [Document]\n"));
                    }
                    Err(_) => text.push_str(&format!(
                        "{name}: {}\n",
                        crate::common::document::render(document)
                    )),
                },
                // Audio has no textual rendering in the Java text-mode merger.
                ContentBlock::Audio(_) => {}
                // Thinking blocks are never sent back to the model.
                ContentBlock::Thinking(_) => {}
                ContentBlock::ToolResult(tr) => {
                    let result = crate::common::text_only(&tr.output);
                    let rendered = if result.is_empty() {
                        "[Empty tool result]".to_string()
                    } else {
                        result
                    };
                    text.push_str(&format!(
                        "{name} ({}): {rendered}\n",
                        tr.name.clone().unwrap_or_default()
                    ));
                }
                ContentBlock::ToolUse(_) => {}
            }
        }
    }
    (text, parts)
}

/// Merges agent messages into one `user` message (`<history>`-wrapped),
/// mirroring `DashScopeConversationMerger#mergeToMessage`.
pub fn merge_conversation(messages: &[Msg], history_prompt: &str) -> DashScopeMessage {
    let (mut text, parts) = merge_history_text_and_parts(messages);
    text = format!("{history_prompt}{HISTORY_START_TAG}\n{text}{HISTORY_END_TAG}");

    if parts.is_empty() {
        DashScopeMessage {
            role: Some("user".to_string()),
            content: Some(DashScopeContent::Text(text)),
            ..Default::default()
        }
    } else {
        let mut content = Vec::with_capacity(parts.len() + 1);
        let mut history_part = DashScopeContentPart::text(text);
        // Document-capable models require a type discriminator on every part of a
        // turn that carries `doc_url` parts.
        if parts.iter().any(|part| part.doc_url.is_some()) {
            history_part.part_type = Some(super::dto::PART_TYPE_TEXT.to_string());
        }
        content.push(history_part);
        content.extend(parts);
        DashScopeMessage {
            role: Some("user".to_string()),
            content: Some(DashScopeContent::Parts(content)),
            ..Default::default()
        }
    }
}

/// Merges agent messages into one multimodal `user` message, mirroring
/// `DashScopeConversationMerger#mergeToMultiModalMessage`.
///
/// Unlike the text merger, every media block becomes its own content part and
/// flushes the accumulated text first; tool results are not expected inside an
/// agent group and are skipped with a diagnostic (Java logs a warning).
pub fn merge_multimodal_conversation(
    messages: &[Msg],
    history_prompt: &str,
    is_first: bool,
) -> DashScopeMessage {
    let mut content: Vec<DashScopeContentPart> = Vec::new();
    let mut text: Vec<String> = Vec::new();

    let opening = if is_first {
        format!("{history_prompt}{HISTORY_START_TAG}")
    } else {
        HISTORY_START_TAG.to_string()
    };
    text.push(opening);

    fn flush(text: &mut Vec<String>, content: &mut Vec<DashScopeContentPart>) {
        if !text.is_empty() {
            content.push(DashScopeContentPart::text(text.join("\n")));
            text.clear();
        }
    }

    for msg in messages {
        let name = agent_name(msg);
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => text.push(format!("{name}: {}", t.text)),
                ContentBlock::Hint(h) => {
                    text.push(format!("{name}: {}", h.hint.clone().unwrap_or_default()))
                }
                ContentBlock::Image(img) => {
                    flush(&mut text, &mut content);
                    content.push(image_part(img));
                }
                ContentBlock::Audio(a) => {
                    flush(&mut text, &mut content);
                    content.push(audio_part(a));
                }
                ContentBlock::Video(v) => {
                    flush(&mut text, &mut content);
                    content.push(video_part(v));
                }
                ContentBlock::Data(d) => {
                    flush(&mut text, &mut content);
                    content.push(data_part(d));
                }
                ContentBlock::Document(document) => {
                    // URL documents never reach this multimodal merge (the
                    // multimodal endpoint has no `doc_url` channel); file ids ride
                    // on the injected `fileid://` message, so a reference keeps
                    // the transcript in step with what the model receives.
                    text.push(format!(
                        "{name}: {}",
                        crate::common::document::render(document)
                    ));
                }
                ContentBlock::Thinking(_) => {}
                ContentBlock::ToolResult(tr) => {
                    crate::common::diagnostics::warn(
                        "tool_result_in_agent_group",
                        "conversation.tool_result",
                        &format!(
                            "Unexpected ToolResultBlock for '{}' in an agent message group; skipping",
                            tr.name.clone().unwrap_or_default()
                        ),
                    );
                }
                ContentBlock::ToolUse(_) => {}
            }
        }
    }

    text.push(HISTORY_END_TAG.to_string());
    flush(&mut text, &mut content);
    if content.is_empty() {
        content.push(DashScopeContentPart::text(""));
    }

    DashScopeMessage {
        role: Some("user".to_string()),
        content: Some(DashScopeContent::Parts(content)),
        ..Default::default()
    }
}

/// Applies the cache directive carried by the last message of a merged group
/// (mirrors `DashScopeMultiAgentFormatter#applyMergedCacheControlMetadata`).
fn apply_merged_cache_directive(messages: &[Msg], merged: &mut DashScopeMessage) {
    for msg in messages.iter().rev() {
        if let Some(true) = metadata_cache_directive(msg) {
            set_cache_control_on_content(merged);
            return;
        }
    }
}

/// Converts messages with multi-agent support, mirroring
/// `DashScopeMultiAgentFormatter#doFormat` / `#formatMultiModal`.
///
/// The leading system message stays separate, runs of tool messages keep normal
/// conversion, and runs of plain agent messages merge into a single
/// `<history>`-wrapped user message. Automatic cache control is applied last.
pub fn to_multi_agent_messages(
    messages: Vec<Msg>,
    force_multimodal: bool,
    history_prompt: &str,
    options: &GenerateOptions,
) -> Result<Vec<DashScopeMessage>> {
    let mut result: Vec<DashScopeMessage> = Vec::new();
    let mut injected: Vec<bool> = Vec::new();
    let mut start = 0;

    // Java picks the merger from the endpoint type, not from media presence:
    // `doFormat` for the text endpoint, `formatMultiModal` for the multimodal one.
    let multimodal = force_multimodal;
    if multimodal {
        reject_document_urls_on_multimodal_endpoint(&messages)?;
    }

    if let Some(first) = messages.first() {
        if first.role == MsgRole::System {
            let mut system = DashScopeMessage {
                role: Some("system".to_string()),
                content: Some(if multimodal {
                    DashScopeContent::Parts(vec![DashScopeContentPart::text(extract_text_content(
                        first,
                    ))])
                } else {
                    DashScopeContent::Text(extract_text_content(first))
                }),
                ..Default::default()
            };
            if let Some(true) = metadata_cache_directive(first) {
                set_cache_control_on_content(&mut system);
            }
            result.push(system);
            injected.push(false);
            start = 1;
        }
    }

    let remaining = &messages[start..];
    let mut is_first_agent_group = true;
    for (kind, group) in group_messages(remaining) {
        match kind {
            GroupType::AgentMessage => {
                // Document blocks inside a merged group still need their own
                // `fileid://` system message ahead of the turn.
                for msg in &group {
                    let documents = document_messages(msg)?;
                    injected.extend(std::iter::repeat(true).take(documents.len()));
                    result.extend(documents);
                }
                let mut merged = if multimodal {
                    merge_multimodal_conversation(&group, history_prompt, is_first_agent_group)
                } else {
                    merge_conversation(
                        &group,
                        if is_first_agent_group {
                            history_prompt
                        } else {
                            ""
                        },
                    )
                };
                apply_merged_cache_directive(&group, &mut merged);
                result.push(merged);
                injected.push(false);
                is_first_agent_group = false;
            }
            GroupType::ToolSequence => {
                let (mut converted, flags) = to_dashscope_messages_flagged(group, multimodal)?;
                result.append(&mut converted);
                injected.extend(flags);
            }
        }
    }

    apply_automatic_cache_control_excluding(&mut result, options, &injected);
    validate_document_url_limit(&result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{HintBlock, TextBlock, ToolUseBlock};

    fn user(text: &str) -> Msg {
        Msg::user(text)
    }

    fn convert(messages: Vec<Msg>, force_multimodal: bool) -> Vec<DashScopeMessage> {
        to_dashscope_messages(messages, force_multimodal).expect("dashscope conversion")
    }

    fn convert_with_options(
        messages: Vec<Msg>,
        force_multimodal: bool,
        options: &GenerateOptions,
    ) -> Vec<DashScopeMessage> {
        to_dashscope_messages_with_options(messages, force_multimodal, options)
            .expect("dashscope conversion")
    }

    fn convert_multi_agent(
        messages: Vec<Msg>,
        force_multimodal: bool,
        history_prompt: &str,
        options: &GenerateOptions,
    ) -> Vec<DashScopeMessage> {
        to_multi_agent_messages(messages, force_multimodal, history_prompt, options)
            .expect("dashscope conversion")
    }

    #[test]
    fn multimodal_model_detection_matches_java() {
        for m in [
            "qvq-72b",
            "qwen-vl-plus",
            "qwen3-asr-flash",
            "qwen3.5-plus",
            "qwen3.6-flash",
            "qwen3.7-plus",
            "qwen3.8-max",
            "kimi-k2.6",
        ] {
            assert!(is_multimodal_model(m), "{m} should be multimodal");
        }
        for m in [
            "qwen3.6-max-preview",
            "qwen3.7-max",
            "qwen3.7-max-2026-05-20",
            "qwen3.8-2.4t-a95b",
            "qwen3-max",
            "qwen-turbo",
        ] {
            assert!(!is_multimodal_model(m), "{m} should be text-only");
        }
    }

    #[test]
    fn simple_message_wraps_text_in_string_content() {
        let msgs = convert(vec![user("hello")], false);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role.as_deref(), Some("user"));
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_text()),
            Some("hello")
        );
        // Text-only content serializes as a plain JSON string.
        let value = serde_json::to_value(&msgs[0]).unwrap();
        assert_eq!(value["content"], serde_json::json!("hello"));
    }

    #[test]
    fn hint_blocks_are_joined_with_newlines() {
        let mut msg = Msg::user("a");
        msg.content.push(ContentBlock::Text(TextBlock::new("b")));
        msg.content
            .push(ContentBlock::Hint(HintBlock::new("h", "c")));
        let msgs = convert(vec![msg], false);
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_text()),
            Some("a\nb\nc")
        );
    }

    #[test]
    fn assistant_tool_use_becomes_tool_calls_with_json_args() {
        let mut msg = Msg::assistant("");
        msg.content.push(ContentBlock::ToolUse(ToolUseBlock::new(
            "call_1",
            "get_weather",
            serde_json::json!({"city": "Paris"})
                .as_object()
                .unwrap()
                .clone(),
        )));
        let msgs = convert(vec![msg], false);
        let calls = msgs[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        let function = calls[0].function.as_ref().unwrap();
        assert_eq!(function.name, "get_weather");
        let args: serde_json::Value =
            serde_json::from_str(function.arguments.as_deref().unwrap()).unwrap();
        assert_eq!(args["city"], serde_json::json!("Paris"));
        // Qwen thinking models require a present (possibly empty) content field.
        assert_eq!(msgs[0].content.as_ref().and_then(|c| c.as_text()), Some(""));
    }

    #[test]
    fn tool_result_becomes_tool_role_message() {
        let mut msg = Msg::new(
            "id",
            None,
            MsgRole::Tool,
            vec![ContentBlock::ToolResult(
                agentscope_core::ToolResultBlock::new(
                    "call_1",
                    "get_weather",
                    vec![ContentBlock::Text(TextBlock::new("sunny"))],
                ),
            )],
            None,
            None,
            None,
        );
        msg.role = MsgRole::Tool;
        let msgs = convert(vec![msg], false);
        assert_eq!(msgs[0].role.as_deref(), Some("tool"));
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msgs[0].name.as_deref(), Some("get_weather"));
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_text()),
            Some("sunny")
        );
    }

    #[test]
    fn base64_image_becomes_data_url_part_in_multimodal_mode() {
        let mut msg = Msg::user("look");
        msg.content
            .push(ContentBlock::Image(ImageBlock::new(Source::Base64(
                agentscope_core::Base64Source::new("image/png", "AAAA"),
            ))));
        let msgs = convert(vec![msg], false);
        match msgs[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].text.as_deref(), Some("look"));
                assert_eq!(
                    parts[1].image.as_deref(),
                    Some("data:image/png;base64,AAAA")
                );
            }
            other => panic!("expected parts, got {other:?}"),
        }
    }

    #[test]
    fn force_multimodal_uses_part_list_for_text_only_messages() {
        let msgs = convert(vec![user("hi")], true);
        match msgs[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].text.as_deref(), Some("hi"));
            }
            other => panic!("expected parts, got {other:?}"),
        }
    }

    #[test]
    fn multimodal_tool_result_keeps_its_media_parts() {
        // Java `DashScopeMultiAgentFormatter` / multimodal path: a tool result
        // carrying media must stay a `tool` message whose content is a part
        // list, not a flattened string (otherwise the image is lost).
        let result = agentscope_core::ToolResultBlock::new(
            "call_1",
            "get_image",
            vec![
                ContentBlock::Text(TextBlock::new("here")),
                ContentBlock::Image(ImageBlock::new(Source::Base64(
                    agentscope_core::Base64Source::new("image/png", "AAAA"),
                ))),
            ],
        );
        let mut msg = Msg::tool("call_1", "get_image", "");
        msg.content = vec![ContentBlock::ToolResult(result)];
        let msgs = convert(vec![msg], true);
        assert_eq!(msgs[0].role.as_deref(), Some("tool"));
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("call_1"));
        match msgs[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => {
                assert_eq!(parts[0].text.as_deref(), Some("here"));
                assert_eq!(
                    parts[1].image.as_deref(),
                    Some("data:image/png;base64,AAAA")
                );
            }
            other => panic!("expected parts, got {other:?}"),
        }
    }

    #[test]
    fn thinking_blocks_are_dropped() {
        let mut msg = Msg::user("x");
        msg.content
            .push(ContentBlock::Thinking(agentscope_core::ThinkingBlock::new(
                "secret",
            )));
        let msgs = convert(vec![msg], false);
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_text()),
            Some("x")
        );
        assert!(!serde_json::to_string(&msgs[0]).unwrap().contains("secret"));
    }

    #[test]
    fn tools_and_tool_choice_conversion() {
        let schema = ToolSchema::new("search", "search the web").parameters(
            serde_json::json!({"type": "object"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let tools = to_dashscope_tools(vec![schema]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "search");

        assert_eq!(
            to_tool_choice_value(&ToolChoice::Auto),
            serde_json::json!("auto")
        );
        assert_eq!(
            to_tool_choice_value(&ToolChoice::None),
            serde_json::json!("none")
        );
        assert_eq!(
            to_tool_choice_value(&ToolChoice::Required),
            serde_json::json!("auto")
        );
        assert_eq!(
            to_tool_choice_value(&ToolChoice::Specific {
                tool_name: "search".to_string()
            }),
            serde_json::json!({"type": "function", "function": {"name": "search"}})
        );
    }

    #[test]
    fn build_request_merges_options_and_marks_streaming() {
        let runtime = GenerateOptions {
            temperature: Some(0.2),
            stream: Some(true),
            ..Default::default()
        };
        let defaults = GenerateOptions {
            temperature: Some(0.9),
            max_tokens: Some(256),
            ..Default::default()
        };
        let req = build_request(
            "qwen-plus",
            convert(vec![user("hi")], false),
            true,
            &runtime,
            &defaults,
            Vec::new(),
            None,
            false,
        );
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["model"], serde_json::json!("qwen-plus"));
        assert_eq!(
            value["parameters"]["result_format"],
            serde_json::json!("message")
        );
        assert_eq!(
            value["parameters"]["incremental_output"],
            serde_json::json!(true)
        );
        assert_eq!(value["parameters"]["temperature"], serde_json::json!(0.2));
        assert_eq!(value["parameters"]["max_tokens"], serde_json::json!(256));
        assert_eq!(
            value["input"]["messages"][0]["content"],
            serde_json::json!("hi")
        );
    }

    #[test]
    fn build_request_maps_reasoning_effort() {
        let runtime = GenerateOptions {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let req = build_request(
            "qwen3-max",
            convert(vec![user("hi")], false),
            false,
            &runtime,
            &GenerateOptions::default(),
            Vec::new(),
            None,
            false,
        );
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value["parameters"]["reasoning_effort"],
            serde_json::json!("high")
        );

        // Absent by default.
        let req = build_request(
            "qwen3-max",
            convert(vec![user("hi")], false),
            false,
            &GenerateOptions::default(),
            &GenerateOptions::default(),
            Vec::new(),
            None,
            false,
        );
        let value = serde_json::to_value(&req).unwrap();
        assert!(value["parameters"].get("reasoning_effort").is_none());
    }

    #[test]
    fn metadata_cache_control_marks_content_not_message() {
        let msg = Msg::builder()
            .role(MsgRole::User)
            .content(vec![ContentBlock::Text(TextBlock::new("hello"))])
            .metadata_entry("_cache_control", serde_json::json!(true))
            .build();
        let msgs = convert(vec![msg], false);
        let value = serde_json::to_value(&msgs[0]).unwrap();
        // The marker must live inside the content block, not at message level.
        assert!(value.get("cache_control").is_none());
        assert_eq!(
            value["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert_eq!(value["content"][0]["text"], serde_json::json!("hello"));
    }

    #[test]
    fn automatic_cache_control_marks_final_and_system_messages() {
        let msgs = vec![
            Msg::builder()
                .role(MsgRole::System)
                .content(vec![ContentBlock::Text(TextBlock::new("sys"))])
                .build(),
            user("hello"),
        ];
        let options = GenerateOptions {
            cache_control: Some(true),
            ..Default::default()
        };
        let converted = convert_with_options(msgs, false, &options);
        let value = serde_json::to_value(&converted).unwrap();
        assert_eq!(
            value[0]["content"][0]["cache_control"]["type"],
            serde_json::json!("ephemeral")
        );
        assert_eq!(
            value[1]["content"][0]["cache_control"]["type"],
            serde_json::json!("ephemeral")
        );
    }

    #[test]
    fn automatic_cache_control_disabled_by_default() {
        let msgs = vec![user("hello")];
        let converted = convert_with_options(msgs, false, &GenerateOptions::default());
        let value = serde_json::to_value(&converted).unwrap();
        assert!(value[0]["content"][0].get("cache_control").is_none());
    }

    // ── Multi-agent formatting ──────────────────────────────────────────────

    fn named(text: &str, name: &str) -> Msg {
        Msg::builder()
            .role(agentscope_core::MsgRole::Assistant)
            .name(name)
            .text_content(text)
            .build()
    }

    #[test]
    fn multi_agent_text_mode_merges_groups_with_history_tags() {
        let messages = vec![
            Msg::system("be nice"),
            named("thinking", "Alice"),
            named("more", "Alice"),
            Msg::tool("call-1", "search", "42"),
        ];
        let out = convert_multi_agent(messages, false, "PROMPT", &GenerateOptions::default());
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role.as_deref(), Some("system"));
        assert_eq!(
            out[0].content.as_ref().and_then(|c| c.as_text()),
            Some("be nice")
        );
        assert_eq!(out[1].role.as_deref(), Some("user"));
        let merged = out[1].content.as_ref().and_then(|c| c.as_text()).unwrap();
        assert_eq!(
            merged,
            "PROMPT<history>\nAlice: thinking\nAlice: more\n</history>"
        );
        assert_eq!(out[2].role.as_deref(), Some("tool"));
        assert_eq!(out[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(
            out[2].content.as_ref().and_then(|c| c.as_text()),
            Some("42")
        );
    }

    #[test]
    fn multi_agent_text_mode_keeps_images_as_extra_parts() {
        let messages = vec![Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("look")),
            ContentBlock::Image(ImageBlock::new(Source::Base64(
                agentscope_core::Base64Source::new("image/png", "QUJD"),
            ))),
        ])];
        let out = convert_multi_agent(messages, false, "", &GenerateOptions::default());
        let parts = match out[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => parts,
            other => panic!("expected parts, got {other:?}"),
        };
        assert_eq!(parts.len(), 2);
        assert!(parts[0]
            .text
            .as_deref()
            .unwrap()
            .contains("Unknown: [Image]"));
        assert!(parts[1]
            .image
            .as_deref()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    #[test]
    fn multi_agent_multimodal_mode_flushes_text_around_media() {
        let messages = vec![Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("look")),
            ContentBlock::Image(ImageBlock::new(Source::Base64(
                agentscope_core::Base64Source::new("image/png", "QUJD"),
            ))),
            ContentBlock::Text(TextBlock::new("done")),
        ])];
        let out = convert_multi_agent(messages, true, "PROMPT", &GenerateOptions::default());
        let parts = match out[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => parts,
            other => panic!("expected parts, got {other:?}"),
        };
        // text(prompt+tag+first line) | image | text(second line + closing tag)
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts[0].text.as_deref(),
            Some("PROMPT<history>\nUnknown: look")
        );
        assert!(parts[1].image.is_some());
        assert_eq!(parts[2].text.as_deref(), Some("Unknown: done\n</history>"));
    }

    #[test]
    fn multi_agent_tool_group_keeps_assistant_tool_calls_and_results() {
        let assistant = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call-1",
            "search",
            agentscope_core::JsonMap::new(),
        ))]);
        let out = convert_multi_agent(
            vec![assistant, Msg::tool("call-1", "search", "42")],
            false,
            "PROMPT",
            &GenerateOptions::default(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role.as_deref(), Some("assistant"));
        assert_eq!(
            out[0].tool_calls.as_ref().unwrap()[0]
                .function
                .as_ref()
                .unwrap()
                .name,
            "search"
        );
        assert_eq!(out[1].role.as_deref(), Some("tool"));
    }

    // ---- Document input (qwen-long `fileid://`) ---------------------------

    fn document_msg(file_id: &str) -> Msg {
        Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("Summarise this")),
            ContentBlock::Document(
                agentscope_core::DocumentBlock::builder()
                    .source(Source::FileId(agentscope_core::FileIdSource::new(
                        "dashscope",
                        file_id,
                    )))
                    .name("report.pdf")
                    .mime_type("application/pdf")
                    .build(),
            ),
        ])
    }

    fn url_document_msg(url: &str, strategy: Option<&str>) -> Msg {
        let mut builder = agentscope_core::DocumentBlock::builder()
            .source(Source::Url(agentscope_core::URLSource::new(url)))
            .name("report.pdf")
            .mime_type("application/pdf");
        if let Some(strategy) = strategy {
            let mut metadata = agentscope_core::JsonMap::new();
            metadata.insert(
                "file_parsing_strategy".to_string(),
                serde_json::json!(strategy),
            );
            builder = builder.metadata(metadata);
        }
        Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("Summarise this")),
            ContentBlock::Document(builder.build()),
        ])
    }

    #[test]
    fn a_url_document_becomes_a_doc_url_part_on_the_user_turn() {
        let msgs = convert(
            vec![url_document_msg(
                "https://example.com/report.pdf",
                Some("text_only"),
            )],
            false,
        );
        assert_eq!(msgs.len(), 1);
        let parts = match msgs[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => parts,
            other => panic!("expected content parts, got {other:?}"),
        };
        assert_eq!(parts.len(), 2);
        // A document turn needs a type discriminator on every one of its parts.
        assert_eq!(parts[0].part_type.as_deref(), Some("text"));
        assert_eq!(parts[0].text.as_deref(), Some("Summarise this"));
        assert_eq!(parts[1].part_type.as_deref(), Some("doc_url"));
        assert_eq!(
            parts[1].doc_url.as_deref(),
            Some(["https://example.com/report.pdf".to_string()].as_slice())
        );
        assert_eq!(parts[1].file_parsing_strategy.as_deref(), Some("text_only"));
    }

    #[test]
    fn a_local_or_foreign_document_degrades_to_a_text_reference() {
        // `file://` cannot be fetched by DashScope: the turn keeps the content-part shape a URL
        // document asks for, but the part itself degrades to a reference (Java throws here; a
        // formatter that throws would break every later turn of the session).
        let local = convert(
            vec![url_document_msg("file:///tmp/report.pdf", None)],
            false,
        );
        let parts = match local[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => parts,
            other => panic!("expected content parts, got {other:?}"),
        };
        assert_eq!(parts.len(), 2);
        let reference = parts[1].text.as_deref().expect("degraded reference");
        assert!(reference.contains("[document: report.pdf"), "{reference}");

        // A file id issued by another provider has no DashScope channel either.
        let foreign = Msg::user_blocks(vec![ContentBlock::Document(
            agentscope_core::DocumentBlock::builder()
                .source(Source::FileId(agentscope_core::FileIdSource::new(
                    "openai", "file-1",
                )))
                .name("report.pdf")
                .build(),
        )]);
        let msgs = convert(vec![foreign], false);
        assert_eq!(msgs.len(), 1, "no fileid:// directive expected: {msgs:?}");
        let text = msgs[0].content.as_ref().and_then(|c| c.as_text()).unwrap();
        assert!(text.contains("provider file id: file-1"), "{text}");
    }

    #[test]
    fn more_than_ten_document_urls_are_rejected() {
        let messages: Vec<Msg> = (0..11)
            .map(|i| url_document_msg(&format!("https://example.com/{i}.pdf"), None))
            .collect();
        let error = to_dashscope_messages(messages, false).unwrap_err();
        assert!(
            error.to_string().contains("at most 10 document URLs"),
            "{error}"
        );
    }

    #[test]
    fn url_documents_are_rejected_on_the_multimodal_endpoint() {
        let error = to_dashscope_messages(
            vec![url_document_msg("https://example.com/report.pdf", None)],
            true,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("text-generation endpoint"),
            "{error}"
        );
    }

    #[test]
    fn a_merged_agent_group_keeps_the_document_as_a_doc_url_part() {
        let group = vec![Msg::builder()
            .role(MsgRole::User)
            .name("Alice")
            .content(vec![
                ContentBlock::Text(TextBlock::new("read this")),
                ContentBlock::Document(
                    agentscope_core::DocumentBlock::builder()
                        .source(Source::Url(agentscope_core::URLSource::new(
                            "https://example.com/report.pdf",
                        )))
                        .build(),
                ),
            ])
            .build()];
        let merged = merge_conversation(&group, "PROMPT\n");
        let parts = match merged.content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => parts,
            other => panic!("expected content parts, got {other:?}"),
        };
        assert!(parts[0].part_type.as_deref() == Some("text"));
        assert_eq!(parts[1].part_type.as_deref(), Some("doc_url"));
        assert!(parts[0]
            .text
            .as_deref()
            .unwrap()
            .contains("Alice: [Document]"));
    }

    #[test]
    fn document_block_emits_file_id_system_message_before_the_turn() {
        let msgs = convert(vec![document_msg("file-abc")], false);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role.as_deref(), Some("system"));
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_text()),
            Some("fileid://file-abc")
        );
        assert_eq!(msgs[1].role.as_deref(), Some("user"));
        // The turn stays self-describing: the file-id document is also named as a reference
        // (Java `DashScopeMessageConverter#extractTextContent`).
        let text = msgs[1]
            .content
            .as_ref()
            .and_then(|c| c.as_text())
            .expect("plain text turn");
        assert!(text.starts_with("Summarise this"), "{text}");
        assert!(
            text.contains("provider file id: file-abc (dashscope)"),
            "{text}"
        );
    }

    #[test]
    fn document_block_keeps_cache_directives_aligned() {
        let options = GenerateOptions::builder().cache_control(true).build();
        let msgs = convert_with_options(vec![document_msg("file-abc")], false, &options);
        assert_eq!(msgs.len(), 2);
        // The injected directive stays a plain string message: it is skipped by
        // automatic cache placement, so it is never rewritten into content parts.
        assert_eq!(msgs[0].role.as_deref(), Some("system"));
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_text()),
            Some("fileid://file-abc")
        );
        // The turn itself still receives the automatic marker.
        match msgs[1].content.as_ref().expect("content") {
            DashScopeContent::Parts(parts) => {
                assert!(parts.iter().any(|p| p.cache_control.is_some()), "{parts:?}");
            }
            other => panic!("expected marked parts, got {other:?}"),
        }
    }

    #[test]
    fn document_block_on_non_user_turn_is_left_alone() {
        // ASSISTANT/TOOL accept arbitrary blocks for back-compat; a document there
        // is aberrant input and must neither be mapped nor rejected.
        let assistant = Msg::assistant_blocks(vec![
            ContentBlock::Text(TextBlock::new("reply")),
            ContentBlock::Document(agentscope_core::DocumentBlock::new(Source::Url(
                agentscope_core::URLSource::new("https://example.com/a.pdf"),
            ))),
        ]);
        let msgs = convert(vec![assistant], false);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role.as_deref(), Some("assistant"));
    }

    #[test]
    fn a_base64_document_degrades_to_a_text_reference() {
        // A Base64 payload has no DashScope channel: it stays visible as a reference instead of
        // failing the whole turn (Java `appendDocumentMessages` throws here).
        let msg = Msg::user_blocks(vec![ContentBlock::Document(
            agentscope_core::DocumentBlock::builder()
                .source(Source::Base64(agentscope_core::Base64Source::new(
                    "application/pdf",
                    "QUJD",
                )))
                .name("report.pdf")
                .mime_type("application/pdf")
                .build(),
        )]);
        let msgs = convert(vec![msg], false);
        assert_eq!(msgs.len(), 1, "no fileid:// directive expected: {msgs:?}");
        let text = msgs[0].content.as_ref().and_then(|c| c.as_text()).unwrap();
        assert!(text.contains("inline base64"), "{text}");
    }

    #[test]
    fn multi_agent_document_block_emits_file_id_system_message() {
        let out = convert_multi_agent(
            vec![document_msg("file-abc")],
            false,
            "PROMPT",
            &GenerateOptions::default(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role.as_deref(), Some("system"));
        assert_eq!(
            out[0].content.as_ref().and_then(|c| c.as_text()),
            Some("fileid://file-abc")
        );
        assert_eq!(out[1].role.as_deref(), Some("user"));
        let merged = out[1].content.as_ref().and_then(|c| c.as_text()).unwrap();
        assert!(merged.contains("Summarise this"), "{merged}");
    }
}
