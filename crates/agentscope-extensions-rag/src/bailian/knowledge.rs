//! `Knowledge` adapter over the Bailian client.
//!
//! Ports `BailianKnowledge`. Bailian manages documents in its own console, so
//! [`Knowledge::add_documents`] is unsupported, mirroring the Java
//! `UnsupportedOperationException` (same message text).

use async_trait::async_trait;

use crate::bailian::client::BailianClient;
use crate::bailian::config::BailianConfig;
use crate::bailian::converter::BailianDocumentConverter;
use crate::bailian::query_history::query_history_from_messages;
use crate::error::{RagError, Result};
use crate::knowledge::{filter_and_rank, Knowledge};
use crate::model::{Document, RetrieveConfig};

/// Knowledge base backed by a Bailian index.
pub struct BailianKnowledge {
    client: BailianClient,
    config: Option<BailianConfig>,
    index_id: String,
}

impl std::fmt::Debug for BailianKnowledge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BailianKnowledge")
            .field("client", &self.client)
            .field("index_id", &self.index_id)
            .finish_non_exhaustive()
    }
}

impl BailianKnowledge {
    /// Creates a knowledge base from an existing client (the shared-client case).
    pub fn from_client(client: BailianClient, index_id: impl Into<String>) -> Result<Self> {
        let index_id = index_id.into();
        if index_id.trim().is_empty() {
            return Err(RagError::InvalidArgument(
                "IndexId cannot be null or empty".to_string(),
            ));
        }
        Ok(Self {
            client,
            config: None,
            index_id,
        })
    }

    /// Creates a knowledge base with the real HTTP transport, deriving the index id from
    /// `config.index_id`.
    pub fn from_config(config: BailianConfig) -> Result<Self> {
        Self::builder().config(config).build()
    }

    /// Starts a builder, mirroring the Java `BailianKnowledge.builder()`.
    pub fn builder() -> BailianKnowledgeBuilder {
        BailianKnowledgeBuilder::default()
    }

    /// The knowledge base index id (Java `getIndexId`).
    pub fn index_id(&self) -> &str {
        &self.index_id
    }

    /// The underlying client (Java `getClient`).
    pub fn client(&self) -> &BailianClient {
        &self.client
    }

    /// The configuration, when the knowledge base was built from one.
    pub fn config(&self) -> Option<&BailianConfig> {
        self.config.as_ref()
    }
}

/// Builder for [`BailianKnowledge`].
///
/// Mirrors the Java builder's invariants: exactly one of `config` / `client` must be supplied,
/// and an index id must come from either the builder or the config.
#[derive(Debug, Default)]
pub struct BailianKnowledgeBuilder {
    config: Option<BailianConfig>,
    client: Option<BailianClient>,
    index_id: Option<String>,
}

impl BailianKnowledgeBuilder {
    /// Sets the configuration; a [`BailianClient`] will be created from it.
    pub fn config(mut self, config: BailianConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Sets a client directly so it can be shared across knowledge bases.
    pub fn client(mut self, client: BailianClient) -> Self {
        self.client = Some(client);
        self
    }

    /// Sets the index id, overriding the one in the config when both are present.
    pub fn index_id(mut self, index_id: impl Into<String>) -> Self {
        self.index_id = Some(index_id.into());
        self
    }

    /// Validates and builds the knowledge base.
    pub fn build(self) -> Result<BailianKnowledge> {
        if self.config.is_some() && self.client.is_some() {
            return Err(RagError::InvalidArgument(
                "Cannot set both config and client. Choose one approach.".to_string(),
            ));
        }

        let client = match (self.client, self.config.as_ref()) {
            (Some(client), _) => client,
            (None, Some(config)) => BailianClient::from_config(config.clone())?,
            (None, None) => {
                return Err(RagError::InvalidArgument(
                    "Either config or client must be provided".to_string(),
                ))
            }
        };

        let index_id = self
            .index_id
            .or_else(|| self.config.as_ref().and_then(|c| c.index_id.clone()))
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                RagError::InvalidArgument(
                    "IndexId must be provided either via config or builder".to_string(),
                )
            })?;

        Ok(BailianKnowledge {
            client,
            config: self.config,
            index_id,
        })
    }
}

#[async_trait]
impl Knowledge for BailianKnowledge {
    async fn add_documents(&self, _documents: Vec<Document>) -> Result<()> {
        Err(RagError::Unsupported(
            "Document upload via API is not yet supported. Please use Bailian console to \
             manage documents, or this feature will be added in future releases."
                .to_string(),
        ))
    }

    async fn retrieve(&self, query: &str, config: &RetrieveConfig) -> Result<Vec<Document>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Java forwarded `config.limit` and the conversation history, then filtered by
        // `score >= scoreThreshold` and limited to `config.limit`.
        let history = config
            .conversation_history
            .as_deref()
            .map(query_history_from_messages)
            .filter(|history| !history.is_empty());

        let response = self
            .client
            .retrieve(
                &self.index_id,
                query,
                Some(config.limit),
                history.as_deref(),
            )
            .await?;
        let documents = BailianDocumentConverter::from_response(&response);
        Ok(filter_and_rank(documents, config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bailian::config::BailianConfig;
    use crate::model::DocumentMetadata;
    use crate::transport::{HttpRequest, HttpResponse, HttpTransport};
    use agentscope_core::message::{Msg, MsgRole};
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

    fn config() -> BailianConfig {
        BailianConfig::builder()
            .access_key_id("ak")
            .access_key_secret("sk")
            .workspace_id("llm-abc")
            .index_id("idx-1")
            .build()
            .unwrap()
    }

    fn knowledge(responses: Vec<HttpResponse>) -> (BailianKnowledge, Arc<ScriptedTransport>) {
        let transport = Arc::new(ScriptedTransport {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        });
        let client = BailianClient::with_transport(config(), transport.clone());
        (
            BailianKnowledge::builder()
                .client(client)
                .index_id("idx-1")
                .build()
                .unwrap(),
            transport,
        )
    }

    fn body(nodes: &str) -> String {
        format!(r#"{{"Code":"0","Data":{{"Nodes":[{nodes}]}}}}"#)
    }

    #[test]
    fn builder_rejects_both_config_and_client_and_missing_index() {
        let client =
            BailianClient::with_transport(config(), Arc::new(ScriptedTransport::default()));
        let both = BailianKnowledge::builder()
            .config(config())
            .client(client)
            .build();
        assert!(
            matches!(both, Err(RagError::InvalidArgument(_))),
            "{both:?}"
        );

        assert!(BailianKnowledge::builder().build().is_err());
        assert!(BailianKnowledge::builder()
            .config(config())
            .index_id("  ")
            .build()
            .is_err());
    }

    #[test]
    fn builder_prefers_the_explicit_index_id_over_the_config_one() {
        let knowledge = BailianKnowledge::builder()
            .config(config())
            .index_id("explicit")
            .build()
            .unwrap();
        assert_eq!(knowledge.index_id(), "explicit");

        let knowledge = BailianKnowledge::builder()
            .config(config())
            .build()
            .unwrap();
        assert_eq!(knowledge.index_id(), "idx-1");
    }

    #[test]
    fn index_id_requires_a_non_blank_value() {
        let client =
            BailianClient::with_transport(config(), Arc::new(ScriptedTransport::default()));
        assert!(BailianKnowledge::from_client(client, "  ").is_err());
    }

    #[tokio::test]
    async fn retrieve_filters_low_scores_and_applies_the_limit() {
        let (knowledge, _) = knowledge(vec![HttpResponse::new(
            200,
            body(concat!(
                r#"{"Text":"low","Score":0.1,"Metadata":{"doc_id":"d1","_id":"c1"}},"#,
                r#"{"Text":"high","Score":0.9,"Metadata":{"doc_id":"d2","_id":"c2"}},"#,
                r#"{"Text":"mid","Score":0.6,"Metadata":{"doc_id":"d3","_id":"c3"}}"#
            )),
        )]);

        let config = RetrieveConfig::new(1, 0.5);
        let documents = knowledge.retrieve("q", &config).await.unwrap();
        assert_eq!(documents.len(), 1);
        // Highest score wins after the descending sort + limit.
        assert_eq!(documents[0].metadata.doc_id, "d2");
        assert_eq!(documents[0].score, Some(0.9));
    }

    #[tokio::test]
    async fn retrieve_drops_documents_without_a_score() {
        let (knowledge, _) = knowledge(vec![HttpResponse::new(
            200,
            body(r#"{"Text":"no score","Metadata":{"doc_id":"d1"}}"#),
        )]);
        let config = RetrieveConfig::new(5, 0.0);
        assert!(knowledge.retrieve("q", &config).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_query_short_circuits_without_sending_a_request() {
        let (knowledge, transport) = knowledge(vec![]);
        let documents = knowledge
            .retrieve("   ", &RetrieveConfig::default())
            .await
            .unwrap();
        assert!(documents.is_empty());
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn conversation_history_is_forwarded_as_query_history() {
        let (knowledge, transport) = knowledge(vec![HttpResponse::new(200, body(""))]);
        let config = RetrieveConfig::new(5, 0.5).with_conversation_history(vec![
            Msg::text(MsgRole::User, "hello"),
            Msg::text(MsgRole::Assistant, "hi"),
            Msg::text(MsgRole::System, "ignored"),
        ]);

        knowledge.retrieve("q", &config).await.unwrap();

        let sent = transport.requests.lock().unwrap().last().cloned().unwrap();
        let body: serde_json::Value = serde_json::from_str(sent.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["queryHistory"].as_array().unwrap().len(), 2);
        assert_eq!(body["queryHistory"][0]["role"], "user");
        assert_eq!(body["queryHistory"][1]["content"], "hi");
        // `config.limit` is forwarded as the call-site limit.
        assert_eq!(body["denseSimilarityTopK"], 5);
    }

    #[tokio::test]
    async fn add_documents_is_unsupported_with_the_java_message() {
        let (knowledge, _) = knowledge(vec![]);
        let error = knowledge
            .add_documents(vec![Document::new(DocumentMetadata::text("x", "d", "0"))])
            .await
            .unwrap_err();
        assert!(matches!(error, RagError::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("Bailian console"), "{error}");
    }
}
