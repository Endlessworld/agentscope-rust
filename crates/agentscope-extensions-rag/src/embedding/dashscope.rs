//! DashScope (Alibaba Cloud Bailian) embedding models.
//!
//! Ports `io.agentscope.core.embedding.dashscope.{DashScopeTextEmbedding,DashScopeMultiModalEmbedding}`.
//! Java called the `com.alibaba.dashscope` SDK; the Rust port posts the same JSON documents to the
//! documented REST paths through [`crate::transport`], so the whole suite stays offline:
//!
//! * text: `POST {base}/services/embeddings/text-embedding/text-embedding`
//!   with `{"model", "input":{"texts":[..]}, "parameters":{"dimension"}}`
//! * multimodal: `POST {base}/services/embeddings/multimodal-embedding/multimodal-embedding`
//!   with `{"model", "input":{"contents":[{"text":..} | {"image":..}], "parameters":{"dimension"}}`
//!
//! Both authenticate with `Authorization: Bearer <apiKey>`. Image blocks are resolved the way
//! Java's `DashScopeMediaConverter.convertImageBlockToUrl` did: base64 sources and remote URLs are
//! forwarded as-is, local paths / `file://` URLs are inlined as `data:` URLs (the HTTP API cannot
//! read the client filesystem).

use std::sync::Arc;
use std::time::Instant;

use agentscope_core::message::{ContentBlock, ImageBlock, Source};
use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::embedding::utils::{
    block_type_name, build_transport, join_url, EmbeddingExecutionConfig,
};
use crate::embedding::{EmbeddingModel, EmbeddingResponse, EmbeddingUsage, UsageListener};
use crate::error::{RagError, Result};
use crate::transport::{map_http_error, HttpRequest, HttpTransport};

/// DashScope API base (already includes the `/api/v1` version segment).
pub const DEFAULT_BASE_URL: &str = "https://dashscope.aliyuncs.com/api/v1";
/// Text embedding endpoint appended to the base URL.
pub const TEXT_EMBEDDING_ENDPOINT: &str = "/services/embeddings/text-embedding/text-embedding";
/// Multimodal embedding endpoint appended to the base URL.
pub const MULTIMODAL_EMBEDDING_ENDPOINT: &str =
    "/services/embeddings/multimodal-embedding/multimodal-embedding";
/// Provider label used in error messages / logs.
pub const PROVIDER: &str = "dashscope";
/// Java `DashScopeTextEmbedding.Builder.dimensions` default.
pub const DEFAULT_DIMENSIONS: usize = 1024;

/// Builds the text-embedding request body.
pub fn build_text_embedding_body(model: &str, text: &str, dimensions: usize) -> Value {
    json!({
        "model": model,
        "input": { "texts": [text] },
        "parameters": { "dimension": dimensions },
    })
}

/// Builds the multimodal request body from already-built `contents` items.
pub fn build_multimodal_embedding_body(
    model: &str,
    contents: Vec<Value>,
    dimensions: usize,
) -> Value {
    json!({
        "model": model,
        "input": { "contents": contents },
        "parameters": { "dimension": dimensions },
    })
}

/// One `{"text": ...}` multimodal content item.
pub fn text_content(text: &str) -> Value {
    json!({ "text": text })
}

/// One `{"image": ...}` multimodal content item.
pub fn image_content(image: &str) -> Value {
    json!({ "image": image })
}

/// Connection settings shared by the two DashScope models.
struct DashScopeEndpoint {
    api_key: String,
    base_url: String,
    execution: EmbeddingExecutionConfig,
    transport: Arc<dyn HttpTransport>,
}

impl DashScopeEndpoint {
    fn build_request(&self, endpoint: &str, body: Value) -> Result<HttpRequest> {
        let payload = serde_json::to_string(&body)
            .map_err(|e| RagError::Embedding(format!("failed to serialize request: {e}")))?;
        Ok(
            HttpRequest::post_json(join_url(&self.base_url, endpoint), payload)
                .with_bearer_token(&self.api_key),
        )
    }

    /// Posts `body` and returns the parsed envelope.
    ///
    /// `empty_response_message` reproduces the Java error text, which differs between the text
    /// and multimodal models.
    async fn post(
        &self,
        endpoint: &str,
        body: Value,
        model_name: &str,
        operation_type: &str,
        empty_response_message: &str,
    ) -> Result<DashScopeEmbeddingEnvelope> {
        let request = self.build_request(endpoint, body)?;
        let response = self
            .execution
            .apply(model_name, PROVIDER, operation_type, || async {
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
            return Err(RagError::Embedding(empty_response_message.to_string()));
        }

        let envelope: DashScopeEmbeddingEnvelope =
            serde_json::from_str(&response.body).map_err(|e| {
                RagError::Embedding(format!("failed to parse DashScope embedding response: {e}"))
            })?;

        // DashScope occasionally reports business errors inside a 2xx envelope.
        if let Some(code) = envelope.code.clone().filter(|code| !code.is_empty()) {
            return Err(RagError::Api {
                status_code: response.status_code,
                error_code: Some(code),
                message: envelope
                    .message
                    .clone()
                    .unwrap_or_else(|| "DashScope embedding error".to_string()),
            });
        }
        Ok(envelope)
    }
}

/// DashScope text embedding model (`text-embedding-v3`, ...).
///
/// Only `TextBlock` is supported; any other block is rejected with the Java message shape.
pub struct DashScopeTextEmbedding {
    endpoint: DashScopeEndpoint,
    model_name: String,
    dimensions: usize,
    usage_listener: Option<UsageListener>,
}

impl std::fmt::Debug for DashScopeTextEmbedding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DashScopeTextEmbedding")
            .field("model_name", &self.model_name)
            .field("dimensions", &self.dimensions)
            .field("base_url", &self.endpoint.base_url)
            .finish_non_exhaustive()
    }
}

impl DashScopeTextEmbedding {
    /// Starts a builder holding the Java defaults.
    pub fn builder() -> DashScopeTextEmbeddingBuilder {
        DashScopeTextEmbeddingBuilder::default()
    }

    /// Replaces the HTTP transport (tests / custom HTTP stacks).
    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.endpoint.transport = transport;
        self
    }

    /// The resolved base URL.
    pub fn base_url(&self) -> &str {
        &self.endpoint.base_url
    }

    /// Builds the wire request for `block` without sending it.
    pub fn build_embedding_request(&self, block: &ContentBlock) -> Result<HttpRequest> {
        let text = extract_text(block, "DashScopeTextEmbedding", &self.model_name)?;
        let body = build_text_embedding_body(&self.model_name, &text, self.dimensions);
        self.endpoint.build_request(TEXT_EMBEDDING_ENDPOINT, body)
    }

    async fn embed_inner(&self, block: &ContentBlock) -> Result<EmbeddingResponse> {
        let text = extract_text(block, "DashScopeTextEmbedding", &self.model_name)?;
        let body = build_text_embedding_body(&self.model_name, &text, self.dimensions);
        let started = Instant::now();
        let envelope = self
            .endpoint
            .post(
                TEXT_EMBEDDING_ENDPOINT,
                body,
                &self.model_name,
                "Embedding",
                "Empty response from DashScope embedding API",
            )
            .await?;

        let embedding = envelope.first_embedding("Empty response from DashScope embedding API")?;
        if embedding.len() != self.dimensions {
            tracing::warn!(
                model = %self.model_name,
                expected = self.dimensions,
                actual = embedding.len(),
                "DashScope embedding dimension mismatch"
            );
        }
        Ok(
            EmbeddingResponse::new(embedding, envelope.to_usage(), self.model_name.clone())
                .with_elapsed(started.elapsed()),
        )
    }
}

#[async_trait]
impl EmbeddingModel for DashScopeTextEmbedding {
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

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

/// DashScope multimodal embedding model (`multimodal-embedding-v1`, ...).
///
/// Supports `TextBlock` and `ImageBlock`; everything else is rejected.
pub struct DashScopeMultiModalEmbedding {
    endpoint: DashScopeEndpoint,
    model_name: String,
    dimensions: usize,
    usage_listener: Option<UsageListener>,
}

impl std::fmt::Debug for DashScopeMultiModalEmbedding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DashScopeMultiModalEmbedding")
            .field("model_name", &self.model_name)
            .field("dimensions", &self.dimensions)
            .field("base_url", &self.endpoint.base_url)
            .finish_non_exhaustive()
    }
}

impl DashScopeMultiModalEmbedding {
    /// Starts a builder holding the Java defaults.
    pub fn builder() -> DashScopeMultiModalEmbeddingBuilder {
        DashScopeMultiModalEmbeddingBuilder::default()
    }

    /// Replaces the HTTP transport (tests / custom HTTP stacks).
    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.endpoint.transport = transport;
        self
    }

    /// The resolved base URL.
    pub fn base_url(&self) -> &str {
        &self.endpoint.base_url
    }

    /// Builds the wire request for `block` without sending it.
    pub fn build_embedding_request(&self, block: &ContentBlock) -> Result<HttpRequest> {
        let content = self.build_content(block)?;
        let body =
            build_multimodal_embedding_body(&self.model_name, vec![content], self.dimensions);
        self.endpoint
            .build_request(MULTIMODAL_EMBEDDING_ENDPOINT, body)
    }

    fn build_content(&self, block: &ContentBlock) -> Result<Value> {
        match block {
            ContentBlock::Text(text) => {
                if text.text.trim().is_empty() {
                    return Err(RagError::Embedding(format!(
                        "TextBlock text cannot be null or empty (model={}, provider={PROVIDER})",
                        self.model_name
                    )));
                }
                Ok(text_content(&text.text))
            }
            ContentBlock::Image(image) => Ok(image_content(&self.resolve_image_url(image)?)),
            other => Err(RagError::Embedding(format!(
                "DashScopeMultiModalEmbedding only supports TextBlock and ImageBlock, but got: {} (model={}, provider={PROVIDER})",
                block_type_name(other),
                self.model_name
            ))),
        }
    }

    /// Resolves an image block to what DashScope expects.
    ///
    /// Mirrors `DashScopeMediaConverter.convertImageBlockToUrl`: base64 sources become `data:`
    /// URLs, remote URLs are forwarded verbatim, and local files are inlined as `data:` URLs.
    fn resolve_image_url(&self, block: &ImageBlock) -> Result<String> {
        match &block.source {
            Source::Base64(source) => Ok(data_url(&source.media_type, &source.data)),
            Source::Url(source) => {
                let url = source.url.trim();
                if url.is_empty() {
                    return Err(RagError::Embedding(format!(
                        "ImageBlock URL source is empty (model={}, provider={PROVIDER})",
                        self.model_name
                    )));
                }
                if url.starts_with("http://")
                    || url.starts_with("https://")
                    || url.starts_with("data:")
                {
                    return Ok(url.to_string());
                }
                let path = url.strip_prefix("file://").unwrap_or(url);
                let bytes = std::fs::read(path).map_err(|e| {
                    RagError::Embedding(format!(
                        "Failed to read image file {path}: {e} (model={}, provider={PROVIDER})",
                        self.model_name
                    ))
                })?;
                Ok(data_url(
                    guess_image_mime_type(path),
                    &base64::engine::general_purpose::STANDARD.encode(bytes),
                ))
            }
            Source::FileId(file) => Err(RagError::Embedding(format!(
                "ImageBlock source 'fileid://{}' carries no image bytes (model={}, provider={PROVIDER})",
                file.file_id, self.model_name
            ))),
        }
    }

    async fn embed_inner(&self, block: &ContentBlock) -> Result<EmbeddingResponse> {
        let content = self.build_content(block)?;
        let body =
            build_multimodal_embedding_body(&self.model_name, vec![content], self.dimensions);
        let started = Instant::now();
        let envelope = self
            .endpoint
            .post(
                MULTIMODAL_EMBEDDING_ENDPOINT,
                body,
                &self.model_name,
                "Embedding",
                "Empty response from DashScope multi-modal embedding API",
            )
            .await?;

        let embedding =
            envelope.first_embedding("Empty response from DashScope multi-modal embedding API")?;
        if embedding.len() != self.dimensions {
            tracing::warn!(
                model = %self.model_name,
                expected = self.dimensions,
                actual = embedding.len(),
                "DashScope multi-modal embedding dimension mismatch"
            );
        }
        Ok(
            EmbeddingResponse::new(embedding, envelope.to_usage(), self.model_name.clone())
                .with_elapsed(started.elapsed()),
        )
    }
}

#[async_trait]
impl EmbeddingModel for DashScopeMultiModalEmbedding {
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

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

/// Extracts the text of a `TextBlock`, rejecting every other block type.
fn extract_text(block: &ContentBlock, model_kind: &str, model_name: &str) -> Result<String> {
    match block {
        ContentBlock::Text(text) => {
            if text.text.trim().is_empty() {
                return Err(RagError::Embedding(format!(
                    "TextBlock text cannot be null or empty (model={model_name}, provider={PROVIDER})"
                )));
            }
            Ok(text.text.clone())
        }
        other => Err(RagError::Embedding(format!(
            "{model_kind} only supports TextBlock, but got: {} (model={model_name}, provider={PROVIDER})",
            block_type_name(other)
        ))),
    }
}

/// `data:<media_type>;base64,<data>` — the inline form DashScope accepts.
fn data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

/// Guesses the MIME type of a local image file from its extension.
fn guess_image_mime_type(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    let extension = lower.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Default, Deserialize)]
struct DashScopeEmbeddingEnvelope {
    #[serde(default)]
    output: Option<DashScopeOutput>,
    #[serde(default)]
    usage: Option<DashScopeUsage>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl DashScopeEmbeddingEnvelope {
    fn first_embedding(&self, empty_response_message: &str) -> Result<Vec<f64>> {
        let output = self
            .output
            .as_ref()
            .ok_or_else(|| RagError::Embedding(empty_response_message.to_string()))?;
        let item = output
            .embeddings
            .first()
            .ok_or_else(|| RagError::Embedding("No embedding data in response".to_string()))?;
        match &item.embedding {
            Some(values) if !values.is_empty() => Ok(values.clone()),
            _ => Err(RagError::Embedding(
                "Empty embedding vector in response".to_string(),
            )),
        }
    }

    /// Maps the vendor `usage` block onto [`EmbeddingUsage`].
    ///
    /// Text embedding only reports `total_tokens`; multimodal reports all three fields. When
    /// only the total is present it is treated as input tokens (text input is the whole bill).
    fn to_usage(&self) -> EmbeddingUsage {
        let Some(usage) = &self.usage else {
            return EmbeddingUsage::default();
        };
        let input = usage.input_tokens.unwrap_or(0);
        let output = usage.output_tokens.unwrap_or(0);
        let total = usage.total_tokens.unwrap_or(input + output);
        EmbeddingUsage {
            input_tokens: if input == 0 && output == 0 {
                total
            } else {
                input
            },
            output_tokens: output,
            total_tokens: total,
            time_seconds: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct DashScopeOutput {
    #[serde(default)]
    embeddings: Vec<DashScopeEmbeddingItem>,
}

#[derive(Debug, Default, Deserialize)]
struct DashScopeEmbeddingItem {
    #[serde(default)]
    embedding: Option<Vec<f64>>,
}

#[derive(Debug, Default, Deserialize)]
struct DashScopeUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

/// Consuming builder for [`DashScopeTextEmbedding`].
#[derive(Clone)]
pub struct DashScopeTextEmbeddingBuilder {
    api_key: Option<String>,
    model_name: Option<String>,
    dimensions: usize,
    base_url: String,
    execution: Option<EmbeddingExecutionConfig>,
    usage_listener: Option<UsageListener>,
}

impl Default for DashScopeTextEmbeddingBuilder {
    fn default() -> Self {
        Self {
            api_key: None,
            model_name: None,
            dimensions: DEFAULT_DIMENSIONS,
            base_url: DEFAULT_BASE_URL.to_string(),
            execution: None,
            usage_listener: None,
        }
    }
}

impl DashScopeTextEmbeddingBuilder {
    /// Sets the API key (required).
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Sets the model name, e.g. `text-embedding-v3` (required).
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    /// Sets the vector dimension (Java default 1024).
    pub fn dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Overrides the API base URL (Java `baseUrl`, `null` for the public endpoint).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
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
    pub fn build(self) -> Result<DashScopeTextEmbedding> {
        let (endpoint, model_name, dimensions) = finish_endpoint(
            self.api_key,
            self.model_name,
            self.dimensions,
            self.base_url,
            self.execution,
        )?;
        Ok(DashScopeTextEmbedding {
            endpoint,
            model_name,
            dimensions,
            usage_listener: self.usage_listener,
        })
    }
}

/// Consuming builder for [`DashScopeMultiModalEmbedding`].
#[derive(Clone)]
pub struct DashScopeMultiModalEmbeddingBuilder {
    api_key: Option<String>,
    model_name: Option<String>,
    dimensions: usize,
    base_url: String,
    execution: Option<EmbeddingExecutionConfig>,
    usage_listener: Option<UsageListener>,
}

impl Default for DashScopeMultiModalEmbeddingBuilder {
    fn default() -> Self {
        Self {
            api_key: None,
            model_name: None,
            dimensions: DEFAULT_DIMENSIONS,
            base_url: DEFAULT_BASE_URL.to_string(),
            execution: None,
            usage_listener: None,
        }
    }
}

impl DashScopeMultiModalEmbeddingBuilder {
    /// Sets the API key (required).
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Sets the model name, e.g. `multimodal-embedding-v1` (required).
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    /// Sets the vector dimension (Java default 1024).
    pub fn dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = dimensions;
        self
    }

    /// Overrides the API base URL (Java `baseUrl`, `null` for the public endpoint).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
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
    pub fn build(self) -> Result<DashScopeMultiModalEmbedding> {
        let (endpoint, model_name, dimensions) = finish_endpoint(
            self.api_key,
            self.model_name,
            self.dimensions,
            self.base_url,
            self.execution,
        )?;
        Ok(DashScopeMultiModalEmbedding {
            endpoint,
            model_name,
            dimensions,
            usage_listener: self.usage_listener,
        })
    }
}

/// Shared validation + transport construction for the two DashScope builders.
fn finish_endpoint(
    api_key: Option<String>,
    model_name: Option<String>,
    dimensions: usize,
    base_url: String,
    execution: Option<EmbeddingExecutionConfig>,
) -> Result<(DashScopeEndpoint, String, usize)> {
    let api_key = api_key.unwrap_or_default();
    if api_key.is_empty() {
        return Err(RagError::InvalidArgument(
            "apiKey is required and cannot be null or empty".to_string(),
        ));
    }
    let model_name = model_name.unwrap_or_default();
    if model_name.is_empty() {
        return Err(RagError::InvalidArgument(
            "modelName is required and cannot be null or empty".to_string(),
        ));
    }
    if dimensions == 0 {
        return Err(RagError::InvalidArgument(
            "dimensions must be positive, got: 0".to_string(),
        ));
    }
    let execution = EmbeddingExecutionConfig::ensure_default(execution);
    let transport = build_transport(&execution, std::collections::HashMap::new())?;
    let endpoint = DashScopeEndpoint {
        api_key,
        base_url,
        execution,
        transport,
    };
    Ok((endpoint, model_name, dimensions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::test_support::ScriptedTransport;
    use crate::transport::HttpResponse;
    use agentscope_core::message::{AudioBlock, Base64Source, TextBlock, URLSource};

    fn text_body() -> String {
        json!({
            "output": {"embeddings": [{"text_index": 0, "embedding": [0.5, 0.25]}]},
            "usage": {"total_tokens": 4},
            "request_id": "req-1",
        })
        .to_string()
    }

    fn multimodal_body() -> String {
        json!({
            "output": {"embeddings": [{"index": 0, "embedding": [1.0, 2.0]}]},
            "usage": {"input_tokens": 2, "output_tokens": 3, "total_tokens": 5},
        })
        .to_string()
    }

    fn text_model(transport: Arc<ScriptedTransport>) -> DashScopeTextEmbedding {
        DashScopeTextEmbedding::builder()
            .api_key("ds-key")
            .model_name("text-embedding-v3")
            .dimensions(2)
            .base_url("https://dash.test/api/v1")
            .execution_config(EmbeddingExecutionConfig::disabled())
            .build()
            .unwrap()
            .with_transport(transport)
    }

    fn multimodal_model(transport: Arc<ScriptedTransport>) -> DashScopeMultiModalEmbedding {
        DashScopeMultiModalEmbedding::builder()
            .api_key("ds-key")
            .model_name("multimodal-embedding-v1")
            .dimensions(2)
            .base_url("https://dash.test/api/v1")
            .execution_config(EmbeddingExecutionConfig::disabled())
            .build()
            .unwrap()
            .with_transport(transport)
    }

    #[test]
    fn text_request_uses_texts_input_and_dimension_parameter() {
        let model = text_model(ScriptedTransport::with_responses(vec![]));
        let request = model
            .build_embedding_request(&ContentBlock::Text(TextBlock::new("hello")))
            .unwrap();

        assert_eq!(
            request.url,
            "https://dash.test/api/v1/services/embeddings/text-embedding/text-embedding"
        );
        assert_eq!(request.header("Authorization"), Some("Bearer ds-key"));
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "text-embedding-v3");
        assert_eq!(body["input"]["texts"], json!(["hello"]));
        assert_eq!(body["parameters"]["dimension"], 2);
    }

    #[tokio::test]
    async fn text_embedding_parses_output_and_total_tokens() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, text_body())]);
        let model = text_model(transport);
        let response = model
            .embed_with_usage(&ContentBlock::Text(TextBlock::new("hello")))
            .await
            .unwrap();

        assert_eq!(response.embedding, vec![0.5, 0.25]);
        assert_eq!(response.usage.total_tokens, 4);
        // Only `total_tokens` is reported, so it is booked as input tokens.
        assert_eq!(response.usage.input_tokens, 4);
        assert_eq!(response.usage.output_tokens, 0);
        assert_eq!(response.model, "text-embedding-v3");
    }

    #[tokio::test]
    async fn text_embedding_rejects_image_block() {
        let model = text_model(ScriptedTransport::with_responses(vec![]));
        let block = ContentBlock::Image(ImageBlock::new(Source::Url(URLSource::new("u"))));
        let err = model.embed(&block).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("DashScopeTextEmbedding only supports TextBlock, but got: ImageBlock"),
            "err={err}"
        );
    }

    #[tokio::test]
    async fn text_embedding_rejects_blank_text() {
        let model = text_model(ScriptedTransport::with_responses(vec![]));
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new(" ")))
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("TextBlock text cannot be null or empty"));
    }

    #[test]
    fn multimodal_text_request_uses_contents() {
        let model = multimodal_model(ScriptedTransport::with_responses(vec![]));
        let request = model
            .build_embedding_request(&ContentBlock::Text(TextBlock::new("hi")))
            .unwrap();

        assert_eq!(
            request.url,
            "https://dash.test/api/v1/services/embeddings/multimodal-embedding/multimodal-embedding"
        );
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["input"]["contents"], json!([{"text": "hi"}]));
        assert_eq!(body["parameters"]["dimension"], 2);
    }

    #[test]
    fn multimodal_base64_image_becomes_data_url() {
        let model = multimodal_model(ScriptedTransport::with_responses(vec![]));
        let image = ImageBlock::new(Source::Base64(Base64Source::new("image/png", "AAAA")));
        let request = model
            .build_embedding_request(&ContentBlock::Image(image))
            .unwrap();
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body["input"]["contents"],
            json!([{"image": "data:image/png;base64,AAAA"}])
        );
    }

    #[test]
    fn multimodal_remote_image_is_forwarded_verbatim() {
        let model = multimodal_model(ScriptedTransport::with_responses(vec![]));
        let image = ImageBlock::new(Source::Url(URLSource::new("https://example.com/a.png")));
        let request = model
            .build_embedding_request(&ContentBlock::Image(image))
            .unwrap();
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body["input"]["contents"],
            json!([{"image": "https://example.com/a.png"}])
        );
    }

    #[test]
    fn multimodal_local_file_is_inlined_as_data_url() {
        let path = std::env::temp_dir().join(format!(
            "rag-embed-inline-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"\x89PNG").unwrap();

        let model = multimodal_model(ScriptedTransport::with_responses(vec![]));
        let image = ImageBlock::new(Source::Url(URLSource::new(format!(
            "file://{}",
            path.display()
        ))));
        let request = model
            .build_embedding_request(&ContentBlock::Image(image))
            .unwrap();
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        let expected = data_url(
            "image/png",
            &base64::engine::general_purpose::STANDARD.encode(b"\x89PNG"),
        );
        assert_eq!(body["input"]["contents"], json!([{"image": expected}]));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn multimodal_missing_local_file_is_an_error() {
        let model = multimodal_model(ScriptedTransport::with_responses(vec![]));
        let image = ImageBlock::new(Source::Url(URLSource::new(
            "file:///definitely/not/here.png",
        )));
        let err = model
            .build_embedding_request(&ContentBlock::Image(image))
            .unwrap_err();
        assert!(err.to_string().contains("Failed to read image file"));
    }

    #[test]
    fn multimodal_rejects_unsupported_blocks() {
        let model = multimodal_model(ScriptedTransport::with_responses(vec![]));
        let block = ContentBlock::Audio(AudioBlock::new(Source::Url(URLSource::new("u"))));
        let err = model.build_embedding_request(&block).unwrap_err();
        assert!(
            err.to_string().contains(
                "DashScopeMultiModalEmbedding only supports TextBlock and ImageBlock, but got: AudioBlock"
            ),
            "err={err}"
        );
    }

    #[tokio::test]
    async fn multimodal_usage_carries_input_and_output_tokens() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, multimodal_body())]);
        let model = multimodal_model(transport);
        let response = model
            .embed_with_usage(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap();

        assert_eq!(response.embedding, vec![1.0, 2.0]);
        assert_eq!(
            response.usage,
            EmbeddingUsage {
                input_tokens: 2,
                output_tokens: 3,
                total_tokens: 5,
                time_seconds: response.usage.time_seconds,
            }
        );
    }

    #[tokio::test]
    async fn envelope_code_becomes_api_error() {
        let body = json!({"code": "InvalidParameter", "message": "bad model"}).to_string();
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, body)]);
        let model = text_model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        match err {
            RagError::Api {
                error_code,
                message,
                ..
            } => {
                assert_eq!(error_code.as_deref(), Some("InvalidParameter"));
                assert_eq!(message, "bad model");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_output_is_an_error() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, "{\"usage\":{}}")]);
        let model = multimodal_model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Empty response from DashScope multi-modal embedding API"),
            "err={err}"
        );
    }

    #[tokio::test]
    async fn empty_embeddings_list_is_an_error() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(
            200,
            "{\"output\":{\"embeddings\":[]}}",
        )]);
        let model = text_model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No embedding data in response"));
    }

    #[test]
    fn builder_validates_required_fields() {
        let err = DashScopeTextEmbedding::builder().build().unwrap_err();
        assert!(err.to_string().contains("apiKey is required"));
        let err = DashScopeMultiModalEmbedding::builder()
            .api_key("k")
            .dimensions(0)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("modelName is required"));
    }
}
