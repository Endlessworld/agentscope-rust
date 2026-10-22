//! The OpenAI Responses [`Model`] implementation for the `openai-official`
//! provider.
//!
//! Java drives the official `openai-java` SDK. Rust has no equivalent
//! dependency, so this adapter speaks the same Responses wire protocol through
//! the shared [`HttpTransport`] seam (mirroring the Gemini adapter) — the
//! provider id, prefix, request mapping, server tools, multi-agent formatter and
//! error policy are all preserved.

use std::sync::Arc;
use std::time::Instant;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;

use agentscope_core::error::{Error, Result};
use agentscope_core::model::catalog::{descriptor_for, ModelDescriptor};
use agentscope_core::model::context_windows::resolve_context_window;
use agentscope_core::model::{ChatResponse, GenerateOptions, Model};
use agentscope_core::{Msg, ToolSchema};

use crate::common::transport::{HttpRequestSpec, SharedTransport};
use crate::common::ReqwestTransport;

use super::constants::{API_KEY_ENV, BASE_URL_ENV, DEFAULT_BASE_URL, PROVIDER_ID};
use super::error::translate_optional_status;
use super::formatter::ResponsesMultiAgentFormatter;
use super::parser::{parse_response, ResponsesStreamingAssembler};
use super::request::map_request;
use super::server_tool::OpenAIServerTool;

/// OpenAI Responses model backed by the official provider semantics.
pub struct OpenAIResponsesChatModel {
    model_name: String,
    api_key: String,
    base_url: String,
    transport: SharedTransport,
    default_options: GenerateOptions,
    stream: bool,
    strict_tools: Option<bool>,
    strict_json_schema: Option<bool>,
    formatter: Option<ResponsesMultiAgentFormatter>,
    server_tools: Vec<OpenAIServerTool>,
    context_window_size: usize,
    descriptor: Option<Arc<ModelDescriptor>>,
}

impl OpenAIResponsesChatModel {
    /// Creates a model with an explicit API key and optional base URL.
    pub fn new(
        model_name: impl Into<String>,
        api_key: impl Into<String>,
        base_url: Option<String>,
        transport: SharedTransport,
    ) -> Self {
        let model_name = model_name.into();
        let configured_base_url = base_url.clone();
        let descriptor = descriptor_for(
            Some(PROVIDER_ID),
            &model_name,
            configured_base_url.as_deref(),
        );
        let context_window_size = descriptor
            .as_ref()
            .and_then(|descriptor| descriptor.effective_context())
            .unwrap_or_else(|| resolve_context_window(&model_name));
        Self {
            context_window_size,
            model_name,
            api_key: api_key.into(),
            base_url: base_url
                .filter(|url| !url.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            transport,
            default_options: GenerateOptions::default(),
            stream: true,
            strict_tools: None,
            strict_json_schema: None,
            formatter: None,
            server_tools: Vec::new(),
            descriptor,
        }
    }

    /// Sets the default options merged under every request.
    pub fn with_default_options(mut self, options: GenerateOptions) -> Self {
        self.default_options = options;
        self
    }

    /// Sets the default streaming mode.
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Enables/disables strict tool schemas at model level.
    pub fn with_strict_tools(mut self, strict: Option<bool>) -> Self {
        self.strict_tools = strict;
        self
    }

    /// Enables/disables strict JSON-schema output at model level.
    pub fn with_strict_json_schema(mut self, strict: Option<bool>) -> Self {
        self.strict_json_schema = strict;
        self
    }

    /// Installs the multi-agent history formatter.
    pub fn with_formatter(mut self, formatter: ResponsesMultiAgentFormatter) -> Self {
        self.formatter = Some(formatter);
        self
    }

    /// Registers provider-executed (server) tools.
    pub fn with_server_tools(mut self, tools: Vec<OpenAIServerTool>) -> Self {
        self.server_tools = tools;
        self
    }

    /// Sets the reported context window size in tokens.
    pub fn with_context_window_size(mut self, size: usize) -> Self {
        if size > 0 {
            self.context_window_size = size;
        }
        self
    }

    /// The resolved base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The configured API key (used by the connection-override validation).
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The endpoint path for the Responses API.
    pub fn endpoint_path(&self) -> &'static str {
        "/responses"
    }

    /// Builds the URL + body for a single request.
    pub fn build_http_request(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: &GenerateOptions,
        stream: bool,
    ) -> Result<HttpRequestSpec> {
        let mut effective = GenerateOptions::merge_options(options, &self.default_options);
        // Java's merged options always carry the model name configured on the
        // builder; a per-request call may omit it and must inherit it here.
        if effective
            .model_name
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            effective.model_name = Some(self.model_name.clone());
        }
        self.validate_connection_fields(&effective)?;

        let mut body = map_request(
            &messages,
            &tools,
            &self.server_tools,
            &effective,
            self.strict_tools,
            self.strict_json_schema,
            self.formatter.as_ref(),
        )?;
        if stream {
            body["stream"] = Value::Bool(true);
        }

        let url = format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.endpoint_path()
        );
        let mut spec = HttpRequestSpec::new(url, body)
            .streaming(stream)
            .provider(PROVIDER_ID)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.api_key));

        // Builder-level headers only; per-request headers are rejected by the
        // request mapper (Java `validateRejectedFields`).
        for (name, value) in &self.default_options.additional_headers {
            spec = spec.header(name.clone(), value.clone());
        }
        Ok(spec)
    }

    fn validate_connection_fields(&self, options: &GenerateOptions) -> Result<()> {
        if let Some(api_key) = options.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if api_key != self.api_key {
                return Err(Error::Model(
                    "apiKey override per request is not supported by the openai-official provider. \
                     The apiKey must match the one configured at model construction."
                        .to_string(),
                ));
            }
        }
        if let Some(base_url) = options.base_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if base_url != self.base_url {
                return Err(Error::Model(
                    "baseUrl override per request is not supported by the openai-official provider. \
                     The baseUrl must match the one configured at model construction."
                        .to_string(),
                ));
            }
        }
        Ok(())
    }
}

impl Model for OpenAIResponsesChatModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        let options = crate::common::clamp_max_tokens(options, self.max_output_tokens());
        let stream_enabled = options.stream.unwrap_or(self.stream);
        Box::pin(async_stream::stream! {
            let spec = match self.build_http_request(messages, tools, &options, stream_enabled) {
                Ok(spec) => spec,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };

            if stream_enabled {
                let mut frames = self.transport.post_json_stream(spec);
                let mut assembler =
                    ResponsesStreamingAssembler::new(self.model_name.clone(), Instant::now());
                while let Some(frame) = frames.next().await {
                    let event = match frame {
                        Ok(event) => event,
                        Err(error) => {
                            yield Err(error);
                            return;
                        }
                    };
                    match assembler.process_event(&event) {
                        Ok(responses) => {
                            for response in responses {
                                yield Ok(response);
                            }
                        }
                        Err(error) => {
                            yield Err(error);
                            return;
                        }
                    }
                }
            } else {
                let start = Instant::now();
                match self.transport.post_json(spec).await {
                    Ok(value) => {
                        if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
                            yield Err(api_error_from_body(error));
                        } else {
                            match parse_response(&value, &self.model_name, start) {
                                Ok(response) => yield Ok(response),
                                Err(error) => yield Err(error),
                            }
                        }
                    }
                    Err(error) => yield Err(error),
                }
            }
        })
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn context_window_size(&self) -> usize {
        self.context_window_size
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
        true
    }

    fn supports_native_structured_output_with_tools(&self) -> bool {
        true
    }

    fn supports_tool_choice_specific(&self) -> bool {
        true
    }
}

fn api_error_from_body(error: &Value) -> Error {
    let message = error.get("message").and_then(Value::as_str);
    let error_type = error.get("type").and_then(Value::as_str).map(str::to_string);
    let code = error.get("code").and_then(Value::as_str).map(str::to_string);
    let status = error
        .get("status")
        .and_then(Value::as_u64)
        .map(|value| value as u16);
    translate_optional_status(status, message, error_type, code).to_core()
}

/// A transport factory so callers can construct models without referencing
/// `reqwest` directly.
pub fn default_transport() -> SharedTransport {
    Arc::new(ReqwestTransport::new())
}

/// Environment aliases kept public for diagnostics and tests.
pub const PROVIDER_API_KEY_ENV: &str = API_KEY_ENV;
/// Environment aliases kept public for diagnostics and tests.
pub const PROVIDER_BASE_URL_ENV: &str = BASE_URL_ENV;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::MemoryTransport;
    use crate::openai_official::server_tool::OpenAIServerTool;
    use agentscope_core::message::{ContentBlock, ToolResultBlock, ToolUseBlock};
    use serde_json::json;

    fn model_with(transport: MemoryTransport, name: &str) -> OpenAIResponsesChatModel {
        OpenAIResponsesChatModel::new(name, "sk-test", Some("https://api.test/v1".into()), Arc::new(transport))
    }

    async fn collect(
        model: &OpenAIResponsesChatModel,
        options: GenerateOptions,
    ) -> Vec<Result<ChatResponse>> {
        model
            .stream(vec![Msg::user("hi")], Vec::new(), options)
            .collect()
            .await
    }

    #[tokio::test]
    async fn non_streaming_request_targets_responses_endpoint() {
        let transport = MemoryTransport::with_response(json!({
            "id": "resp_1",
            "status": "completed",
            "output": [{ "type": "message", "role": "assistant",
                "content": [{ "type": "output_text", "text": "Hi there" }] }],
            "usage": { "input_tokens": 4, "output_tokens": 7 }
        }));
        let model = model_with(transport.clone(), "gpt-5");
        let out = collect(&model, GenerateOptions { stream: Some(false), ..Default::default() }).await;

        assert_eq!(out.len(), 1);
        let response = out[0].as_ref().unwrap();
        assert!(matches!(&response.content[0], ContentBlock::Text(t) if t.text == "Hi there"));
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(response.finish_reason.as_deref(), Some("completed"));

        let spec = transport.last_request().unwrap();
        assert_eq!(spec.url, "https://api.test/v1/responses");
        assert!(!spec.stream);
        assert_eq!(spec.body["model"], json!("gpt-5"));
        assert_eq!(spec.body["store"], json!(false));
        assert_eq!(spec.body["input"][0]["content"], json!("hi"));
        assert!(spec
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer sk-test"));
    }

    #[tokio::test]
    async fn streaming_uses_sse_and_yields_incremental_blocks() {
        let transport = MemoryTransport::with_stream(vec![
            json!({ "type": "response.output_text.delta", "delta": "Hel" }),
            json!({ "type": "response.output_text.delta", "delta": "lo" }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_2",
                    "status": "completed",
                    "output": [],
                    "usage": { "input_tokens": 2, "output_tokens": 3 }
                }
            }),
        ]);
        let model = model_with(transport.clone(), "gpt-5");
        let out = collect(&model, GenerateOptions::default()).await;

        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "Hel"));
        assert!(matches!(&out[1].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "lo"));
        assert_eq!(out[2].as_ref().unwrap().usage.output_tokens, 3);

        let spec = transport.last_request().unwrap();
        assert!(spec.stream);
        assert_eq!(spec.body["stream"], json!(true));
    }

    #[tokio::test]
    async fn server_tools_and_formatter_reach_the_request_body() {
        let transport = MemoryTransport::with_response(json!({
            "id": "resp_3",
            "status": "completed",
            "output": []
        }));
        let model = model_with(transport.clone(), "gpt-5")
            .with_server_tools(vec![OpenAIServerTool::web_search()])
            .with_formatter(ResponsesMultiAgentFormatter::new());
        let messages = vec![
            Msg::system("be nice"),
            Msg::user("hello"),
            Msg::assistant("hi"),
        ];
        let spec = model
            .build_http_request(messages, Vec::new(), &GenerateOptions::default(), false)
            .unwrap();

        assert_eq!(spec.body["tools"][0]["type"], json!("web_search"));
        assert_eq!(
            spec.body["include"],
            json!([
                "web_search_call.results",
                "web_search_call.action.sources"
            ])
        );
        // Formatter merges the user+assistant turns into one `<history>` user message.
        let input = spec.body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], json!("system"));
        assert_eq!(input[1]["role"], json!("user"));
        assert!(input[1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<history>"));
    }

    #[tokio::test]
    async fn per_request_api_key_override_is_rejected() {
        let transport = MemoryTransport::with_response(json!({ "id": "x", "output": [] }));
        let model = model_with(transport, "gpt-5");
        let options = GenerateOptions {
            api_key: Some("other".to_string()),
            stream: Some(false),
            ..Default::default()
        };
        let out = collect(&model, options).await;
        assert!(out[0]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("apiKey override"));
    }

    #[tokio::test]
    async fn in_band_error_body_is_normalised() {
        let transport = MemoryTransport::with_response(json!({
            "error": {
                "message": "slow down",
                "type": "rate_limit_error",
                "code": "rate_limit_exceeded"
            }
        }));
        let model = model_with(transport, "gpt-5");
        let out = collect(&model, GenerateOptions { stream: Some(false), ..Default::default() }).await;
        let rendered = out[0].as_ref().unwrap_err().to_string();
        assert!(rendered.contains("rate_limit_error"), "{rendered}");
        assert!(rendered.contains("rate_limit_exceeded"), "{rendered}");
    }

    #[test]
    fn multimodal_and_tool_history_reach_the_body() {
        let model = OpenAIResponsesChatModel::new(
            "gpt-5",
            "sk",
            Some("https://api.test/v1".into()),
            Arc::new(MemoryTransport::default()),
        );
        let assistant = Msg::assistant_blocks(vec![
            ContentBlock::Text(agentscope_core::TextBlock::new("checking")),
            ContentBlock::ToolUse(ToolUseBlock::new(
                "call_1",
                "get_weather",
                [("city".to_string(), json!("Beijing"))].into_iter().collect(),
            )),
        ]);
        let tool = Msg::tool_blocks(vec![ToolResultBlock::new(
            "call_1",
            "get_weather",
            vec![ContentBlock::Text(agentscope_core::TextBlock::new("sunny"))],
        )]);
        let spec = model
            .build_http_request(
                vec![Msg::user("weather?"), assistant, tool],
                Vec::new(),
                &GenerateOptions::default(),
                false,
            )
            .unwrap();
        let input = spec.body["input"].as_array().unwrap();
        assert_eq!(input[0]["content"], json!("weather?"));
        // The assistant text is emitted before its function calls (Java order).
        assert_eq!(input[1]["type"], json!("message"));
        assert_eq!(input[1]["content"], json!("checking"));
        assert_eq!(input[2]["type"], json!("function_call"));
        assert_eq!(input[3]["type"], json!("function_call_output"));
        assert_eq!(input[3]["output"], json!("sunny"));
    }

    #[test]
    fn capabilities_are_reported() {
        let model = OpenAIResponsesChatModel::new(
            "gpt-5",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_context_window_size(400_000)
        .with_strict_tools(Some(true))
        .with_strict_json_schema(Some(true));
        assert_eq!(model.model_name(), "gpt-5");
        assert_eq!(model.context_window_size(), 400_000);
        assert_eq!(model.base_url(), DEFAULT_BASE_URL);
        assert_eq!(model.endpoint_path(), "/responses");
        assert!(model.supports_native_structured_output());
        assert!(model.supports_native_structured_output_with_tools());
        assert!(model.supports_tool_choice_specific());
        assert_eq!(model.api_key(), "sk");
    }
}
