//! RAG mode plus the generic (automatic) retrieval hook.
//!
//! Ports the two Java **core** classes `io.agentscope.core.rag.{RAGMode, GenericRAGHook}` that
//! the `simple` submodule depends on (both `@Deprecated(forRemoval, since = "2.0.0")`, still the
//! only definition of how retrieved knowledge reaches the model):
//!
//! * [`RagMode`] — `GENERIC` (retrieve automatically), `AGENTIC` (retrieve through a tool),
//!   `NONE`.
//! * [`GenericRagHook`] — retrieves from a [`Knowledge`] and appends a
//!   `<retrieved_knowledge>` user message; never fails, never mutates the input when nothing is
//!   retrieved.
//!
//! Rust core has no `Hook`/`PreCallEvent` layer. The equivalent seam is
//! [`agentscope_core::middleware::Middleware`], whose `on_reasoning` receives the messages about
//! to be sent to the model and may append to them; [`GenericRagMiddleware`] wires the hook onto
//! it (Java `priority() == 50` → [`Middleware::order`] == 50). Nothing in core/harness had to
//! change.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::Msg;
use agentscope_core::middleware::{EventStream, Middleware, Next, ReasoningInput};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::knowledge::Knowledge;
use crate::message::{build_retrieved_knowledge_msg, extract_query_from_messages};
use crate::model::RetrieveConfig;

/// Java `RAGMode`.
///
/// The wire form is the Java enum constant name (`GENERIC` / `AGENTIC` / `NONE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RagMode {
    /// Retrieve automatically before every reasoning step (see [`GenericRagHook`]).
    Generic,
    /// The agent decides when to retrieve, through a tool
    /// (`message::format_documents_for_tool`).
    Agentic,
    /// No retrieval at all (Java default when no mode is configured).
    #[default]
    None,
}

impl RagMode {
    /// Java enum constant name.
    pub fn as_str(&self) -> &'static str {
        match self {
            RagMode::Generic => "GENERIC",
            RagMode::Agentic => "AGENTIC",
            RagMode::None => "NONE",
        }
    }

    /// Parses a Java enum constant name (exact match, like `Enum.valueOf`).
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "GENERIC" => Some(RagMode::Generic),
            "AGENTIC" => Some(RagMode::Agentic),
            "NONE" => Some(RagMode::None),
            _ => None,
        }
    }

    /// Whether retrieval happens before each reasoning step.
    pub fn is_generic(&self) -> bool {
        matches!(self, RagMode::Generic)
    }
}

/// Retrieves knowledge for the last user message and appends it as a user message.
///
/// Ports Java `GenericRAGHook`. Differences from the Java implementation are structural only:
///
/// * Java's constructor threw `IllegalArgumentException` for a null knowledge base / config;
///   [`GenericRagHook::new`] takes an `Arc` and a value, so those states are unrepresentable.
/// * Java returned the input messages untouched when the query was blank, when nothing matched,
///   or when retrieval failed (only a warning was logged); [`GenericRagHook::enhance_messages`]
///   does exactly the same and never returns an error.
/// * Java hooked `PreCallEvent`; [`GenericRagMiddleware`] hooks the reasoning phase, which is
///   what the Java javadoc describes ("injected before each reasoning step").
#[derive(Clone)]
pub struct GenericRagHook {
    knowledge: Arc<dyn Knowledge>,
    default_config: RetrieveConfig,
}

impl std::fmt::Debug for GenericRagHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenericRagHook")
            .field("default_config", &self.default_config)
            .finish_non_exhaustive()
    }
}

impl GenericRagHook {
    /// Java `GenericRAGHook(Knowledge)` — default config (limit 5, score threshold 0.5).
    pub fn new(knowledge: Arc<dyn Knowledge>) -> Self {
        Self {
            knowledge,
            default_config: RetrieveConfig::default(),
        }
    }

    /// Java `GenericRAGHook(Knowledge, RetrieveConfig)`.
    pub fn with_config(knowledge: Arc<dyn Knowledge>, default_config: RetrieveConfig) -> Self {
        Self {
            knowledge,
            default_config,
        }
    }

    /// The knowledge base this hook retrieves from (Java `getKnowledgeBase`).
    pub fn knowledge(&self) -> &Arc<dyn Knowledge> {
        &self.knowledge
    }

    /// The default retrieval configuration (Java `getDefaultConfig`).
    pub fn default_config(&self) -> &RetrieveConfig {
        &self.default_config
    }

    /// Java `priority()`: run early in the hook chain.
    pub fn priority(&self) -> i32 {
        50
    }

    /// Returns `messages` plus the `<retrieved_knowledge>` message, or `messages` unchanged.
    ///
    /// Unchanged when: the message list is empty, the last user message has blank text, the
    /// knowledge base returns nothing, or retrieval fails (logged, never propagated — Java
    /// `onErrorResume`).
    pub async fn enhance_messages(&self, messages: &[Msg]) -> Vec<Msg> {
        if messages.is_empty() {
            return Vec::new();
        }

        let query = extract_query_from_messages(messages);
        if query.trim().is_empty() {
            return messages.to_vec();
        }

        match self.knowledge.retrieve(&query, &self.default_config).await {
            Ok(documents) if documents.is_empty() => messages.to_vec(),
            Ok(documents) => {
                let mut enhanced = messages.to_vec();
                enhanced.push(build_retrieved_knowledge_msg(&documents));
                enhanced
            }
            Err(error) => {
                tracing::warn!(%error, "Generic RAG retrieval failed");
                messages.to_vec()
            }
        }
    }
}

/// [`Middleware`] adapter that runs a [`GenericRagHook`] before each reasoning step.
///
/// Register it on an agent to get Java `RAGMode.GENERIC` behaviour:
///
/// ```ignore
/// let hook = Arc::new(GenericRagHook::new(knowledge));
/// builder.middleware(Arc::new(GenericRagMiddleware::new(hook)));
/// ```
#[derive(Clone)]
pub struct GenericRagMiddleware {
    hook: Arc<GenericRagHook>,
}

impl GenericRagMiddleware {
    /// Wraps a hook.
    pub fn new(hook: Arc<GenericRagHook>) -> Self {
        Self { hook }
    }

    /// The wrapped hook.
    pub fn hook(&self) -> &Arc<GenericRagHook> {
        &self.hook
    }
}

impl Middleware for GenericRagMiddleware {
    fn order(&self) -> i32 {
        self.hook.priority()
    }

    fn on_reasoning(
        &self,
        _ctx: &RuntimeContext,
        mut input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        let hook = self.hook.clone();
        Box::pin(
            futures::stream::once(async move {
                let previous = std::mem::take(&mut input.messages);
                input.messages = hook.enhance_messages(&previous).await;
                next(input)
            })
            .flatten(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{RagError, Result};
    use crate::model::{Document, DocumentMetadata, RetrieveConfig};
    use agentscope_core::message::{MsgRole, TextBlock};
    use agentscope_core::model::GenerateOptions;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct StubKnowledge {
        documents: Vec<Document>,
        error: bool,
        seen: Mutex<Vec<(String, RetrieveConfig)>>,
    }

    impl StubKnowledge {
        fn with_documents(documents: Vec<Document>) -> Arc<Self> {
            Arc::new(Self {
                documents,
                error: false,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                documents: Vec::new(),
                error: true,
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Knowledge for StubKnowledge {
        async fn add_documents(&self, _documents: Vec<Document>) -> Result<()> {
            Err(RagError::Unsupported("read-only stub".to_string()))
        }

        async fn retrieve(&self, query: &str, config: &RetrieveConfig) -> Result<Vec<Document>> {
            self.seen
                .lock()
                .unwrap()
                .push((query.to_string(), config.clone()));
            if self.error {
                return Err(RagError::Api {
                    status_code: 500,
                    error_code: None,
                    message: "boom".to_string(),
                });
            }
            Ok(self.documents.clone())
        }
    }

    fn document(text: &str, score: f64) -> Document {
        let mut document = Document::new(DocumentMetadata::text(text, "doc-1", "0"));
        document.score = Some(score);
        document
    }

    fn user(text: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::User)
            .name("user")
            .content(vec![agentscope_core::message::ContentBlock::Text(
                TextBlock::new(text),
            )])
            .build()
    }

    #[test]
    fn rag_mode_wire_values_match_the_java_enum() {
        assert_eq!(RagMode::default(), RagMode::None);
        for mode in [RagMode::Generic, RagMode::Agentic, RagMode::None] {
            assert_eq!(RagMode::from_wire(mode.as_str()), Some(mode));
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{}\"", mode.as_str()));
            assert_eq!(serde_json::from_str::<RagMode>(&json).unwrap(), mode);
        }
        assert_eq!(RagMode::from_wire("generic"), None);
        assert!(RagMode::Generic.is_generic());
        assert!(!RagMode::Agentic.is_generic());
    }

    #[test]
    fn hook_defaults_match_java() {
        let hook = GenericRagHook::new(StubKnowledge::with_documents(Vec::new()));
        assert_eq!(hook.default_config().limit, 5);
        assert_eq!(hook.default_config().score_threshold, 0.5);
        assert_eq!(hook.priority(), 50);

        let hook = GenericRagHook::with_config(
            StubKnowledge::with_documents(Vec::new()),
            RetrieveConfig::new(2, 0.9),
        );
        assert_eq!(hook.default_config().limit, 2);
        assert_eq!(hook.default_config().score_threshold, 0.9);
    }

    #[tokio::test]
    async fn enhance_appends_the_retrieved_knowledge_message() {
        let knowledge = StubKnowledge::with_documents(vec![document("rust is safe", 0.9)]);
        let hook = GenericRagHook::new(knowledge.clone());
        let messages = vec![user("what is rust")];

        let enhanced = hook.enhance_messages(&messages).await;
        assert_eq!(enhanced.len(), 2);
        assert_eq!(enhanced[0].text_content(), "what is rust");
        assert_eq!(enhanced[1].role(), MsgRole::User);
        assert_eq!(enhanced[1].name(), Some("user"));
        let injected = enhanced[1].text_content();
        assert!(injected.contains("<retrieved_knowledge>"));
        assert!(injected.contains("rust is safe"));
        assert!(injected.contains("</retrieved_knowledge>"));

        let seen = knowledge.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "what is rust");
        assert_eq!(seen[0].1.limit, 5);
    }

    #[tokio::test]
    async fn enhance_uses_the_last_user_message_as_the_query() {
        let knowledge = StubKnowledge::with_documents(vec![document("x", 0.9)]);
        let hook = GenericRagHook::new(knowledge.clone());
        let messages = vec![
            user("first question"),
            Msg::builder()
                .role(MsgRole::Assistant)
                .text_content("an answer")
                .build(),
            user("second question"),
        ];
        hook.enhance_messages(&messages).await;
        assert_eq!(knowledge.seen.lock().unwrap()[0].0, "second question");
    }

    #[tokio::test]
    async fn enhance_keeps_the_messages_when_nothing_is_retrieved() {
        let knowledge = StubKnowledge::with_documents(Vec::new());
        let hook = GenericRagHook::new(knowledge.clone());
        let messages = vec![user("what is rust")];

        let enhanced = hook.enhance_messages(&messages).await;
        assert_eq!(enhanced, messages);
        assert_eq!(knowledge.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn enhance_keeps_the_messages_when_retrieval_fails() {
        let knowledge = StubKnowledge::failing();
        let hook = GenericRagHook::new(knowledge.clone());
        let messages = vec![user("what is rust")];

        let enhanced = hook.enhance_messages(&messages).await;
        assert_eq!(enhanced, messages);
    }

    #[tokio::test]
    async fn enhance_short_circuits_without_a_user_query() {
        let knowledge = StubKnowledge::with_documents(vec![document("x", 0.9)]);
        let hook = GenericRagHook::new(knowledge.clone());

        assert!(hook.enhance_messages(&[]).await.is_empty());

        let blank = vec![user("   ")];
        assert_eq!(hook.enhance_messages(&blank).await, blank);

        let assistant_only = vec![Msg::builder()
            .role(MsgRole::Assistant)
            .text_content("hello")
            .build()];
        assert_eq!(hook.enhance_messages(&assistant_only).await, assistant_only);

        // No user message / blank query never reaches the knowledge base.
        assert!(knowledge.seen.lock().unwrap().is_empty());
    }

    fn reasoning_input(messages: Vec<Msg>) -> ReasoningInput {
        ReasoningInput::new(messages, Vec::new(), GenerateOptions::default())
    }

    async fn drive(
        middleware: &GenericRagMiddleware,
        messages: Vec<Msg>,
    ) -> Vec<Msg> {
        let captured: Arc<Mutex<Option<Vec<Msg>>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();
        let next: Next<ReasoningInput> = Box::new(move |input| {
            *sink.lock().unwrap() = Some(input.messages);
            Box::pin(futures::stream::empty())
        });

        let mut stream = middleware.on_reasoning(&RuntimeContext::new(), reasoning_input(messages), next);
        while stream.next().await.is_some() {}
        let taken = captured.lock().unwrap().take().unwrap();
        taken
    }

    #[tokio::test]
    async fn middleware_injects_the_knowledge_message_before_next() {
        let hook = Arc::new(GenericRagHook::new(StubKnowledge::with_documents(vec![
            document("rust is safe", 0.9),
        ])));
        let middleware = GenericRagMiddleware::new(hook.clone());
        assert_eq!(middleware.order(), 50);
        assert!(Arc::ptr_eq(middleware.hook(), &hook));

        let delivered = drive(&middleware, vec![user("what is rust")]).await;
        assert_eq!(delivered.len(), 2);
        assert!(delivered[1].text_content().contains("<retrieved_knowledge>"));
    }

    #[tokio::test]
    async fn middleware_passes_the_input_through_when_nothing_matches() {
        let hook = Arc::new(GenericRagHook::new(StubKnowledge::with_documents(Vec::new())));
        let middleware = GenericRagMiddleware::new(hook);

        let delivered = drive(&middleware, vec![user("what is rust")]).await;
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].text_content(), "what is rust");
    }
}
