//! Embedding abstraction and offline implementations.
//!
//! Ports `io.agentscope.core.embedding.EmbeddingModel`. The Java interface takes a
//! `ContentBlock` and returns `Mono<double[]>`; here it becomes an `async_trait`
//! returning `Vec<f64>`, so both HTTP-backed and purely local models share one surface.
//!
//! The four Java provider models (`openai`, `dashscope`, `ollama`) are HTTP clients ported in
//! the submodules below. They talk HTTP through the crate-wide [`crate::transport`] seam, so
//! every provider test runs against an in-memory transport and never opens a socket. The
//! local, dependency-free [`HashEmbeddingModel`] stays available for offline `simple` use.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::message::ContentBlock;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{RagError, Result};

pub mod dashscope;
pub mod ollama;
pub mod openai;
#[cfg(test)]
mod test_support;
pub mod utils;

pub use dashscope::{
    DashScopeMultiModalEmbedding, DashScopeMultiModalEmbeddingBuilder, DashScopeTextEmbedding,
    DashScopeTextEmbeddingBuilder,
};
pub use ollama::{OllamaTextEmbedding, OllamaTextEmbeddingBuilder};
pub use openai::{OpenAiTextEmbedding, OpenAiTextEmbeddingBuilder};
pub use utils::EmbeddingExecutionConfig;

/// Default vector dimension used when none is configured (Java default: 1024).
pub const DEFAULT_DIMENSIONS: usize = 1024;

/// Callback invoked with the token usage of every completed embedding call.
///
/// Java's `EmbeddingModel` exposed neither a usage DTO nor a listener (the vendor SDK kept the
/// `usage` block private), so token accounting was not observable from the RAG layer. The Rust
/// port carries it explicitly because every provider below already parses the field.
pub type UsageListener = Arc<dyn Fn(&EmbeddingUsage) + Send + Sync>;

/// Token usage reported by an embedding provider.
///
/// `input_tokens` covers text tokens (and image + text tokens for the multimodal DashScope
/// API); `output_tokens` is only non-zero for multimodal models.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    /// Tokens billed as input.
    #[serde(default)]
    pub input_tokens: u64,
    /// Tokens billed as output (multimodal models only).
    #[serde(default)]
    pub output_tokens: u64,
    /// Total tokens reported by the provider.
    #[serde(default)]
    pub total_tokens: u64,
}

impl EmbeddingUsage {
    /// Creates a usage record, deriving `total_tokens` as the sum.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
        }
    }

    /// Overrides `total_tokens` with the value the provider reported verbatim.
    pub fn with_total_tokens(mut self, total_tokens: u64) -> Self {
        self.total_tokens = total_tokens;
        self
    }

    /// Whether no token information was reported at all.
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0 && self.total_tokens == 0
    }
}

/// One embedding call: the vector plus provider-side bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    /// The embedding vector.
    #[serde(default)]
    pub embedding: Vec<f64>,
    /// Token usage reported by the provider (`EmbeddingUsage::default()` when unreported).
    #[serde(default)]
    pub usage: EmbeddingUsage,
    /// Model name echoed by the provider, or the configured name when it echoed none.
    #[serde(default)]
    pub model: String,
    /// Client-side wall-clock duration of the call (never serialized).
    #[serde(skip)]
    pub elapsed: Option<Duration>,
}

impl EmbeddingResponse {
    /// Creates a response without a measured duration.
    pub fn new(embedding: Vec<f64>, usage: EmbeddingUsage, model: impl Into<String>) -> Self {
        Self {
            embedding,
            usage,
            model: model.into(),
            elapsed: None,
        }
    }

    /// Records the measured client-side duration.
    pub fn with_elapsed(mut self, elapsed: Duration) -> Self {
        self.elapsed = Some(elapsed);
        self
    }

    /// Length of the produced vector.
    pub fn dimensions(&self) -> usize {
        self.embedding.len()
    }
}

/// Generates embedding vectors from content blocks.
#[async_trait]
pub trait EmbeddingModel: Send + Sync {
    /// Embeds a single content block.
    async fn embed(&self, block: &ContentBlock) -> Result<Vec<f64>>;

    /// Embeds a single content block and reports the provider's token usage.
    ///
    /// The default implementation delegates to [`Self::embed`] and reports an empty
    /// [`EmbeddingUsage`], so purely local models need not implement it.
    async fn embed_with_usage(&self, block: &ContentBlock) -> Result<EmbeddingResponse> {
        let embedding = self.embed(block).await?;
        Ok(EmbeddingResponse::new(
            embedding,
            EmbeddingUsage::default(),
            self.model_name(),
        ))
    }

    /// Model name used for logging / identification.
    fn model_name(&self) -> &str;

    /// Dimension of the produced vectors.
    fn dimensions(&self) -> usize;
}

/// Extracts the embeddable text from a content block.
///
/// Text blocks yield their text; every other block type yields its `Display` render,
/// matching Java `DocumentMetadata.getContentText()` semantics loosely. Returns an error
/// when the block is empty, mirroring the Java providers rejecting unsupported input.
pub fn extract_embeddable_text(block: &ContentBlock) -> Result<String> {
    let text = crate::model::content_block_text(block);
    if text.trim().is_empty() {
        return Err(RagError::Embedding(
            "content block contains no embeddable text".to_string(),
        ));
    }
    Ok(text)
}

/// Deterministic, dependency-free embedding model for local development and tests.
///
/// This is the Rust stand-in for the `simple` submodule's provider models: it is
/// deterministic (the same text always maps to the same vector), needs no network, and
/// is L2-normalized so that cosine similarity behaves sensibly. It is intentionally a
/// *feature-hashing* model rather than a semantic one — useful for wiring and tests,
/// not for production retrieval quality.
#[derive(Debug, Clone)]
pub struct HashEmbeddingModel {
    name: String,
    dimensions: usize,
}

impl Default for HashEmbeddingModel {
    fn default() -> Self {
        Self::new(DEFAULT_DIMENSIONS)
    }
}

impl HashEmbeddingModel {
    /// Creates a model with the given vector dimension.
    pub fn new(dimensions: usize) -> Self {
        Self {
            name: "hash-embedding".to_string(),
            dimensions: dimensions.max(1),
        }
    }

    /// Creates a model with a custom name and dimension.
    pub fn with_name(name: impl Into<String>, dimensions: usize) -> Self {
        Self {
            name: name.into(),
            dimensions: dimensions.max(1),
        }
    }
}

#[async_trait]
impl EmbeddingModel for HashEmbeddingModel {
    async fn embed(&self, block: &ContentBlock) -> Result<Vec<f64>> {
        let text = extract_embeddable_text(block)?;
        Ok(hash_embed(&text, self.dimensions))
    }

    fn model_name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

/// Feature-hashes `text` into an L2-normalized vector of length `dimensions`.
///
/// Each whitespace-delimited token contributes to two buckets (a full-token hash and a
/// signed bucket via a second hasher), which is enough to give deterministic,
/// collision-tolerant similarity without external crates.
pub fn hash_embed(text: &str, dimensions: usize) -> Vec<f64> {
    let dims = dimensions.max(1);
    let mut v = vec![0.0f64; dims];

    for token in text.split_whitespace() {
        let lower = token.to_lowercase();
        let mut h1 = DefaultHasher::new();
        lower.hash(&mut h1);
        let bucket = (h1.finish() as usize) % dims;

        let mut h2 = DefaultHasher::new();
        lower.hash(&mut h2);
        "sign".hash(&mut h2);
        let sign = if h2.finish() % 2 == 0 { 1.0 } else { -1.0 };

        v[bucket] += sign;
    }

    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::TextBlock;

    #[tokio::test]
    async fn hash_embedding_is_deterministic() {
        let model = HashEmbeddingModel::new(32);
        let block = ContentBlock::Text(TextBlock::new("alpha beta gamma"));
        let a = model.embed(&block).await.unwrap();
        let b = model.embed(&block).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        assert_eq!(model.dimensions(), 32);
        assert_eq!(model.model_name(), "hash-embedding");
    }

    #[test]
    fn hash_embedding_is_l2_normalized() {
        let v = hash_embed("one two three", 64);
        let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-9, "norm was {norm}");
    }

    #[test]
    fn identical_text_is_more_similar_than_unrelated_text() {
        let q = hash_embed("rust ownership model", 256);
        let same = hash_embed("rust ownership model", 256);
        let other = hash_embed("completely different subject", 256);
        let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
        assert!(dot(&q, &same) > dot(&q, &other));
    }

    #[tokio::test]
    async fn empty_text_is_rejected() {
        let model = HashEmbeddingModel::default();
        let block = ContentBlock::Text(TextBlock::new("   "));
        let err = model.embed(&block).await.unwrap_err();
        assert!(matches!(err, RagError::Embedding(_)));
    }
}
