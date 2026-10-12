//! The Ollama [`Model`] implementation.
//!
//! Calls `POST {base_url}/api/chat` through an injected [`HttpTransport`],
//! which keeps the model fully unit-testable without network access.

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;

use agentscope_core::{
    ChatResponse, Error, GenerateOptions, Model, Result, ToolSchema, Msg,
};

use crate::common::{HttpRequestSpec, SharedTransport};
use crate::ollama::converter::{build_request, to_ollama_messages};
use crate::ollama::dto::{OllamaMessage, OllamaResponse};
use crate::ollama::options::OllamaOptions;
use crate::ollama::parser::{parse_response, OllamaStreamIncrementer};

/// Default Ollama server address.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";
/// Ollama chat endpoint.
pub const CHAT_ENDPOINT: &str = "/api/chat";
/// Ollama embedding endpoint.
pub const EMBED_ENDPOINT: &str = "/api/embed";

/// A chat model backed by a local Ollama server.
pub struct OllamaChatModel {
    model_name: String,
    base_url: String,
    default_options: OllamaOptions,
    transport: SharedTransport,
    stream: bool,
    context_window_size: usize,
}

impl OllamaChatModel {
    /// Creates a model with the given name and base URL.
    ///
    /// `base_url == None` falls back to [`DEFAULT_BASE_URL`].
    pub fn new(
        model_name: impl Into<String>,
        base_url: Option<String>,
        transport: SharedTransport,
    ) -> Self {
        Self {
            model_name: model_name.into(),
            base_url: base_url
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            default_options: OllamaOptions::default(),
            transport,
            stream: true,
            context_window_size: 0,
        }
    }

    /// Sets the default options merged under every request.
    pub fn with_default_options(mut self, options: OllamaOptions) -> Self {
        self.default_options = options;
        self
    }

    /// Sets the default streaming mode (overridable per request).
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Generates embeddings via `POST {base_url}/api/embed` (mirrors Java's
    /// `OllamaHttpClient.embed`). Returns one embedding vector per input string.
    pub async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.embed_with_options(input, None).await
    }

    /// Embedding variant carrying an optional per-call `OllamaOptions`.
    pub async fn embed_with_options(
        &self,
        input: Vec<String>,
        options: Option<&OllamaOptions>,
    ) -> Result<Vec<Vec<f32>>> {
        let request = crate::ollama::dto::OllamaEmbeddingRequest::new(&self.model_name, input);
        let body = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
        let spec = HttpRequestSpec::new(
            format!("{}{}", self.base_url.trim_end_matches('/'), EMBED_ENDPOINT),
            body,
        )
        .provider("ollama");
        let _ = options; // reserved for future per-call options plumbing
        let value = self.transport.post_json(spec).await?;
        let response: crate::ollama::dto::OllamaEmbeddingResponse =
            serde_json::from_value(value).map_err(Error::from)?;
        Ok(response.embeddings)
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

    /// The default options.
    pub fn default_options(&self) -> &OllamaOptions {
        &self.default_options
    }

    /// Converts a message list and builds the wire request body.
    pub fn build_body(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: &GenerateOptions,
        stream: bool,
    ) -> Result<serde_json::Value> {
        let runtime = OllamaOptions::from_generate_options(options);
        let ollama_messages: Vec<OllamaMessage> = to_ollama_messages(messages)?;
        let request = build_request(
            &self.model_name,
            ollama_messages,
            stream,
            &runtime,
            &self.default_options,
            tools,
            options.tool_choice.as_ref(),
        );
        serde_json::to_value(&request).map_err(Error::from)
    }
}

impl Model for OllamaChatModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        let stream_enabled = options.stream.unwrap_or(self.stream);

        Box::pin(async_stream::stream! {
            let body = match self.build_body(messages, tools, &options, stream_enabled) {
                Ok(body) => body,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            let spec = HttpRequestSpec::new(
                format!("{}{}", self.base_url.trim_end_matches('/'), CHAT_ENDPOINT),
                body,
            )
            .streaming(stream_enabled)
            .provider("ollama");

            if stream_enabled {
                let mut frames = self.transport.post_json_stream(spec);
                let mut incrementer = OllamaStreamIncrementer::new();
                while let Some(frame) = frames.next().await {
                    let value = match frame {
                        Ok(value) => value,
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    };
                    let response: OllamaResponse = match serde_json::from_value(value) {
                        Ok(response) => response,
                        Err(e) => {
                            yield Err(Error::from(e));
                            return;
                        }
                    };
                    if let Some(message) = response.error {
                        yield Err(crate::common::error::ModelError::new("ollama", 0, message).to_core());
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
                    Ok(value) => match serde_json::from_value::<OllamaResponse>(value) {
                        Ok(response) => {
                            if let Some(message) = response.error {
                                yield Err(crate::common::error::ModelError::new("ollama", 0, message).to_core());
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

    fn model_with(transport: MemoryTransport) -> OllamaChatModel {
        OllamaChatModel::new("llama3", None, Arc::new(transport))
    }

    #[tokio::test]
    async fn embed_posts_to_embed_endpoint() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "model": "nomic-embed-text",
            "embeddings": [[0.1, 0.2], [0.3, 0.4]]
        }));
        let model = OllamaChatModel::new("nomic-embed-text", None, Arc::new(transport.clone()));
        let vectors = model.embed(vec!["a".into(), "b".into()]).await.unwrap();
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0], vec![0.1_f32, 0.2]);
        let req = transport.last_request().unwrap();
        assert!(req.url.ends_with("/api/embed"), "url={}", req.url);
        assert_eq!(req.provider, "ollama");
    }

    async fn collect(model: &OllamaChatModel, options: GenerateOptions) -> Vec<Result<ChatResponse>> {
        model
            .stream(vec![Msg::user("hi")], Vec::new(), options)
            .collect()
            .await
    }

    #[tokio::test]
    async fn non_streaming_request_hits_chat_endpoint() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "model": "llama3",
            "message": { "role": "assistant", "content": "Hi there" },
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 4,
            "eval_count": 7
        }));
        let model = model_with(transport.clone());
        let options = GenerateOptions { stream: Some(false), ..Default::default() };
        let out = collect(&model, options).await;

        assert_eq!(out.len(), 1);
        let response = out[0].as_ref().unwrap();
        assert!(matches!(&response.content[0], ContentBlock::Text(t) if t.text == "Hi there"));
        assert_eq!(response.usage.output_tokens, 7);

        let spec = transport.last_request().unwrap();
        assert_eq!(spec.url, "http://localhost:11434/api/chat");
        assert!(!spec.stream);
        assert_eq!(spec.body["model"], serde_json::json!("llama3"));
        assert_eq!(spec.body["stream"], serde_json::json!(false));
        assert_eq!(spec.body["messages"][0]["role"], serde_json::json!("user"));
    }

    #[tokio::test]
    async fn streaming_yields_incremental_chunks_then_flushes() {
        let transport = MemoryTransport::with_stream(vec![
            serde_json::json!({ "message": { "content": "Hel" }, "done": false }),
            serde_json::json!({ "message": { "content": "lo" }, "done": false }),
            serde_json::json!({
                "message": { "content": "" },
                "done": true,
                "prompt_eval_count": 2,
                "eval_count": 3
            }),
        ]);
        let model = model_with(transport.clone());
        let out = collect(&model, GenerateOptions::default()).await;

        // two text deltas + one terminal flush
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "Hel"));
        assert!(matches!(&out[1].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "lo"));
        assert_eq!(out[2].as_ref().unwrap().usage.output_tokens, 3);
        assert!(transport.last_request().unwrap().stream);
    }

    #[tokio::test]
    async fn error_frame_surfaces_as_model_error() {
        let transport = MemoryTransport::with_response(
            serde_json::json!({ "error": "model not found" }),
        );
        let model = model_with(transport);
        let options = GenerateOptions { stream: Some(false), ..Default::default() };
        let out = collect(&model, options).await;
        assert!(out[0].is_err());
    }

    #[test]
    fn capabilities_reported() {
        let model = OllamaChatModel::new("llama3", None, Arc::new(MemoryTransport::default()))
            .with_context_window_size(8192);
        assert_eq!(model.model_name(), "llama3");
        assert_eq!(model.context_window_size(), 8192);
        assert!(!model.supports_native_structured_output());
    }
}
