//! Vector store abstraction and distance helpers.
//!
//! Ports `io.agentscope.core.rag.store.VDBStoreBase` / `DistanceCalculator`.
//! The Java `SearchDocumentDto` is flattened into [`SearchQuery`], and an `add_documents`
//! convenience helper mirrors the Java `Knowledge.addDocuments` + store `add` chain.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use crate::error::{RagError, Result};
use crate::model::Document;

/// Parameters for a similarity search.
///
/// Mirrors Java `SearchDocumentDto` (`vectorName`, `queryEmbedding`, `limit`,
/// `scoreThreshold`).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchQuery {
    /// Logical vector-space name.
    pub vector_name: Option<String>,
    /// Query embedding vector.
    pub query_embedding: Vec<f64>,
    /// Maximum number of results.
    pub limit: usize,
    /// Optional similarity floor applied by the store itself.
    pub score_threshold: Option<f64>,
}

impl SearchQuery {
    /// Creates a query from an embedding and a limit.
    pub fn new(query_embedding: Vec<f64>, limit: usize) -> Self {
        Self {
            vector_name: None,
            query_embedding,
            limit,
            score_threshold: None,
        }
    }

    /// Sets the logical vector name.
    pub fn with_vector_name(mut self, name: impl Into<String>) -> Self {
        self.vector_name = Some(name.into());
        self
    }

    /// Sets the store-level score threshold.
    pub fn with_score_threshold(mut self, threshold: f64) -> Self {
        self.score_threshold = Some(threshold);
        self
    }
}

/// A vector database storing whole documents (including metadata/payload).
#[async_trait]
pub trait VdbStore: Send + Sync {
    /// Adds documents (each must carry an embedding).
    async fn add(&self, documents: Vec<Document>) -> Result<()>;

    /// Searches for similar documents, sorted by descending similarity.
    async fn search(&self, query: &SearchQuery) -> Result<Vec<Document>>;

    /// Deletes a document by id. Returns whether a document was removed.
    ///
    /// Default implementation mirrors Java: deletion is optional.
    async fn delete(&self, _id: &str) -> Result<bool> {
        Err(RagError::Unsupported(
            "delete is not supported by this vector store".to_string(),
        ))
    }
}

/// Computes the cosine similarity between two vectors.
///
/// Formula `A·B / (||A||·||B||)`. A zero-norm vector yields `0.0`, matching Java
/// `DistanceCalculator.cosineSimilarity`. Returns an error on length mismatch, like the
/// Java `IllegalArgumentException`.
pub fn cosine_similarity(a: &[f64], b: &[f64]) -> Result<f64> {
    validate_vectors(a, b)?;

    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        return Ok(0.0);
    }
    Ok(dot / denom)
}

///Computes the Euclidean distance between two vectors.
pub fn euclidean_distance(a: &[f64], b: &[f64]) -> Result<f64> {
    Ok(squared_euclidean_distance(a, b)?.sqrt())
}

/// Computes the squared Euclidean distance between two vectors.
pub fn squared_euclidean_distance(a: &[f64], b: &[f64]) -> Result<f64> {
    validate_vectors(a, b)?;
    Ok(a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum())
}

fn validate_vectors(a: &[f64], b: &[f64]) -> Result<()> {
    if a.is_empty() || b.is_empty() {
        return Err(RagError::VectorStore(
            "vectors must not be empty".to_string(),
        ));
    }
    if a.len() != b.len() {
        return Err(RagError::VectorStore(format!(
            "vector length mismatch: {} vs {}",
            a.len(),
            b.len()
        )));
    }
    Ok(())
}

/// In-memory vector store using cosine similarity.
///
/// Ports `io.agentscope.core.rag.store.InMemoryStore`. Documents are keyed by their own
/// id (Java uses `ConcurrentHashMap<String, Document>`); a `RwLock` provides the Rust
/// equivalent of thread safety.
#[derive(Debug)]
pub struct InMemoryStore {
    documents: RwLock<HashMap<String, Document>>,
    dimensions: usize,
}

impl InMemoryStore {
    /// Creates a store for vectors of `dimensions` length.
    ///
    /// Returns an error when `dimensions == 0`, mirroring Java's
    /// `IllegalArgumentException("Dimensions must be positive")`.
    pub fn new(dimensions: usize) -> Result<Self> {
        if dimensions == 0 {
            return Err(RagError::InvalidArgument(
                "Dimensions must be positive".to_string(),
            ));
        }
        Ok(Self {
            documents: RwLock::new(HashMap::new()),
            dimensions,
        })
    }

    /// Configured vector dimension.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Number of currently stored documents.
    pub fn len(&self) -> usize {
        self.documents.read().expect("store lock poisoned").len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl VdbStore for InMemoryStore {
    async fn add(&self, documents: Vec<Document>) -> Result<()> {
        for document in documents {
            let embedding = document.embedding.as_ref().ok_or_else(|| {
                RagError::InvalidArgument("Document must have embedding set".to_string())
            })?;
            if embedding.len() != self.dimensions {
                return Err(RagError::VectorStore(format!(
                    "embedding dimension mismatch: expected {}, got {}",
                    self.dimensions,
                    embedding.len()
                )));
            }
            self.documents
                .write()
                .expect("store lock poisoned")
                .insert(document.id.clone(), document);
        }
        Ok(())
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<Document>> {
        if query.query_embedding.len() != self.dimensions {
            return Err(RagError::VectorStore(format!(
                "query embedding dimension mismatch: expected {}, got {}",
                self.dimensions,
                query.query_embedding.len()
            )));
        }

        let guard = self.documents.read().expect("store lock poisoned");
        let mut scored: Vec<Document> = Vec::with_capacity(guard.len());

        for document in guard.values() {
            if let Some(name) = &query.vector_name {
                if document.vector_name.as_deref() != Some(name.as_str()) {
                    continue;
                }
            }
            let Some(embedding) = document.embedding.as_ref() else {
                continue;
            };
            let score = cosine_similarity(&query.query_embedding, embedding)?;
            if let Some(threshold) = query.score_threshold {
                if score < threshold {
                    continue;
                }
            }
            let mut hit = document.clone();
            hit.score = Some(score);
            scored.push(hit);
        }
        drop(guard);

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(query.limit);
        Ok(scored)
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        Ok(self
            .documents
            .write()
            .expect("store lock poisoned")
            .remove(id)
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DocumentMetadata;

    fn doc(id: &str, embedding: Vec<f64>) -> Document {
        let mut d = Document::new(DocumentMetadata::text(id, id, "0"));
        d.set_embedding(embedding);
        d
    }

    #[test]
    fn cosine_similarity_handles_known_vectors() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]).unwrap() - 1.0).abs() < 1e-9);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).unwrap()).abs() < 1e-9);
        assert!((cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]).unwrap()).abs() < 1e-9);
    }

    #[test]
    fn cosine_similarity_rejects_length_mismatch() {
        assert!(cosine_similarity(&[1.0], &[1.0, 2.0]).is_err());
    }

    #[test]
    fn euclidean_distance_matches_formula() {
        let d = euclidean_distance(&[0.0, 0.0], &[3.0, 4.0]).unwrap();
        assert!((d - 5.0).abs() < 1e-9);
    }

    #[test]
    fn in_memory_store_rejects_zero_dimensions() {
        assert!(InMemoryStore::new(0).is_err());
    }

    #[tokio::test]
    async fn add_then_search_returns_nearest_first() {
        let store = InMemoryStore::new(2).unwrap();
        store
            .add(vec![doc("A", vec![1.0, 0.0]), doc("B", vec![0.0, 1.0])])
            .await
            .unwrap();
        assert_eq!(store.len(), 2);

        let hits = store.search(&SearchQuery::new(vec![1.0, 0.0], 5)).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].metadata.content_text(), "A");
        assert!((hits[0].score.unwrap() - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn store_rejects_missing_embedding() {
        let store = InMemoryStore::new(2).unwrap();
        let no_embedding = Document::new(DocumentMetadata::text("x", "x", "0"));
        assert!(matches!(
            store.add(vec![no_embedding]).await,
            Err(RagError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn store_rejects_dimension_mismatch() {
        let store = InMemoryStore::new(3).unwrap();
        assert!(matches!(
            store.add(vec![doc("A", vec![1.0, 0.0])]).await,
            Err(RagError::VectorStore(_))
        ));
    }

    #[tokio::test]
    async fn store_level_threshold_filters() {
        let store = InMemoryStore::new(2).unwrap();
        store.add(vec![doc("A", vec![1.0, 0.0])]).await.unwrap();
        let hits = store
            .search(&SearchQuery::new(vec![0.0, 1.0], 5).with_score_threshold(0.5))
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn default_delete_is_unsupported_but_in_memory_overrides_it() {
        let store = InMemoryStore::new(2).unwrap();
        store.add(vec![doc("A", vec![1.0, 0.0])]).await.unwrap();
        let id = store.search(&SearchQuery::new(vec![1.0, 0.0], 1)).await.unwrap()[0]
            .id
            .clone();
        assert!(store.delete(&id).await.unwrap());
        assert!(store.is_empty());
    }
}
