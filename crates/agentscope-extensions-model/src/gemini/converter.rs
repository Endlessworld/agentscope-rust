//! Conversion between AgentScope [`Msg`]/[`ToolSchema`] and the Gemini REST wire
//! format.
//!
//! Merges the Java formatter classes:
//!
//! | Java | Rust |
//! |---|---|
//! | `GeminiChatFormatter` | [`build_request`] / [`apply_options`] |
//! | `GeminiMessageConverter` | [`to_gemini_contents`] |
//! | `GeminiToolsHelper` | [`to_gemini_tools`], [`to_tool_choice`] |
//! | `GeminiMediaConverter` | [`media_part`] |
//! | `GeminiConversationMerger` | [`merge_to_content`] |
//! | `GeminiMultiAgentFormatter` | [`to_multi_agent_contents`] |
//!
//! Gemini specifics the OpenAI shape does not have:
//! * no `system` role: system prompts are sent as a `user` content;
//! * tool results are **independent** `user` contents (each is its own turn);
//! * tool-call replay must carry back the `thoughtSignature` from metadata.

use agentscope_core::{
    determine_media_type, extract_text_content, file_to_base64, is_local_file, resolve_mime_type,
    AudioBlock, ContentBlock, DataBlock, GenerateOptions, HintBlock, ImageBlock, Msg, MsgRole,
    Result, Source, ToolChoice, ToolSchema, ToolUseBlock, VideoBlock,
};

use super::dto::{
    GeminiContent, GeminiFunctionCall, GeminiFunctionCallingConfig, GeminiFunctionDeclaration,
    GeminiFunctionResponse, GeminiGenerationConfig, GeminiPart, GeminiRequest, GeminiThinkingConfig,
    GeminiTool, GeminiToolConfig,
};

/// Marks the start of the merged history block (shared by merger + formatter).
pub const HISTORY_START_TAG: &str = "<history>";
/// Marks the end of the merged history block.
pub const HISTORY_END_TAG: &str = "</history>";

/// Default conversation-history preamble (identical to the Java constant).
pub const DEFAULT_CONVERSATION_HISTORY_PROMPT: &str = "# Conversation History\nThe content between <history></history> tags contains your conversation history\n";

/// Maps an AgentScope role to a Gemini role string.
///
/// Gemini has only `user` and `model`; `assistant` maps to `model` and every
/// other role (including `system` and `tool`) maps to `user`.
pub fn role_str(role: MsgRole) -> &'static str {
    match role {
        MsgRole::Assistant => "model",
        MsgRole::User | MsgRole::System | MsgRole::Tool => "user",
    }
}

/// Converts a media [`Source`] into a Gemini inline-data part.
///
/// Gemini accepts only inline bytes (or Files API URIs). Local paths are read
/// from disk; remote URLs and `oss://` URLs are rejected because the adapter
/// intentionally performs no network download (see `MODEL__GAP_TRACKER.md`
/// §3.2 P1 ``remote image download``).
pub fn media_part(source: &Source, media_type: &str) -> Result<GeminiPart> {
    match source {
        Source::Base64(b64) => {
            let mime = if b64.media_type.trim().is_empty() {
                format!("{media_type}/png")
            } else {
                b64.media_type.clone()
            };
            Ok(GeminiPart::inline(mime, b64.data.clone()))
        }
        Source::Url(url_source) => {
            let url = &url_source.url;
            if is_local_file(url) {
                let path = url.strip_prefix("file://").unwrap_or(url);
                Ok(GeminiPart::inline(
                    determine_media_type(path),
                    file_to_base64(path)?,
                ))
            } else {
                Err(agentscope_core::Error::Other(format!(
                    "Gemini adapter cannot download remote media '{url}'; provide a base64 source or a local path"
                )))
            }
        }
    }
}

/// Converts a data block, resolving its MIME type first.
pub fn data_part(block: &DataBlock) -> Result<GeminiPart> {
    let mime = resolve_mime_type(&block.source)?;
    match &block.source {
        Source::Base64(b64) => Ok(GeminiPart::inline(mime, b64.data.clone())),
        Source::Url(url_source) if is_local_file(&url_source.url) => {
            let path = url_source.url.strip_prefix("file://").unwrap_or(&url_source.url);
            Ok(GeminiPart::inline(mime, file_to_base64(path)?))
        }
        Source::Url(url_source) => Err(agentscope_core::Error::Other(format!(
            "Gemini adapter cannot download remote media '{}'; provide a base64 source or a local path",
            url_source.url
        ))),
    }
}

fn media_fallback(text: &str) -> GeminiPart {
    GeminiPart::text(text)
}

fn image_part(block: &ImageBlock) -> GeminiPart {
    match media_part(&block.source, "image") {
        Ok(part) => part,
        Err(e) => media_fallback(&format!("[Image - processing failed: {e}]")),
    }
}

fn audio_part(block: &AudioBlock) -> GeminiPart {
    match media_part(&block.source, "audio") {
        Ok(part) => part,
        Err(e) => media_fallback(&format!("[Audio - processing failed: {e}]")),
    }
}

fn video_part(block: &VideoBlock) -> GeminiPart {
    match media_part(&block.source, "video") {
        Ok(part) => part,
        Err(e) => media_fallback(&format!("[Video - processing failed: {e}]")),
    }
}

fn block_data_part(block: &DataBlock) -> GeminiPart {
    match data_part(block) {
        Ok(part) => part,
        Err(e) => media_fallback(&format!("[Data - processing failed: {e}]")),
    }
}

/// Resolves the JSON argument map of a tool call, preferring the raw `content`
/// string (mirroring `GeminiMessageConverter`: try to parse `content`, fall back
/// to the parsed `input` map).
pub fn tool_arguments(block: &ToolUseBlock) -> serde_json::Map<String, serde_json::Value> {
    if let Some(raw) = block.content.as_deref().filter(|c| !c.is_empty()) {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(raw) {
            return map;
        }
    }
    block.input.clone()
}

/// Converts a tool-use block into a `functionCall` part, preserving the Gemini
/// `thoughtSignature` carried in [`ToolUseBlock::METADATA_THOUGHT_SIGNATURE`].
pub fn tool_call_part(block: &ToolUseBlock) -> GeminiPart {
    let call = GeminiFunctionCall {
        id: (!block.id.is_empty()).then(|| block.id.clone()),
        name: block.name.clone(),
        args: tool_arguments(block),
    };
    let signature = block
        .metadata
        .get(ToolUseBlock::METADATA_THOUGHT_SIGNATURE)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    GeminiPart {
        function_call: Some(call),
        thought_signature: signature,
        ..Default::default()
    }
}

/// Converts a tool result into an **independent** `user` content.
pub fn tool_result_content(block: &ToolUseBlockResult) -> GeminiContent {
    let mut response = serde_json::Map::new();
    response.insert(
        "output".to_string(),
        serde_json::Value::String(agentscope_core::convert_tool_result_to_string(&block.output)),
    );
    let function_response = GeminiFunctionResponse {
        id: block.id.map(str::to_string),
        name: block.name.unwrap_or_default().to_string(),
        response,
    };
    GeminiContent {
        role: "user".to_string(),
        parts: vec![GeminiPart {
            function_response: Some(function_response),
            ..Default::default()
        }],
    }
}

/// Borrowed view of a tool result needed by [`tool_result_content`].
#[derive(Debug, Clone, Copy)]
pub struct ToolUseBlockResult<'a> {
    pub id: Option<&'a str>,
    pub name: Option<&'a str>,
    pub output: &'a [ContentBlock],
}

/// Converts a message list into Gemini contents.
///
/// Tool results become their own `user` contents inserted in place; a message is
/// emitted only when it still carries at least one part after that extraction
/// (Gemini rejects empty parts). `ThinkingBlock`s are never sent back to the API.
pub fn to_gemini_contents(messages: &[Msg]) -> Vec<GeminiContent> {
    let mut out: Vec<GeminiContent> = Vec::new();
    for msg in messages {
        let mut parts: Vec<GeminiPart> = Vec::new();
        let mut tool_results: Vec<GeminiContent> = Vec::new();

        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => parts.push(GeminiPart::text(t.text.clone())),
                ContentBlock::Hint(HintBlock { hint, .. }) => {
                    parts.push(GeminiPart::text(hint.clone().unwrap_or_default()))
                }
                ContentBlock::ToolUse(t) => parts.push(tool_call_part(t)),
                ContentBlock::ToolResult(tr) => tool_results.push(tool_result_content(&ToolUseBlockResult {
                    id: tr.id.as_deref(),
                    name: tr.name.as_deref(),
                    output: &tr.output,
                })),
                ContentBlock::Image(i) => parts.push(image_part(i)),
                ContentBlock::Audio(a) => parts.push(audio_part(a)),
                ContentBlock::Video(v) => parts.push(video_part(v)),
                ContentBlock::Data(d) => parts.push(block_data_part(d)),
                // Thinking blocks never go back to the API.
                ContentBlock::Thinking(_) => {}
            }
        }

        if !parts.is_empty() {
            out.push(GeminiContent {
                role: role_str(msg.role).to_string(),
                parts,
            });
        }
        out.extend(tool_results);
    }
    out
}

/// Converts tool schemas into a single Gemini `Tool` (or `None` when empty).
pub fn to_gemini_tools(tools: &[ToolSchema]) -> Option<GeminiTool> {
    if tools.is_empty() {
        return None;
    }
    let declarations = tools
        .iter()
        .map(|schema| GeminiFunctionDeclaration {
            name: schema.name.clone(),
            description: schema.description.clone(),
            parameters_json_schema: (!schema.parameters.is_empty())
                .then(|| serde_json::Value::Object(schema.parameters.clone())),
        })
        .collect();
    Some(GeminiTool { function_declarations: declarations })
}

/// Builds the Gemini `toolConfig`, or `None` for the default `AUTO` mode.
///
/// Mapping (per `GeminiToolsHelper`): `Auto → None` (server default),
/// `None → NONE`, `Required → ANY`, `Specific → ANY + allowedFunctionNames`.
pub fn to_tool_choice(choice: &ToolChoice) -> Option<GeminiToolConfig> {
    let (mode, names) = match choice {
        ToolChoice::Auto => return None,
        ToolChoice::None => ("NONE", None),
        ToolChoice::Required => ("ANY", None),
        ToolChoice::Specific { tool_name } => ("ANY", Some(vec![tool_name.clone()])),
    };
    Some(GeminiToolConfig {
        function_calling_config: GeminiFunctionCallingConfig {
            mode: mode.to_string(),
            allowed_function_names: names,
        },
    })
}

/// Applies generation options onto a `generationConfig`, with `default_options`
/// supplying the fallbacks (mirroring `GeminiChatFormatter#applyOptions`).
pub fn apply_options(
    options: &GenerateOptions,
    default_options: &GenerateOptions,
) -> GeminiGenerationConfig {
    let pick = |f: fn(&GenerateOptions) -> Option<f64>| f(options).or_else(|| f(default_options));
    let pick_u32 = |f: fn(&GenerateOptions) -> Option<u32>| f(options).or_else(|| f(default_options));
    let pick_i64 = |f: fn(&GenerateOptions) -> Option<i64>| f(options).or_else(|| f(default_options));

    let thinking_budget = pick_u32(|o| o.thinking_budget);
    GeminiGenerationConfig {
        temperature: pick(|o| o.temperature),
        top_p: pick(|o| o.top_p),
        top_k: pick_u32(|o| o.top_k).map(|v| v as f64),
        seed: pick_i64(|o| o.seed),
        max_output_tokens: pick_u32(|o| o.max_tokens),
        frequency_penalty: pick(|o| o.frequency_penalty),
        presence_penalty: pick(|o| o.presence_penalty),
        thinking_config: thinking_budget.map(|budget| GeminiThinkingConfig {
            include_thoughts: true,
            thinking_budget: budget,
        }),
    }
}

/// Assembles the full Gemini request body.
pub fn build_request(
    messages: &[Msg],
    tools: &[ToolSchema],
    options: &GenerateOptions,
    default_options: &GenerateOptions,
) -> GeminiRequest {
    let generation_config = apply_options(options, default_options);
    let tool_choice = options
        .tool_choice
        .as_ref()
        .or(default_options.tool_choice.as_ref())
        .and_then(to_tool_choice);

    GeminiRequest {
        contents: to_gemini_contents(messages),
        tools: to_gemini_tools(tools).map(|tool| vec![tool]),
        tool_config: tool_choice,
        generation_config: (!generation_config.is_empty()).then_some(generation_config),
    }
}

// ---------------------------------------------------------------------------
// Multi-agent formatting
// ---------------------------------------------------------------------------

/// Merges agent messages into a single `user` content wrapped in `<history>`
/// tags, mirroring `GeminiConversationMerger#mergeToContent`.
///
/// Text/hint blocks accumulate as `"<name>: <content>"`; tool results as
/// `"Tool: <name>\n<output>"`; thinking blocks are skipped. Media flushes the
/// pending text and appears as its own part; the prompt and open/close tags are
/// attached to the first/last part (inserting text parts when those are media).
pub fn merge_to_content(
    messages: &[Msg],
    history_prompt: &str,
) -> GeminiContent {
    let mut parts: Vec<GeminiPart> = Vec::new();
    let mut text: Vec<String> = Vec::new();

    fn flush(text: &mut Vec<String>, parts: &mut Vec<GeminiPart>) {
        if !text.is_empty() {
            parts.push(GeminiPart::text(text.join("\n")));
            text.clear();
        }
    }

    for msg in messages {
        let name = msg.name.clone().unwrap_or_else(|| "Unknown".to_string());
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => text.push(format!("{name}: {}", t.text)),
                ContentBlock::Hint(HintBlock { hint, .. }) => {
                    text.push(format!("{name}: {}", hint.clone().unwrap_or_default()))
                }
                ContentBlock::Thinking(_) => {}
                ContentBlock::ToolResult(tr) => {
                    let output = agentscope_core::convert_tool_result_to_string(&tr.output);
                    text.push(format!(
                        "Tool: {}\n{output}",
                        tr.name.clone().unwrap_or_default()
                    ));
                }
                ContentBlock::Image(i) => {
                    flush(&mut text, &mut parts);
                    parts.push(image_part(i));
                }
                ContentBlock::Audio(a) => {
                    flush(&mut text, &mut parts);
                    parts.push(audio_part(a));
                }
                ContentBlock::Video(v) => {
                    flush(&mut text, &mut parts);
                    parts.push(video_part(v));
                }
                ContentBlock::Data(d) => {
                    flush(&mut text, &mut parts);
                    parts.push(block_data_part(d));
                }
                ContentBlock::ToolUse(_) => {}
            }
        }
    }
    flush(&mut text, &mut parts);

    // Attach the prompt + opening tag, and the closing tag.
    if !parts.is_empty() {
        if parts[0].text.is_some() {
            let first = parts[0].text.take().unwrap_or_default();
            parts[0].text = Some(format!("{history_prompt}{HISTORY_START_TAG}{first}"));
        } else {
            parts.insert(0, GeminiPart::text(format!("{history_prompt}{HISTORY_START_TAG}")));
        }

        let last = parts.len() - 1;
        if let Some(text) = parts[last].text.take() {
            parts[last].text = Some(format!("{text}\n{HISTORY_END_TAG}"));
        } else {
            parts.push(GeminiPart::text(HISTORY_END_TAG));
        }
    }

    GeminiContent { role: "user".to_string(), parts }
}

/// Message group kind used by the multi-agent formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupType {
    AgentMessage,
    ToolSequence,
}

/// Converts messages with multi-agent support, mirroring
/// `GeminiMultiAgentFormatter#doFormat`: the system prompt becomes a `user`
/// content, runs of tool-related messages keep normal conversion, and runs of
/// plain agent messages merge into a single `<history>`-wrapped content.
pub fn to_multi_agent_contents(
    messages: &[Msg],
    conversation_history_prompt: &str,
) -> Vec<GeminiContent> {
    let mut result: Vec<GeminiContent> = Vec::new();
    let mut start = 0;

    if let Some(first) = messages.first() {
        if first.role == MsgRole::System {
            result.push(GeminiContent {
                role: "user".to_string(),
                parts: vec![GeminiPart::text(extract_text_content(first))],
            });
            start = 1;
        }
    }

    let mut is_first_agent_group = true;
    let mut current: Vec<Msg> = Vec::new();
    let mut current_type: Option<GroupType> = None;

    let flush = |group_type: GroupType,
                 group: &[Msg],
                 is_first: &mut bool,
                 out: &mut Vec<GeminiContent>| match group_type {
        GroupType::AgentMessage => {
            let prompt = if *is_first { conversation_history_prompt } else { "" };
            *is_first = false;
            out.push(merge_to_content(group, prompt));
        }
        GroupType::ToolSequence => out.extend(to_gemini_contents(group)),
    };

    for msg in messages.iter().skip(start) {
        let msg_type = if msg.role == MsgRole::Tool
            || msg.content.iter().any(|b| matches!(b, ContentBlock::ToolUse(_)))
            || msg.content.iter().any(|b| matches!(b, ContentBlock::ToolResult(_)))
        {
            GroupType::ToolSequence
        } else {
            GroupType::AgentMessage
        };

        match current_type {
            None => {
                current_type = Some(msg_type);
                current.push(msg.clone());
            }
            Some(kind) if kind == msg_type => current.push(msg.clone()),
            Some(kind) => {
                flush(kind, &current, &mut is_first_agent_group, &mut result);
                current.clear();
                current.push(msg.clone());
                current_type = Some(msg_type);
            }
        }
    }
    if !current.is_empty() {
        flush(
            current_type.unwrap_or(GroupType::AgentMessage),
            &current,
            &mut is_first_agent_group,
            &mut result,
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{Base64Source, JsonMap, TextBlock, ThinkingBlock, URLSource};

    fn tool_schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.to_string(),
            description: format!("{name} desc"),
            parameters: serde_json::json!({"type": "object"}).as_object().unwrap().clone(),
            output_schema: None,
            strict: None,
        }
    }

    #[test]
    fn system_message_becomes_user_content() {
        let contents = to_gemini_contents(&[Msg::system("be brief"), Msg::user("hi")]);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[0].parts[0].text.as_deref(), Some("be brief"));
        assert_eq!(contents[1].parts[0].text.as_deref(), Some("hi"));
    }

    #[test]
    fn assistant_maps_to_model_role() {
        let contents = to_gemini_contents(&[Msg::assistant("hello")]);
        assert_eq!(contents[0].role, "model");
    }

    #[test]
    fn tool_result_is_an_independent_user_content() {
        let messages = vec![
            Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                "call_1",
                "get_weather",
                JsonMap::new(),
            ))]),
            Msg::tool("call_1", "get_weather", "sunny"),
        ];
        let contents = to_gemini_contents(&messages);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1].role, "user");
        let response = contents[1].parts[0].function_response.as_ref().unwrap();
        assert_eq!(response.id.as_deref(), Some("call_1"));
        assert_eq!(response.name, "get_weather");
        assert_eq!(response.response["output"], serde_json::json!("sunny"));
    }

    #[test]
    fn tool_call_prefers_raw_content_json_and_keeps_signature() {
        let mut block = ToolUseBlock::new("call_1", "get_weather", JsonMap::new());
        block.content = Some("{\"city\":\"Paris\"}".into());
        block.metadata.insert(
            ToolUseBlock::METADATA_THOUGHT_SIGNATURE.to_string(),
            serde_json::json!("sig-1"),
        );
        let part = tool_call_part(&block);
        let call = part.function_call.unwrap();
        assert_eq!(call.args.get("city"), Some(&serde_json::json!("Paris")));
        assert_eq!(part.thought_signature.as_deref(), Some("sig-1"));
    }

    #[test]
    fn tool_call_falls_back_to_input_when_content_is_not_json() {
        let mut block = ToolUseBlock::new("call_1", "f", JsonMap::new());
        block.input.insert("a".to_string(), serde_json::json!(1));
        block.content = Some("not-json".into());
        let call = tool_call_part(&block).function_call.unwrap();
        assert_eq!(call.args.get("a"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn thinking_blocks_are_not_sent_back() {
        let contents = to_gemini_contents(&[Msg::assistant_blocks(vec![
            ContentBlock::Thinking(ThinkingBlock::new("reasoning")),
        ])]);
        assert!(contents.is_empty());
    }

    #[test]
    fn base64_media_becomes_inline_data() {
        let contents = to_gemini_contents(&[Msg::user_blocks(vec![ContentBlock::Image(
            ImageBlock::new(Source::Base64(Base64Source::new("image/png", "AAAA"))),
        )])]);
        let inline = contents[0].parts[0].inline_data.as_ref().unwrap();
        assert_eq!(inline.mime_type, "image/png");
        assert_eq!(inline.data, "AAAA");
    }

    #[test]
    fn remote_media_is_rejected_with_a_text_fallback() {
        let contents = to_gemini_contents(&[Msg::user_blocks(vec![ContentBlock::Image(
            ImageBlock::new(Source::Url(URLSource::new("https://x/a.png"))),
        )])]);
        let text = contents[0].parts[0].text.as_deref().unwrap();
        assert!(text.starts_with("[Image - processing failed:"), "got {text}");
        assert!(text.contains("cannot download remote media"));
    }

    #[test]
    fn local_media_is_read_as_inline_data() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("agentscope_gemini_{}.png", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"hello").unwrap();
        let contents = to_gemini_contents(&[Msg::user_blocks(vec![ContentBlock::Image(
            ImageBlock::new(Source::Url(URLSource::new(path.to_string_lossy().to_string()))),
        )])]);
        let inline = contents[0].parts[0].inline_data.as_ref().unwrap();
        assert_eq!(inline.mime_type, "image/png");
        assert_eq!(inline.data, "aGVsbG8=");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tools_are_converted_to_function_declarations() {
        let tool = to_gemini_tools(&[tool_schema("a"), tool_schema("b")]).unwrap();
        assert_eq!(tool.function_declarations.len(), 2);
        assert_eq!(tool.function_declarations[0].name, "a");
        assert_eq!(
            tool.function_declarations[0].parameters_json_schema,
            Some(serde_json::json!({"type": "object"}))
        );
        assert!(to_gemini_tools(&[]).is_none());
    }

    #[test]
    fn tool_choice_mapping_matches_java() {
        assert!(to_tool_choice(&ToolChoice::Auto).is_none());
        assert_eq!(
            to_tool_choice(&ToolChoice::None).unwrap().function_calling_config.mode,
            "NONE"
        );
        assert_eq!(
            to_tool_choice(&ToolChoice::Required).unwrap().function_calling_config.mode,
            "ANY"
        );
        let specific = to_tool_choice(&ToolChoice::Specific { tool_name: "f".into() }).unwrap();
        assert_eq!(specific.function_calling_config.mode, "ANY");
        assert_eq!(
            specific.function_calling_config.allowed_function_names,
            Some(vec!["f".to_string()])
        );
    }

    #[test]
    fn options_fall_back_to_defaults_and_thinking_is_wired() {
        let defaults = GenerateOptions {
            temperature: Some(0.2),
            top_k: Some(40),
            seed: Some(9),
            ..Default::default()
        };
        let options = GenerateOptions {
            temperature: Some(0.8),
            max_tokens: Some(2048),
            thinking_budget: Some(512),
            ..Default::default()
        };
        let config = apply_options(&options, &defaults);
        assert_eq!(config.temperature, Some(0.8));
        assert_eq!(config.top_k, Some(40.0));
        assert_eq!(config.seed, Some(9));
        assert_eq!(config.max_output_tokens, Some(2048));
        assert_eq!(config.thinking_config.unwrap().thinking_budget, 512);
    }

    #[test]
    fn build_request_omits_empty_generation_config() {
        let request = build_request(
            &[Msg::user("hi")],
            &[],
            &GenerateOptions::default(),
            &GenerateOptions::default(),
        );
        assert!(request.generation_config.is_none());
        assert!(request.tools.is_none());
        assert!(request.tool_config.is_none());
        assert_eq!(request.contents.len(), 1);
    }

    #[test]
    fn build_request_wires_tools_and_tool_choice() {
        let options = GenerateOptions {
            tool_choice: Some(ToolChoice::Specific { tool_name: "f".into() }),
            ..Default::default()
        };
        let request = build_request(&[Msg::user("hi")], &[tool_schema("f")], &options, &GenerateOptions::default());
        assert!(request.tools.is_some());
        assert_eq!(
            request.tool_config.unwrap().function_calling_config.allowed_function_names,
            Some(vec!["f".to_string()])
        );
    }

    #[test]
    fn merge_to_content_wraps_history_tags_and_names() {
        let messages = vec![
            Msg::builder().role(MsgRole::User).name("Alice").text_content("hi").build(),
            Msg::builder().role(MsgRole::Assistant).name("Bob").text_content("hello").build(),
        ];
        let content = merge_to_content(&messages, "PROMPT\n");
        assert_eq!(content.role, "user");
        let text = content.parts[0].text.as_deref().unwrap();
        assert!(text.starts_with("PROMPT\n<history>Alice: hi\nBob: hello"));
        assert!(text.ends_with("\n</history>"));
    }

    #[test]
    fn merge_to_content_inserts_tag_parts_around_media() {
        let messages = vec![Msg::user_blocks(vec![ContentBlock::Image(ImageBlock::new(
            Source::Base64(Base64Source::new("image/png", "AAAA")),
        ))])];
        let content = merge_to_content(&messages, "");
        // Opening tag text part, inline image, closing tag text part.
        assert_eq!(content.parts.len(), 3);
        assert_eq!(content.parts[0].text.as_deref(), Some("<history>"));
        assert!(content.parts[1].inline_data.is_some());
        assert_eq!(content.parts[2].text.as_deref(), Some("</history>"));
    }

    #[test]
    fn multi_agent_groups_tool_sequences_separately() {
        let messages = vec![
            Msg::system("be brief"),
            Msg::builder().role(MsgRole::User).name("Alice").text_content("hi").build(),
            Msg::builder().role(MsgRole::Assistant).name("Bob").text_content("hello").build(),
            Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                "call_1",
                "f",
                JsonMap::new(),
            ))]),
            Msg::tool("call_1", "f", "done"),
        ];
        let contents = to_multi_agent_contents(&messages, DEFAULT_CONVERSATION_HISTORY_PROMPT);
        // system(user) + merged agent conversation(user) + assistant tool_use(model) + tool result(user)
        assert_eq!(contents.len(), 4);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[0].parts[0].text.as_deref(), Some("be brief"));
        assert_eq!(contents[1].role, "user");
        assert!(contents[1].parts[0]
            .text
            .as_deref()
            .unwrap()
            .contains("Alice: hi"));
        assert_eq!(contents[2].role, "model");
        assert!(contents[3].parts[0].function_response.is_some());
    }

    #[test]
    fn multi_agent_without_system_still_merges_agents() {
        let messages = vec![Msg::builder()
            .role(MsgRole::User)
            .name("Alice")
            .text_content("hi")
            .build()];
        let contents = to_multi_agent_contents(&messages, DEFAULT_CONVERSATION_HISTORY_PROMPT);
        assert_eq!(contents.len(), 1);
        assert!(contents[0].parts[0]
            .text
            .as_deref()
            .unwrap()
            .contains("<history>"));
    }

    #[test]
    fn text_block_helper_used_by_multimodal_paths() {
        assert_eq!(GeminiPart::text("x").text.as_deref(), Some("x"));
        let _ = TextBlock::new("x");
    }
}
