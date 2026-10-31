//! The Gemini [`Model`] implementation.
//!
//! Java drives the official Google GenAI SDK (`com.google.genai.Client`). Rust
//! has no equivalent dependency, so this model speaks the REST wire protocol
//! (`models/{model}:generateContent` / `:streamGenerateContent?alt=sse`) through
//! an injected [`HttpTransport`], keeping the adapter fully offline-testable.
//! This follows option (b) from `MODEL__GAP_TRACKER.md` §4.2.

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;

use agentscope_core::model::catalog::{descriptor_for, ModelDescriptor};
use agentscope_core::{ChatResponse, Error, GenerateOptions, Model, Msg, Result, ToolSchema};

use crate::common::error::ModelError;
use crate::common::{HttpRequestSpec, SharedTransport};
use crate::gemini::converter::{build_request_with_server_tools, GeminiServerTool};
use crate::gemini::credential::{
    generate_content_path, stream_generate_content_path, API_KEY_HEADER, DEFAULT_BASE_URL,
};
use crate::gemini::dto::{GeminiErrorBody, GeminiResponse};
use crate::gemini::parser::{parse_response, GeminiStreamIncrementer};

/// Provider knobs consumed by the provider/model: they must never be merged
/// into the request body (Java keeps them in `ModelCreationContext#options`).
const PROVIDER_OPTION_KEYS: [&str; 2] = ["vertexAI", "contextWindowSize"];

/// Normalises a Gemini error body into a structured [`ModelError`].
fn gemini_error(error: &GeminiErrorBody) -> Error {
    ModelError::from_parts(
        "gemini",
        error.code.clamp(0, u16::MAX as i64) as u16,
        error.message.clone(),
        Some(error.status.clone()),
        Some(error.code.to_string()),
    )
    .to_core()
}

/// A chat model backed by the Google Gemini (GenAI) REST API.
pub struct GeminiChatModel {
    model_name: String,
    api_key: String,
    base_url: String,
    transport: SharedTransport,
    default_options: GenerateOptions,
    stream: bool,
    context_window_size: usize,
    descriptor: Option<Arc<ModelDescriptor>>,
    server_tools: Vec<GeminiServerTool>,
}

impl GeminiChatModel {
    /// Creates a model with the given name, API key and optional base URL.
    pub fn new(
        model_name: impl Into<String>,
        api_key: impl Into<String>,
        base_url: Option<String>,
        transport: SharedTransport,
    ) -> Self {
        let model_name = model_name.into();
        let configured_base_url = base_url.clone();
        let base_url = configured_base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let descriptor =
            descriptor_for(Some("gemini"), &model_name, configured_base_url.as_deref());
        let context_window_size = descriptor
            .as_ref()
            .map(|descriptor| descriptor.context_window)
            .unwrap_or(0);
        Self {
            model_name,
            api_key: api_key.into(),
            base_url,
            transport,
            default_options: GenerateOptions::default(),
            stream: true,
            context_window_size,
            descriptor,
            server_tools: Vec::new(),
        }
    }

    /// Registers provider-executed (server) tools such as Google Search.
    pub fn with_server_tools(mut self, tools: Vec<GeminiServerTool>) -> Self {
        self.server_tools = tools;
        self
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

    /// The endpoint path selected for the given mode.
    pub fn endpoint_path(&self, stream: bool) -> String {
        if stream {
            stream_generate_content_path(&self.model_name)
        } else {
            generate_content_path(&self.model_name)
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
        let request = build_request_with_server_tools(
            &messages,
            &tools,
            options,
            &self.default_options,
            &self.server_tools,
        );
        let mut body = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
        // Extra body params merge last so callers can override any field, but
        // provider knobs are never wire fields (MODEL-09): Java keeps them in
        // `ModelCreationContext#options`, so they are dropped here.
        if let Some(map) = body.as_object_mut() {
            for opts in [&self.default_options, options] {
                for (key, value) in &opts.additional_body_params {
                    if PROVIDER_OPTION_KEYS.contains(&key.as_str()) {
                        continue;
                    }
                    map.insert(key.clone(), value.clone());
                }
            }
        }

        let url = format!("{}{}", self.base_url, self.endpoint_path(stream));
        let mut spec = HttpRequestSpec::new(url, body)
            .streaming(stream)
            .provider("gemini")
            .header("content-type", "application/json")
            .header(API_KEY_HEADER, self.api_key.clone());
        for opts in [&self.default_options, options] {
            for (name, value) in &opts.additional_headers {
                spec = spec.header(name.clone(), value.clone());
            }
        }
        spec
    }
}

impl Model for GeminiChatModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        let options = crate::common::clamp_max_tokens(options, self.max_output_tokens());
        let stream_enabled = options.stream.unwrap_or(self.stream);
        Box::pin(async_stream::stream! {
            let spec = self.build_http_request(messages, tools, &options, stream_enabled);
            if stream_enabled {
                let mut frames = self.transport.post_json_stream(spec);
                let mut incrementer = GeminiStreamIncrementer::new();
                while let Some(frame) = frames.next().await {
                    let value = match frame {
                        Ok(value) => value,
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    };
                    let response: GeminiResponse = match serde_json::from_value(value) {
                        Ok(response) => response,
                        Err(e) => {
                            yield Err(Error::from(e));
                            return;
                        }
                    };
                    if let Some(error) = &response.error {
                        yield Err(gemini_error(error));
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
                    Ok(value) => match serde_json::from_value::<GeminiResponse>(value) {
                        Ok(response) => {
                            if let Some(error) = &response.error {
                                yield Err(gemini_error(error));
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

    fn supports_tool_choice_specific(&self) -> bool {
        true
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
    use agentscope_core::{ContentBlock, JsonMap, ToolChoice, ToolUseBlock};

    fn model_with(transport: MemoryTransport, name: &str) -> GeminiChatModel {
        GeminiChatModel::new(name, "gm-test", None, Arc::new(transport))
    }

    async fn collect(
        model: &GeminiChatModel,
        options: GenerateOptions,
    ) -> Vec<Result<ChatResponse>> {
        model
            .stream(vec![Msg::user("hi")], Vec::new(), options)
            .collect()
            .await
    }

    #[tokio::test]
    async fn non_streaming_request_targets_generate_content() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "candidates": [{ "content": { "role": "model", "parts": [{ "text": "Hi there" }] }, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 4, "candidatesTokenCount": 7 },
            "responseId": "resp_1"
        }));
        let model = model_with(transport.clone(), "gemini-2.5-flash");
        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;

        assert_eq!(out.len(), 1);
        let response = out[0].as_ref().unwrap();
        assert!(matches!(&response.content[0], ContentBlock::Text(t) if t.text == "Hi there"));
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(response.finish_reason.as_deref(), Some("STOP"));

        let spec = transport.last_request().unwrap();
        assert_eq!(
            spec.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert!(!spec.stream);
        assert_eq!(spec.body["contents"][0]["role"], serde_json::json!("user"));
        assert!(spec
            .headers
            .iter()
            .any(|(k, v)| k == "x-goog-api-key" && v == "gm-test"));
    }

    #[tokio::test]
    async fn base_url_override_is_respected() {
        let transport = MemoryTransport::with_response(serde_json::json!({ "candidates": [] }));
        let model = GeminiChatModel::new(
            "gemini-2.5-flash",
            "gm",
            Some("https://proxy.test".into()),
            Arc::new(transport.clone()),
        );
        let _ = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            transport.last_request().unwrap().url,
            "https://proxy.test/v1beta/models/gemini-2.5-flash:generateContent"
        );
    }

    #[tokio::test]
    async fn streaming_uses_sse_endpoint_and_yields_incremental_chunks() {
        let transport = MemoryTransport::with_stream(vec![
            serde_json::json!({ "responseId": "resp_2", "candidates": [{ "content": { "parts": [{ "text": "Hel" }] } }] }),
            serde_json::json!({ "candidates": [{ "content": { "parts": [{ "text": "lo" }] } }] }),
            serde_json::json!({
                "candidates": [{ "content": { "parts": [{ "functionCall": { "id": "call_1", "name": "f", "args": {} } }] }, "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 2, "candidatesTokenCount": 3 }
            }),
        ]);
        let model = model_with(transport.clone(), "gemini-2.5-flash");
        let out = collect(&model, GenerateOptions::default()).await;

        assert_eq!(out.len(), 3);
        assert!(
            matches!(&out[0].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "Hel")
        );
        assert!(
            matches!(&out[1].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "lo")
        );
        let tail = out[2].as_ref().unwrap();
        assert_eq!(tail.usage.output_tokens, 3);
        assert_eq!(tail.finish_reason.as_deref(), Some("STOP"));
        assert!(matches!(&tail.content[0], ContentBlock::ToolUse(t) if t.name == "f"));

        let spec = transport.last_request().unwrap();
        assert!(spec.stream);
        assert_eq!(
            spec.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[tokio::test]
    async fn error_payload_surfaces_as_model_error() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "error": { "code": 400, "message": "bad key", "status": "INVALID_ARGUMENT" }
        }));
        let model = model_with(transport, "gemini-2.5-flash");
        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;
        let err = out[0].as_ref().unwrap_err().to_string();
        assert!(err.contains("INVALID_ARGUMENT"), "unexpected error: {err}");
        assert!(err.contains("bad key"), "unexpected error: {err}");
    }

    #[test]
    fn multimodal_and_tool_branches_reach_the_body() {
        use agentscope_core::{Base64Source, ImageBlock, Source};
        let model = GeminiChatModel::new(
            "gemini-2.5-flash",
            "gm",
            None,
            Arc::new(MemoryTransport::default()),
        );
        let options = GenerateOptions {
            tool_choice: Some(ToolChoice::Specific {
                tool_name: "f".into(),
            }),
            thinking_budget: Some(256),
            ..Default::default()
        };
        let messages = vec![Msg::user_blocks(vec![
            ContentBlock::Text(agentscope_core::TextBlock::new("look")),
            ContentBlock::Image(ImageBlock::new(Source::Base64(Base64Source::new(
                "image/png",
                "AAAA",
            )))),
        ])];
        let spec =
            model.build_http_request(messages, vec![ToolSchema::new("f", "d")], &options, false);
        assert_eq!(
            spec.body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            serde_json::json!("image/png")
        );
        assert_eq!(
            spec.body["tools"][0]["functionDeclarations"][0]["name"],
            serde_json::json!("f")
        );
        assert_eq!(
            spec.body["toolConfig"]["functionCallingConfig"]["mode"],
            serde_json::json!("ANY")
        );
        assert_eq!(
            spec.body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            serde_json::json!(256)
        );
    }

    #[test]
    fn provider_knobs_never_reach_the_request_body() {
        // MODEL-09: `contextWindowSize` / `vertexAI` are provider knobs (Java
        // keeps them in `ModelCreationContext#options`), so they must not be
        // merged into the REST body.
        let mut defaults = GenerateOptions::default();
        defaults
            .additional_body_params
            .insert("contextWindowSize".to_string(), serde_json::json!(321_000));
        defaults
            .additional_body_params
            .insert("vertexAI".to_string(), serde_json::json!(false));
        let model = model_with(MemoryTransport::default(), "gemini-2.5-flash")
            .with_default_options(defaults);
        let mut options = GenerateOptions::default();
        options
            .additional_body_params
            .insert("contextWindowSize".to_string(), serde_json::json!(4096));
        let spec = model.build_http_request(vec![Msg::user("hi")], Vec::new(), &options, false);
        assert!(
            spec.body.get("contextWindowSize").is_none(),
            "provider knob leaked into the body: {:?}",
            spec.body
        );
        assert!(
            spec.body.get("vertexAI").is_none(),
            "provider knob leaked into the body: {:?}",
            spec.body
        );
    }

    #[test]
    fn extra_body_params_are_merged_last() {
        let options = GenerateOptions {
            additional_body_params: [("safetySettings".to_string(), serde_json::json!([]))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let model = GeminiChatModel::new(
            "gemini-2.5-flash",
            "gm",
            None,
            Arc::new(MemoryTransport::default()),
        );
        let spec = model.build_http_request(vec![Msg::user("hi")], Vec::new(), &options, false);
        assert_eq!(spec.body["safetySettings"], serde_json::json!([]));
        let _ = JsonMap::new();
        let _ = ToolUseBlock::new("id", "f", JsonMap::new());
    }

    #[test]
    fn capabilities_are_reported() {
        let model = GeminiChatModel::new(
            "gemini-2.5-flash",
            "gm",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_context_window_size(1_048_576);
        assert_eq!(model.model_name(), "gemini-2.5-flash");
        assert_eq!(model.context_window_size(), 1_048_576);
        assert!(model.supports_native_structured_output());
        assert_eq!(
            model.base_url(),
            "https://generativelanguage.googleapis.com"
        );
        assert_eq!(
            model.endpoint_path(false),
            "/v1beta/models/gemini-2.5-flash:generateContent"
        );
    }
}
