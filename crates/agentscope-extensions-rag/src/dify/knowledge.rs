//! `Knowledge` adapter over the Dify client.
//!
//! Ports `io.agentscope.core.rag.integration.dify.DifyKnowledge`. Dify is retrieval-only, so
//! [`Knowledge::add_documents`] is unsupported, mirroring the Java
//! `UnsupportedOperationException`.

use async_trait::async_trait;

use crate::dify::client::DifyRAGClient;
use crate::dify::converter::DifyDocumentConverter;
use crate::error::{RagError, Result};
use crate::knowledge::{filter_and_rank, Knowledge};
use crate::model::{Document, RetrieveConfig};

/// Knowledge base backed by a Dify dataset.
pub struct DifyKnowledge {
    client: DifyRAGClient,
}

impl std::fmt::Debug for DifyKnowledge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DifyKnowledge")
            .field("client", &self.client)
            .finish()
    }
}

impl DifyKnowledge {
    /// Creates a knowledge base from a client.
    pub fn new(client: DifyRAGClient) -> Self {
        Self { client }
    }

    /// Creates a knowledge base with the real HTTP transport.
    pub fn from_config(config: crate::dify::config::DifyRAGConfig) -> Result<Self> {
        Ok(Self::new(DifyRAGClient::new(config)?))
    }

    /// The underlying client.
    pub fn client(&self) -> &DifyRAGClient {
        &self.client
    }
}

#[async_trait]
impl Knowledge for DifyKnowledge {
    async fn add_documents(&self, _documents: Vec<Document>) -> Result<()> {
        Err(RagError::Unsupported(
            "Document upload is not supported via this plugin. Please use Dify console to \
             manage documents. This design keeps the plugin focused on retrieval and \
             compatible with Bailian RAG plugin."
                .to_string(),
        ))
    }

    async fn retrieve(&self, query: &str, config: &RetrieveConfig) -> Result<Vec<Document>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Java forwards the config limit to the API as `top_k`.
        let limit = i32::try_from(config.limit).ok();
        let response = self.client.retrieve(query, limit).await?;
        let documents = DifyDocumentConverter::from_dify_response(&response);
        Ok(filter_and_rank(documents, config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dify::client::{DifyTransport, HttpRequest, HttpResponse};
    use crate::dify::config::DifyRAGConfig;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockTransport {
        body: Mutex<String>,
    }

    #[async_trait]
    impl DifyTransport for MockTransport {
        async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse> {
            Ok(HttpResponse {
                status_code: 200,
                body: self.body.lock().unwrap().clone(),
            })
        }
    }

    const RESPONSE: &str = r#"{
      "query": {"content": "q"},
      "records": [
        {"segment": {"id": "s1", "document_id": "d1", "content": "high"}, "score": 0.9},
        {"segment": {"id": "s2", "document_id": "d2", "content": "low"}, "score": 0.1},
        {"segment": {"id": "s3", "document_id": "d3", "content": "mid"}, "score": 0.6}
      ]
    }"#;

    fn knowledge(body: &str) -> DifyKnowledge {
        let config = DifyRAGConfig::builder()
            .api_key("k")
            .dataset_id("ds")
            .build_or_panic();
        let transport = MockTransport {
            body: Mutex::new(body.to_string()),
        };
        DifyKnowledge::new(DifyRAGClient::with_transport(config, Box::new(transport)))
    }

    #[tokio::test]
    async fn retrieve_converts_filters_sorts_and_limits() {
        let knowledge = knowledge(RESPONSE);
        let cfg = RetrieveConfig::new(2, 0.5);
        let hits = knowledge.retrieve("q", &cfg).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].metadata.content_text(), "high");
        assert_eq!(hits[1].metadata.content_text(), "mid");
    }

    #[tokio::test]
    async fn threshold_removes_low_scores() {
        let knowledge = knowledge(RESPONSE);
        let cfg = RetrieveConfig::new(10, 0.8);
        let hits = knowledge.retrieve("q", &cfg).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].metadata.content_text(), "high");
    }

    #[tokio::test]
    async fn empty_query_short_circuits() {
        let knowledge = knowledge(RESPONSE);
        assert!(knowledge
            .retrieve("  ", &RetrieveConfig::default())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn add_documents_is_unsupported_like_java() {
        let knowledge = knowledge(RESPONSE);
        let err = knowledge.add_documents(vec![]).await.unwrap_err();
        assert!(matches!(err, RagError::Unsupported(_)));
        assert!(err.to_string().contains("Dify console"));
    }

    #[tokio::test]
    async fn empty_records_produce_no_hits() {
        let knowledge = knowledge(r#"{"query":{"content":"q"},"records":[]}"#);
        assert!(knowledge
            .retrieve("q", &RetrieveConfig::default())
            .await
            .unwrap()
            .is_empty());
    }
}
