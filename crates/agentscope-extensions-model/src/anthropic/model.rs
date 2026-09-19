//! The Anthropic [`Model`] implementation.
//!
//! Java drives the official Anthropic SDK (`AnthropicOkHttpClient`). Rust has no
//! equivalent core dependency, so this model speaks the Messages wire protocol
//! itself through an injected [`HttpTransport`], keeping the whole adapter
//! offline-testable. This follows option (b) from `MODEL__GAP_TRACKER.md` §4.2
//! (`reqwest` + hand-written DTO/parser, exactly like the ollama/dashscope
//! adapters).

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;

use agentscope_core::{ChatResponse, Error, GenerateOptions, Model, Msg, Result, ToolSchema};

use crate::anthropic::converter::{
    apply_cache_control, apply_extra_body_params, build_request, extra_headers, ChatFormatOptions,
};
use crate::anthropic::credential::{AnthropicAuthMode, DEFAULT_BASE_URL, MESSAGES_ENDPOINT};
use crate::anthropic::dto::{AnthropicMessageResponse, AnthropicStreamEvent};
use crate::anthropic::parser::{parse_message, AnthropicStreamIncrementer};
use crate::common::{HttpRequestSpec, SharedTransport};

/// Anthropic API version header name.
pub const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";

/// A chat model backed by the Anthropic Messages API.
pub struct AnthropicChatModel {
    model_name: String,
    auth_mode: AnthropicAuthMode,
    secret: String,
    base_url: String,
    transport: SharedTransport,
    default_options: GenerateOptions,
    stream: bool,
    cache_ttl: Option<String>,
    context_window_size: usize,
}

impl AnthropicChatModel {
    /// Creates a model from a resolved auth mode + secret.
    pub fn new(
        model_name: impl Into<String>,
        auth_mode: AnthropicAuthMode,
        secret: impl Into<String>,
        base_url: Option<String>,
        transport: SharedTransport,
    ) -> Self {
        Self {
            model_name: model_name.into(),
            auth_mode,
            secret: secret.into(),
            base_url: base_url
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            transport,
            default_options: GenerateOptions::default(),
            stream: true,
            cache_ttl: None,
            context_window_size: 0,
        }
    }

    /// Creates a model from an API key (the native Anthropic auth scheme).
    pub fn from_api_key(
        model_name: impl Into<String>,
        api_key: impl Into<String>,
        base_url: Option<String>,
        transport: SharedTransport,
    ) -> Self {
        Self::new(model_name, AnthropicAuthMode::ApiKey, api_key, base_url, transport)
    }

    /// Sets the default options merged under every request.
    pub fn with_default_options(mut self, options: GenerateOptions) -> Self {
        self.default_options = options;
        self
    }

    /// Sets the default streaming mode (overridable per request).
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Sets the TTL applied to prompt-caching markers (e.g. `"1h"`).
    pub fn with_cache_ttl(mut self, ttl: impl Into<String>) -> Self {
        let ttl = ttl.into();
        self.cache_ttl = if ttl.trim().is_empty() { None } else { Some(ttl) };
        self
    }

    /// Sets the reported context window size in tokens.
    pub fn with_context_window_size(mut self, size: usize) -> Self {
        self.context_window_size = size;
        self
    }

    /// The resolved base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The resolved messages endpoint URL.
    pub fn endpoint_url(&self) -> String {
        format!("{}{}", self.base_url, MESSAGES_ENDPOINT)
    }

    /// Builds the URL + body for a single request.
    pub fn build_http_request(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: &GenerateOptions,
        stream: bool,
    ) -> HttpRequestSpec {
        let effective = GenerateOptions::merge_options(options, &self.default_options);
        let fmt = ChatFormatOptions::from_options(&effective, self.cache_ttl.as_deref());
        let mut request = build_request(
            &self.model_name,
            &messages,
            &tools,
            options,
            &self.default_options,
            &fmt,
            stream,
        );
        apply_cache_control(&mut request.messages, &fmt);

        let mut body = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
        apply_extra_body_params(&mut body, &[&self.default_options, options]);

        let mut spec = HttpRequestSpec::new(self.endpoint_url(), body)
            .streaming(stream)
            .header("content-type", "application/json")
            .header(
                ANTHROPIC_VERSION_HEADER,
                crate::anthropic::credential::ANTHROPIC_VERSION,
            );
        spec = match self.auth_mode {
            AnthropicAuthMode::ApiKey => spec.header("x-api-key", self.secret.clone()),
            AnthropicAuthMode::AuthToken => {
                spec.header("authorization", format!("Bearer {}", self.secret))
            }
        };
        for (name, value) in extra_headers(&[&self.default_options, options]) {
            spec = spec.header(name, value);
        }
        spec
    }
}

impl Model for AnthropicChatModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        let stream_enabled = options.stream.unwrap_or(self.stream);
        Box::pin(async_stream::stream! {
            let spec = self.build_http_request(messages, tools, &options, stream_enabled);
            if stream_enabled {
                let mut frames = self.transport.post_json_stream(spec);
                let mut incrementer = AnthropicStreamIncrementer::new();
                while let Some(frame) = frames.next().await {
                    let value = match frame {
                        Ok(value) => value,
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    };
                    let event: AnthropicStreamEvent = match serde_json::from_value(value) {
                        Ok(event) => event,
                        Err(e) => {
                            yield Err(Error::from(e));
                            return;
                        }
                    };
                    if let Some(error) = &event.error {
                        yield Err(Error::Model(format!(
                            "Anthropic error {}: {}",
                            error.kind, error.message
                        )));
                        return;
                    }
                    if let Some(chunk) = incrementer.accumulate(&event) {
                        yield Ok(chunk);
                    }
                }
                if let Some(tail) = incrementer.into_final() {
                    yield Ok(tail);
                }
            } else {
                match self.transport.post_json(spec).await {
                    Ok(value) => match serde_json::from_value::<AnthropicMessageResponse>(value) {
                        Ok(response) => {
                            if let Some(error) = &response.error {
                                yield Err(Error::Model(format!(
                                    "Anthropic error {}: {}",
                                    error.kind, error.message
                                )));
                            } else {
                                yield Ok(parse_message(&response));
                            }
                        }
                        Err(e) => yield Err(Error::from(e)),
                    },
                    Err(e) => yield Err(e),
                }
            }
        })
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn supports_native_structured_output(&self) -> bool {
        false
    }

    fn context_window_size(&self) -> usize {
        self.context_window_size
    }
}

/// A transport factory so callers can construct models without referencing
/// `reqwest` directly.
pub fn default_transport() -> SharedTransport {
    Arc::new(crate::common::ReqwestTransport::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::MemoryTransport;
    use agentscope_core::{ContentBlock, JsonMap, ToolChoice};

    fn model_with(transport: MemoryTransport, name: &str) -> AnthropicChatModel {
        AnthropicChatModel::from_api_key(name, "sk-ant-test", None, Arc::new(transport))
    }

    async fn collect(
        model: &AnthropicChatModel,
        options: GenerateOptions,
    ) -> Vec<Result<ChatResponse>> {
        model
            .stream(vec![Msg::user("hi")], Vec::new(), options)
            .collect()
            .await
    }

    #[tokio::test]
    async fn non_streaming_request_targets_messages_endpoint() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "id": "msg_1",
            "content": [{ "type": "text", "text": "Hi there" }],
            "usage": { "input_tokens": 4, "output_tokens": 7 },
            "stop_reason": "end_turn"
        }));
        let model = model_with(transport.clone(), "claude-sonnet-4-5-20250929");
        let out = collect(&model, GenerateOptions { stream: Some(false), ..Default::default() }).await;

        assert_eq!(out.len(), 1);
        let response = out[0].as_ref().unwrap();
        assert!(matches!(&response.content[0], ContentBlock::Text(t) if t.text == "Hi there"));
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(response.finish_reason.as_deref(), Some("end_turn"));

        let spec = transport.last_request().unwrap();
        assert_eq!(spec.url, "https://api.anthropic.com/v1/messages");
        assert!(!spec.stream);
        assert_eq!(spec.body["model"], serde_json::json!("claude-sonnet-4-5-20250929"));
        assert_eq!(spec.body["max_tokens"], serde_json::json!(4096));
        assert!(spec
            .headers
            .iter()
            .any(|(k, v)| k == "x-api-key" && v == "sk-ant-test"));
        assert!(spec
            .headers
            .iter()
            .any(|(k, v)| k == "anthropic-version" && v == "2023-06-01"));
    }

    #[tokio::test]
    async fn bearer_token_uses_authorization_header() {
        let transport = MemoryTransport::with_response(serde_json::json!({ "id": "m", "content": [] }));
        let model = AnthropicChatModel::new(
            "claude",
            AnthropicAuthMode::AuthToken,
            "tok-123",
            Some("https://gateway.test".into()),
            Arc::new(transport.clone()),
        );
        let _ = collect(&model, GenerateOptions { stream: Some(false), ..Default::default() }).await;

        let spec = transport.last_request().unwrap();
        assert_eq!(spec.url, "https://gateway.test/v1/messages");
        assert!(spec
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer tok-123"));
        assert!(!spec.headers.iter().any(|(k, _)| k == "x-api-key"));
    }

    #[tokio::test]
    async fn streaming_yields_incremental_chunks_then_finalizes_usage() {
        let transport = MemoryTransport::with_stream(vec![
            serde_json::json!({
                "type": "message_start",
                "message": { "id": "msg_2", "usage": { "input_tokens": 3, "cache_read_input_tokens": 1 } }
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "Hel" }
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "lo" }
            }),
            serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 5 }
            }),
        ]);
        let model = model_with(transport.clone(), "claude");
        let out = collect(&model, GenerateOptions::default()).await;

        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "Hel"));
        assert!(matches!(&out[1].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "lo"));
        let tail = out[2].as_ref().unwrap();
        assert_eq!(tail.usage.output_tokens, 5);
        assert_eq!(tail.usage.input_tokens, 4);
        assert_eq!(tail.finish_reason.as_deref(), Some("end_turn"));
        assert!(transport.last_request().unwrap().stream);
    }

    #[tokio::test]
    async fn error_payload_surfaces_as_model_error() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "bad key" }
        }));
        let model = model_with(transport, "claude");
        let out = collect(&model, GenerateOptions { stream: Some(false), ..Default::default() }).await;
        let err = out[0].as_ref().unwrap_err().to_string();
        assert!(err.contains("authentication_error"), "unexpected error: {err}");
        assert!(err.contains("bad key"), "unexpected error: {err}");
    }

    #[test]
    fn tool_and_thinking_branches_reach_the_body() {
        let model = AnthropicChatModel::from_api_key(
            "claude",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        );
        let options = GenerateOptions {
            tool_choice: Some(ToolChoice::Required),
            thinking_budget: Some(2048),
            ..Default::default()
        };
        let tools = vec![
            ToolSchema::new("f", "desc").parameters(
                serde_json::json!({"type": "object"}).as_object().unwrap().clone(),
            ),
        ];
        let spec = model.build_http_request(vec![Msg::user("hi")], tools, &options, true);
        assert_eq!(spec.body["thinking"]["type"], serde_json::json!("enabled"));
        assert_eq!(spec.body["thinking"]["budget_tokens"], serde_json::json!(2048));
        assert_eq!(spec.body["tool_choice"]["type"], serde_json::json!("any"));
        assert_eq!(spec.body["tools"][0]["name"], serde_json::json!("f"));
        assert_eq!(
            spec.body["tools"][0]["input_schema"]["type"],
            serde_json::json!("object")
        );
        let _ = JsonMap::new();
    }

    #[test]
    fn cache_control_marks_system_and_last_message_and_ttl_is_applied() {
        let model = AnthropicChatModel::from_api_key(
            "claude",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_cache_ttl("1h");
        let options = GenerateOptions { cache_control: Some(true), ..Default::default() };
        let messages = vec![Msg::system("be brief"), Msg::user("hi")];
        let spec = model.build_http_request(messages, Vec::new(), &options, false);
        assert_eq!(spec.body["system"][0]["cache_control"]["ttl"], serde_json::json!("1h"));
        assert_eq!(
            spec.body["messages"][0]["content"][0]["cache_control"]["type"],
            serde_json::json!("ephemeral")
        );
    }

    #[test]
    fn capabilities_are_reported() {
        let model = AnthropicChatModel::from_api_key(
            "claude",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_context_window_size(200_000);
        assert_eq!(model.model_name(), "claude");
        assert_eq!(model.context_window_size(), 200_000);
        assert!(!model.supports_native_structured_output());
        assert_eq!(model.base_url(), "https://api.anthropic.com");
    }
}
