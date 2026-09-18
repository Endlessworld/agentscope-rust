//! The DashScope [`Model`] implementation.
//!
//! Calls the DashScope native generation / multimodal-generation endpoints
//! through an injected [`HttpTransport`], which keeps the model fully
//! unit-testable without network access.

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;

use agentscope_core::{
    ChatResponse, Error, GenerateOptions, Model, Msg, Result, ToolSchema,
};

use crate::common::{HttpRequestSpec, SharedTransport};
use crate::dashscope::converter::{
    build_request, is_multimodal_model, to_dashscope_messages, DEFAULT_BASE_URL,
    MULTIMODAL_GENERATION_ENDPOINT, TEXT_GENERATION_ENDPOINT,
};
use crate::dashscope::credential::EndpointType;
use crate::dashscope::dto::DashScopeResponse;
use crate::dashscope::parser::{parse_response, DashScopeStreamIncrementer};

/// Public keys endpoint used for request encryption.
pub const PUBLIC_KEYS_ENDPOINT: &str = "/api/v1/public-keys/latest";

/// A chat model backed by the DashScope (Alibaba Cloud) native API.
pub struct DashScopeChatModel {
    model_name: String,
    api_key: String,
    base_url: String,
    endpoint_type: EndpointType,
    default_options: GenerateOptions,
    transport: SharedTransport,
    stream: bool,
    enable_thinking: bool,
    context_window_size: usize,
}

impl DashScopeChatModel {
    /// Creates a model with the given name, API key and optional base URL.
    pub fn new(
        model_name: impl Into<String>,
        api_key: impl Into<String>,
        base_url: Option<String>,
        transport: SharedTransport,
    ) -> Self {
        Self {
            model_name: model_name.into(),
            api_key: api_key.into(),
            base_url: base_url
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            endpoint_type: EndpointType::Auto,
            default_options: GenerateOptions::default(),
            transport,
            stream: true,
            enable_thinking: false,
            context_window_size: 0,
        }
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

    /// Forces a specific endpoint type.
    pub fn with_endpoint_type(mut self, endpoint_type: EndpointType) -> Self {
        self.endpoint_type = endpoint_type;
        self
    }

    /// Enables thinking mode by default.
    pub fn with_enable_thinking(mut self, enable_thinking: bool) -> Self {
        self.enable_thinking = enable_thinking;
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

    /// Whether this model talks to the multimodal endpoint.
    pub fn requires_multimodal_api(&self) -> bool {
        match self.endpoint_type {
            EndpointType::Multimodal => true,
            EndpointType::Text => false,
            EndpointType::Auto => is_multimodal_model(&self.model_name),
        }
    }

    /// The endpoint path selected for this model.
    pub fn endpoint_path(&self) -> &'static str {
        if self.requires_multimodal_api() {
            MULTIMODAL_GENERATION_ENDPOINT
        } else {
            TEXT_GENERATION_ENDPOINT
        }
    }

    /// Builds the URL + body for a single request.
    pub fn build_http_request(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: &GenerateOptions,
        stream: bool,
    ) -> HttpRequestSpec {
        let force_multimodal = self.requires_multimodal_api();
        let ds_messages = to_dashscope_messages(messages, force_multimodal);
        let request = build_request(
            &self.model_name,
            ds_messages,
            stream,
            options,
            &self.default_options,
            tools,
            options.tool_choice.as_ref(),
            self.enable_thinking,
        );
        let body = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);

        let mut spec = HttpRequestSpec::new(
            format!("{}{}", self.base_url, self.endpoint_path()),
            body,
        )
        .streaming(stream)
        .header("Authorization", format!("Bearer {}", self.api_key))
        .header("Content-Type", "application/json");
        if stream {
            spec = spec.header("X-DashScope-SSE", "enable");
        }
        for (name, value) in &options.additional_headers {
            spec = spec.header(name.clone(), value.clone());
        }
        spec
    }
}

impl Model for DashScopeChatModel {
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
                let mut incrementer = DashScopeStreamIncrementer::new();
                while let Some(frame) = frames.next().await {
                    let value = match frame {
                        Ok(value) => value,
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    };
                    let response: DashScopeResponse = match serde_json::from_value(value) {
                        Ok(response) => response,
                        Err(e) => {
                            yield Err(Error::from(e));
                            return;
                        }
                    };
                    if let Some(code) = response.code.clone() {
                        yield Err(Error::Model(format!(
                            "DashScope error {code}: {}",
                            response.message.clone().unwrap_or_default()
                        )));
                        return;
                    }
                    if let Some(chunk) = incrementer.accumulate(&response) {
                        yield Ok(chunk);
                    }
                }
                if let Some(tail) = incrementer.into_final() {
                    yield Ok(tail);
                }
            } else {
                match self.transport.post_json(spec).await {
                    Ok(value) => match serde_json::from_value::<DashScopeResponse>(value) {
                        Ok(response) => {
                            if let Some(code) = response.code.clone() {
                                yield Err(Error::Model(format!(
                                    "DashScope error {code}: {}",
                                    response.message.clone().unwrap_or_default()
                                )));
                            } else {
                                yield Ok(parse_response(&response));
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
        true
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
    use agentscope_core::{ContentBlock, Msg};

    fn model_with(transport: MemoryTransport, name: &str) -> DashScopeChatModel {
        DashScopeChatModel::new(name, "sk-test", None, Arc::new(transport))
    }

    async fn collect(
        model: &DashScopeChatModel,
        options: GenerateOptions,
    ) -> Vec<Result<ChatResponse>> {
        model
            .stream(vec![Msg::user("hi")], Vec::new(), options)
            .collect()
            .await
    }

    #[tokio::test]
    async fn non_streaming_request_hits_text_endpoint() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "request_id": "req-1",
            "output": { "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "Hi there" } }] },
            "usage": { "input_tokens": 4, "output_tokens": 7 }
        }));
        let model = model_with(transport.clone(), "qwen-plus");
        let out = collect(&model, GenerateOptions { stream: Some(false), ..Default::default() }).await;

        assert_eq!(out.len(), 1);
        let response = out[0].as_ref().unwrap();
        assert!(matches!(&response.content[0], ContentBlock::Text(t) if t.text == "Hi there"));
        assert_eq!(response.usage.output_tokens, 7);

        let spec = transport.last_request().unwrap();
        assert_eq!(
            spec.url,
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/text-generation/generation"
        );
        assert!(!spec.stream);
        assert_eq!(spec.body["model"], serde_json::json!("qwen-plus"));
        assert_eq!(spec.body["input"]["messages"][0]["role"], serde_json::json!("user"));
        assert!(spec
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer sk-test"));
    }

    #[tokio::test]
    async fn multimodal_model_hits_multimodal_endpoint() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "request_id": "req-2",
            "output": { "choices": [{ "message": { "content": "seen" } }] }
        }));
        let model = model_with(transport.clone(), "qwen-vl-plus");
        let _ = collect(&model, GenerateOptions { stream: Some(false), ..Default::default() }).await;

        let spec = transport.last_request().unwrap();
        assert_eq!(
            spec.url,
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
        );
    }

    #[tokio::test]
    async fn streaming_yields_incremental_chunks_then_flushes() {
        let transport = MemoryTransport::with_stream(vec![
            serde_json::json!({ "request_id": "r", "output": { "choices": [{ "message": { "content": "Hel" } }] } }),
            serde_json::json!({ "output": { "choices": [{ "message": { "content": "lo" } }] } }),
            serde_json::json!({
                "output": { "choices": [{ "finish_reason": "stop", "message": { "content": "" } }] },
                "usage": { "input_tokens": 2, "output_tokens": 3 }
            }),
        ]);
        let model = model_with(transport.clone(), "qwen-plus");
        let out = collect(&model, GenerateOptions::default()).await;

        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "Hel"));
        assert!(matches!(&out[1].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "lo"));
        assert_eq!(out[2].as_ref().unwrap().usage.output_tokens, 3);
        let spec = transport.last_request().unwrap();
        assert!(spec.stream);
        assert!(spec.headers.iter().any(|(k, v)| k == "X-DashScope-SSE" && v == "enable"));
    }

    #[tokio::test]
    async fn error_payload_surfaces_as_model_error() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "code": "InvalidApiKey",
            "message": "bad key"
        }));
        let model = model_with(transport, "qwen-plus");
        let out = collect(&model, GenerateOptions { stream: Some(false), ..Default::default() }).await;
        let err = out[0].as_ref().unwrap_err().to_string();
        assert!(err.contains("InvalidApiKey"), "unexpected error: {err}");
    }

    #[test]
    fn endpoint_type_override_wins_over_auto_detection() {
        let model = DashScopeChatModel::new(
            "qwen-vl-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_endpoint_type(EndpointType::Text);
        assert!(!model.requires_multimodal_api());
        assert_eq!(model.endpoint_path(), TEXT_GENERATION_ENDPOINT);

        let forced = DashScopeChatModel::new(
            "qwen-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_endpoint_type(EndpointType::Multimodal);
        assert!(forced.requires_multimodal_api());
    }

    #[test]
    fn capabilities_reported() {
        let model = DashScopeChatModel::new(
            "qwen-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_context_window_size(131072);
        assert_eq!(model.model_name(), "qwen-plus");
        assert_eq!(model.context_window_size(), 131072);
        assert!(model.supports_native_structured_output());
    }
}
