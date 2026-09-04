//! OpenAI-compatible chat model implemented on top of the open-source
//! `async-openai` crate (v0.41, remote crates.io dependency).
//!
//! This is the first **real** provider: `Model::stream` calls
//! `chat().create_stream()` and maps the SSE `delta` chunks into
//! [`crate::model::ChatResponse`]/[`ContentBlock`]. Because `async-openai`
//! speaks the OpenAI protocol, this single provider also serves any
//! OpenAI-compatible gateway (DeepSeek/Kimi/GLM/Ollama/... ) by overriding the
//! base URL and API key.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;

use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionNamedToolChoice, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestToolMessage, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
    ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionTools,
    ChatCompletionToolChoiceOption, CompletionUsage, CreateChatCompletionRequest, FinishReason,
    FunctionCall, FunctionCallStream, FunctionName, FunctionObject, ReasoningEffort,
    ResponseFormat as OaiResponseFormat, ResponseFormatJsonSchema,
    ToolChoiceOptions,
};
use async_openai::Client;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};

use crate::error::{Error, Result};
use crate::message::{ChatUsage, ContentBlock, JsonMap, Msg, MsgRole, TextBlock, ThinkingBlock, ToolUseBlock};
use crate::model::context_windows::resolve_context_window;
use crate::model::generate_options::GenerateOptions;
use crate::model::model::{Model, ModelCapabilities};
use crate::model::provider::{
    DEFAULT_PROVIDER_ID, ModelCreationContext, ModelProvider, ModelRegistry,
};
use crate::model::tool_choice::ToolChoice;
use crate::model::tool_schema::ToolSchema;
use crate::formatter::{
    convert_tool_result_to_string, extract_text_content, ResponseFormat,
};

/// An OpenAI-compatible chat model backed by `async-openai`.
pub struct OpenAiChatModel {
    client: Client<OpenAIConfig>,
    model_name: String,
    capabilities: ModelCapabilities,
}

impl OpenAiChatModel {
    /// Creates a model from an explicit model id, api key and base URL.
    pub fn new(
        model_name: impl Into<String>,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self> {
        let model_name = model_name.into();
        let mut cfg = OpenAIConfig::new();
        if let Some(key) = api_key {
            cfg = cfg.with_api_key(key);
        }
        if let Some(base) = base_url {
            cfg = cfg.with_api_base(base);
        }
        let client = Client::with_config(cfg);
        let context = resolve_context_window(&model_name);
        Ok(Self {
            client,
            capabilities: ModelCapabilities::new().context_window_size(context),
            model_name,
        })
    }

    /// Creates a model from a [`ModelCreationContext`].
    pub fn from_context(ctx: &ModelCreationContext) -> Result<Self> {
        let model_name = if ctx.model.is_empty() {
            ctx.options
                .model_name
                .clone()
                .unwrap_or_else(|| "gpt-4o".to_string())
        } else {
            ctx.model.clone()
        };

        let mut cfg = OpenAIConfig::new();
        if let Some(key) = &ctx.api_key {
            cfg = cfg.with_api_key(key);
        }
        if let Some(base) = &ctx.base_url {
            cfg = cfg.with_api_base(base);
        }
        // Merge per-request additional headers into the client config.
        for (k, v) in &ctx.options.additional_headers {
            let name = reqwest::header::HeaderName::from_str(k)
                .map_err(|e| Error::Model(format!("invalid header name '{k}': {e}")))?;
            cfg = cfg.with_header(name.clone(), v.as_str()).map_err(|e| {
                Error::Model(format!("failed to set header '{k}': {e}"))
            })?;
        }
        let client = Client::with_config(cfg);
        let context = resolve_context_window(&model_name);
        Ok(Self {
            client,
            capabilities: ModelCapabilities::new().context_window_size(context),
            model_name,
        })
    }

    /// Access the underlying async-openai client (e.g. for retry middleware).
    pub fn client(&self) -> &Client<OpenAIConfig> {
        &self.client
    }
}

#[async_trait::async_trait]
impl Model for OpenAiChatModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, Result<crate::model::ChatResponse>> {
        let client = self.client.clone();
        let model_name = options
            .model_name
            .clone()
            .unwrap_or_else(|| self.model_name.clone());
        // Vendor-specific body params (e.g. `enable_thinking`) must reach the
        // wire: `CreateChatCompletionRequest` has no extension field, so the
        // typed request is serialized to a JSON object and the extra params
        // are merged in before the BYOT call.
        let extra_body = options.additional_body_params.clone();
        let request = build_request(model_name, messages, tools, options);
        // Serialize eagerly; on failure fall back to the typed request so the
        // call still proceeds (this can only fail on non-string map keys).
        let body: serde_json::Value = match serde_json::to_value(&request) {
            Ok(mut v) => {
                if !extra_body.is_empty() {
                    if let Some(obj) = v.as_object_mut() {
                        for (k, v) in extra_body {
                            obj.insert(k, v);
                        }
                    }
                }
                v
            }
            Err(_) => serde_json::to_value(&request).unwrap_or(serde_json::Value::Null),
        };

        // create_stream_byot lets us deserialize each SSE chunk into a custom
        // type that also captures `reasoning_content`, so thinking deltas can be
        // streamed incrementally instead of being swallowed by the default
        // `CreateChatCompletionStreamResponse` (which has no reasoning field).
        let setup = futures::stream::once(async move {
            client
                .chat()
                .create_stream_byot::<_, OpenAiStreamChunk>(body)
                .await
                .map_err(map_openai_error)
        });
        let chunks = setup
            .map_ok(|s| s.map(|item| item.map_err(map_openai_error)))
            .try_flatten()
            .boxed();

        // Yield one incremental `ChatResponse` per SSE chunk: text/thinking
        // deltas are emitted immediately (token-by-token), while tool-call
        // fragments are accumulated and flushed as a single block when the
        // stream ends. This preserves true streaming for consumers that render
        // `TextBlockDelta`/`ThinkingBlockDelta` events as they arrive.
        let stream = futures::stream::unfold((chunks, StreamIncrementer::default()), |state| {
            let (mut chunks, mut acc) = state;
            async move {
                loop {
                    match chunks.next().await {
                        Some(Ok(chunk)) => {
                            if let Some(resp) = acc.accumulate(&chunk) {
                                return Some((Ok(resp), (chunks, acc)));
                            }
                            // No incremental content in this chunk; keep polling.
                        }
                        Some(Err(e)) => return Some((Err(e), (chunks, acc))),
                        None => {
                            let final_resp = acc.into_final();
                            match final_resp {
                                Some(resp) => {
                                    return Some((Ok(resp), (chunks, StreamIncrementer::default())))
                                }
                                None => return None,
                            }
                        }
                    }
                }
            }
        });

        Box::pin(stream)
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn context_window_size(&self) -> usize {
        self.capabilities.context_window_size
    }

    fn supports_native_structured_output(&self) -> bool {
        self.capabilities.native_structured_output
    }

    fn supports_native_structured_output_with_tools(&self) -> bool {
        self.capabilities.native_structured_output_with_tools
    }
}

/// Provider that registers [`OpenAiChatModel`] under the `openai` provider id.
pub struct OpenAiProvider;

impl ModelProvider for OpenAiProvider {
    fn provider_id(&self) -> &str {
        DEFAULT_PROVIDER_ID
    }
    fn supports(&self, _model: &str) -> bool {
        // OpenAI-compatible gateway: accepts any model id; the base URL may be
        // redirected to DeepSeek/Kimi/GLM/Ollama etc.
        true
    }
    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        let model = OpenAiChatModel::from_context(&ctx)?;
        Ok(Arc::new(model))
    }
}

/// Builds a registry pre-populated with the default OpenAI-compatible provider.
pub fn default_registry() -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    registry.register(Arc::new(OpenAiProvider));
    registry
}

fn map_openai_error(e: OpenAIError) -> Error {
    match e {
        OpenAIError::ApiError(a) => Error::HttpStatus {
            status: a.status_code.as_u16(),
            message: a.api_error.message,
        },
        other => Error::Model(format!("{other}")),
    }
}

#[allow(deprecated)] // async-openai marks `seed` deprecated; we keep it for parity.
fn build_request(
    model: String,
    messages: Vec<Msg>,
    tools: Vec<ToolSchema>,
    options: GenerateOptions,
) -> CreateChatCompletionRequest {
    let request_tools = to_request_tools(tools);
    let has_tools = request_tools.is_some();

    let mut req = CreateChatCompletionRequest {
        model,
        messages: to_request_messages(messages),
        stream: Some(true),
        stream_options: Some(ChatCompletionStreamOptions {
            include_usage: Some(true),
            include_obfuscation: None,
        }),
        tools: request_tools,
        ..Default::default()
    };

    if let Some(tc) = &options.tool_choice {
        if has_tools {
            req.tool_choice = to_tool_choice(tc);
        }
    }
    if let Some(t) = options.temperature {
        req.temperature = Some(t as f32);
    }
    if let Some(p) = options.top_p {
        req.top_p = Some(p as f32);
    }
    if let Some(n) = options.max_completion_tokens {
        req.max_completion_tokens = Some(n);
    } else if let Some(n) = options.max_tokens {
        req.max_completion_tokens = Some(n);
    }
    if let Some(f) = options.frequency_penalty {
        req.frequency_penalty = Some(f as f32);
    }
    if let Some(p) = options.presence_penalty {
        req.presence_penalty = Some(p as f32);
    }
    if let Some(s) = options.seed {
        req.seed = Some(s);
    }
    if let Some(p) = options.parallel_tool_calls {
        req.parallel_tool_calls = Some(p);
    }
    if let Some(rc) = options.reasoning_effort.as_deref() {
        req.reasoning_effort = to_reasoning_effort(rc);
    }
    if let Some(rf) = &options.response_format {
        req.response_format = to_response_format(rf);
    }
    req
}

fn to_request_messages(messages: Vec<Msg>) -> Vec<ChatCompletionRequestMessage> {
    let mut out = Vec::new();
    for msg in messages {
        match msg.role {
            MsgRole::System => {
                out.push(ChatCompletionRequestMessage::System(
                    ChatCompletionRequestSystemMessage {
                        content: ChatCompletionRequestSystemMessageContent::Text(
                            extract_text_content(&msg),
                        ),
                        name: None,
                    },
                ));
            }
            MsgRole::User => {
                out.push(ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(extract_text_content(&msg)),
                    name: None,
                }));
            }
            MsgRole::Assistant => {
                let text = extract_text_content(&msg);
                let mut tool_calls = Vec::new();
                for tu in msg.tool_use_blocks() {
                    let args = serde_json::to_string(&tu.input).unwrap_or_default();
                    tool_calls.push(ChatCompletionMessageToolCalls::Function(
                        ChatCompletionMessageToolCall {
                            id: tu.id.clone(),
                            function: FunctionCall {
                                name: tu.name.clone(),
                                arguments: args,
                            },
                        },
                    ));
                }
                let content = if text.is_empty() {
                    None
                } else {
                    Some(ChatCompletionRequestAssistantMessageContent::Text(text))
                };
                out.push(ChatCompletionRequestMessage::Assistant(
                    ChatCompletionRequestAssistantMessage {
                        content,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        ..Default::default()
                    },
                ));
            }
            MsgRole::Tool => {
                for tr in msg.tool_result_blocks() {
                    let text = convert_tool_result_to_string(&tr.output);
                    if let Some(id) = &tr.id {
                        out.push(ChatCompletionRequestMessage::Tool(
                            ChatCompletionRequestToolMessage {
                                content: ChatCompletionRequestToolMessageContent::Text(text),
                                tool_call_id: id.clone(),
                            },
                        ));
                    }
                }
            }
        }
    }
    out
}

fn to_request_tools(tools: Vec<ToolSchema>) -> Option<Vec<ChatCompletionTools>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .into_iter()
            .map(|t| {
                let parameters = if t.parameters.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Object(t.parameters))
                };
                let description = if t.description.is_empty() {
                    None
                } else {
                    Some(t.description)
                };
                ChatCompletionTools::Function(ChatCompletionTool {
                    function: FunctionObject {
                        name: t.name,
                        description,
                        parameters,
                        strict: t.strict,
                    },
                })
            })
            .collect(),
    )
}

fn to_tool_choice(tc: &ToolChoice) -> Option<ChatCompletionToolChoiceOption> {
    match tc {
        ToolChoice::Auto => Some(ChatCompletionToolChoiceOption::Mode(ToolChoiceOptions::Auto)),
        ToolChoice::None => Some(ChatCompletionToolChoiceOption::Mode(ToolChoiceOptions::None)),
        ToolChoice::Required => {
            Some(ChatCompletionToolChoiceOption::Mode(ToolChoiceOptions::Required))
        }
        ToolChoice::Specific { tool_name } => Some(ChatCompletionToolChoiceOption::Function(
            ChatCompletionNamedToolChoice {
                function: FunctionName {
                    name: tool_name.clone(),
                },
            },
        )),
    }
}

fn to_reasoning_effort(s: &str) -> Option<ReasoningEffort> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" => Some(ReasoningEffort::None),
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" | "x-high" => Some(ReasoningEffort::Xhigh),
        _ => None,
    }
}

fn to_response_format(rf: &ResponseFormat) -> Option<OaiResponseFormat> {
    match rf.format_type.as_str() {
        "json_object" => Some(OaiResponseFormat::JsonObject),
        "json_schema" => {
            let js = rf.json_schema.as_ref()?;
            let name = js
                .name
                .clone()
                .unwrap_or_else(|| "output_schema".to_string());
            Some(OaiResponseFormat::JsonSchema {
                json_schema: ResponseFormatJsonSchema {
                    name,
                    schema: serde_json::Value::Object(js.schema.clone()),
                    strict: js.strict,
                    description: None,
                },
            })
        }
        _ => None,
    }
}

/// A partial tool call accumulated from streamed fragments keyed by `index`.
#[derive(Default)]
struct ToolCallFragment {
    id: String,
    name: String,
    arguments: String,
}

/// A single SSE chunk from an OpenAI-compatible streaming response, decoded with
/// an extra `reasoning_content` field so thinking deltas (e.g. DeepSeek/O1) can
/// be surfaced incrementally. Serde ignores any other unknown fields.
#[derive(Debug, Deserialize)]
struct OpenAiStreamChunk {
    #[serde(default)]
    id: String,
    #[serde(default)]
    choices: Vec<OpenAiStreamChoice>,
    #[serde(default)]
    usage: Option<CompletionUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamChoice {
    #[serde(default)]
    delta: OpenAiStreamDelta,
    #[serde(default)]
    finish_reason: Option<FinishReason>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatCompletionMessageToolCallChunk>>,
}

impl OpenAiStreamDelta {
    /// Reasoning text carried by this chunk, from either the OpenAI-style
    /// `reasoning_content` field (DeepSeek/O1) or the `reasoning` field used by
    /// some gateways (e.g. opencode zen). Whichever is present wins.
    fn reasoning_text(&self) -> Option<&str> {
        if let Some(r) = &self.reasoning_content {
            if !r.is_empty() {
                return Some(r);
            }
        }
        if let Some(r) = &self.reasoning {
            if !r.is_empty() {
                return Some(r);
            }
        }
        None
    }
}

/// Incremental stream decoder: emits one [`ChatResponse`] per SSE chunk for
/// text/thinking deltas (true token-by-token streaming) while accumulating
/// tool-call fragments, usage and finish reason across chunks. The accumulated
/// tool calls / usage are flushed as a single final [`ChatResponse`] when the
/// underlying stream ends.
#[derive(Default)]
struct StreamIncrementer {
    id: String,
    tool_calls: BTreeMap<u32, ToolCallFragment>,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

impl StreamIncrementer {
    /// Consumes one chunk, returning a `ChatResponse` carrying only the
    /// incremental text/thinking content of this chunk (or `None` when the chunk
    /// carries no incremental content, e.g. an empty usage-only chunk).
    fn accumulate(&mut self, chunk: &OpenAiStreamChunk) -> Option<crate::model::ChatResponse> {
        if self.id.is_empty() && !chunk.id.is_empty() {
            self.id = chunk.id.clone();
        }
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for choice in &chunk.choices {
            let delta = &choice.delta;
            if let Some(content) = &delta.content {
                if !content.is_empty() {
                    blocks.push(ContentBlock::Text(TextBlock::new(content.clone())));
                }
            }
            if let Some(reasoning) = delta.reasoning_text() {
                blocks.push(ContentBlock::Thinking(ThinkingBlock::new(reasoning.to_string())));
            }
            if let Some(refusal) = &delta.refusal {
                if !refusal.is_empty() {
                    blocks.push(ContentBlock::Text(TextBlock::new(refusal.clone())));
                }
            }
            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    let frag = self.tool_calls.entry(tc.index).or_default();
                    if let Some(id) = &tc.id {
                        frag.id = id.clone();
                    }
                    if let Some(f) = &tc.function {
                        if let Some(name) = &f.name {
                            frag.name = name.clone();
                        }
                        if let Some(args) = &f.arguments {
                            frag.arguments.push_str(args);
                        }
                    }
                }
            }
            if let Some(fr) = &choice.finish_reason {
                self.finish_reason = Some(finish_reason_string(fr.clone()));
            }
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(ChatUsage {
                input_tokens: usage.prompt_tokens as u64,
                output_tokens: usage.completion_tokens as u64,
                cached_tokens: 0,
                time: 0.0,
            });
        }
        if blocks.is_empty() {
            None
        } else {
            Some(crate::model::ChatResponse::new(
                self.id.clone(),
                blocks,
                ChatUsage::default(),
                None,
                None,
            ))
        }
    }

    /// Flushes the accumulated tool calls, usage and finish reason into a final
    /// [`ChatResponse`]. Returns `None` when nothing was accumulated.
    fn into_final(self) -> Option<crate::model::ChatResponse> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for frag in self.tool_calls.values() {
            let input = parse_arguments(&frag.arguments);
            let mut tool_use = ToolUseBlock::new(frag.id.clone(), frag.name.clone(), input);
            if tool_use.input.is_empty() && !frag.arguments.is_empty() {
                // The gateway streamed arguments that are not a JSON object
                // (or not valid JSON at all): keep the raw text so the
                // parameters still reach the event stream instead of being
                // silently dropped.
                tool_use.content = Some(frag.arguments.clone());
            }
            blocks.push(ContentBlock::ToolUse(tool_use));
        }
        if blocks.is_empty() && self.usage.is_none() {
            return None;
        }
        Some(crate::model::ChatResponse::new(
            self.id,
            blocks,
            self.usage.unwrap_or_default(),
            None,
            self.finish_reason,
        ))
    }
}

fn finish_reason_string(fr: FinishReason) -> String {
    match fr {
        FinishReason::Stop => "stop".to_string(),
        FinishReason::Length => "length".to_string(),
        FinishReason::ToolCalls => "tool_calls".to_string(),
        FinishReason::ContentFilter => "content_filter".to_string(),
        FinishReason::FunctionCall => "function_call".to_string(),
    }
}

fn parse_arguments(args: &str) -> JsonMap {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock};

    fn sample_msg() -> Msg {
        Msg::builder()
            .role(MsgRole::User)
            .text_content("hello")
            .build()
    }

    #[test]
    fn user_message_converts_to_user_role() {
        let msgs = to_request_messages(vec![sample_msg()]);
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            ChatCompletionRequestMessage::User(u) => {
                let text = match &u.content {
                    ChatCompletionRequestUserMessageContent::Text(t) => t.clone(),
                    _ => String::new(),
                };
                assert_eq!(text, "hello");
            }
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn assistant_tool_calls_round_trip() {
        let input: JsonMap = serde_json::from_str(r#"{"city":"Beijing"}"#).unwrap();
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                "call_1", "get_weather", input,
            ))])
            .build();
        let msgs = to_request_messages(vec![msg]);
        match &msgs[0] {
            ChatCompletionRequestMessage::Assistant(a) => {
                let calls = a.tool_calls.as_ref().unwrap();
                assert_eq!(calls.len(), 1);
            }
            _ => panic!("expected assistant message"),
        }
    }

    #[test]
    fn tool_message_uses_result_id() {
        let tr = ToolResultBlock::new("call_9", "get_weather", vec![ContentBlock::Text(
            TextBlock::new("sunny"),
        )]);
        let msg = Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(tr)])
            .build();
        let msgs = to_request_messages(vec![msg]);
        match &msgs[0] {
            ChatCompletionRequestMessage::Tool(t) => assert_eq!(t.tool_call_id, "call_9"),
            _ => panic!("expected tool message"),
        }
    }

    #[test]
    fn empty_tools_yield_none() {
        assert!(to_request_tools(vec![]).is_none());
    }

    #[test]
    fn tool_choice_specific_maps_to_function() {
        let choice = ToolChoice::specific("get_weather");
        let mapped = to_tool_choice(&choice).unwrap();
        assert!(matches!(
            mapped,
            ChatCompletionToolChoiceOption::Function(_)
        ));
    }

    #[test]
    fn reasoning_effort_mapping() {
        assert_eq!(to_reasoning_effort("high"), Some(ReasoningEffort::High));
        assert_eq!(to_reasoning_effort("x-high"), Some(ReasoningEffort::Xhigh));
        assert_eq!(to_reasoning_effort("nope"), None);
    }

    #[test]
    fn parse_arguments_object_and_garbage() {
        assert!(parse_arguments(r#"{"a":1}"#).contains_key("a"));
        assert!(parse_arguments("not json").is_empty());
    }

    #[test]
    fn stream_incrementer_emits_text_and_thinking_deltas() {
        let mut inc = StreamIncrementer::default();
        // Chunk 1: only reasoning_content (thinking delta).
        let chunk1 = OpenAiStreamChunk {
            id: "r1".into(),
            choices: vec![OpenAiStreamChoice {
                delta: OpenAiStreamDelta {
                    content: None,
                    reasoning_content: Some("let me think".into()),
                    reasoning: None,
                    refusal: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let r1 = inc.accumulate(&chunk1).expect("thinking delta emitted");
        assert!(matches!(
            &r1.content[0],
            ContentBlock::Thinking(t) if t.thinking == "let me think"
        ));

        // Chunk 2: only content (text delta).
        let chunk2 = OpenAiStreamChunk {
            id: "r1".into(),
            choices: vec![OpenAiStreamChoice {
                delta: OpenAiStreamDelta {
                    content: Some("hello".into()),
                    reasoning_content: None,
                    reasoning: None,
                    refusal: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let r2 = inc.accumulate(&chunk2).expect("text delta emitted");
        assert!(matches!(
            &r2.content[0],
            ContentBlock::Text(t) if t.text == "hello"
        ));

        // No tool calls accumulated -> into_final yields nothing.
        assert!(inc.into_final().is_none());
    }

    #[test]
    fn stream_incrementer_flushes_tool_calls_and_usage_at_end() {
        let mut inc = StreamIncrementer::default();
        // Tool-call fragment arrives split across two chunks.
        let frag1 = OpenAiStreamChunk {
            id: "r2".into(),
            choices: vec![OpenAiStreamChoice {
                delta: OpenAiStreamDelta {
                    content: None,
                    reasoning_content: None,
                    reasoning: None,
                    refusal: None,
                    tool_calls: Some(vec![ChatCompletionMessageToolCallChunk {
                        index: 0,
                        id: Some("call_1".into()),
                        r#type: None,
                        function: Some(FunctionCallStream {
                            name: Some("get_weather".into()),
                            arguments: Some(r#"{"city":"Bei"#.into()),
                        }),
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        };
        assert!(inc.accumulate(&frag1).is_none());
        let frag2 = OpenAiStreamChunk {
            id: "r2".into(),
            choices: vec![OpenAiStreamChoice {
                delta: OpenAiStreamDelta {
                    content: None,
                    reasoning_content: None,
                    reasoning: None,
                    refusal: None,
                    tool_calls: Some(vec![ChatCompletionMessageToolCallChunk {
                        index: 0,
                        id: None,
                        r#type: None,
                        function: Some(FunctionCallStream {
                            name: None,
                            arguments: Some(r#"jing"}"#.into()),
                        }),
                    }]),
                },
                finish_reason: Some(FinishReason::ToolCalls),
            }],
            usage: None,
        };
        assert!(inc.accumulate(&frag2).is_none());

        let final_resp = inc.into_final().expect("tool call flushed");
        let tool = match &final_resp.content[0] {
            ContentBlock::ToolUse(t) => t,
            other => panic!("expected ToolUse, got {other:?}"),
        };
        assert_eq!(tool.id, "call_1");
        assert_eq!(tool.name, "get_weather");
        assert_eq!(tool.input.get("city").and_then(|v| v.as_str()), Some("Beijing"));
        assert_eq!(final_resp.finish_reason.as_deref(), Some("tool_calls"));
    }
}
