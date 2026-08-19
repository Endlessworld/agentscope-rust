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

use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionNamedToolChoice, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestToolMessage, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
    ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionTools,
    ChatCompletionToolChoiceOption, CreateChatCompletionRequest,
    CreateChatCompletionStreamResponse, FinishReason, FunctionCall, FunctionName, FunctionObject,
    ReasoningEffort, ResponseFormat as OaiResponseFormat, ResponseFormatJsonSchema,
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
use crate::formatter::ResponseFormat;

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
        let request = build_request(model_name, messages, tools, options);

        // create_stream is async; wrap in a once-stream then flatten.
        let setup = futures::stream::once(async move {
            client
                .chat()
                .create_stream(request)
                .await
                .map_err(map_openai_error)
        });
        let chunks = setup
            .map_ok(|s| s.map(|item| item.map_err(map_openai_error)))
            .try_flatten()
            .boxed();

        let stream = futures::stream::unfold((chunks, StreamAccumulator::default()), |state| {
            let (mut chunks, mut acc) = state;
            async move {
                loop {
                    match chunks.next().await {
                        Some(Ok(chunk)) => acc.accumulate(chunk),
                        Some(Err(e)) => return Some((Err(e), (chunks, acc))),
                        None => match acc.into_response() {
                            Some(resp) => {
                                return Some((Ok(resp), (chunks, StreamAccumulator::default())))
                            }
                            None => return None,
                        },
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
                            msg.text_content(),
                        ),
                        name: None,
                    },
                ));
            }
            MsgRole::User => {
                out.push(ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(msg.text_content()),
                    name: None,
                }));
            }
            MsgRole::Assistant => {
                let text = msg.text_content();
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
                    let text = tr
                        .output
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
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

/// Accumulates all deltas of one chat completion into a single [`ChatResponse`].
#[derive(Default)]
struct StreamAccumulator {
    id: String,
    text: String,
    thinking: String,
    tool_calls: BTreeMap<u32, ToolCallFragment>,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

impl StreamAccumulator {
    fn accumulate(&mut self, chunk: CreateChatCompletionStreamResponse) {
        if self.id.is_empty() && !chunk.id.is_empty() {
            self.id = chunk.id;
        }
        for choice in chunk.choices {
            let delta = choice.delta;
            if let Some(content) = delta.content {
                self.text.push_str(&content);
            }
            if let Some(refusal) = delta.refusal {
                self.text.push_str(&refusal);
            }
            if let Some(tool_calls) = delta.tool_calls {
                for tc in tool_calls {
                    let frag = self.tool_calls.entry(tc.index).or_default();
                    if let Some(id) = tc.id {
                        frag.id = id;
                    }
                    if let Some(f) = tc.function {
                        if let Some(name) = f.name {
                            frag.name = name;
                        }
                        if let Some(args) = f.arguments {
                            frag.arguments.push_str(&args);
                        }
                    }
                }
            }
            if let Some(fr) = choice.finish_reason {
                self.finish_reason = Some(finish_reason_string(fr));
            }
        }
        if let Some(usage) = chunk.usage {
            self.usage = Some(ChatUsage {
                input_tokens: usage.prompt_tokens as u64,
                output_tokens: usage.completion_tokens as u64,
                cached_tokens: 0,
                time: 0.0,
            });
        }
    }

    fn into_response(self) -> Option<crate::model::ChatResponse> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !self.thinking.is_empty() {
            blocks.push(ContentBlock::Thinking(ThinkingBlock::new(self.thinking)));
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text(TextBlock::new(self.text)));
        }
        for frag in self.tool_calls.values() {
            let input = parse_arguments(&frag.arguments);
            blocks.push(ContentBlock::ToolUse(ToolUseBlock::new(
                frag.id.clone(),
                frag.name.clone(),
                input,
            )));
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
}
