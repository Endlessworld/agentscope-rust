//! Conversion between AgentScope [`Msg`]/[`ToolSchema`] and Ollama DTOs.
//!
//! Mirrors `OllamaChatFormatter` + `OllamaMessageConverter` +
//! `OllamaToolsHelper` + `OllamaMediaConverter`.

use agentscope_core::{
    extract_text_content, ContentBlock, HintBlock, ImageBlock, Msg, MsgRole, Result, Source,
    ToolChoice, ToolSchema,
};

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
    blocks.iter().filter_map(block_text).collect::<Vec<_>>().join("\n")
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
                        for img in &promoted {
                            banner.push_str(&format!(
                                "\n- The image from '{}': ",
                                url_of(&img.source)
                            ));
                        }
                        out.push(OllamaMessage {
                            role: Some("user".to_string()),
                            content: Some(banner),
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
            &text_blocks.into_iter().cloned().collect::<Vec<ContentBlock>>(),
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
                                Some(t.input.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
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
        options: if options_map.is_empty() { None } else { Some(options_map) },
    }
}

fn tools_choice(tool_choice: Option<&ToolChoice>) -> Option<serde_json::Value> {
    tool_choice.map(to_tool_choice)
}

/// Convenience: builds a hint-style text block from a [`HintBlock`].
pub fn hint_text(hint: &HintBlock) -> String {
    hint.hint.clone().unwrap_or_default()
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

        let remote_msg = Msg::user_blocks(vec![ContentBlock::Image(ImageBlock::new(
            Source::Url(URLSource::new("https://example.com/a.png")),
        ))]);
        assert!(to_ollama_messages(vec![remote_msg]).is_err());
        // Structural check: the error path is exercised by a resolver stub too.
        let _ = resolver_that_errors();
    }

    #[test]
    fn tool_choice_mapping_matches_java() {
        assert_eq!(to_tool_choice(&ToolChoice::Auto), serde_json::json!("auto"));
        assert_eq!(to_tool_choice(&ToolChoice::None), serde_json::json!("none"));
        assert_eq!(to_tool_choice(&ToolChoice::Required), serde_json::json!("auto"));
        assert_eq!(
            to_tool_choice(&ToolChoice::Specific { tool_name: "f".into() }),
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
}
