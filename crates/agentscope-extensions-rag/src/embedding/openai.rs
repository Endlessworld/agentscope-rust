//! OpenAI text embedding model.
//!
//! Ports `io.agentscope.core.embedding.openai.OpenAITextEmbedding`: it `POST`s
//! `{base_url}/embeddings` with `Authorization: Bearer <apiKey>` and `input: [text]`, then reads
//! `data[0].embedding` back. Java used the official `com.openai` SDK; the Rust port speaks the
//! same REST endpoint through [`crate::transport`], so tests stay offline.
//!
//! Only `TextBlock` is supported — any other block is rejected with the Java message shape
//! `"OpenAITextEmbedding only supports TextBlock, but got: ImageBlock"`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use agentscope_core::message::ContentBlock;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::embedding::utils::{
    block_type_name, build_transport, join_url, EmbeddingExecutionConfig,
};
use crate::embedding::{EmbeddingModel, EmbeddingResponse, EmbeddingUsage, UsageListener};
use crate::error::{RagError, Result};
use crate::transport::{map_http_error, HttpRequest, HttpTransport};

/// Java `OpenAITextEmbedding.Builder.dimensions` default.
pub const DEFAULT_DIMENSIONS: usize = 1536;
/// Public OpenAI API base (no trailing slash).
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Embedding endpoint appended to the base URL.
pub const EMBEDDINGS_ENDPOINT: &str = "/embeddings";
/// Provider label used in error messages / logs.
pub const PROVIDER: &str = "openai";

/// OpenAI text embedding model (`text-embedding-3-*`, `text-embedding-ada-002`).
///
/// Built through [`OpenAiTextEmbedding::builder`], which validates the same required fields as
/// the Java builder (api key, model name, positive dimensions).
///
/// The HTTP client is created once in [`OpenAiTextEmbedding::builder`] and shared by every call
/// and every retry — Java `07c5f3078` fixed the same leak in `OpenAITextEmbedding`, which built a
/// new SDK client per subscription. Call [`OpenAiTextEmbedding::close`] when the model is no
/// longer needed: subsequent embeddings fail immediately and never enter the retry pipeline.
pub struct OpenAiTextEmbedding {
    api_key: String,
    model_name: String,
    dimensions: usize,
    base_url: String,
    execution: EmbeddingExecutionConfig,
    transport: Arc<dyn HttpTransport>,
    usage_listener: Option<UsageListener>,
    closed: AtomicBool,
}

impl std::fmt::Debug for OpenAiTextEmbedding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiTextEmbedding")
            .field("model_name", &self.model_name)
            .field("dimensions", &self.dimensions)
            .field("base_url", &self.base_url)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl OpenAiTextEmbedding {
    /// Starts a builder holding the Java defaults.
    pub fn builder() -> OpenAiTextEmbeddingBuilder {
        OpenAiTextEmbeddingBuilder::default()
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

    /// The execution (timeout + retry) configuration.
    pub fn execution_config(&self) -> &EmbeddingExecutionConfig {
        &self.execution
    }

    /// Closes the model so that further embeddings fail without entering the retry pipeline.
    ///
    /// Ports Java `OpenAITextEmbedding#close` (commit `07c5f3078`, `AutoCloseable`). Repeated
    /// calls have no effect and closing an unused model performs no I/O, matching Java
    /// `closingUnusedModelDoesNotCreateClient`.
    ///
    /// Embedding futures created **before** the close but first polled afterwards also fail:
    /// the closed check runs when the future is polled, exactly like Java's subscription-time
    /// `Mono.defer` guard. Wait for in-flight embeddings to finish before closing.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Whether [`Self::close`] has been called (Java `closed`).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Rejects every call after [`Self::close`] with the Java error text.
    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(RagError::Embedding(format!(
                "OpenAITextEmbedding is closed (model={}, provider={PROVIDER})",
                self.model_name
            )));
        }
        Ok(())
    }

    /// Builds the wire request for `block` without sending it.
    ///
    /// Exposed so tests can pin the exact wire contract (URL, bearer token, `encoding_format`).
    pub fn build_embedding_request(&self, block: &ContentBlock) -> Result<HttpRequest> {
        let text = self.extract_text(block)?;
        let body = serde_json::to_string(&self.build_body(&text))
            .map_err(|e| RagError::Embedding(format!("failed to serialize request: {e}")))?;
        Ok(
            HttpRequest::post_json(join_url(&self.base_url, EMBEDDINGS_ENDPOINT), body)
                .with_bearer_token(&self.api_key),
        )
    }

    /// Builds the JSON body (`model`, `input`, `dimensions`, `encoding_format`).
    pub fn build_body(&self, text: &str) -> Value {
        json!({
            "model": self.model_name,
            "input": [text],
            "dimensions": self.dimensions,
            "encoding_format": "float",
        })
    }

    /// Validates the block and returns its text.
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
                "OpenAITextEmbedding only supports TextBlock, but got: {} (model={}, provider={PROVIDER})",
                block_type_name(other),
                self.model_name
            ))),
        }
    }

    /// Performs one HTTP call and parses the response into an [`EmbeddingResponse`].
    async fn embed_inner(&self, block: &ContentBlock) -> Result<EmbeddingResponse> {
        let request = self.build_embedding_request(block)?;
        let started = Instant::now();
        let response = self
            .execution
            .apply(&self.model_name, PROVIDER, "Embedding", || async {
                let response = self.transport.execute(request.clone()).await?;
                // The status check lives inside the retried closure so that 429 / 5xx
                // are retried, matching Java `EmbeddingUtils.applyTimeoutAndRetry`
                // (which wraps the whole call, not just the transport hop).
                if !response.is_success() {
                    return Err(map_http_error(response.status_code, &response.body));
                }
                Ok(response)
            })
            .await?;

        let parsed: OpenAiEmbeddingResponseBody = parse_body(&response.body)?;
        let embedding = parsed.first_embedding()?;
        if embedding.len() != self.dimensions {
            tracing::warn!(
                model = %self.model_name,
                expected = self.dimensions,
                actual = embedding.len(),
                "OpenAI embedding dimension mismatch"
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
impl EmbeddingModel for OpenAiTextEmbedding {
    async fn embed(&self, block: &ContentBlock) -> Result<Vec<f64>> {
        Ok(self.embed_with_usage(block).await?.embedding)
    }

    async fn embed_with_usage(&self, block: &ContentBlock) -> Result<EmbeddingResponse> {
        // Checked before the retry pipeline so a closed model emits exactly one error and
        // never sleeps / re-sends (Java `closedModelDoesNotRetryEvenWhenRetriesAreEnabled`).
        self.ensure_open()?;
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

    fn close(&self) {
        OpenAiTextEmbedding::close(self);
    }
}

/// Parses the body, mapping a malformed / empty body onto the Java error text.
fn parse_body(body: &str) -> Result<OpenAiEmbeddingResponseBody> {
    if body.trim().is_empty() {
        return Err(RagError::Embedding(
            "Empty response from OpenAI embedding API".to_string(),
        ));
    }
    serde_json::from_str(body)
        .map_err(|e| RagError::Embedding(format!("failed to parse OpenAI embedding response: {e}")))
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiEmbeddingResponseBody {
    #[serde(default)]
    data: Vec<OpenAiEmbeddingItem>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

impl OpenAiEmbeddingResponseBody {
    fn first_embedding(&self) -> Result<Vec<f64>> {
        let item = self
            .data
            .first()
            .ok_or_else(|| RagError::Embedding("No embedding data in response".to_string()))?;
        match &item.embedding {
            Some(values) if !values.is_empty() => Ok(values.clone()),
            _ => Err(RagError::Embedding(
                "Empty embedding vector in response".to_string(),
            )),
        }
    }

    fn to_usage(&self) -> EmbeddingUsage {
        match &self.usage {
            Some(usage) => {
                let input = usage.prompt_tokens.unwrap_or(0);
                let total = usage.total_tokens.unwrap_or(input);
                EmbeddingUsage {
                    input_tokens: input,
                    output_tokens: 0,
                    total_tokens: total,
                    time_seconds: None,
                }
            }
            None => EmbeddingUsage::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiEmbeddingItem {
    #[serde(default)]
    embedding: Option<Vec<f64>>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

/// Consuming builder for [`OpenAiTextEmbedding`].
#[derive(Clone)]
pub struct OpenAiTextEmbeddingBuilder {
    api_key: Option<String>,
    model_name: Option<String>,
    dimensions: usize,
    base_url: String,
    execution: Option<EmbeddingExecutionConfig>,
    usage_listener: Option<UsageListener>,
}

impl Default for OpenAiTextEmbeddingBuilder {
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

impl OpenAiTextEmbeddingBuilder {
    /// Sets the API key (required).
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Sets the model name, e.g. `text-embedding-3-small` (required).
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    /// Sets the vector dimension (Java default 1536).
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
    ///
    /// Java threw `IllegalStateException` with these messages; here they are
    /// [`RagError::InvalidArgument`], matching the rest of the crate.
    pub fn build(self) -> Result<OpenAiTextEmbedding> {
        let api_key = self.api_key.unwrap_or_default();
        if api_key.is_empty() {
            return Err(RagError::InvalidArgument(
                "apiKey is required and cannot be null or empty".to_string(),
            ));
        }
        let model_name = self.model_name.unwrap_or_default();
        if model_name.is_empty() {
            return Err(RagError::InvalidArgument(
                "modelName is required and cannot be null or empty".to_string(),
            ));
        }
        if self.dimensions == 0 {
            return Err(RagError::InvalidArgument(
                "dimensions must be positive, got: 0".to_string(),
            ));
        }
        let execution = EmbeddingExecutionConfig::ensure_default(self.execution);
        let transport = build_transport(&execution, HashMap::new())?;
        Ok(OpenAiTextEmbedding {
            api_key,
            model_name,
            dimensions: self.dimensions,
            base_url: self.base_url,
            execution,
            transport,
            usage_listener: self.usage_listener,
            closed: AtomicBool::new(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::test_support::ScriptedTransport;
    use crate::retry::RetryPolicy;
    use crate::transport::HttpResponse;
    use agentscope_core::message::{ImageBlock, Source, TextBlock, URLSource};
    use std::sync::Mutex;

    fn success_body() -> String {
        json!({
            "object": "list",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
            "model": "text-embedding-3-small",
            "usage": {"prompt_tokens": 6, "total_tokens": 6},
        })
        .to_string()
    }

    fn model(transport: Arc<ScriptedTransport>) -> OpenAiTextEmbedding {
        OpenAiTextEmbedding::builder()
            .api_key("sk-test")
            .model_name("text-embedding-3-small")
            .dimensions(3)
            .base_url("https://api.test/v1")
            .execution_config(EmbeddingExecutionConfig::disabled())
            .build()
            .unwrap()
            .with_transport(transport)
    }

    #[test]
    fn build_embedding_request_pins_the_wire_contract() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let model = model(transport);
        let request = model
            .build_embedding_request(&ContentBlock::Text(TextBlock::new("hello")))
            .unwrap();

        assert_eq!(request.url, "https://api.test/v1/embeddings");
        assert_eq!(request.header("Authorization"), Some("Bearer sk-test"));
        assert_eq!(
            request.header("Content-Type"),
            Some("application/json; charset=utf-8")
        );
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "text-embedding-3-small");
        assert_eq!(body["input"], json!(["hello"]));
        assert_eq!(body["dimensions"], 3);
        assert_eq!(body["encoding_format"], "float");
    }

    #[tokio::test]
    async fn parses_embedding_and_usage() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
        let model = model(transport.clone());
        let response = model
            .embed_with_usage(&ContentBlock::Text(TextBlock::new("hello")))
            .await
            .unwrap();

        assert_eq!(response.embedding, vec![0.1, 0.2, 0.3]);
        assert_eq!(response.dimensions(), 3);
        // `with_elapsed` also stamps `usage.time_seconds` (Java `EmbeddingUsage.time`).
        assert_eq!(response.usage.input_tokens, 6);
        assert_eq!(response.usage.output_tokens, 0);
        assert_eq!(response.usage.total_tokens, 6);
        assert!(response.usage.time_seconds.is_some());
        assert_eq!(response.model, "text-embedding-3-small");
        assert!(response.elapsed.is_some());
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn embed_returns_the_bare_vector() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
        let model = model(transport);
        let vector = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap();
        assert_eq!(vector, vec![0.1, 0.2, 0.3]);
        assert_eq!(model.model_name(), "text-embedding-3-small");
        assert_eq!(model.dimensions(), 3);
    }

    #[tokio::test]
    async fn usage_listener_receives_every_call() {
        let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
        {
            let seen = seen.clone();
            let transport =
                ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
            let model = OpenAiTextEmbedding::builder()
                .api_key("k")
                .model_name("m")
                .dimensions(3)
                .base_url("https://api.test/v1")
                .execution_config(EmbeddingExecutionConfig::disabled())
                .usage_listener(Arc::new(move |usage: &EmbeddingUsage| {
                    seen.lock().unwrap().push(usage.total_tokens);
                }))
                .build()
                .unwrap()
                .with_transport(transport);

            model
                .embed(&ContentBlock::Text(TextBlock::new("hello")))
                .await
                .unwrap();
        }
        assert_eq!(*seen.lock().unwrap(), vec![6]);
    }

    #[tokio::test]
    async fn rejects_non_text_blocks_with_the_java_message() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let model = model(transport);
        let block = ContentBlock::Image(ImageBlock::new(Source::Url(URLSource::new("u"))));
        let err = model.embed(&block).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("OpenAITextEmbedding only supports TextBlock, but got: ImageBlock"),
            "err={err}"
        );
    }

    #[tokio::test]
    async fn rejects_blank_text() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let model = model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("   ")))
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("TextBlock text cannot be null or empty"));
    }

    #[tokio::test]
    async fn maps_401_to_auth_error() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(401, "{\"error\":\"nope\"}")]);
        let model = model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RagError::Auth {
                    status_code: 401,
                    ..
                }
            ),
            "err={err:?}"
        );
    }

    #[tokio::test]
    async fn retries_a_500_before_failing() {
        // LIFO: the last pushed response is served first, so push the success first.
        let transport = ScriptedTransport::with_responses(vec![
            HttpResponse::new(200, success_body()),
            HttpResponse::new(500, "{\"error\":\"boom\"}"),
        ]);
        let model = OpenAiTextEmbedding::builder()
            .api_key("k")
            .model_name("m")
            .dimensions(3)
            .base_url("https://api.test/v1")
            .execution_config(EmbeddingExecutionConfig::disabled().with_retry(RetryPolicy::new(1)))
            .build()
            .unwrap()
            .with_transport(transport.clone());

        let vector = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap();
        assert_eq!(vector, vec![0.1, 0.2, 0.3]);
        assert_eq!(transport.requests().len(), 2);
    }

    #[tokio::test]
    async fn empty_data_is_an_error() {
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(
            200,
            "{\"data\":[],\"model\":\"m\"}",
        )]);
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
            "{\"data\":[{\"embedding\":[]}]}",
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
        let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, "   ")]);
        let model = model(transport);
        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Empty response from OpenAI embedding API"));
    }

    #[test]
    fn builder_validates_required_fields() {
        let err = OpenAiTextEmbedding::builder().build().unwrap_err();
        assert!(err.to_string().contains("apiKey is required"));

        let err = OpenAiTextEmbedding::builder()
            .api_key("k")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("modelName is required"));

        let err = OpenAiTextEmbedding::builder()
            .api_key("k")
            .model_name("m")
            .dimensions(0)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("dimensions must be positive"));
    }

    // ---- lifecycle (Java `OpenAITextEmbeddingLifecycleTest`, commit 07c5f3078) ----

    #[tokio::test]
    async fn repeated_calls_reuse_one_transport() {
        // Java built a new SDK client per subscription (and per retry), accumulating resources
        // until 07c5f3078 made it lazy-but-single. The Rust port builds one transport in
        // `build()` and every call goes through that instance.
        let transport = ScriptedTransport::with_responses(
            (0..20)
                .map(|_| HttpResponse::new(200, success_body()))
                .collect(),
        );
        let model = model(transport.clone());
        for _ in 0..20 {
            model
                .embed(&ContentBlock::Text(TextBlock::new("hello")))
                .await
                .unwrap();
        }
        assert_eq!(transport.requests().len(), 20);
        assert!(!model.is_closed());
    }

    #[tokio::test]
    async fn close_is_idempotent_and_rejects_later_embeddings() {
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
        let model = model(transport.clone());
        model
            .embed(&ContentBlock::Text(TextBlock::new("hello")))
            .await
            .unwrap();

        model.close();
        model.close();
        assert!(model.is_closed());

        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hello")))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("OpenAITextEmbedding is closed"),
            "err={err}"
        );
        // The rejected call never reached the transport.
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn closed_model_bypasses_the_retry_pipeline() {
        // Java `closedModelDoesNotRetryEvenWhenRetriesAreEnabled`, uninitialized variant: a
        // closed model must fail before the retry wrapper, so the transport is never touched.
        let transport = ScriptedTransport::with_responses(vec![]);
        let model = OpenAiTextEmbedding::builder()
            .api_key("k")
            .model_name("m")
            .dimensions(3)
            .base_url("https://api.test/v1")
            .execution_config(EmbeddingExecutionConfig::disabled().with_retry(RetryPolicy::new(3)))
            .build()
            .unwrap()
            .with_transport(transport.clone());
        model.close();

        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("closed"), "err={err}");
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn future_created_before_close_is_rejected() {
        // Mirrors Java's "Mono created before close(), subscribed after".
        let transport =
            ScriptedTransport::with_responses(vec![HttpResponse::new(200, success_body())]);
        let model = model(transport.clone());
        let block = ContentBlock::Text(TextBlock::new("hello"));
        let pending = model.embed(&block);
        model.close();

        let err = pending.await.unwrap_err();
        assert!(err.to_string().contains("closed"), "err={err}");
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn trait_object_close_marks_the_model_closed() {
        let transport = ScriptedTransport::with_responses(vec![]);
        let model: Arc<dyn EmbeddingModel> = Arc::new(model(transport));
        model.close();

        let err = model
            .embed(&ContentBlock::Text(TextBlock::new("hi")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("closed"), "err={err}");
    }
}
