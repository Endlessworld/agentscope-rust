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
    DashScopeContent, DashScopeContentPart, DashScopeFunction, DashScopeInput, DashScopeMessage,
    DashScopeParameters, DashScopeRequest, DashScopeTool, DashScopeToolCall, DashScopeToolFunction,
};

/// DashScope native text-generation endpoint.
pub const TEXT_GENERATION_ENDPOINT: &str =
    "/api/v1/services/aigc/text-generation/generation";
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

/// Concatenates the text found in `TextBlock`/`HintBlock` (newline-joined).
fn simple_text(msg: &Msg) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            ContentBlock::Hint(h) => h.hint.clone(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn multimodal_parts(blocks: &[ContentBlock]) -> Vec<DashScopeContentPart> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(DashScopeContentPart::text(t.text.clone())),
            ContentBlock::Hint(h) => {
                parts.push(DashScopeContentPart::text(h.hint.clone().unwrap_or_default()))
            }
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
        }
    }
    if parts.is_empty() {
        parts.push(DashScopeContentPart::text(""));
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
            multimodal_parts(&tr.output)
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
        content: Some(DashScopeContent::Parts(multimodal_parts(&msg.content))),
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
) -> Vec<DashScopeMessage> {
    messages
        .into_iter()
        .map(|msg| {
            if force_multimodal || has_media_content(&msg) {
                multimodal_message(&msg)
            } else {
                simple_message(&msg)
            }
        })
        .collect()
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
        ToolChoice::Required => serde_json::json!("auto"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{HintBlock, TextBlock, ToolUseBlock};

    fn user(text: &str) -> Msg {
        Msg::user(text)
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
        let msgs = to_dashscope_messages(vec![user("hello")], false);
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
        let msgs = to_dashscope_messages(vec![msg], false);
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
        let msgs = to_dashscope_messages(vec![msg], false);
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
            vec![ContentBlock::ToolResult(agentscope_core::ToolResultBlock::new(
                "call_1",
                "get_weather",
                vec![ContentBlock::Text(TextBlock::new("sunny"))],
            ))],
            None,
            None,
            None,
        );
        msg.role = MsgRole::Tool;
        let msgs = to_dashscope_messages(vec![msg], false);
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
        msg.content.push(ContentBlock::Image(ImageBlock::new(
            Source::Base64(agentscope_core::Base64Source::new("image/png", "AAAA")),
        )));
        let msgs = to_dashscope_messages(vec![msg], false);
        match msgs[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].text.as_deref(), Some("look"));
                assert_eq!(parts[1].image.as_deref(), Some("data:image/png;base64,AAAA"));
            }
            other => panic!("expected parts, got {other:?}"),
        }
    }

    #[test]
    fn force_multimodal_uses_part_list_for_text_only_messages() {
        let msgs = to_dashscope_messages(vec![user("hi")], true);
        match msgs[0].content.as_ref().unwrap() {
            DashScopeContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].text.as_deref(), Some("hi"));
            }
            other => panic!("expected parts, got {other:?}"),
        }
    }

    #[test]
    fn thinking_blocks_are_dropped() {
        let mut msg = Msg::user("x");
        msg.content
            .push(ContentBlock::Thinking(agentscope_core::ThinkingBlock::new("secret")));
        let msgs = to_dashscope_messages(vec![msg], false);
        assert_eq!(msgs[0].content.as_ref().and_then(|c| c.as_text()), Some("x"));
        assert!(!serde_json::to_string(&msgs[0]).unwrap().contains("secret"));
    }

    #[test]
    fn tools_and_tool_choice_conversion() {
        let schema = ToolSchema::new("search", "search the web")
            .parameters(serde_json::json!({"type": "object"}).as_object().unwrap().clone());
        let tools = to_dashscope_tools(vec![schema]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "search");

        assert_eq!(to_tool_choice_value(&ToolChoice::Auto), serde_json::json!("auto"));
        assert_eq!(to_tool_choice_value(&ToolChoice::None), serde_json::json!("none"));
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
            to_dashscope_messages(vec![user("hi")], false),
            true,
            &runtime,
            &defaults,
            Vec::new(),
            None,
            false,
        );
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["model"], serde_json::json!("qwen-plus"));
        assert_eq!(value["parameters"]["result_format"], serde_json::json!("message"));
        assert_eq!(value["parameters"]["incremental_output"], serde_json::json!(true));
        assert_eq!(value["parameters"]["temperature"], serde_json::json!(0.2));
        assert_eq!(value["parameters"]["max_tokens"], serde_json::json!(256));
        assert_eq!(value["input"]["messages"][0]["content"], serde_json::json!("hi"));
    }

    #[test]
    fn build_request_enables_thinking_for_budget() {
        let runtime = GenerateOptions {
            thinking_budget: Some(1024),
            ..Default::default()
        };
        let req = build_request(
            "qwen3-max",
            to_dashscope_messages(vec![user("hi")], false),
            false,
            &runtime,
            &GenerateOptions::default(),
            Vec::new(),
            None,
            false,
        );
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["parameters"]["enable_thinking"], serde_json::json!(true));
        assert_eq!(value["parameters"]["thinking_budget"], serde_json::json!(1024));
    }
}
