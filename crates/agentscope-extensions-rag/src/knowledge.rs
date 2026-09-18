//! Knowledge base abstraction.
//!
//! Ports `io.agentscope.core.rag.Knowledge`. Java returns Reactor `Mono`; here the
//! methods are `async_trait` futures.

use async_trait::async_trait;

use crate::error::Result;
use crate::model::{Document, RetrieveConfig};

/// A store of documents that can be queried by text.
///
/// Implementations live next to their backend (e.g. `simple::SimpleKnowledge`,
/// `dify::DifyKnowledge`).
#[async_trait]
pub trait Knowledge: Send + Sync {
    /// Adds (embeds and stores) documents.
    ///
    /// Remote, retrieval-only backends return [`crate::error::RagError::Unsupported`],
    /// mirroring the Java `UnsupportedOperationException`.
    async fn add_documents(&self, documents: Vec<Document>) -> Result<()>;

    /// Retrieves documents relevant to `query`, filtered and sorted by relevance.
    async fn retrieve(&self, query: &str, config: &RetrieveConfig) -> Result<Vec<Document>>;
}

/// Filters by `score_threshold`, sorts by descending score and truncates to `limit`.
///
/// Shared by every backend and mirrors the Java `Knowledge` implementations, which all
/// end with `filter(score >= threshold).sorted(desc).limit(limit)`.
///
/// Documents whose score is `None` are dropped, exactly like Java's
/// `doc.getScore() != null && doc.getScore() >= threshold` predicate. Ordering of
/// equal-score documents is preserved (stable sort).
pub fn filter_and_rank(mut documents: Vec<Document>, config: &RetrieveConfig) -> Vec<Document> {
    documents.retain(|d| d.score.is_some_and(|s| s >= config.score_threshold));
    documents.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    documents.truncate(config.limit);
    documents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DocumentMetadata;

    fn doc(text: &str, score: Option<f64>) -> Document {
        let mut d = Document::new(DocumentMetadata::text(text, "d", text));
        d.score = score;
        d
    }

    #[test]
    fn filter_drops_below_threshold() {
        let cfg = RetrieveConfig::new(10, 0.5);
        let out = filter_and_rank(vec![doc("a", Some(0.4)), doc("b", Some(0.9))], &cfg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].metadata.content_text(), "b");
    }

    #[test]
    fn filter_drops_missing_scores() {
        let cfg = RetrieveConfig::new(10, 0.0);
        let out = filter_and_rank(vec![doc("a", None), doc("b", Some(0.1))], &cfg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].metadata.content_text(), "b");
    }

    #[test]
    fn filter_sorts_descending_and_truncates() {
        let cfg = RetrieveConfig::new(2, 0.0);
        let out = filter_and_rank(
            vec![doc("a", Some(0.1)), doc("b", Some(0.9)), doc("c", Some(0.5))],
            &cfg,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].metadata.content_text(), "b");
        assert_eq!(out[1].metadata.content_text(), "c");
    }

    #[test]
    fn filter_includes_threshold_boundary() {
        let cfg = RetrieveConfig::new(10, 0.5);
        let out = filter_and_rank(vec![doc("a", Some(0.5))], &cfg);
        assert_eq!(out.len(), 1);
    }
}
