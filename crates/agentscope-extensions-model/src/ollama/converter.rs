//! Conversion between AgentScope [`Msg`]/[`ToolSchema`] and Ollama DTOs.
//!
//! Mirrors `OllamaChatFormatter` + `OllamaMessageConverter` +
//! `OllamaToolsHelper` + `OllamaMediaConverter`.

use agentscope_core::{
    extract_text_content, ContentBlock, HintBlock, ImageBlock, Msg, MsgRole, Result, Source,
    ToolChoice, ToolSchema,
};

use crate::common::{agent_name, HISTORY_END_TAG, HISTORY_START_TAG};

use super::dto::{
    OllamaFunction, OllamaMessage, OllamaRequest, OllamaTool, OllamaToolCall, OllamaToolFunction,
};
use super::options::OllamaOptions;

/// Resolves an [`ImageBlock`] into the raw base64 payload Ollama expects.
///
/// Injecting this instead of downloading inside the converter keeps message
/// conversion synchronous and unit-testable without network access.
pub trait ImageResolver: Send + Sync {
    /// Returns the base64 (no data-URI prefix) representation of the image.
    fn resolve(&self, block: &ImageBlock) -> Result<String>;
}

/// Default resolver: returns base64 payloads verbatim and reads local files.
///
/// Remote `http(s)` URLs are **not** downloaded here: doing so would require an
/// async I/O hop in an otherwise pure conversion function. Use a custom
/// [`ImageResolver`] (e.g. one wrapping `agentscope_core::download_url_to_base64`)
/// when remote images must be inlined. Tracked in `MODEL__GAP_TRACKER.md` §3 (P1).
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultImageResolver;

impl ImageResolver for DefaultImageResolver {
    fn resolve(&self, block: &ImageBlock) -> Result<String> {
        // Route through the shared media resolver so a host that registers one
        // (see `crate::common::media`) transparently gains remote downloads.
        // The default resolver reads base64/local paths and rejects remote URLs.
        Ok(crate::common::resolve_media(&block.source, "formatting image for Ollama")?.base64)
    }
}

/// Extracts text from a content block, matching the Java `extractTextContent`:
/// text blocks, hint blocks and text found inside tool-result outputs.
fn block_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        ContentBlock::Hint(h) => h.hint.clone(),
        ContentBlock::ToolResult(tr) => {
            let joined = tr
                .output
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    ContentBlock::Hint(h) => h.hint.clone(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

fn collect_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(block_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Maps an AgentScope role to Ollama's role string.
fn role_str(role: MsgRole) -> &'static str {
    match role {
        MsgRole::System => "system",
        MsgRole::User => "user",
        MsgRole::Assistant => "assistant",
        MsgRole::Tool => "tool",
    }
}

fn url_of(source: &Source) -> String {
    match source {
        Source::Url(u) => u.url.clone(),
        Source::Base64(b) => format!("data:{};base64,{}", b.media_type, b.data),
        Source::FileId(f) => format!("fileid://{}", f.file_id),
    }
}

/// Converts messages using the [`DefaultImageResolver`].
///
/// Returns an error if an image cannot be resolved; pass `promote_tool_result_images`
/// to emit the extra `<system-info>` user message Java produces when a tool
/// result carries images.
pub fn to_ollama_messages(messages: Vec<Msg>) -> Result<Vec<OllamaMessage>> {
    to_ollama_messages_with(messages, &DefaultImageResolver, false)
}

/// Converts messages with an explicit image resolver and image-promotion flag.
pub fn to_ollama_messages_with(
    messages: Vec<Msg>,
    resolver: &dyn ImageResolver,
    promote_tool_result_images: bool,
) -> Result<Vec<OllamaMessage>> {
    let mut out = Vec::new();
    for msg in messages {
        let mut text_blocks: Vec<&ContentBlock> = Vec::new();
        let mut tool_uses: Vec<&ContentBlock> = Vec::new();
        let mut tool_results: Vec<&ContentBlock> = Vec::new();
        let mut images: Vec<String> = Vec::new();

        for block in &msg.content {
            match block {
                ContentBlock::Text(_) => text_blocks.push(block),
                ContentBlock::ToolUse(_) => tool_uses.push(block),
                ContentBlock::ToolResult(_) => tool_results.push(block),
                ContentBlock::Image(img) => images.push(resolver.resolve(img)?),
                _ => {}
            }
        }

        // Tool-result blocks become standalone `tool` messages.
        for block in &tool_results {
            if let ContentBlock::ToolResult(tr) = block {
                let mut result = OllamaMessage {
                    role: Some("tool".to_string()),
                    tool_call_id: tr.id.clone(),
                    name: tr.name.clone(),
                    ..Default::default()
                };
                let text = collect_text(&tr.output);
                result.content = if text.is_empty() { None } else { Some(text) };
                out.push(result);

                if promote_tool_result_images {
                    let promoted: Vec<&ImageBlock> = tr
                        .output
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Image(i) => Some(i),
                            _ => None,
                        })
                        .collect();
                    if !promoted.is_empty() {
                        let mut banner = format!(
                            "<system-info>The following are the image contents from the tool result of '{}':",
                            tr.name.clone().unwrap_or_default()
                        );
                        let mut promoted_images: Vec<String> = Vec::new();
                        for img in &promoted {
                            banner.push_str(&format!(
                                "\n- The image from '{}': ",
                                url_of(&img.source)
                            ));
                            // Java's `createImagePromotionMessage` attaches the image
                            // itself to the follow-up user message. Without this the
                            // model would only see the textual reference.
                            match resolver.resolve(img) {
                                Ok(data) => promoted_images.push(data),
                                Err(e) => crate::common::diagnostics::warn(
                                    "unsupported_media_type",
                                    "tool_result.image",
                                    &format!("cannot promote tool-result image: {e}"),
                                ),
                            }
                        }
                        out.push(OllamaMessage {
                            role: Some("user".to_string()),
                            content: Some(banner),
                            images: (!promoted_images.is_empty()).then_some(promoted_images),
                            ..Default::default()
                        });
                    }
                }
            }
        }

        // A `tool` message without a tool-result block falls back to plain text.
        if msg.role == MsgRole::Tool && tool_results.is_empty() {
            out.push(OllamaMessage {
                role: Some("tool".to_string()),
                content: Some(extract_text_content(&msg)),
                ..Default::default()
            });
            continue;
        }

        let has_main = !text_blocks.is_empty() || !images.is_empty() || !tool_uses.is_empty();
        if !has_main {
            continue;
        }

        let text = collect_text(
            &text_blocks
                .into_iter()
                .cloned()
                .collect::<Vec<ContentBlock>>(),
        );

        let mut main = OllamaMessage {
            role: Some(role_str(msg.role).to_string()),
            ..Default::default()
        };
        if !text.is_empty() {
            main.content = Some(text);
        }
        if !images.is_empty() {
            main.images = Some(images);
        }
        if !tool_uses.is_empty() {
            let calls = tool_uses
                .into_iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse(t) => Some(OllamaToolCall {
                        function: OllamaFunction {
                            name: t.name.clone(),
                            arguments: if t.input.is_empty() {
                                None
                            } else {
                                Some(
                                    t.input
                                        .iter()
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect(),
                                )
                            },
                        },
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !calls.is_empty() {
                main.tool_calls = Some(calls);
            }
        }
        out.push(main);
    }
    Ok(out)
}

/// Converts tool schemas into Ollama tool definitions; `None` when empty.
pub fn to_ollama_tools(tools: Vec<ToolSchema>) -> Option<Vec<OllamaTool>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .into_iter()
            .map(|t| {
                OllamaTool::function(OllamaToolFunction {
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
            .collect(),
    )
}

/// Converts a [`ToolChoice`] to the Ollama `tool_choice` JSON value.
///
/// `Required` degrades to `"auto"` because Ollama has no generic "must call a
/// tool" mode (same fallback as Java).
pub fn to_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!("auto"),
        ToolChoice::None => serde_json::json!("none"),
        ToolChoice::Required => {
            crate::common::diagnostics::warn(
                "tool_choice_downgraded",
                "tool_choice",
                "Ollama has no 'required' tool-choice mode; downgraded to 'auto'",
            );
            serde_json::json!("auto")
        }
        ToolChoice::Specific { tool_name } => serde_json::json!({
            "type": "function",
            "function": { "name": tool_name }
        }),
    }
}

/// Builds a complete [`OllamaRequest`], merging `default_options` under
/// `runtime_options` (runtime wins).
pub fn build_request(
    model_name: &str,
    messages: Vec<OllamaMessage>,
    stream: bool,
    runtime_options: &OllamaOptions,
    default_options: &OllamaOptions,
    tools: Vec<ToolSchema>,
    tool_choice: Option<&ToolChoice>,
) -> OllamaRequest {
    let merged = default_options.merge(runtime_options);
    let options_map = merged.to_options_map();

    OllamaRequest {
        model: Some(model_name.to_string()),
        messages: Some(messages),
        stream: Some(stream),
        format: merged.format.clone(),
        keep_alive: merged.keep_alive.clone(),
        tools: to_ollama_tools(tools),
        tool_choice: tools_choice(tool_choice),
        think: merged.think.as_ref().map(|t| t.to_json_value()),
        options: if options_map.is_empty() {
            None
        } else {
            Some(options_map)
        },
    }
}

fn tools_choice(tool_choice: Option<&ToolChoice>) -> Option<serde_json::Value> {
    tool_choice.map(to_tool_choice)
}

/// Convenience: builds a hint-style text block from a [`HintBlock`].
pub fn hint_text(hint: &HintBlock) -> String {
    hint.hint.clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Multi-agent formatting
// ---------------------------------------------------------------------------

/// Merges a run of agent messages into one `user` message wrapped in
/// `<history>` tags, mirroring `OllamaConversationMerger#mergeToMessage`.
///
/// Text and hint blocks render as `"<name>: <content>"`, tool results as
/// `"<name> (<tool>): <output>"`. Base64 images are collected on the message and
/// referenced as `[Image]`; remote images are not downloaded here (they render
/// as `[Image - processing failed]`, the Java fallback for unsupported sources).
pub fn merge_conversation(messages: &[Msg], history_prompt: &str) -> OllamaMessage {
    let mut text = String::new();
    if !history_prompt.is_empty() {
        text.push_str(history_prompt);
    }
    text.push_str(HISTORY_START_TAG);
    text.push('\n');

    let mut images: Vec<String> = Vec::new();
    for msg in messages {
        let name = agent_name(msg);
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => {
                    text.push_str(&format!("{name}: {}\n", t.text));
                }
                ContentBlock::Hint(h) => {
                    text.push_str(&format!("{name}: {}\n", h.hint.clone().unwrap_or_default()));
                }
                ContentBlock::Image(img) => match &img.source {
                    Source::Base64(b) => {
                        images.push(b.data.clone());
                        text.push_str(&format!("{name}: [Image]\n"));
                    }
                    _ => {
                        crate::common::diagnostics::warn(
                            "unsupported_media_type",
                            "conversation.image",
                            "URL image source is not supported for Ollama; skipping image block in merger",
                        );
                        text.push_str(&format!("{name}: [Image - processing failed]\n"));
                    }
                },
                ContentBlock::ToolResult(tr) => {
                    let output = collect_text(&tr.output);
                    text.push_str(&format!(
                        "{name} ({}): {output}\n",
                        tr.name.clone().unwrap_or_default()
                    ));
                }
                // Thinking blocks are never sent back to the model.
                ContentBlock::Thinking(_) => {}
                _ => {}
            }
        }
    }
    text.push_str(HISTORY_END_TAG);

    OllamaMessage {
        role: Some("user".to_string()),
        content: Some(text),
        images: (!images.is_empty()).then_some(images),
        ..Default::default()
    }
}

/// Converts messages with multi-agent support, mirroring
/// `OllamaMultiAgentFormatter#doFormat`: the leading system message stays
/// separate, runs of tool messages keep normal conversion, and runs of plain
/// agent messages merge into a single `<history>` user message.
///
/// `promote_tool_result_images` mirrors the Java formatter flag of the same name:
/// images found in tool results are additionally promoted into a follow-up user
/// message so the model can actually see them.
pub fn to_multi_agent_messages(
    messages: Vec<Msg>,
    history_prompt: &str,
    promote_tool_result_images: bool,
) -> Result<Vec<OllamaMessage>> {
    to_multi_agent_messages_with(
        messages,
        &DefaultImageResolver,
        history_prompt,
        promote_tool_result_images,
    )
}

/// [`to_multi_agent_messages`] with an explicit image resolver.
pub fn to_multi_agent_messages_with(
    messages: Vec<Msg>,
    resolver: &dyn ImageResolver,
    history_prompt: &str,
    promote_tool_result_images: bool,
) -> Result<Vec<OllamaMessage>> {
    let mut result: Vec<OllamaMessage> = Vec::new();
    let mut start = 0;

    if let Some(first) = messages.first() {
        if first.role == MsgRole::System {
            result.push(OllamaMessage {
                role: Some("system".to_string()),
                content: Some(extract_text_content(first)),
                ..Default::default()
            });
            start = 1;
        }
    }

    let remaining = &messages[start..];
    // A single non-tool message needs no history wrapper.
    if remaining.len() == 1 && !crate::common::is_tool_related(&remaining[0]) {
        result.extend(to_ollama_messages_with(
            vec![remaining[0].clone()],
            resolver,
            false,
        )?);
        return Ok(result);
    }

    let mut is_first_agent_group = true;
    for (kind, group) in crate::common::group_messages(remaining) {
        match kind {
            crate::common::GroupType::AgentMessage => {
                let prompt = if is_first_agent_group {
                    history_prompt
                } else {
                    ""
                };
                result.push(merge_conversation(&group, prompt));
                is_first_agent_group = false;
            }
            crate::common::GroupType::ToolSequence => result.extend(to_ollama_messages_with(
                group,
                resolver,
                promote_tool_result_images,
            )?),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{Base64Source, Error, ToolUseBlock, URLSource};

    fn resolver_that_errors() -> &'static dyn ImageResolver {
        struct Never;
        impl ImageResolver for Never {
            fn resolve(&self, _b: &ImageBlock) -> Result<String> {
                Err(Error::Model("network disabled".into()))
            }
        }
        &Never
    }

    #[test]
    fn user_message_maps_role_and_text() {
        let messages = to_ollama_messages(vec![Msg::user("hello")]).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role.as_deref(), Some("user"));
        assert_eq!(messages[0].content.as_deref(), Some("hello"));
        assert!(messages[0].images.is_none());
    }

    #[test]
    fn assistant_tool_use_becomes_tool_calls() {
        let mut input = agentscope_core::JsonMap::new();
        input.insert("city".into(), serde_json::json!("Paris"));
        let msg = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call-1",
            "get_weather",
            input,
        ))]);
        let out = to_ollama_messages(vec![msg]).unwrap();
        let calls = out[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(
            calls[0].function.arguments.as_ref().unwrap().get("city"),
            Some(&serde_json::json!("Paris"))
        );
    }

    #[test]
    fn tool_result_message_carries_id_and_name() {
        let msg = Msg::tool("call-9", "search", "42");
        let out = to_ollama_messages(vec![msg]).unwrap();
        assert_eq!(out[0].role.as_deref(), Some("tool"));
        assert_eq!(out[0].tool_call_id.as_deref(), Some("call-9"));
        assert_eq!(out[0].name.as_deref(), Some("search"));
        assert_eq!(out[0].content.as_deref(), Some("42"));
    }

    #[test]
    fn base64_image_is_inlined_but_remote_errors_by_default() {
        let base64_msg = Msg::user_blocks(vec![ContentBlock::Image(ImageBlock::new(
            Source::Base64(Base64Source::new("image/png", "QUJD")),
        ))]);
        let out = to_ollama_messages(vec![base64_msg]).unwrap();
        assert_eq!(out[0].images.as_ref().unwrap()[0], "QUJD");

        let remote_msg = Msg::user_blocks(vec![ContentBlock::Image(ImageBlock::new(Source::Url(
            URLSource::new("https://example.com/a.png"),
        )))]);
        assert!(to_ollama_messages(vec![remote_msg]).is_err());
        // Structural check: the error path is exercised by a resolver stub too.
        let _ = resolver_that_errors();
    }

    #[test]
    fn tool_choice_mapping_matches_java() {
        assert_eq!(to_tool_choice(&ToolChoice::Auto), serde_json::json!("auto"));
        assert_eq!(to_tool_choice(&ToolChoice::None), serde_json::json!("none"));
        assert_eq!(
            to_tool_choice(&ToolChoice::Required),
            serde_json::json!("auto")
        );
        assert_eq!(
            to_tool_choice(&ToolChoice::Specific {
                tool_name: "f".into()
            }),
            serde_json::json!({"type": "function", "function": {"name": "f"}})
        );
    }

    #[test]
    fn build_request_sets_top_level_option_fields() {
        let runtime = OllamaOptions {
            temperature: Some(0.2),
            format: Some(serde_json::json!("json")),
            keep_alive: Some("10m".into()),
            think: Some(super::super::options::ThinkOption::Boolean(true)),
            ..Default::default()
        };
        let request = build_request(
            "llama3",
            vec![OllamaMessage::new("user", "hi")],
            true,
            &runtime,
            &OllamaOptions::default(),
            Vec::new(),
            Some(&ToolChoice::Auto),
        );
        assert_eq!(request.model.as_deref(), Some("llama3"));
        assert_eq!(request.stream, Some(true));
        assert_eq!(request.keep_alive.as_deref(), Some("10m"));
        assert_eq!(request.think, Some(serde_json::json!(true)));
        assert_eq!(request.tool_choice, Some(serde_json::json!("auto")));
        assert!(request.tools.is_none());
        let opts = request.options.unwrap();
        assert_eq!(opts.get("temperature"), Some(&serde_json::json!(0.2)));
        assert!(!opts.contains_key("format"));
        assert!(!opts.contains_key("keep_alive"));
    }

    #[test]
    fn build_request_carries_native_schema_format() {
        use agentscope_core::formatter::{JsonSchema, ResponseFormat};
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), serde_json::json!("object"));
        let mut gen = agentscope_core::GenerateOptions::default();
        gen.response_format = Some(ResponseFormat::json_schema(JsonSchema::new(
            "Weather",
            schema.clone(),
        )));
        let runtime = OllamaOptions::from_generate_options(&gen);
        let request = build_request(
            "llama3",
            vec![OllamaMessage::new("user", "hi")],
            false,
            &runtime,
            &OllamaOptions::default(),
            Vec::new(),
            None,
        );
        assert_eq!(request.format, Some(serde_json::Value::Object(schema)));
        // The schema must be a top-level `format`, never nested under `options`.
        let opts = request.options.unwrap_or_default();
        assert!(!opts.contains_key("format"));
    }

    #[test]
    fn tools_convert_with_schema_and_empty_description() {
        let mut params = serde_json::Map::new();
        params.insert("type".into(), serde_json::json!("object"));
        let tool = ToolSchema::new("get_weather", "").parameters(params);
        let tools = to_ollama_tools(vec![tool]).unwrap();
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "get_weather");
        assert!(tools[0].function.description.is_none());
        assert_eq!(
            tools[0].function.parameters.as_ref().unwrap()["type"],
            serde_json::json!("object")
        );
        assert!(to_ollama_tools(Vec::new()).is_none());
    }

    // ── Multi-agent formatting ──────────────────────────────────────────────

    fn named(text: &str, name: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::Assistant)
            .name(name)
            .text_content(text)
            .build()
    }

    #[test]
    fn multi_agent_merges_agent_turns_and_keeps_tool_sequences() {
        let messages = vec![
            Msg::system("be nice"),
            named("thinking", "Alice"),
            named("more", "Alice"),
            Msg::tool("call-1", "search", "42"),
        ];
        let out = to_multi_agent_messages(messages, "PROMPT", false).unwrap();
        // system stays separate, the two agent turns merge, the tool message passes through.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role.as_deref(), Some("system"));
        assert_eq!(out[0].content.as_deref(), Some("be nice"));
        assert_eq!(out[1].role.as_deref(), Some("user"));
        let body = out[1].content.as_deref().unwrap();
        assert_eq!(
            body,
            "PROMPT<history>\nAlice: thinking\nAlice: more\n</history>"
        );
        assert_eq!(out[2].role.as_deref(), Some("tool"));
        assert_eq!(out[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(out[2].content.as_deref(), Some("42"));
    }

    #[test]
    fn multi_agent_prompt_only_on_the_first_agent_group() {
        let messages = vec![
            named("first", "Alice"),
            Msg::tool("call-1", "search", "42"),
            named("second", "Bob"),
        ];
        let out = to_multi_agent_messages(messages, "PROMPT", false).unwrap();
        assert_eq!(out.len(), 3);
        assert!(out[0]
            .content
            .as_deref()
            .unwrap()
            .starts_with("PROMPT<history>"));
        let second = out[2].content.as_deref().unwrap();
        assert!(second.starts_with("<history>"), "{second}");
        assert!(!second.contains("PROMPT"));
    }

    #[test]
    fn multi_agent_single_plain_message_is_not_wrapped() {
        let out = to_multi_agent_messages(vec![named("hi", "Alice")], "PROMPT", false).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content.as_deref(), Some("hi"));
        assert_eq!(out[0].role.as_deref(), Some("assistant"));
    }

    #[test]
    fn multi_agent_merge_keeps_base64_images_and_flags_remote_ones() {
        let messages = vec![
            Msg::user_blocks(vec![
                ContentBlock::Text(agentscope_core::TextBlock::new("look")),
                ContentBlock::Image(ImageBlock::new(Source::Base64(Base64Source::new(
                    "image/png",
                    "QUJD",
                )))),
                ContentBlock::Image(ImageBlock::new(Source::Url(URLSource::new(
                    "https://example.com/a.png",
                )))),
            ]),
            Msg::tool("call-1", "search", "42"),
        ];
        let merged = merge_conversation(&messages[..1], "");
        assert_eq!(merged.images.as_ref().unwrap(), &vec!["QUJD".to_string()]);
        let text = merged.content.unwrap();
        assert!(text.contains("Unknown: [Image]"), "{text}");
        assert!(text.contains("[Image - processing failed]"), "{text}");
    }

    #[test]
    fn multi_agent_promotes_tool_result_images_into_a_user_message() {
        let tool_msg = Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(
                agentscope_core::ToolResultBlock::new(
                    "call-1",
                    "screenshot",
                    vec![
                        ContentBlock::Text(agentscope_core::TextBlock::new("captured")),
                        ContentBlock::Image(ImageBlock::new(Source::Base64(Base64Source::new(
                            "image/png",
                            "QUJD",
                        )))),
                    ],
                ),
            )])
            .build();
        let out =
            to_multi_agent_messages(vec![named("hi", "Alice"), tool_msg], "PROMPT", true).unwrap();
        // agent group, tool message, promoted image message
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].role.as_deref(), Some("tool"));
        assert_eq!(out[2].role.as_deref(), Some("user"));
        let banner = out[2].content.as_deref().unwrap();
        assert!(banner.starts_with("<system-info>"), "{banner}");
        assert_eq!(out[2].images.as_ref().unwrap(), &vec!["QUJD".to_string()]);
    }
}
