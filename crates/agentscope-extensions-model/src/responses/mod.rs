//! OpenAI Responses API model (`POST {base_url}/responses`), implemented on top
//! of the `async-openai` crate's `responses` feature.
//!
//! This is a sibling of [`crate::openai`] (Chat Completions): it speaks a
//! different wire protocol but maps into the same
//! [`agentscope_core::model::ChatResponse`]/[`ContentBlock`] types, so the ReAct agent and
//! ACP layers are protocol-agnostic.
//!
//! Streaming events are mapped as follows:
//! - `response.output_text.delta`            → [`ContentBlock::Text`]
//! - `response.reasoning_text.delta`         → [`ContentBlock::Thinking`]
//! - `response.function_call_arguments.*`    → [`ContentBlock::ToolUse`]
//! - `response.completed`                    → usage + finish reason

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, FunctionCallOutput,
    FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, InputItem, InputParam, Item,
    MessageType, OutputItem, ResponseStreamEvent, Role, Status, Tool, ToolChoiceFunction,
    ToolChoiceOptions, ToolChoiceParam,
};
use async_openai::Client;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::{convert_tool_result_to_string, extract_text_content};
use agentscope_core::message::{
    ChatUsage, ContentBlock, JsonMap, Msg, MsgRole, TextBlock, ThinkingBlock, ToolUseBlock,
};
use agentscope_core::model::context_windows::resolve_context_window;
use agentscope_core::model::generate_options::GenerateOptions;
use agentscope_core::model::model::{Model, ModelCapabilities};
use agentscope_core::model::provider::{
    ModelCreationContext, ModelProvider, ModelRegistry, RESPONSES_PROVIDER_ID,
};
use agentscope_core::model::tool_choice::ToolChoice;
use agentscope_core::model::tool_schema::ToolSchema;

/// An OpenAI Responses API model backed by `async-openai`.
pub struct OpenAiResponsesModel {
    client: Client<OpenAIConfig>,
    model_name: String,
    capabilities: ModelCapabilities,
}

impl OpenAiResponsesModel {
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

    /// Access the underlying async-openai client.
    pub fn client(&self) -> &Client<OpenAIConfig> {
        &self.client
    }
}

#[async_trait::async_trait]
impl Model for OpenAiResponsesModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, Result<agentscope_core::model::ChatResponse>> {
        let client = self.client.clone();
        let model_name = options
            .model_name
            .clone()
            .unwrap_or_else(|| self.model_name.clone());
        let request = build_request(model_name, messages, tools, options);

        let setup = futures::stream::once(async move {
            client
                .responses()
                .create_stream(request)
                .await
                .map_err(map_openai_error)
        });
        let chunks = setup
            .map_ok(|s| s.map(|item| item.map_err(map_openai_error)))
            .try_flatten()
            .boxed();

        let stream = futures::stream::unfold((chunks, ResponsesIncrementer::default()), |state| {
            let (mut chunks, mut acc) = state;
            async move {
                loop {
                    match chunks.next().await {
                        Some(Ok(event)) => {
                            if let Some(resp) = acc.accumulate(&event) {
                                return Some((Ok(resp), (chunks, acc)));
                            }
                        }
                        Some(Err(e)) => return Some((Err(e), (chunks, acc))),
                        None => {
                            return match acc.into_final() {
                                Some(resp) => {
                                    Some((Ok(resp), (chunks, ResponsesIncrementer::default())))
                                }
                                None => None,
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

/// Provider that registers [`OpenAiResponsesModel`] under the responses provider id.
pub struct OpenAiResponsesProvider;

impl ModelProvider for OpenAiResponsesProvider {
    fn provider_id(&self) -> &str {
        RESPONSES_PROVIDER_ID
    }
    fn supports(&self, _model: &str) -> bool {
        true
    }
    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        Ok(Arc::new(OpenAiResponsesModel::from_context(&ctx)?))
    }
}

/// Builds a registry pre-populated with the Responses provider.
pub fn default_responses_registry() -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    registry.register(Arc::new(OpenAiResponsesProvider));
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

fn build_request(
    model: String,
    messages: Vec<Msg>,
    tools: Vec<ToolSchema>,
    options: GenerateOptions,
) -> CreateResponse {
    let mut instructions: Vec<String> = Vec::new();
    let mut items: Vec<InputItem> = Vec::new();

    for msg in messages {
        match msg.role {
            MsgRole::System => {
                let text = extract_text_content(&msg);
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            MsgRole::User => items.push(easy_message(Role::User, extract_text_content(&msg))),
            MsgRole::Assistant => {
                let text = extract_text_content(&msg);
                if !text.is_empty() {
                    items.push(easy_message(Role::Assistant, text));
                }
                for tu in msg.tool_use_blocks() {
                    items.push(InputItem::Item(Item::FunctionCall(FunctionToolCall {
                        arguments: serde_json::to_string(&tu.input).unwrap_or_default(),
                        call_id: tu.id.clone(),
                        namespace: None,
                        name: tu.name.clone(),
                        id: None,
                        status: None,
                    })));
                }
            }
            MsgRole::Tool => {
                for tr in msg.tool_result_blocks() {
                    items.push(InputItem::Item(Item::FunctionCallOutput(
                        FunctionCallOutputItemParam {
                            call_id: tr.id.clone().unwrap_or_default(),
                            output: FunctionCallOutput::Text(convert_tool_result_to_string(
                                &tr.output,
                            )),
                            id: None,
                            status: None,
                        },
                    )));
                }
            }
        }
    }

    let request_tools = to_request_tools(tools);
    let has_tools = request_tools.is_some();

    let mut req = CreateResponse {
        input: InputParam::Items(items),
        model: Some(model),
        stream: Some(true),
        tools: request_tools,
        ..Default::default()
    };

    if !instructions.is_empty() {
        req.instructions = Some(instructions.join("\n\n"));
    }
    if has_tools {
        if let Some(tc) = &options.tool_choice {
            req.tool_choice = to_tool_choice(tc);
        }
    }
    if let Some(t) = options.temperature {
        req.temperature = Some(t as f32);
    }
    if let Some(p) = options.top_p {
        req.top_p = Some(p as f32);
    }
    if let Some(n) = options.max_completion_tokens.or(options.max_tokens) {
        req.max_output_tokens = Some(n);
    }
    req
}

fn easy_message(role: Role, text: String) -> InputItem {
    InputItem::EasyMessage(EasyInputMessage {
        r#type: MessageType::Message,
        role,
        content: EasyInputContent::Text(text),
        phase: None,
    })
}

fn to_request_tools(tools: Vec<ToolSchema>) -> Option<Vec<Tool>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .into_iter()
            .map(|t| {
                Tool::Function(FunctionTool {
                    name: t.name,
                    parameters: if t.parameters.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::Object(t.parameters))
                    },
                    strict: t.strict,
                    description: if t.description.is_empty() {
                        None
                    } else {
                        Some(t.description)
                    },
                    defer_loading: None,
                })
            })
            .collect(),
    )
}

fn to_tool_choice(tc: &ToolChoice) -> Option<ToolChoiceParam> {
    match tc {
        ToolChoice::Auto => Some(ToolChoiceParam::Mode(ToolChoiceOptions::Auto)),
        ToolChoice::None => Some(ToolChoiceParam::Mode(ToolChoiceOptions::None)),
        ToolChoice::Required => Some(ToolChoiceParam::Mode(ToolChoiceOptions::Required)),
        ToolChoice::Specific { tool_name } => Some(ToolChoiceParam::Function(ToolChoiceFunction {
            name: tool_name.clone(),
        })),
    }
}

fn status_string(status: &Status) -> String {
    match status {
        Status::Completed => "stop".to_string(),
        Status::Incomplete => "length".to_string(),
        Status::Failed => "failed".to_string(),
        Status::Cancelled => "cancelled".to_string(),
        Status::Queued => "queued".to_string(),
        Status::InProgress => "in_progress".to_string(),
    }
}

/// A partial tool call accumulated from streamed fragments keyed by `output_index`.
#[derive(Default)]
struct ToolCallFragment {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental stream decoder for the Responses API: emits one [`ChatResponse`]
/// per text/thinking delta while accumulating tool-call fragments, usage and
/// finish reason across events. The accumulated parts are flushed as a single
/// final [`ChatResponse`] when the underlying stream ends.
#[derive(Default)]
struct ResponsesIncrementer {
    id: String,
    tool_calls: BTreeMap<u32, ToolCallFragment>,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

impl ResponsesIncrementer {
    fn accumulate(&mut self, event: &ResponseStreamEvent) -> Option<agentscope_core::model::ChatResponse> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        match event {
            ResponseStreamEvent::ResponseOutputTextDelta(e) => {
                if !e.delta.is_empty() {
                    blocks.push(ContentBlock::Text(TextBlock::new(e.delta.clone())));
                }
            }
            ResponseStreamEvent::ResponseReasoningTextDelta(e) => {
                if !e.delta.is_empty() {
                    blocks.push(ContentBlock::Thinking(ThinkingBlock::new(e.delta.clone())));
                }
            }
            ResponseStreamEvent::ResponseOutputItemAdded(e) => {
                if let OutputItem::FunctionCall(fc) = &e.item {
                    let frag = self.tool_calls.entry(e.output_index).or_default();
                    frag.id = fc.call_id.clone();
                    frag.name = fc.name.clone();
                    if !fc.arguments.is_empty() {
                        frag.arguments = fc.arguments.clone();
                    }
                }
            }
            ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(e) => {
                let frag = self.tool_calls.entry(e.output_index).or_default();
                frag.arguments.push_str(&e.delta);
            }
            ResponseStreamEvent::ResponseFunctionCallArgumentsDone(e) => {
                let frag = self.tool_calls.entry(e.output_index).or_default();
                frag.arguments = e.arguments.clone();
                if let Some(name) = &e.name {
                    frag.name = name.clone();
                }
            }
            ResponseStreamEvent::ResponseCompleted(e) => {
                if self.id.is_empty() {
                    self.id = e.response.id.clone();
                }
                if let Some(u) = &e.response.usage {
                    self.usage = Some(ChatUsage {
                        input_tokens: u.input_tokens as u64,
                        output_tokens: u.output_tokens as u64,
                        cached_tokens: 0,
                        time: 0.0,
                    });
                }
                self.finish_reason = Some(status_string(&e.response.status));
            }
            ResponseStreamEvent::ResponseError(e) => {
                return Some(agentscope_core::model::ChatResponse::new(
                    self.id.clone(),
                    vec![ContentBlock::Text(TextBlock::new(format!(
                        "[responses error] {}",
                        e.message
                    )))],
                    ChatUsage::default(),
                    None,
                    Some("error".to_string()),
                ))
            }
            _ => {}
        }

        if blocks.is_empty() {
            None
        } else {
            Some(agentscope_core::model::ChatResponse::new(
                self.id.clone(),
                blocks,
                ChatUsage::default(),
                None,
                None,
            ))
        }
    }

    fn into_final(self) -> Option<agentscope_core::model::ChatResponse> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for frag in self.tool_calls.values() {
            let input = parse_arguments(&frag.arguments);
            let mut tool_use = ToolUseBlock::new(frag.id.clone(), frag.name.clone(), input);
            if tool_use.input.is_empty() && !frag.arguments.is_empty() {
                tool_use.content = Some(frag.arguments.clone());
            }
            blocks.push(ContentBlock::ToolUse(tool_use));
        }
        if blocks.is_empty() && self.usage.is_none() {
            return None;
        }
        Some(agentscope_core::model::ChatResponse::new(
            self.id,
            blocks,
            self.usage.unwrap_or_default(),
            None,
            self.finish_reason,
        ))
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
    use agentscope_core::message::{TextBlock, ToolResultBlock};
    use async_openai::types::responses::{
        ResponseFunctionCallArgumentsDeltaEvent, ResponseOutputItemAddedEvent,
        ResponseTextDeltaEvent,
    };

    fn user_msg(text: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::User)
            .text_content(text)
            .build()
    }

    #[test]
    fn system_message_becomes_instructions() {
        let msg = Msg::builder()
            .role(MsgRole::System)
            .text_content("be nice")
            .build();
        let req = build_request(
            "gpt-5".to_string(),
            vec![msg, user_msg("hi")],
            vec![],
            GenerateOptions::default(),
        );
        assert_eq!(req.instructions.as_deref(), Some("be nice"));
        match req.input {
            InputParam::Items(items) => assert_eq!(items.len(), 1),
            _ => panic!("expected items"),
        }
    }

    #[test]
    fn user_message_becomes_easy_message() {
        let req = build_request(
            "gpt-5".to_string(),
            vec![user_msg("hello")],
            vec![],
            GenerateOptions::default(),
        );
        match &req.input {
            InputParam::Items(items) => match &items[0] {
                InputItem::EasyMessage(m) => {
                    assert_eq!(m.role, Role::User);
                    match &m.content {
                        EasyInputContent::Text(t) => assert_eq!(t, "hello"),
                        _ => panic!("expected text"),
                    }
                }
                _ => panic!("expected easy message"),
            },
            _ => panic!("expected items"),
        }
    }

    #[test]
    fn assistant_tool_use_becomes_function_call() {
        let input: JsonMap = serde_json::from_str(r#"{"city":"Beijing"}"#).unwrap();
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                "call_1",
                "get_weather",
                input,
            ))])
            .build();
        let req = build_request(
            "gpt-5".to_string(),
            vec![msg],
            vec![],
            GenerateOptions::default(),
        );
        match &req.input {
            InputParam::Items(items) => match &items[0] {
                InputItem::Item(Item::FunctionCall(fc)) => {
                    assert_eq!(fc.call_id, "call_1");
                    assert_eq!(fc.name, "get_weather");
                    assert!(fc.arguments.contains("Beijing"));
                }
                _ => panic!("expected function call"),
            },
            _ => panic!("expected items"),
        }
    }

    #[test]
    fn tool_result_becomes_function_call_output() {
        let tr = ToolResultBlock::new(
            "call_9",
            "get_weather",
            vec![ContentBlock::Text(TextBlock::new("sunny"))],
        );
        let msg = Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(tr)])
            .build();
        let req = build_request(
            "gpt-5".to_string(),
            vec![msg],
            vec![],
            GenerateOptions::default(),
        );
        match &req.input {
            InputParam::Items(items) => match &items[0] {
                InputItem::Item(Item::FunctionCallOutput(out)) => {
                    assert_eq!(out.call_id, "call_9");
                    match &out.output {
                        FunctionCallOutput::Text(t) => assert!(t.contains("sunny")),
                        _ => panic!("expected text output"),
                    }
                }
                _ => panic!("expected function call output"),
            },
            _ => panic!("expected items"),
        }
    }

    #[test]
    fn text_delta_yields_text_block() {
        let mut acc = ResponsesIncrementer::default();
        let event = ResponseStreamEvent::ResponseOutputTextDelta(ResponseTextDeltaEvent {
            sequence_number: 1,
            item_id: "item_1".to_string(),
            output_index: 0,
            content_index: 0,
            delta: "hello".to_string(),
            logprobs: None,
        });
        let resp = acc.accumulate(&event).expect("expected a response");
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn tool_call_fragments_flush_on_final() {
        let mut acc = ResponsesIncrementer::default();
        acc.accumulate(&ResponseStreamEvent::ResponseOutputItemAdded(
            ResponseOutputItemAddedEvent {
                sequence_number: 1,
                output_index: 0,
                item: OutputItem::FunctionCall(FunctionToolCall {
                    arguments: String::new(),
                    call_id: "call_1".to_string(),
                    namespace: None,
                    name: "get_weather".to_string(),
                    id: None,
                    status: None,
                }),
            },
        ));
        acc.accumulate(&ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
            ResponseFunctionCallArgumentsDeltaEvent {
                sequence_number: 2,
                item_id: "item_1".to_string(),
                output_index: 0,
                delta: r#"{"city":"Beijing"}"#.to_string(),
            },
        ));
        let final_resp = acc.into_final().expect("expected final response");
        assert_eq!(final_resp.content.len(), 1);
        match &final_resp.content[0] {
            ContentBlock::ToolUse(tu) => {
                assert_eq!(tu.id, "call_1");
                assert_eq!(tu.name, "get_weather");
                assert_eq!(tu.input.get("city").and_then(|v| v.as_str()), Some("Beijing"));
            }
            _ => panic!("expected tool use block"),
        }
    }
}
