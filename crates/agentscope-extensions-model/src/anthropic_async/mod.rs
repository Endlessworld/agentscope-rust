//! Anthropic Messages API model (`POST {base_url}/v1/messages`), implemented on
//! top of the `async-anthropic` crate (v0.6).
//!
//! This is a sibling of [`crate::openai`] (Chat Completions) and
//! [`crate::responses`] (OpenAI Responses): it speaks the Anthropic wire
//! protocol but maps into the same [`agentscope_core::model::ChatResponse`]/
//! [`ContentBlock`] types, so the ReAct agent and ACP layers stay
//! protocol-agnostic.
//!
//! Tool calling is fully supported: `ToolSchema` → Anthropic `tools`
//! (`input_schema`), assistant `tool_use` blocks → `MessageContent::ToolUse`,
//! tool results → `MessageContent::ToolResult` inside a `user` message.
//!
//! Streaming events are mapped as follows:
//! - `content_block_delta` / `text_delta`        → [`ContentBlock::Text`]
//! - `content_block_delta` / `input_json_delta`  → accumulated tool arguments
//! - `message_delta`                             → finish reason + output usage
//!
//! Note: `async-anthropic` 0.6 has no `thinking_delta` variant, so this
//! protocol never produces [`ContentBlock::Thinking`].

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use async_anthropic::errors::AnthropicError;
use async_anthropic::types::{
    ContentBlockDelta, CreateMessagesRequest, Message, MessageContent, MessageContentList,
    MessageRole, MessagesStreamEvent, Text, ToolChoice, ToolResult, ToolUse,
};
use async_anthropic::Client;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::{convert_tool_result_to_string, extract_text_content};
use agentscope_core::message::{
    ChatUsage, ContentBlock, JsonMap, Msg, MsgRole, TextBlock, ToolUseBlock,
};
use agentscope_core::model::catalog::{descriptor_for, ModelDescriptor};
use agentscope_core::model::context_windows::resolve_context_window;
use agentscope_core::model::generate_options::GenerateOptions;
use agentscope_core::model::model::{Model, ModelCapabilities};
use agentscope_core::model::provider::{
    ModelCreationContext, ModelProvider, ModelRegistry, ANTHROPIC_PROVIDER_ID,
};
use agentscope_core::model::tool_choice::ToolChoice as CoreToolChoice;
use agentscope_core::model::tool_schema::ToolSchema;

/// Fallback `max_tokens` when the caller does not specify one. Anthropic
/// requires this field, so we cannot omit it.
const DEFAULT_MAX_TOKENS: i32 = 2048;

/// An Anthropic Messages API model backed by `async-anthropic`.
pub struct AnthropicMessagesModel {
    client: Client,
    model_name: String,
    capabilities: ModelCapabilities,
    descriptor: Option<Arc<ModelDescriptor>>,
}

impl AnthropicMessagesModel {
    /// Creates a model from an explicit model id, api key and base URL.
    pub fn new(
        model_name: impl Into<String>,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self> {
        let model_name = model_name.into();
        let descriptor = descriptor_for(None, &model_name, base_url.as_deref());
        let no_headers = std::collections::HashMap::new();
        Self::build(model_name, api_key, base_url, &no_headers, descriptor)
    }

    /// Creates a model from a [`ModelCreationContext`].
    pub fn from_context(ctx: &ModelCreationContext) -> Result<Self> {
        let model_name = if ctx.model.is_empty() {
            ctx.options
                .model_name
                .clone()
                .unwrap_or_else(|| "claude-3-5-sonnet-latest".to_string())
        } else {
            ctx.model.clone()
        };
        let descriptor = ctx.catalog.clone().or_else(|| {
            descriptor_for(Some(&ctx.provider_id), &model_name, ctx.base_url.as_deref())
        });
        Self::build(
            model_name,
            ctx.api_key.clone(),
            ctx.base_url.clone(),
            &ctx.options.additional_headers,
            descriptor,
        )
    }

    fn build(
        model_name: String,
        api_key: Option<String>,
        base_url: Option<String>,
        additional_headers: &std::collections::HashMap<String, String>,
        descriptor: Option<Arc<ModelDescriptor>>,
    ) -> Result<Self> {
        // `async-anthropic` sends every request through `Client::http_client`,
        // so per-request additional headers (e.g. `x-opencode-session`) are
        // injected as default headers of a dedicated reqwest client.
        let mut headers = reqwest12::header::HeaderMap::new();
        for (k, v) in additional_headers {
            let name = reqwest12::header::HeaderName::from_str(k)
                .map_err(|e| Error::Model(format!("invalid header name '{k}': {e}")))?;
            let value = reqwest12::header::HeaderValue::from_str(v)
                .map_err(|e| Error::Model(format!("invalid header value for '{k}': {e}")))?;
            headers.insert(name, value);
        }
        // Apply the shared transport timeouts so the SDK path cannot hang where
        // the transport-seam providers would have timed out (see
        // `MODEL__GAP_TRACKER.md` §3.3 / the anthropic double-implementation
        // convergence note in §4.2).
        let transport_cfg = crate::common::TransportConfig::default();
        let http = reqwest12::Client::builder()
            .default_headers(headers)
            .connect_timeout(transport_cfg.connect_timeout)
            .read_timeout(transport_cfg.read_timeout)
            .tcp_keepalive(transport_cfg.tcp_keepalive)
            .build()
            .map_err(|e| Error::Model(format!("failed to build http client: {e}")))?;

        // `ClientBuilder` setters take `&mut self`, so the builder must be
        // bound to a local before chaining.
        let mut builder = Client::builder();
        builder.http_client(http);
        if let Some(base) = base_url {
            builder.base_url(base);
        }
        if let Some(key) = api_key {
            builder.api_key(key);
        }
        let client = builder
            .build()
            .map_err(|e| Error::Model(format!("failed to build anthropic client: {e}")))?;

        let context = descriptor
            .as_ref()
            .and_then(|descriptor| descriptor.effective_context())
            .unwrap_or_else(|| resolve_context_window(&model_name));
        Ok(Self {
            client,
            capabilities: ModelCapabilities::new().context_window_size(context),
            descriptor,
            model_name,
        })
    }
}

#[async_trait::async_trait]
impl Model for AnthropicMessagesModel {
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
        let options = crate::common::clamp_max_tokens(options, self.max_output_tokens());
        let request = build_request(model_name, messages, tools, options);

        let setup = futures::stream::once(async move {
            Ok::<_, Error>(client.messages().create_stream(request).await)
        });
        let chunks = setup
            .map_ok(|s| s.map(|item| item.map_err(map_anthropic_error)))
            .try_flatten()
            .boxed();

        let stream = futures::stream::unfold((chunks, AnthropicIncrementer::default()), |state| {
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
                                    Some((Ok(resp), (chunks, AnthropicIncrementer::default())))
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

    fn max_output_tokens(&self) -> usize {
        self.descriptor
            .as_ref()
            .map(|descriptor| descriptor.max_output)
            .unwrap_or(0)
    }

    fn descriptor(&self) -> Option<&ModelDescriptor> {
        self.descriptor.as_deref()
    }

    fn supports_native_structured_output(&self) -> bool {
        self.capabilities.native_structured_output
    }

    fn supports_native_structured_output_with_tools(&self) -> bool {
        self.capabilities.native_structured_output_with_tools
    }

    fn supports_tool_choice_specific(&self) -> bool {
        true
    }
}

/// Provider that registers [`AnthropicMessagesModel`] under the anthropic provider id.
pub struct AnthropicMessagesProvider;

impl ModelProvider for AnthropicMessagesProvider {
    fn provider_id(&self) -> &str {
        ANTHROPIC_PROVIDER_ID
    }
    fn supports(&self, _model: &str) -> bool {
        true
    }
    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        Ok(Arc::new(AnthropicMessagesModel::from_context(&ctx)?))
    }
}

/// Builds a registry pre-populated with the Anthropic Messages provider.
pub fn default_anthropic_registry() -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    registry.register(Arc::new(AnthropicMessagesProvider));
    registry
}

fn map_anthropic_error(e: AnthropicError) -> Error {
    match e {
        AnthropicError::Unauthorized => Error::HttpStatus {
            status: 401,
            message: "unauthorized".to_string(),
        },
        AnthropicError::BadRequest(m) => Error::HttpStatus {
            status: 400,
            message: m,
        },
        other => Error::Model(format!("{other}")),
    }
}

fn build_request(
    model: String,
    messages: Vec<Msg>,
    tools: Vec<ToolSchema>,
    options: GenerateOptions,
) -> CreateMessagesRequest {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Message> = Vec::new();

    for msg in messages {
        match msg.role {
            MsgRole::System => {
                let text = extract_text_content(&msg);
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            MsgRole::User => out.push(text_message(MessageRole::User, extract_text_content(&msg))),
            MsgRole::Assistant => {
                let mut content: Vec<MessageContent> = Vec::new();
                let text = extract_text_content(&msg);
                if !text.is_empty() {
                    content.push(MessageContent::Text(Text { text }));
                }
                for tu in msg.tool_use_blocks() {
                    content.push(MessageContent::ToolUse(ToolUse {
                        id: tu.id.clone(),
                        input: serde_json::Value::Object(tu.input.clone()),
                        name: tu.name.clone(),
                    }));
                }
                if !content.is_empty() {
                    out.push(Message {
                        role: MessageRole::Assistant,
                        content: MessageContentList(content),
                    });
                }
            }
            MsgRole::Tool => {
                // Anthropic requires tool results to be carried by a `user`
                // message; one message per result keeps ordering unambiguous.
                for tr in msg.tool_result_blocks() {
                    out.push(Message {
                        role: MessageRole::User,
                        content: MessageContentList(vec![MessageContent::ToolResult(ToolResult {
                            tool_use_id: tr.id.clone().unwrap_or_default(),
                            content: Some(convert_tool_result_to_string(&tr.output)),
                            is_error: false,
                        })]),
                    });
                }
            }
        }
    }

    let request_tools = to_request_tools(tools);
    let has_tools = request_tools.is_some();

    let mut req = CreateMessagesRequest {
        messages: out,
        model,
        max_tokens: options
            .max_completion_tokens
            .or(options.max_tokens)
            .map(|n| n as i32)
            .unwrap_or(DEFAULT_MAX_TOKENS),
        metadata: None,
        stop_sequences: None,
        stream: true,
        temperature: options.temperature.map(|t| t as f32),
        tool_choice: None,
        tools: request_tools,
        top_k: options.top_k,
        top_p: options.top_p.map(|p| p as f32),
        system: if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        },
    };

    if has_tools {
        if let Some(tc) = &options.tool_choice {
            req.tool_choice = to_tool_choice(tc);
        }
    }
    req
}

fn text_message(role: MessageRole, text: String) -> Message {
    Message {
        role,
        content: MessageContentList(vec![MessageContent::Text(Text { text })]),
    }
}

fn to_request_tools(
    tools: Vec<ToolSchema>,
) -> Option<Vec<serde_json::Map<String, serde_json::Value>>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .into_iter()
            .map(|t| {
                let mut m = serde_json::Map::new();
                m.insert("name".to_string(), serde_json::Value::String(t.name));
                m.insert(
                    "description".to_string(),
                    serde_json::Value::String(t.description),
                );
                let schema = if t.parameters.is_empty() {
                    serde_json::json!({"type": "object", "properties": {}})
                } else {
                    serde_json::Value::Object(t.parameters)
                };
                m.insert("input_schema".to_string(), schema);
                m
            })
            .collect(),
    )
}

fn to_tool_choice(tc: &CoreToolChoice) -> Option<ToolChoice> {
    match tc {
        CoreToolChoice::Auto => Some(ToolChoice::Auto),
        // Anthropic has no "none": omitting tool_choice keeps tools available
        // but the model is not forced to call one.
        CoreToolChoice::None => None,
        CoreToolChoice::Required => Some(ToolChoice::Any),
        CoreToolChoice::Specific { tool_name } => Some(ToolChoice::Tool(tool_name.clone())),
    }
}

fn stop_reason_string(reason: &str) -> String {
    match reason {
        "tool_use" => "tool_calls".to_string(),
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        other => other.to_string(),
    }
}

/// A partial tool call accumulated from streamed fragments keyed by block index.
#[derive(Default)]
struct ToolCallFragment {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental stream decoder for the Anthropic Messages API: emits one
/// [`ChatResponse`] per text delta while accumulating tool-call fragments,
/// usage and finish reason across events. The accumulated parts are flushed as
/// a single final [`ChatResponse`] when the underlying stream ends.
#[derive(Default)]
struct AnthropicIncrementer {
    id: String,
    tool_calls: BTreeMap<usize, ToolCallFragment>,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

impl AnthropicIncrementer {
    fn accumulate(
        &mut self,
        event: &MessagesStreamEvent,
    ) -> Option<agentscope_core::model::ChatResponse> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        match event {
            MessagesStreamEvent::MessageStart { message, usage } => {
                if self.id.is_empty() {
                    self.id = message.id.clone();
                }
                if let Some(u) = usage {
                    let mut cur = self.usage.unwrap_or_default();
                    cur.input_tokens = u.input_tokens.unwrap_or(0) as u64;
                    self.usage = Some(cur);
                }
            }
            MessagesStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                if let MessageContent::ToolUse(tu) = content_block {
                    let frag = self.tool_calls.entry(*index).or_default();
                    frag.id = tu.id.clone();
                    frag.name = tu.name.clone();
                    if !tu.input.is_null() {
                        frag.arguments = serde_json::to_string(&tu.input).unwrap_or_default();
                    }
                }
            }
            MessagesStreamEvent::ContentBlockDelta { index, delta } => match delta {
                ContentBlockDelta::TextDelta { text } => {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text(TextBlock::new(text.clone())));
                    }
                }
                ContentBlockDelta::InputJsonDelta { partial_json } => {
                    let frag = self.tool_calls.entry(*index).or_default();
                    frag.arguments.push_str(partial_json);
                }
            },
            MessagesStreamEvent::MessageDelta { delta, usage } => {
                if let Some(reason) = &delta.stop_reason {
                    self.finish_reason = Some(stop_reason_string(reason));
                }
                if let Some(u) = usage {
                    let mut cur = self.usage.unwrap_or_default();
                    if let Some(out) = u.output_tokens {
                        cur.output_tokens = out as u64;
                    }
                    self.usage = Some(cur);
                }
            }
            MessagesStreamEvent::ContentBlockStop { .. } | MessagesStreamEvent::MessageStop => {}
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
    use async_anthropic::types::{MessageDelta, MessageStart, Usage};

    fn user_msg(text: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::User)
            .text_content(text)
            .build()
    }

    #[test]
    fn system_message_goes_to_top_level_system_field() {
        let sys = Msg::builder()
            .role(MsgRole::System)
            .text_content("be nice")
            .build();
        let req = build_request(
            "minimax-m3".to_string(),
            vec![sys, user_msg("hi")],
            vec![],
            GenerateOptions::default(),
        );
        assert_eq!(req.system.as_deref(), Some("be nice"));
        // system must NOT appear as a message
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::User);
    }

    #[test]
    fn user_message_becomes_text_content() {
        let req = build_request(
            "minimax-m3".to_string(),
            vec![user_msg("hello")],
            vec![],
            GenerateOptions::default(),
        );
        assert_eq!(req.messages.len(), 1);
        match &req.messages[0].content.0[0] {
            MessageContent::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn assistant_tool_use_becomes_tool_use_block() {
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
            "minimax-m3".to_string(),
            vec![msg],
            vec![],
            GenerateOptions::default(),
        );
        assert_eq!(req.messages[0].role, MessageRole::Assistant);
        match &req.messages[0].content.0[0] {
            MessageContent::ToolUse(tu) => {
                assert_eq!(tu.id, "call_1");
                assert_eq!(tu.name, "get_weather");
                assert_eq!(
                    tu.input.get("city").and_then(|v| v.as_str()),
                    Some("Beijing")
                );
            }
            _ => panic!("expected tool use"),
        }
    }

    #[test]
    fn tool_result_becomes_user_message_with_tool_result() {
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
            "minimax-m3".to_string(),
            vec![msg],
            vec![],
            GenerateOptions::default(),
        );
        assert_eq!(req.messages[0].role, MessageRole::User);
        match &req.messages[0].content.0[0] {
            MessageContent::ToolResult(r) => {
                assert_eq!(r.tool_use_id, "call_9");
                assert!(r.content.as_deref().unwrap_or("").contains("sunny"));
            }
            _ => panic!("expected tool result"),
        }
    }

    #[test]
    fn tools_use_input_schema_key() {
        let mut params = serde_json::Map::new();
        params.insert("type".to_string(), serde_json::json!("object"));
        let tool = ToolSchema::new("get_weather", "Get weather")
            .parameters(params)
            .strict(true);
        let req = build_request(
            "minimax-m3".to_string(),
            vec![user_msg("hi")],
            vec![tool],
            GenerateOptions::default(),
        );
        let tools = req.tools.expect("expected tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].get("name").and_then(|v| v.as_str()),
            Some("get_weather")
        );
        assert!(tools[0].contains_key("input_schema"));
        assert!(!tools[0].contains_key("parameters"));
    }

    #[test]
    fn text_delta_yields_text_block() {
        let mut acc = AnthropicIncrementer::default();
        let event = MessagesStreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::TextDelta {
                text: "hello".to_string(),
            },
        };
        let resp = acc.accumulate(&event).expect("expected a response");
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn tool_call_fragments_flush_on_final() {
        let mut acc = AnthropicIncrementer::default();
        acc.accumulate(&MessagesStreamEvent::ContentBlockStart {
            index: 0,
            content_block: MessageContent::ToolUse(ToolUse {
                id: "call_1".to_string(),
                input: serde_json::Value::Null,
                name: "get_weather".to_string(),
            }),
        });
        acc.accumulate(&MessagesStreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: r#"{"city":"Beijing"}"#.to_string(),
            },
        });
        let final_resp = acc.into_final().expect("expected final response");
        match &final_resp.content[0] {
            ContentBlock::ToolUse(tu) => {
                assert_eq!(tu.id, "call_1");
                assert_eq!(tu.name, "get_weather");
                assert_eq!(
                    tu.input.get("city").and_then(|v| v.as_str()),
                    Some("Beijing")
                );
            }
            _ => panic!("expected tool use block"),
        }
    }

    #[test]
    fn message_delta_records_finish_reason_and_usage() {
        let mut acc = AnthropicIncrementer::default();
        acc.accumulate(&MessagesStreamEvent::MessageStart {
            message: MessageStart {
                id: "msg_1".to_string(),
                model: "minimax-m3".to_string(),
                role: "assistant".to_string(),
                content: vec![],
                stop_reason: None,
                stop_sequence: None,
                usage: None,
            },
            usage: Some(Usage {
                input_tokens: Some(11),
                output_tokens: None,
            }),
        });
        acc.accumulate(&MessagesStreamEvent::MessageDelta {
            delta: MessageDelta {
                stop_reason: Some("tool_use".to_string()),
                stop_sequence: None,
            },
            usage: Some(Usage {
                input_tokens: None,
                output_tokens: Some(7),
            }),
        });
        let final_resp = acc.into_final().expect("expected final response");
        assert_eq!(final_resp.id, "msg_1");
        assert_eq!(final_resp.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(final_resp.usage.input_tokens, 11);
        assert_eq!(final_resp.usage.output_tokens, 7);
    }
}
