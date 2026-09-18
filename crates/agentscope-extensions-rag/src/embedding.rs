//! Embedding abstraction and offline implementations.
//!
//! Ports `io.agentscope.core.embedding.EmbeddingModel`. The Java interface takes a
//! `ContentBlock` and returns `Mono<double[]>`; here it becomes an `async_trait`
//! returning `Vec<f64>`, so both HTTP-backed and purely local models share one surface.
//!
//! Note: the four Java provider models (`dashscope/*`, `ollama/*`, `openai/*`) live in the
//! `simple` submodule but are HTTP clients; they are **not** ported here because their
//! request/response shapes belong to the MODEL extension. Only the local, dependency-free
//! [`HashEmbeddingModel`] is provided so the `simple` backend can be tested offline.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use agentscope_core::message::ContentBlock;
use async_trait::async_trait;

use crate::error::{RagError, Result};

/// Default vector dimension used when none is configured (Java default: 1024).
pub const DEFAULT_DIMENSIONS: usize = 1024;

/// Generates embedding vectors from content blocks.
#[async_trait]
pub trait EmbeddingModel: Send + Sync {
    /// Embeds a single content block.
    async fn embed(&self, block: &ContentBlock) -> Result<Vec<f64>>;

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
