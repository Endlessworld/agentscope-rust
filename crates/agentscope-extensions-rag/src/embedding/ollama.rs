//! Ollama text embedding model.
//!
//! Ports `io.agentscope.core.embedding.ollama.OllamaTextEmbedding`, which delegated the HTTP call
//! to the MODEL extension's `OllamaHttpClient`. The wire contract is the same: `POST
//! {base_url}/api/embed` with `{"model", "input":[text]}` and no authentication (Ollama is a
//! local daemon); the response carries `embeddings[0]` as `f32` and `prompt_eval_count`.
//!
//! Java defaulted `dimensions` to `-1` to mean "unspecified" and only logged a mismatch warning.
//! `usize` cannot be negative, so `0` is the unspecified marker here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use agentscope_core::message::ContentBlock;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::embedding::utils::{
    block_type_name, build_transport, f32_slice_to_f64, join_url, EmbeddingExecutionConfig,
};
use crate::embedding::{EmbeddingModel, EmbeddingResponse, EmbeddingUsage, UsageListener};
use crate::error::{RagError, Result};
use crate::transport::{map_http_error, HttpRequest, HttpTransport};

/// Java `OllamaTextEmbedding.Builder.baseUrl` default.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";
/// Java `OllamaHttpClient.EMBED_ENDPOINT`.
pub const EMBED_ENDPOINT: &str = "/api/embed";
/// Provider label used in error messages / logs.
pub const PROVIDER: &str = "ollama";

/// Ollama text embedding model (`nomic-embed-text`, `all-minilm`, ...).
pub struct OllamaTextEmbedding {
    base_url: String,
    model_name: String,
    dimensions: usize,
    execution: EmbeddingExecutionConfig,
    transport: Arc<dyn HttpTransport>,
    usage_listener: Option<UsageListener>,
}

impl std::fmt::Debug for OllamaTextEmbedding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaTextEmbedding")
            .field("model_name", &self.model_name)
            .field("dimensions", &self.dimensions)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl OllamaTextEmbedding {
    /// Starts a builder holding the Java defaults.
    pub fn builder() -> OllamaTextEmbeddingBuilder {
        OllamaTextEmbeddingBuilder::default()
    }

    /// Replaces the HTTP transport (tests / custom HTTP stacks).
    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// The resolved base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Builds the wire request for `block` without sending it.
    pub fn build_embedding_request(&self, block: &ContentBlock) -> Result<HttpRequest> {
        let text = self.extract_text(block)?;
        let body = serde_json::to_string(&self.build_body(&text))
            .map_err(|e| RagError::Embedding(format!("failed to serialize request: {e}")))?;
        Ok(HttpRequest::post_json(
            join_url(&self.base_url, EMBED_ENDPOINT),
            body,
        ))
    }

    /// Builds the JSON body (`model`, `input`).
    ///
    /// Java explicitly nulled `keep_alive` / `truncate` / `options` and Jackson dropped the nulls,
    /// so only these two keys go on the wire.
    pub fn build_body(&self, text: &str) -> Value {
        json!({ "model": self.model_name, "input": [text] })
    }

    fn extract_text(&self, block: &ContentBlock) -> Result<String> {
        match block {
            ContentBlock::Text(text) => {
                if text.text.trim().is_empty() {
                    return Err(RagError::Embedding(format!(
                        "TextBlock text cannot be null or empty (model={}, provider={PROVIDER})",
                        self.model_name
                    )));
                }
                Ok(text.text.clone())
            }
            other => Err(RagError::Embedding(format!(
                "OllamaTextEmbedding only supports TextBlock, but got: {} (model={}, provider={PROVIDER})",
                block_type_name(other),
                self.model_name
            ))),
        }
    }

    async fn embed_inner(&self, block: &ContentBlock) -> Result<EmbeddingResponse> {
        let request = self.build_embedding_request(block)?;
        let started = Instant::now();
        let response = self
            .execution
            .apply(&self.model_name, PROVIDER, "Embedding", || async {
                let response = self.transport.execute(request.clone()).await?;
                // See `openai.rs`: the status check must sit inside the retried closure
                // so 429 / 5xx are retried, like Java `EmbeddingUtils`.
                if !response.is_success() {
                    return Err(map_http_error(response.status_code, &response.body));
                }
                Ok(response)
            })
            .await?;

        if response.body.trim().is_empty() {
            return Err(RagError::Embedding(
                "Empty response from Ollama embedding API".to_string(),
            ));
        }
        let parsed: OllamaEmbeddingResponseBody =
            serde_json::from_str(&response.body).map_err(|e| {
                RagError::Embedding(format!("failed to parse Ollama embedding response: {e}"))
            })?;

        let embedding = parsed.first_embedding()?;
        // Java only warned when an explicit dimension was configured (`dimensions > 0`).
        if self.dimensions > 0 && embedding.len() != self.dimensions {
            tracing::warn!(
                model = %self.model_name,
                expected = self.dimensions,
                actual = embedding.len(),
                "Ollama embedding dimension mismatch"
            );
        }

        let model = parsed
            .model
            .clone()
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| self.model_name.clone());
        Ok(EmbeddingResponse::new(embedding, parsed.to_usage(), model)
            .with_elapsed(started.elapsed()))
    }
}

#[async_trait]
impl EmbeddingModel for OllamaTextEmbedding {
    async fn embed(&self, block: &ContentBlock) -> Result<Vec<f64>> {
        Ok(self.embed_with_usage(block).await?.embedding)
    }

    async fn embed_with_usage(&self, block: &ContentBlock) -> Result<EmbeddingResponse> {
        let response = self.embed_inner(block).await?;
        if let Some(listener) = &self.usage_listener {
            listener(&response.usage);
        }
        Ok(response)
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Configured dimensions; `0` means "unspecified" (the Java `-1`).
    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[derive(Debug, Default, Deserialize)]
struct OllamaEmbeddingResponseBody {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
    #[serde(default)]
    prompt_eval_count: Option<i64>,
}

impl OllamaEmbeddingResponseBody {
    fn first_embedding(&self) -> Result<Vec<f64>> {
        let values = self
            .embeddings
            .first()
            .ok_or_else(|| RagError::Embedding("No embedding data in response".to_string()))?;
        if values.is_empty() {
            return Err(RagError::Embedding(
                "Empty embedding vector in response".to_string(),
            ));
        }
        Ok(f32_slice_to_f64(values))
    }

    /// Ollama reports `prompt_eval_count` instead of a `usage` block.
    fn to_usage(&self) -> EmbeddingUsage {
        match self.prompt_eval_count {
            Some(count) if count > 0 => {
                EmbeddingUsage::new(count as u64, 0).with_total_tokens(count as u64)
            }
            _ => EmbeddingUsage::default(),
        }
    }
}

/// Consuming builder for [`OllamaTextEmbedding`].
#[derive(Clone)]
pub struct OllamaTextEmbeddingBuilder {
    base_url: String,
    model_name: Option<String>,
    dimensions: usize,
    execution: Option<EmbeddingExecutionConfig>,
    usage_listener: Option<UsageListener>,
}

impl Default for OllamaTextEmbeddingBuilder {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model_name: None,
            dimensions: 0,
            execution: None,
            usage_listener: None,
        }
    }
}

impl OllamaTextEmbeddingBuilder {
    /// Sets the Ollama base URL (Java default `http://localhost:11434`).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets the model name, e.g. `nomic-embed-text` (required).
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    /// Sets the vector dimension; `0` means "unspecified" (the Java `-1`).
    pub fn dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Sets the timeout / retry configuration (Java `executionConfig`).
    pub fn execution_config(mut self, config: EmbeddingExecutionConfig) -> Self {
        self.execution = Some(config);
        self
    }

    /// Registers a token usage callback.
    pub fn usage_listener(mut self, listener: UsageListener) -> Self {
        self.usage_listener = Some(listener);
        self
    }

    /// Validates and builds the model.
    pub fn build(self) -> Result<OllamaTextEmbedding> {
        let model_name = self.model_name.unwrap_or_default();
        if model_name.is_empty() {
            return Err(RagError::InvalidArgument(
                "modelName is required and cannot be null or empty".to_string(),
            ));
        }
        let execution = EmbeddingExecutionConfig::ensure_default(self.execution);
        let transport = build_transport(&execution, HashMap::new())?;
        Ok(OllamaTextEmbedding {
            base_url: self.base_url,
            model_name,
            dimensions: self.dimensions,
            execution,
            transport,
            usage_listener: self.usage_listener,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::test_support::ScriptedTransport;
    use crate::transport::HttpResponse;
    use agentscope_core::message::TextBlock;

    fn success_body() -> String {
        json!({
            "model": "nomic-embed-text",
            "embeddings": [[0.5, -0.5]],
            "prompt_eval_count": 7,
            "total_duration": 1234,
        })
        .to_string()
    }

    fn model(transport: Arc<ScriptedTransport>) -> OllamaTextEmbedding {
        OllamaTextEmbedding::builder()
            .base_url("http://localhost:11434/")
            .model_name("nomic-embed-text")
            .dimensions(2)
            .execution_config(EmbeddingExecutionConfig::disabled())
            .build()
            .unwrap()
            .with_transport(transport)
    }

    #[test]
    fn request_targets_api_embed_without_auth() {
        let model = model(ScriptedTransport::with_responses(vec![]));
        let request = model
            .build_embedding_request(&ContentBlock::Text(TextBlock::new("hello")))
            .unwrap();

        // The trailing slash on the configured base URL is normalized away.
        assert_eq!(request.url, "http://localhost:11434/api/embed");
        assert!(request.header("Authorization").is_none());
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "nomic-embed-text");
        assert_eq!(body["input"], json!(["hello"]));
        assert!(body.get("keep_alive").is_none());
        assert!(body.get("truncate").is_none());
    }

    #[tokio::test]
    async fn parses_f32_embeddings_and_prompt_eval_count() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
        let model = model(transport);
        let response = model
            .embed_with_usage(&ContentBlock::Text(TextBlock::new("hello")))
            .await
            .unwrap();

        assert_eq!(response.embedding, vec![0.5, -0.5]);
        assert_eq!(
            response.usage,
            EmbeddingUsage {
                input_tokens: 7,
                output_tokens: 0,
                total_tokens: 7
            }
        );
        assert_eq!(response.model, "nomic-embed-text");
        assert!(response.elapsed.is_some());
        assert_eq!(model.dimensions(), 2);
    }

    #[tokio::test]
    async fn missing_prompt_eval_count_reports_empty_usage() {
        let body = json!({"embeddings": [[1.0]]}).to_string();
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, body)]);
        let model = OllamaTextEmbedding::builder()
            .model_name("m")
            .execution_config(EmbeddingExecutionConfig::disabled())
            .build()
            .unwrap()
            .with_transport(transport);

        let response = model
            .embed_with_usage(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap();
        assert!(response.usage.is_empty());
        assert_eq!(model.dimensions(), 0, "unspecified dimensions stay 0");
    }

    #[tokio::test]
    async fn empty_embeddings_is_an_error() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, "{\"embeddings\":[]}")]);
        let model = model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No embedding data in response"));
    }

    #[tokio::test]
    async fn empty_vector_is_an_error() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(
            200,
            "{\"embeddings\":[[]]}",
        )]);
        let model = model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Empty embedding vector in response"));
    }

    #[tokio::test]
    async fn blank_body_is_an_error() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, "")]);
        let model = model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Empty response from Ollama embedding API"));
    }

    #[tokio::test]
    async fn transport_error_is_surfaced() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let model = model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(matches!(err, RagError::Http(_)), "err={err:?}");
    }

    #[test]
    fn builder_requires_model_name() {
        let err = OllamaTextEmbedding::builder().build().unwrap_err();
        assert!(err.to_string().contains("modelName is required"));
    }
}
