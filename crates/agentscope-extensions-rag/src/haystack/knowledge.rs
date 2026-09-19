//! `Knowledge` adapter over the HayStack client.
//!
//! Ports `HayStackKnowledge`. Haystack document ingestion is managed by the Haystack service
//! itself, so [`Knowledge::add_documents`] is unsupported, mirroring the Java
//! `UnsupportedOperationException` (same message text).
//!
//! Unlike the sibling backends, the Haystack adapter does **not** go through
//! [`crate::knowledge::filter_and_rank`]: Java deliberately kept documents whose score is
//! `None` (the `SentenceWindowRetriever` returns none) and skipped the descending sort, so the
//! server's ordering is preserved verbatim.

use async_trait::async_trait;

use crate::error::{RagError, Result};
use crate::haystack::client::HayStackClient;
use crate::haystack::config::HayStackConfig;
use crate::haystack::converter::HayStackDocumentConverter;
use crate::knowledge::Knowledge;
use crate::model::{Document, RetrieveConfig};

/// Knowledge base backed by a Haystack RAG server.
pub struct HayStackKnowledge {
    client: HayStackClient,
    config: HayStackConfig,
}

impl std::fmt::Debug for HayStackKnowledge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HayStackKnowledge")
            .field("client", &self.client)
            .finish()
    }
}

impl HayStackKnowledge {
    /// Creates a knowledge base from an existing client (the shared-client case).
    pub fn from_client(client: HayStackClient) -> Self {
        let config = client.config().clone();
        Self { client, config }
    }

    /// Creates a knowledge base with the real HTTP transport.
    pub fn from_config(config: HayStackConfig) -> Result<Self> {
        Ok(Self::from_client(HayStackClient::from_config(config)?))
    }

    /// Starts a builder, mirroring the Java `HayStackKnowledge.builder()`.
    pub fn builder() -> HayStackKnowledgeBuilder {
        HayStackKnowledgeBuilder::default()
    }

    /// The underlying client.
    pub fn client(&self) -> &HayStackClient {
        &self.client
    }

    /// The active configuration.
    pub fn config(&self) -> &HayStackConfig {
        &self.config
    }
}

/// Builder for [`HayStackKnowledge`].
///
/// Mirrors the Java builder: a config is required, and an explicit client overrides the one
/// derived from it.
#[derive(Debug, Default)]
pub struct HayStackKnowledgeBuilder {
    config: Option<HayStackConfig>,
    client: Option<HayStackClient>,
}

impl HayStackKnowledgeBuilder {
    /// Sets the configuration (required).
    pub fn config(mut self, config: HayStackConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Sets a client directly so it can be shared across knowledge bases.
    pub fn client(mut self, client: HayStackClient) -> Self {
        self.client = Some(client);
        self
    }

    /// Validates and builds the knowledge base.
    pub fn build(self) -> Result<HayStackKnowledge> {
        let config = self
            .config
            .ok_or_else(|| RagError::InvalidArgument("HayStackConfig is required".to_string()))?;
        let client = match self.client {
            Some(client) => client,
            None => HayStackClient::from_config(config.clone())?,
        };
        Ok(HayStackKnowledge { client, config })
    }
}

#[async_trait]
impl Knowledge for HayStackKnowledge {
    async fn add_documents(&self, _documents: Vec<Document>) -> Result<()> {
        Err(RagError::Unsupported(
            "Document upload is not supported via this plugin. Please use HayStack to manage \
             documents. This design keeps the plugin focused on retrieval and compatible with \
             Bailian RAG plugin."
                .to_string(),
        ))
    }

    async fn retrieve(&self, query: &str, config: &RetrieveConfig) -> Result<Vec<Document>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Java forwarded `limit` only when positive and `scoreThreshold` only when above zero;
        // otherwise the client fell back to the configured values.
        let top_k = i32::try_from(config.limit).ok().filter(|limit| *limit > 0);
        let score_threshold = (config.score_threshold > 0.0).then_some(config.score_threshold);

        let response = self.client.retrieve(query, top_k, score_threshold).await?;
        if response.is_empty_payload() {
            tracing::warn!(query = %query, "HayStack returned an empty response");
            return Ok(Vec::new());
        }

        let documents =
            HayStackDocumentConverter::convert_to_documents(response.effective_documents());

        // Java kept unscored documents (sentence-window mode) and never re-sorted.
        Ok(documents
            .into_iter()
            .filter(|document| match document.score {
                Some(score) => score >= config.score_threshold,
                None => true,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DocumentMetadata;
    use crate::transport::{HttpRequest, HttpResponse, HttpTransport};
    use async_trait::async_trait;
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

    fn config() -> HayStackConfig {
        HayStackConfig::builder()
            .base_url("http://localhost:8080/retrieve")
            .top_k(3)
            .build()
            .unwrap()
    }

    fn knowledge(responses: Vec<HttpResponse>) -> (HayStackKnowledge, Arc<ScriptedTransport>) {
        let transport = Arc::new(ScriptedTransport {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        });
        let client = HayStackClient::with_transport(config(), transport.clone());
        (HayStackKnowledge::from_client(client), transport)
    }

    #[test]
    fn builder_requires_a_config() {
        let error = HayStackKnowledge::builder().build().unwrap_err();
        assert!(matches!(error, RagError::InvalidArgument(_)), "{error:?}");
        assert!(
            error.to_string().contains("HayStackConfig is required"),
            "{error}"
        );
        assert!(HayStackKnowledge::builder().config(config()).build().is_ok());
    }

    #[test]
    fn builder_accepts_an_explicit_client() {
        let client =
            HayStackClient::with_transport(config(), Arc::new(ScriptedTransport::default()));
        let knowledge = HayStackKnowledge::builder()
            .config(config())
            .client(client)
            .build()
            .unwrap();
        assert_eq!(knowledge.config().top_k, Some(3));
    }

    #[tokio::test]
    async fn retrieve_converts_documents_and_preserves_order() {
        let (knowledge, _) = knowledge(vec![HttpResponse::new(
            200,
            r#"{"code":0,"documents":[
                {"id":"doc-1","content":"first","score":0.91,"meta":{"file_path":"intro.txt"}},
                {"id":"doc-2","content":"second","score":0.85,"meta":{"file_path":"intro1.txt"}}
            ]}"#,
        )]);

        let documents = knowledge
            .retrieve("q", &RetrieveConfig::new(5, 0.5))
            .await
            .unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].metadata.doc_id, "intro.txt");
        assert_eq!(documents[1].metadata.doc_id, "intro1.txt");
        assert_eq!(documents[0].metadata.content_text(), "first");
    }

    #[tokio::test]
    async fn retrieve_filters_scores_but_keeps_unscored_documents() {
        let (knowledge, _) = knowledge(vec![HttpResponse::new(
            200,
            r#"{"code":0,"documents":[
                {"id":"low","content":"low","score":0.1,"meta":{"file_path":"a"}},
                {"id":"high","content":"high","score":0.9,"meta":{"file_path":"b"}},
                {"id":"none","content":"window","meta":{"source":"c.txt"}}
            ]}"#,
        )]);

        let documents = knowledge
            .retrieve("q", &RetrieveConfig::new(5, 0.5))
            .await
            .unwrap();
        // The below-threshold document is dropped; the unscored one is kept.
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].metadata.doc_id, "b");
        assert_eq!(documents[1].metadata.doc_id, "c.txt");
        assert_eq!(documents[1].score, None);
    }

    #[tokio::test]
    async fn retrieve_uses_context_documents_for_sentence_window_responses() {
        let (knowledge, _) = knowledge(vec![HttpResponse::new(
            200,
            r#"{"code":0,"context_windows":["win"],"context_documents":[
                {"id":"9e4cd66e","content":"window content","meta":{"source":"usr_90.txt"}}
            ]}"#,
        )]);
        let documents = knowledge
            .retrieve("q", &RetrieveConfig::new(5, 0.0))
            .await
            .unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].metadata.doc_id, "usr_90.txt");
    }

    #[tokio::test]
    async fn empty_document_lists_yield_an_empty_result() {
        let (knowledge, _) = knowledge(vec![HttpResponse::new(200, r#"{"code":0}"#)]);
        let documents = knowledge
            .retrieve("q", &RetrieveConfig::default())
            .await
            .unwrap();
        assert!(documents.is_empty());
    }

    #[tokio::test]
    async fn empty_query_short_circuits_without_sending_a_request() {
        let (knowledge, transport) = knowledge(vec![]);
        let documents = knowledge
            .retrieve("  ", &RetrieveConfig::default())
            .await
            .unwrap();
        assert!(documents.is_empty());
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn zero_limit_and_threshold_fall_back_to_the_config() {
        let (knowledge, transport) = knowledge(vec![HttpResponse::new(
            200,
            r#"{"code":0,"documents":[]}"#,
        )]);
        // limit = 0 and threshold = 0.0 are treated as "unset" by the adapter.
        let documents = knowledge
            .retrieve("q", &RetrieveConfig::new(0, 0.0))
            .await
            .unwrap();
        assert!(documents.is_empty());

        let sent = transport.requests.lock().unwrap().last().cloned().unwrap();
        let body: serde_json::Value = serde_json::from_str(sent.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["top_k"], 3);
        assert!(body.get("score_threshold").is_none());
    }

    #[tokio::test]
    async fn add_documents_is_unsupported_with_the_java_message() {
        let (knowledge, _) = knowledge(vec![]);
        let error = knowledge
            .add_documents(vec![Document::new(DocumentMetadata::text("x", "d", "0"))])
            .await
            .unwrap_err();
        assert!(matches!(error, RagError::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("HayStack to manage"), "{error}");
    }
}
