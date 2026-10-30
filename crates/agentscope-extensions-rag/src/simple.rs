//! Local / in-memory RAG backend.
//!
//! Ports the offline part of the Java `rag-simple` submodule: `SimpleKnowledge` plus the
//! `reader` / `store` / `embedding` building blocks it composes (those live in the crate
//! root because they are shared with other backends).
//!
//! The Java submodule also ships HTTP embedding providers (`dashscope`, `ollama`, `openai`),
//! ported in [`crate::embedding`], and external vector stores (`ElasticsearchStore`,
//! `MilvusStore`, `PgVectorStore`, `QdrantStore`), ported in [`crate::store`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::embedding::{EmbeddingModel, EmbeddingResponse, UsageListener};
use crate::error::{RagError, Result};
use crate::knowledge::{filter_and_rank, Knowledge};
use crate::model::{Document, RetrieveConfig};
use crate::store::{SearchQuery, VdbStore};

/// Knowledge base backed by an [`EmbeddingModel`] and a [`VdbStore`].
///
/// Ports `io.agentscope.core.rag.knowledge.SimpleKnowledge`:
/// * `add_documents`: embed every document's content, then batch-store them.
/// * `retrieve`: embed the query, search the store, filter/sort/limit.
///
/// Both paths report the provider's [`crate::embedding::EmbeddingUsage`] to an optional
/// [`UsageListener`], exactly like Java's `usageListener` callback.
pub struct SimpleKnowledge {
    embedding_model: Arc<dyn EmbeddingModel>,
    embedding_store: Arc<dyn VdbStore>,
    usage_listener: Option<UsageListener>,
}

impl SimpleKnowledge {
    /// Creates a knowledge base from an embedding model and a vector store.
    pub fn new(embedding_model: Arc<dyn EmbeddingModel>, embedding_store: Arc<dyn VdbStore>) -> Self {
        Self {
            embedding_model,
            embedding_store,
            usage_listener: None,
        }
    }

    /// Registers a token-usage callback (Java builder `usageListener`).
    pub fn with_usage_listener(mut self, listener: UsageListener) -> Self {
        self.usage_listener = Some(listener);
        self
    }

    /// The registered usage callback, if any.
    pub fn usage_listener(&self) -> Option<&UsageListener> {
        self.usage_listener.as_ref()
    }

    /// The embedding model backing this knowledge base.
    pub fn embedding_model(&self) -> &Arc<dyn EmbeddingModel> {
        &self.embedding_model
    }

    /// The vector store backing this knowledge base.
    pub fn embedding_store(&self) -> &Arc<dyn VdbStore> {
        &self.embedding_store
    }

    /// Notifies the listener after an embedding response (Java `notifyUsage`).
    fn notify_usage(&self, response: &EmbeddingResponse) {
        if let Some(listener) = &self.usage_listener {
            listener(&response.usage);
        }
    }
}

#[async_trait]
impl Knowledge for SimpleKnowledge {
    async fn add_documents(&self, documents: Vec<Document>) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }

        let mut embedded = Vec::with_capacity(documents.len());
        for mut document in documents {
            let response = self
                .embedding_model
                .embed_with_usage(&document.metadata.content)
                .await?;
            self.notify_usage(&response);
            document.set_embedding(response.embedding);
            embedded.push(document);
        }

        self.embedding_store.add(embedded).await
    }

    async fn retrieve(&self, query: &str, config: &RetrieveConfig) -> Result<Vec<Document>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let query_block = agentscope_core::message::ContentBlock::Text(
            agentscope_core::message::TextBlock::new(query),
        );
        let response = self.embedding_model.embed_with_usage(&query_block).await?;
        self.notify_usage(&response);

        // Java passes `scoreThreshold = null` to the store and filters afterwards;
        // the shared `filter_and_rank` helper reproduces that final step.
        let mut search = SearchQuery::new(response.embedding, config.limit);
        if let Some(name) = &config.vector_name {
            search = search.with_vector_name(name.clone());
        }

        let hits = self.embedding_store.search(&search).await?;
        Ok(filter_and_rank(hits, config))
    }
}

/// Build-in convenience constructor: a `SimpleKnowledge` with the offline
/// [`crate::embedding::HashEmbeddingModel`] and an [`crate::store::InMemoryStore`].
///
/// The Java equivalent requires wiring `DashScopeTextEmbedding` + `InMemoryStore`
/// manually; this helper makes the local path a one-liner that needs no network.
pub fn in_memory_knowledge(dimensions: usize) -> Result<SimpleKnowledge> {
    let model = Arc::new(crate::embedding::HashEmbeddingModel::new(dimensions));
    let store = Arc::new(crate::store::InMemoryStore::new(dimensions)?);
    Ok(SimpleKnowledge::new(model, store))
}

/// A knowledge base with no documents: useful as a null-object default.
#[derive(Debug, Default)]
pub struct EmptyKnowledge;

#[async_trait]
impl Knowledge for EmptyKnowledge {
    async fn add_documents(&self, _documents: Vec<Document>) -> Result<()> {
        Err(RagError::Unsupported(
            "EmptyKnowledge does not store documents".to_string(),
        ))
    }

    async fn retrieve(&self, _query: &str, _config: &RetrieveConfig) -> Result<Vec<Document>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::EmbeddingUsage;
    use crate::model::DocumentMetadata;
    use crate::reader::{Reader, ReaderInput, SplitStrategy, TextReader};
    use std::sync::Arc;

    fn text_doc(text: &str, doc_id: &str, chunk_id: &str) -> Document {
        Document::new(DocumentMetadata::text(text, doc_id, chunk_id))
    }

    #[tokio::test]
    async fn end_to_end_add_then_retrieve() {
        let knowledge = in_memory_knowledge(256).unwrap();
        knowledge
            .add_documents(vec![
                text_doc("rust ownership and borrowing rules", "d1", "0"),
                text_doc("baking sourdough bread at home", "d2", "0"),
            ])
            .await
            .unwrap();

        let cfg = RetrieveConfig::new(5, 0.0);
        let hits = knowledge
            .retrieve("rust ownership and borrowing rules", &cfg)
            .await
            .unwrap();

        assert!(!hits.is_empty());
        assert_eq!(hits[0].metadata.doc_id, "d1");
        assert!(hits[0].score.is_some());
    }

    #[tokio::test]
    async fn retrieve_applies_threshold_and_limit() {
        let knowledge = in_memory_knowledge(128).unwrap();
        knowledge
            .add_documents(vec![
                text_doc("alpha beta", "d1", "0"),
                text_doc("gamma delta", "d2", "0"),
                text_doc("epsilon zeta", "d3", "0"),
            ])
            .await
            .unwrap();

        let cfg = RetrieveConfig::new(1, 0.0);
        let hits = knowledge.retrieve("alpha beta", &cfg).await.unwrap();
        assert_eq!(hits.len(), 1);

        // A threshold above 1.0 can never be met by cosine similarity.
        let strict = RetrieveConfig::new(5, 1.5);
        assert!(knowledge
            .retrieve("alpha beta", &strict)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn empty_query_returns_empty_without_embedding() {
        let knowledge = in_memory_knowledge(32).unwrap();
        let hits = knowledge
            .retrieve("   ", &RetrieveConfig::default())
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn empty_document_list_is_a_no_op() {
        let knowledge = in_memory_knowledge(32).unwrap();
        assert!(knowledge.add_documents(vec![]).await.is_ok());
    }

    #[tokio::test]
    async fn reader_output_feeds_knowledge_end_to_end() {
        // chunk_size 20 keeps both paragraphs intact as separate chunks (10 and 11 chars);
        // a chunk_size of 10 would re-split the second paragraph by character.
        let reader = TextReader::new(20, SplitStrategy::Paragraph, 0).unwrap();
        let docs = reader
            .read(ReaderInput::from_string("alpha beta\n\ngamma delta"))
            .await
            .unwrap();
        assert_eq!(docs.len(), 2);

        let knowledge = in_memory_knowledge(64).unwrap();
        knowledge.add_documents(docs).await.unwrap();

        let hits = knowledge
            .retrieve("alpha beta", &RetrieveConfig::new(1, 0.0))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].metadata.chunk_id, "0");
    }

    #[tokio::test]
    async fn injectable_traits_allow_a_mock_embedder() {
        // Confirms the offline test surface: both collaborators are trait objects.
        struct ConstEmbedding;
        #[async_trait]
        impl EmbeddingModel for ConstEmbedding {
            async fn embed(
                &self,
                _block: &agentscope_core::message::ContentBlock,
            ) -> Result<Vec<f64>> {
                Ok(vec![1.0, 0.0])
            }
            fn model_name(&self) -> &str {
                "const"
            }
            fn dimensions(&self) -> usize {
                2
            }
        }

        let store = Arc::new(crate::store::InMemoryStore::new(2).unwrap());
        let knowledge = SimpleKnowledge::new(Arc::new(ConstEmbedding), store);
        knowledge
            .add_documents(vec![text_doc("whatever", "d", "0")])
            .await
            .unwrap();
        let hits = knowledge
            .retrieve("anything", &RetrieveConfig::new(1, 0.0))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score.unwrap() - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn empty_knowledge_retrieves_nothing_and_rejects_writes() {
        let knowledge = EmptyKnowledge;
        assert!(knowledge
            .retrieve("q", &RetrieveConfig::default())
            .await
            .unwrap()
            .is_empty());
        assert!(matches!(
            knowledge.add_documents(vec![text_doc("x", "d", "0")]).await,
            Err(RagError::Unsupported(_))
        ));
    }

    /// Embedder that reports usage, like the HTTP providers do.
    struct UsageEmbedding {
        calls: std::sync::Mutex<usize>,
    }

    #[async_trait]
    impl EmbeddingModel for UsageEmbedding {
        async fn embed(
            &self,
            block: &agentscope_core::message::ContentBlock,
        ) -> Result<Vec<f64>> {
            Ok(self.embed_with_usage(block).await?.embedding)
        }

        async fn embed_with_usage(
            &self,
            _block: &agentscope_core::message::ContentBlock,
        ) -> Result<EmbeddingResponse> {
            *self.calls.lock().unwrap() += 1;
            Ok(EmbeddingResponse::new(
                vec![1.0, 0.0],
                EmbeddingUsage::new(7, 0).with_time_seconds(0.125),
                "usage-model",
            ))
        }

        fn model_name(&self) -> &str {
            "usage-model"
        }

        fn dimensions(&self) -> usize {
            2
        }
    }

    #[tokio::test]
    async fn usage_listener_receives_add_and_retrieve_usage() {
        // Java `SimpleKnowledge` calls `notifyUsage` after every embedding in `addDocuments`
        // and once in `retrieve`.
        let seen: Arc<std::sync::Mutex<Vec<EmbeddingUsage>>> = Arc::new(Default::default());
        let sink = seen.clone();
        let store = Arc::new(crate::store::InMemoryStore::new(2).unwrap());
        let knowledge = SimpleKnowledge::new(
            Arc::new(UsageEmbedding {
                calls: std::sync::Mutex::new(0),
            }),
            store,
        )
        .with_usage_listener(Arc::new(move |usage: &EmbeddingUsage| {
            sink.lock().unwrap().push(*usage);
        }));
        assert!(knowledge.usage_listener().is_some());

        knowledge
            .add_documents(vec![text_doc("a", "d1", "0"), text_doc("b", "d2", "0")])
            .await
            .unwrap();
        knowledge
            .retrieve("a", &RetrieveConfig::new(1, 0.0))
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].input_tokens, 7);
        assert_eq!(seen[0].total_tokens, 7);
        assert_eq!(seen[0].time_seconds, Some(0.125));
    }

    #[tokio::test]
    async fn no_usage_listener_is_a_no_op() {
        let store = Arc::new(crate::store::InMemoryStore::new(2).unwrap());
        let knowledge = SimpleKnowledge::new(
            Arc::new(UsageEmbedding {
                calls: std::sync::Mutex::new(0),
            }),
            store,
        );
        assert!(knowledge.usage_listener().is_none());
        knowledge
            .add_documents(vec![text_doc("a", "d1", "0")])
            .await
            .unwrap();
    }
}
