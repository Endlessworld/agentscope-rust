//! The agentic-mode retrieval tool.
//!
//! Ports `io.agentscope.core.rag.KnowledgeRetrievalTools#retrieveKnowledge`
//! (`@Tool(name = "retrieve_knowledge")`): the agent decides when to search, the documents come
//! back in the textual format of [`format_documents_for_tool`], and a retrieval failure becomes
//! an **error result** rather than a failed call (Java `onErrorResume`).
//!
//! Java read the conversation history from the agent state
//! (`RuntimeContext.resolveAgentState(ctx, agent).getContext()`). Rust exposes the same data over
//! the call's runtime channel ([`ToolExecutionContext::conversation_history`], key
//! `conversationHistory`), so a default assembly needs no extra wiring; an installed
//! [`ConversationHistorySource`] is an explicit override that takes precedence. No history keeps
//! Java's `null`.

use std::sync::Arc;

use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::message::{JsonMap, Msg, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::knowledge::Knowledge;
use crate::message::format_documents_for_tool;
use crate::model::RetrieveConfig;

/// Java `@Tool(name = "retrieve_knowledge")`.
pub const RETRIEVE_KNOWLEDGE_TOOL_NAME: &str = "retrieve_knowledge";

/// Java `KnowledgeRetrievalTools.retrieveKnowledge` default for the optional `limit`.
pub const DEFAULT_RETRIEVE_LIMIT: usize = 5;

/// Supplies an explicit conversation history, overriding the call's runtime channel.
///
/// The runtime channel already carries the agent's recent messages, so this is only needed when a
/// host wants a different view (e.g. a longer history for Bailian's `queryHistory`).
pub type ConversationHistorySource = dyn Fn(&ToolExecutionContext) -> Vec<Msg> + Send + Sync;

/// Agentic RAG: `retrieve_knowledge`, implemented on the core [`Tool`] trait.
pub struct KnowledgeRetrievalTool {
    knowledge: Arc<dyn Knowledge>,
    default_config: RetrieveConfig,
    history_source: Option<Arc<ConversationHistorySource>>,
}

impl std::fmt::Debug for KnowledgeRetrievalTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnowledgeRetrievalTool")
            .field("default_config", &self.default_config)
            .field("has_history_source", &self.history_source.is_some())
            .finish_non_exhaustive()
    }
}

impl KnowledgeRetrievalTool {
    /// Java `KnowledgeRetrievalTools(Knowledge)`: limit 5, score threshold 0.5, no history.
    pub fn new(knowledge: Arc<dyn Knowledge>) -> Self {
        Self::with_config(knowledge, RetrieveConfig::default())
    }

    /// Java `KnowledgeRetrievalTools(Knowledge, RetrieveConfig)`.
    pub fn with_config(knowledge: Arc<dyn Knowledge>, default_config: RetrieveConfig) -> Self {
        Self {
            knowledge,
            default_config,
            history_source: None,
        }
    }

    /// Installs an explicit source for `RetrieveConfig::conversation_history`, overriding the
    /// runtime channel [`ToolExecutionContext::conversation_history`].
    pub fn with_history_source(mut self, source: Arc<ConversationHistorySource>) -> Self {
        self.history_source = Some(source);
        self
    }

    /// The knowledge base this tool retrieves from (Java `getKnowledgeBase`).
    pub fn knowledge(&self) -> &Arc<dyn Knowledge> {
        &self.knowledge
    }

    /// The default retrieval configuration (Java `getDefaultConfig`).
    pub fn default_config(&self) -> &RetrieveConfig {
        &self.default_config
    }

    /// The history Java took from the agent state: an installed source wins, otherwise the
    /// call's runtime channel (an empty history keeps Java's `null`).
    fn conversation_history(&self, ctx: &ToolExecutionContext) -> Option<Vec<Msg>> {
        if let Some(source) = &self.history_source {
            return Some(source(ctx));
        }
        let history = ctx.conversation_history();
        (!history.is_empty()).then_some(history)
    }
}

/// Reads an optional integer argument (`limit`), defaulting to [`DEFAULT_RETRIEVE_LIMIT`].
fn limit_arg(args: &JsonMap) -> CoreResult<usize> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(DEFAULT_RETRIEVE_LIMIT),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                Error::Tool(
                    "invalid tool arguments: limit must be a non-negative integer".to_string(),
                )
            }),
    }
}

#[async_trait]
impl Tool for KnowledgeRetrievalTool {
    fn name(&self) -> &str {
        RETRIEVE_KNOWLEDGE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Retrieve relevant documents from knowledge base. Use this tool when you need to find \
         specific information or when user asks questions about stored knowledge."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query to find relevant documents in the knowledge base"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of documents to retrieve (default: 5)"
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> CoreResult<ToolResultBlock> {
        let query = args.get("query").and_then(Value::as_str).ok_or_else(|| {
            Error::Tool("invalid tool arguments: query must be a string".to_string())
        })?;

        let mut config = self.default_config.clone();
        config.limit = limit_arg(&args)?;
        config.conversation_history = self.conversation_history(&ctx);

        match self.knowledge.retrieve(query, &config).await {
            Ok(documents) => Ok(ToolResultBlock::success(format_documents_for_tool(
                &documents,
            ))),
            // Java `onErrorResume`: the model is told the retrieval failed, the call succeeds.
            Err(_) => Ok(ToolResultBlock::error(format!(
                "Failed to retrieve knowledge for query: {query}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use agentscope_core::message::{MsgRole, ToolResultState};

    use crate::error::{RagError, Result};
    use crate::model::{Document, DocumentMetadata};

    /// Records the config of every `retrieve` call and replays a fixed outcome.
    struct RecordingKnowledge {
        documents: Vec<Document>,
        fail: bool,
        configs: Mutex<Vec<RetrieveConfig>>,
    }

    impl RecordingKnowledge {
        fn new(documents: Vec<Document>) -> Self {
            Self {
                documents,
                fail: false,
                configs: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                documents: Vec::new(),
                fail: true,
                configs: Mutex::new(Vec::new()),
            }
        }

        fn last_config(&self) -> RetrieveConfig {
            self.configs.lock().unwrap().last().cloned().unwrap()
        }
    }

    #[async_trait]
    impl Knowledge for RecordingKnowledge {
        async fn add_documents(&self, _documents: Vec<Document>) -> Result<()> {
            Err(RagError::Unsupported("read-only".to_string()))
        }

        async fn retrieve(&self, _query: &str, config: &RetrieveConfig) -> Result<Vec<Document>> {
            self.configs.lock().unwrap().push(config.clone());
            if self.fail {
                return Err(RagError::Api {
                    status_code: 500,
                    error_code: None,
                    message: "knowledge backend is down".to_string(),
                });
            }
            Ok(self.documents.clone())
        }
    }

    fn scored_document(text: &str, score: f64) -> Document {
        let mut document = Document::new(DocumentMetadata::text(text, "doc-1", "0"));
        document.set_score(score);
        document
    }

    fn tool(knowledge: Arc<RecordingKnowledge>) -> KnowledgeRetrievalTool {
        KnowledgeRetrievalTool::new(knowledge)
    }

    fn args(value: Value) -> JsonMap {
        match value {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    #[tokio::test]
    async fn retrieves_with_the_java_default_limit_and_formats_the_result() {
        let knowledge = Arc::new(RecordingKnowledge::new(vec![scored_document(
            "hello", 0.75,
        )]));
        let block = tool(knowledge.clone())
            .call(
                args(json!({ "query": "hello" })),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(block.state, ToolResultState::Success);
        let text = block.text_content();
        assert!(
            text.starts_with("Retrieved 1 relevant document(s):"),
            "{text}"
        );
        assert!(text.contains("Document 1 (Score: 0.750):\nhello"), "{text}");

        let config = knowledge.last_config();
        assert_eq!(config.limit, DEFAULT_RETRIEVE_LIMIT);
        assert_eq!(config.conversation_history, None);
        assert_eq!(
            config.score_threshold,
            RetrieveConfig::default().score_threshold
        );
    }

    #[tokio::test]
    async fn an_explicit_limit_overrides_the_default() {
        let knowledge = Arc::new(RecordingKnowledge::new(vec![]));
        tool(knowledge.clone())
            .call(
                args(json!({ "query": "hello", "limit": 2 })),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(knowledge.last_config().limit, 2);
    }

    #[tokio::test]
    async fn a_retrieval_failure_is_reported_as_an_error_result() {
        let block = tool(Arc::new(RecordingKnowledge::failing()))
            .call(
                args(json!({ "query": "hello" })),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(block.state, ToolResultState::Error);
        assert_eq!(
            block.text_content(),
            "Error: Failed to retrieve knowledge for query: hello"
        );
    }

    #[tokio::test]
    async fn the_history_source_fills_the_retrieve_config() {
        let knowledge = Arc::new(RecordingKnowledge::new(vec![]));
        let history = vec![Msg::text(
            agentscope_core::message::MsgRole::User,
            "earlier",
        )];
        let tool =
            tool(knowledge.clone()).with_history_source(Arc::new(move |_ctx| history.clone()));

        tool.call(
            args(json!({ "query": "hello" })),
            ToolExecutionContext::default(),
        )
        .await
        .unwrap();

        let config = knowledge.last_config();
        assert_eq!(config.conversation_history.unwrap().len(), 1);
    }

    /// A call context whose runtime carries the agent's conversation history.
    fn context_with_history(history: &[Msg]) -> ToolExecutionContext {
        let mut context = ToolExecutionContext::default();
        context.runtime.insert(
            agentscope_core::tool::CONVERSATION_HISTORY_KEY.to_string(),
            serde_json::to_value(history).unwrap(),
        );
        context
    }

    #[tokio::test]
    async fn the_runtime_conversation_history_fills_the_retrieve_config() {
        let knowledge = Arc::new(RecordingKnowledge::new(vec![]));
        let history = vec![
            Msg::text(MsgRole::User, "earlier"),
            Msg::text(MsgRole::Assistant, "answer"),
        ];

        tool(knowledge.clone())
            .call(
                args(json!({ "query": "hello" })),
                context_with_history(&history),
            )
            .await
            .unwrap();

        let filled = knowledge.last_config().conversation_history.unwrap();
        assert_eq!(filled.len(), 2);
        assert_eq!(filled[0].text_content(), "earlier");
        assert_eq!(filled[1].text_content(), "answer");
    }

    #[tokio::test]
    async fn a_call_without_runtime_history_keeps_the_config_none() {
        let knowledge = Arc::new(RecordingKnowledge::new(vec![]));
        tool(knowledge.clone())
            .call(
                args(json!({ "query": "hello" })),
                ToolExecutionContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(knowledge.last_config().conversation_history, None);
    }

    #[tokio::test]
    async fn the_history_source_overrides_the_runtime_conversation_history() {
        let knowledge = Arc::new(RecordingKnowledge::new(vec![]));
        let runtime_history = vec![Msg::text(MsgRole::User, "from runtime")];
        let injected = vec![Msg::text(MsgRole::User, "from source")];
        let tool =
            tool(knowledge.clone()).with_history_source(Arc::new(move |_ctx| injected.clone()));

        tool.call(
            args(json!({ "query": "hello" })),
            context_with_history(&runtime_history),
        )
        .await
        .unwrap();

        let filled = knowledge.last_config().conversation_history.unwrap();
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].text_content(), "from source");
    }

    #[tokio::test]
    async fn a_missing_query_argument_is_rejected() {
        let error = tool(Arc::new(RecordingKnowledge::new(vec![])))
            .call(args(json!({ "limit": 1 })), ToolExecutionContext::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("query must be a string"));
    }

    #[test]
    fn the_schema_matches_the_java_tool() {
        let tool = KnowledgeRetrievalTool::new(Arc::new(RecordingKnowledge::new(vec![])));
        assert_eq!(tool.name(), "retrieve_knowledge");
        assert_eq!(
            tool.description(),
            "Retrieve relevant documents from knowledge base. Use this tool when you need to find \
             specific information or when user asks questions about stored knowledge."
        );

        let schema = tool.parameters_schema();
        assert_eq!(schema["required"], json!(["query"]));
        assert_eq!(schema["properties"]["query"]["type"], json!("string"));
        assert_eq!(schema["properties"]["limit"]["type"], json!("integer"));
    }
}
