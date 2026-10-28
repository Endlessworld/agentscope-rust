//! ACL-first retrieval decoration (`JevKnowledge`) and the `JevKnowledgeTool` projection.
//!
//! Java implements the deprecated core `Knowledge` SPI; the port keeps a minimal
//! [`KnowledgeSource`] seam so the decoration logic (ACL in every mode, candidate limit under
//! `ENFORCE`, shadowing, fail-closed enforcement) stays testable without the core RAG types.
//!
//! Documents are [`crate::jev::application::KnowledgeDocument`] so that retrieval and the
//! application `JevRag` workflow share one document identity.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::JevError;
use crate::jev::application::KnowledgeDocument;
use crate::jev::execution::{Mode, Options, Status};

use crate::jev::application::JevCandidateSelector;

/// Why the tool cannot be registered from this crate.
pub const KNOWLEDGE_TOOL_UNAVAILABLE: &str = "JevKnowledgeTool registration needs the core Toolkit/RuntimeContext seam; only the pure passage projection is ported";

/// The minimum a host knowledge backend must offer (`Knowledge.retrieve`).
#[async_trait]
pub trait KnowledgeSource: Send + Sync {
    /// Adds documents to the backend (`Knowledge.addDocuments`).
    async fn add_documents(&self, documents: Vec<KnowledgeDocument>) -> Result<(), JevError>;

    /// Retrieves up to `limit` documents for `query`.
    async fn retrieve(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeDocument>, JevError>;
}

/// The agent-visible passage (`JevKnowledgeTool.Passage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgePassage {
    /// The document id.
    pub id: String,
    /// The document text.
    pub text: String,
}

/// Projects documents onto the agent-visible passages.
pub fn knowledge_passages(documents: &[KnowledgeDocument]) -> Vec<KnowledgePassage> {
    documents
        .iter()
        .map(|document| KnowledgePassage {
            id: document.id.clone(),
            text: document.text.clone(),
        })
        .collect()
}

/// Wraps a knowledge source with JEV selection.
pub struct JevKnowledge {
    source: Arc<dyn KnowledgeSource>,
    selector: Arc<JevCandidateSelector>,
    mode: Mode,
    candidate_limit: usize,
}

impl std::fmt::Debug for JevKnowledge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevKnowledge")
            .field("mode", &self.mode)
            .field("candidate_limit", &self.candidate_limit)
            .finish_non_exhaustive()
    }
}

impl JevKnowledge {
    /// Creates the decorator; the selector and adapter modes must match.
    pub fn new(
        source: Arc<dyn KnowledgeSource>,
        selector: Arc<JevCandidateSelector>,
        options: Options,
        candidate_limit: usize,
    ) -> Result<Self, JevError> {
        if selector.mode() != options.mode {
            return Err(JevError::invalid("selector and adapter modes must match"));
        }
        if !(1..=1024).contains(&candidate_limit) {
            return Err(JevError::invalid("candidateLimit must be 1..1024"));
        }
        Ok(Self {
            source,
            selector,
            mode: options.mode,
            candidate_limit,
        })
    }

    /// The configured mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The candidate limit.
    pub fn candidate_limit(&self) -> usize {
        self.candidate_limit
    }

    /// Adds documents through to the backend.
    pub async fn add_documents(&self, documents: Vec<KnowledgeDocument>) -> Result<(), JevError> {
        self.source.add_documents(documents).await
    }

    /// Retrieves, applies the ACL, then (outside `OFF`) shadows or enforces the selection.
    pub async fn retrieve(
        &self,
        query: &str,
        limit: usize,
        acl: &dyn Fn(&KnowledgeDocument) -> bool,
    ) -> Result<Vec<KnowledgeDocument>, JevError> {
        let delegate_limit = if self.mode == Mode::Enforce {
            self.candidate_limit.max(limit)
        } else {
            limit
        };
        let documents = self.source.retrieve(query, delegate_limit).await?;
        let visible: Vec<KnowledgeDocument> = documents
            .into_iter()
            .filter(|document| acl(document))
            .collect();
        if self.mode == Mode::Off || visible.is_empty() {
            return Ok(visible.into_iter().take(limit).collect());
        }
        let mut candidates: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut originals: BTreeMap<String, KnowledgeDocument> = BTreeMap::new();
        for document in &visible {
            if originals
                .insert(document.id.clone(), document.clone())
                .is_some()
            {
                return Err(JevError::invalid("duplicate retrieved document id"));
            }
            candidates.insert(document.id.clone(), serde_json::json!(document.text));
        }
        let decision = self
            .selector
            .select(
                serde_json::json!({ "query": query }),
                "Does this passage provide evidence relevant to the query?",
                candidates,
            )
            .await?;
        if self.mode == Mode::Shadow {
            return Ok(visible.into_iter().take(limit).collect());
        }
        if decision.status != Status::Decided {
            return Err(JevError::protocol(format!(
                "JEV retrieval unavailable: {}",
                decision.reason
            )));
        }
        let Some(selection) = decision.value else {
            return Err(JevError::protocol("JEV retrieval unavailable: empty decision"));
        };
        if selection.selected.is_empty() && !selection.uncertain.is_empty() {
            return Err(JevError::protocol("JEV retrieval inconclusive"));
        }
        Ok(selection
            .selected
            .iter()
            .take(limit)
            .filter_map(|id| originals.get(id).cloned())
            .collect())
    }
}

/// The tool's structural skeleton (`JevKnowledgeTool`).
#[derive(Debug, Clone)]
pub struct KnowledgeTool {
    limit: usize,
}

impl KnowledgeTool {
    /// Creates the tool with a retrieval limit.
    pub fn new(limit: usize) -> Result<Self, JevError> {
        if limit < 1 {
            return Err(JevError::invalid("positive limit required"));
        }
        Ok(Self { limit })
    }

    /// The configured limit.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Fails closed: registration needs the core toolkit.
    pub fn register(&self) -> Result<(), JevError> {
        Err(JevError::protocol(KNOWLEDGE_TOOL_UNAVAILABLE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::dto::{Answer, NoulAnswer, SystemOneResult, Usage};
    use std::time::Duration;

    struct Fixed(Vec<KnowledgeDocument>);

    #[async_trait]
    impl KnowledgeSource for Fixed {
        async fn add_documents(&self, _documents: Vec<KnowledgeDocument>) -> Result<(), JevError> {
            Ok(())
        }
        async fn retrieve(
            &self,
            _query: &str,
            limit: usize,
        ) -> Result<Vec<KnowledgeDocument>, JevError> {
            Ok(self.0.iter().take(limit).cloned().collect())
        }
    }

    fn document(id: &str) -> KnowledgeDocument {
        KnowledgeDocument::new(id, format!("text {id}"), "v1").unwrap()
    }

    fn selector(values: &[f64], mode: Mode) -> Arc<JevCandidateSelector> {
        let mut answers = BTreeMap::new();
        for (index, value) in values.iter().enumerate() {
            answers.insert(
                format!("item_{index}"),
                Answer::Noul(NoulAnswer {
                    noul: Some(*value),
                }),
            );
        }
        let scripted = Arc::new(ScriptedCaller::new(vec![Ok(SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(answers),
            usage: Some(Usage::new(1, 1)),
        })]));
        Arc::new(
            JevCandidateSelector::new(
                scripted.as_caller(),
                Options::new(mode, Duration::from_secs(2), "v1").unwrap(),
                0.2,
                0.8,
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn off_mode_applies_the_acl_only() {
        let knowledge = JevKnowledge::new(
            Arc::new(Fixed(vec![document("a"), document("b")])),
            selector(&[], Mode::Off),
            Options::disabled(),
            8,
        )
        .unwrap();
        assert_eq!(knowledge.mode(), Mode::Off);
        assert_eq!(knowledge.candidate_limit(), 8);
        let retrieved = knowledge
            .retrieve("q", 5, &|document| document.id == "b")
            .await
            .unwrap();
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].id, "b");
        assert!(format!("{knowledge:?}").contains("JevKnowledge"));
        assert!(knowledge.add_documents(vec![document("c")]).await.is_ok());
    }

    #[tokio::test]
    async fn shadow_keeps_retrieval_order_and_enforce_selects() {
        let knowledge = JevKnowledge::new(
            Arc::new(Fixed(vec![document("a"), document("b")])),
            selector(&[0.1, 0.9], Mode::Shadow),
            Options::new(Mode::Shadow, Duration::from_secs(2), "v1").unwrap(),
            8,
        )
        .unwrap();
        let retrieved = knowledge.retrieve("q", 5, &|_| true).await.unwrap();
        assert_eq!(retrieved.len(), 2, "shadow returns whatever was retrieved");

        let knowledge = JevKnowledge::new(
            Arc::new(Fixed(vec![document("a"), document("b")])),
            selector(&[0.1, 0.9], Mode::Enforce),
            Options::new(Mode::Enforce, Duration::from_secs(2), "v1").unwrap(),
            8,
        )
        .unwrap();
        let retrieved = knowledge.retrieve("q", 5, &|_| true).await.unwrap();
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].id, "b");
        assert_eq!(retrieved[0].text, "text b");
    }

    #[test]
    fn construction_validates_modes_and_limits() {
        let source = Arc::new(Fixed(vec![]));
        assert_eq!(
            JevKnowledge::new(
                source.clone(),
                selector(&[], Mode::Shadow),
                Options::disabled(),
                8
            )
            .unwrap_err()
            .message(),
            "selector and adapter modes must match"
        );
        for limit in [0usize, 1025] {
            assert_eq!(
                JevKnowledge::new(source.clone(), selector(&[], Mode::Off), Options::disabled(), limit)
                    .unwrap_err()
                    .message(),
                "candidateLimit must be 1..1024"
            );
        }
        assert_eq!(
            KnowledgeTool::new(0).unwrap_err().message(),
            "positive limit required"
        );
        let tool = KnowledgeTool::new(4).unwrap();
        assert_eq!(tool.limit(), 4);
        assert_eq!(tool.register().unwrap_err().message(), KNOWLEDGE_TOOL_UNAVAILABLE);
        assert_eq!(
            knowledge_passages(&[document("a")]),
            vec![KnowledgePassage {
                id: "a".to_string(),
                text: "text a".to_string()
            }]
        );
    }
}
