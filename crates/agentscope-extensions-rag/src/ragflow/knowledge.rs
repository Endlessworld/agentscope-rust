//! `Knowledge` adapter over the RAGFlow client.
//!
//! Ports `io.agentscope.core.rag.integration.ragflow.RAGFlowKnowledge`. RAGFlow is
//! retrieval-only, so [`Knowledge::add_documents`] is unsupported, mirroring the Java
//! `UnsupportedOperationException` (same message text).

use async_trait::async_trait;

use crate::error::{RagError, Result};
use crate::knowledge::{filter_and_rank, Knowledge};
use crate::model::{Document, RetrieveConfig};
use crate::ragflow::client::RAGFlowClient;
use crate::ragflow::config::RAGFlowConfig;
use crate::ragflow::converter::RAGFlowDocumentConverter;

/// Knowledge base backed by a RAGFlow dataset.
pub struct RAGFlowKnowledge {
    client: RAGFlowClient,
}

impl std::fmt::Debug for RAGFlowKnowledge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RAGFlowKnowledge")
            .field("client", &self.client)
            .finish()
    }
}

impl RAGFlowKnowledge {
    /// Creates a knowledge base from a client.
    pub fn new(client: RAGFlowClient) -> Self {
        Self { client }
    }

    /// Creates a knowledge base with the real HTTP transport.
    pub fn from_config(config: RAGFlowConfig) -> Result<Self> {
        Ok(Self::new(RAGFlowClient::from_config(config)?))
    }

    /// The underlying client.
    pub fn client(&self) -> &RAGFlowClient {
        &self.client
    }
}

#[async_trait]
impl Knowledge for RAGFlowKnowledge {
    async fn add_documents(&self, _documents: Vec<Document>) -> Result<()> {
        Err(RagError::Unsupported(
            "Document upload is not supported via this plugin. Please use RAGFlow console to \
             manage documents. This design keeps the plugin focused on retrieval and \
             compatible with Bailian and Dify RAG plugins."
                .to_string(),
        ))
    }

    async fn retrieve(&self, query: &str, config: &RetrieveConfig) -> Result<Vec<Document>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Java forwards `config.limit` as `top_k` and `config.scoreThreshold` as
        // `similarity_threshold`; the metadata condition always comes from the config.
        let top_k = i32::try_from(config.limit).ok();
        let response = self
            .client
            .retrieve(query, top_k, Some(config.score_threshold), None)
            .await?;
        let documents = RAGFlowDocumentConverter::from_response(&response);
        Ok(filter_and_rank(documents, config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ragflow::config::RAGFlowConfig;
    use crate::transport::{HttpRequest, HttpResponse, HttpTransport};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct ScriptedTransport {
        responses: Mutex<Vec<HttpResponse>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| RagError::Http("no scripted response".to_string()))
        }
    }

    fn knowledge(body: &str) -> (RAGFlowKnowledge, Arc<ScriptedTransport>) {
        let transport = Arc::new(ScriptedTransport {
            responses: Mutex::new(vec![HttpResponse::new(200, body)]),
            requests: Mutex::new(Vec::new()),
        });
        let config = RAGFlowConfig::builder()
            .api_key("k")
            .base_url("http://localhost:9380")
            .add_dataset_id("ds-1")
            .build()
            .unwrap();
        (
            RAGFlowKnowledge::new(RAGFlowClient::with_transport(
                config,
                transport.clone(),
            )),
            transport,
        )
    }

    const RESPONSE: &str = r#"{
      "code": 0,
      "data": {
        "chunks": [
          {"id": "s1", "document_id": "d1", "content": "high", "similarity": 0.9},
          {"id": "s2", "document_id": "d2", "content": "low", "similarity": 0.1},
          {"id": "s3", "document_id": "d3", "content": "mid", "similarity": 0.6}
        ],
        "total": 3
      }
    }"#;

    #[tokio::test]
    async fn retrieve_filters_ranks_and_limits() {
        let (knowledge, _) = knowledge(RESPONSE);
        let config = RetrieveConfig::new(2, 0.5);
        let documents = knowledge.retrieve("what", &config).await.unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].metadata.content_text(), "high");
        assert_eq!(documents[1].metadata.content_text(), "mid");
    }

    #[tokio::test]
    async fn retrieve_sends_limit_and_threshold() {
        let (knowledge, transport) = knowledge(RESPONSE);
        let config = RetrieveConfig::new(4, 0.25);
        knowledge.retrieve("what", &config).await.unwrap();

        let requests = transport.requests.lock().unwrap();
        let body: Value = serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["top_k"], serde_json::json!(4));
        assert_eq!(body["similarity_threshold"], serde_json::json!(0.25));
    }

    #[tokio::test]
    async fn blank_query_short_circuits() {
        let (knowledge, transport) = knowledge(RESPONSE);
        let documents = knowledge
            .retrieve("  ", &RetrieveConfig::default())
            .await
            .unwrap();
        assert!(documents.is_empty());
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_payload_yields_no_documents() {
        let (knowledge, _) = knowledge(r#"{"code": 0, "data": false}"#);
        let documents = knowledge
            .retrieve("q", &RetrieveConfig::new(5, 0.0))
            .await
            .unwrap();
        assert!(documents.is_empty());
    }

    #[tokio::test]
    async fn add_documents_is_unsupported() {
        let (knowledge, _) = knowledge(RESPONSE);
        let err = knowledge.add_documents(Vec::new()).await.unwrap_err();
        assert!(matches!(err, RagError::Unsupported(_)));
        assert!(err.to_string().contains("RAGFlow console"));
    }
}
