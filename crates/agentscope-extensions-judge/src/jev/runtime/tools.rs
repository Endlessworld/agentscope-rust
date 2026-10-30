//! The `@Tool` registration layer: `search_evidence`, `search_knowledge`,
//! `review_code_snapshot`.
//!
//! Java declares the three tools with `@Tool`/`@ToolParam` and registers them on the core
//! `Toolkit`; each method also takes a `RuntimeContext` from which it reads the caller's identity
//! and a typed `Source`. Rust's [`Tool::call`] receives a [`ToolExecutionContext`] whose `runtime`
//! map carries `userId` / `sessionId` (core writes them in `tool_runtime`), so:
//!
//! * identity is read from the call context exactly like the harness' own tools
//!   (`artifact_delivery`, `wait_async`) — no parallel mechanism;
//! * the typed `Source` is injected at construction time, because [`ToolExecutionContext`] cannot
//!   carry a `RuntimeContext::extensions` bag. That is the single semantic difference and it is
//!   recorded in the tracker (§9.4).
//!
//! Everything the tools *do* (limits, ACL ordering, selection, projection, fail-closed codes) is
//! delegated to the already ported pure modules.

use std::sync::Arc;

use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::application::KnowledgeDocument;
use crate::jev::call::Caller;
use crate::jev::code_review::{JevCodeReviewer, ReviewReport};
use crate::jev::evidence::tool::{evidence_result, EvidenceResult, JevEvidenceTool};
use crate::jev::evidence::{JevEvidenceProcessor, JevPassage};
use crate::jev::execution::{Decision, Mode};
use crate::jev::integration::knowledge::{knowledge_passages, JevKnowledge};
use crate::jev::review::JevReviewInput;
use crate::jev::runtime::review_driver::ReviewDriver;

/// The evidence tool name (`@Tool(name = "search_evidence")`).
pub const EVIDENCE_TOOL_NAME: &str = "search_evidence";
/// The knowledge tool name (`@Tool(name = "search_knowledge")`).
pub const KNOWLEDGE_TOOL_NAME: &str = "search_knowledge";
/// The code-review tool name (`@Tool(name = "review_code_snapshot")`).
pub const CODE_REVIEW_TOOL_NAME: &str = "review_code_snapshot";

/// The caller identity a tool call runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolScope {
    /// The authenticated user id.
    pub user_id: String,
    /// The session id.
    pub session_id: String,
}

/// Reads the caller identity from a tool call's runtime map.
///
/// Accepts both spellings core writes (`userId`/`sessionId`) and the `snake_case` alias the
/// harness' own tools tolerate; a missing or blank id yields `None` so every tool fails closed.
pub fn tool_scope(runtime: &JsonMap) -> Option<ToolScope> {
    fn pick(runtime: &JsonMap, camel: &str, snake: &str) -> Option<String> {
        runtime
            .get(camel)
            .or_else(|| runtime.get(snake))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }
    Some(ToolScope {
        user_id: pick(runtime, "userId", "user_id")?,
        session_id: pick(runtime, "sessionId", "session_id")?,
    })
}

fn tool_error(error: JevError) -> Error {
    Error::Tool(error.message().to_string())
}

fn string_arg(args: &JsonMap, key: &str) -> CoreResult<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Tool(format!("invalid tool arguments: {key} must be a string")))
}

fn scope_or(code: &str, runtime: &JsonMap) -> CoreResult<ToolScope> {
    tool_scope(runtime).ok_or_else(|| Error::Tool(code.to_string()))
}

fn result_json(value: &Value) -> CoreResult<ToolResultBlock> {
    Ok(ToolResultBlock::text(serde_json::to_string(value)?))
}

fn evidence_json(result: &EvidenceResult) -> Value {
    let passages: Vec<Value> = result
        .passages
        .iter()
        .map(|passage| {
            json!({
                "id": passage.id,
                "version": passage.version,
                "text": passage.text,
                "classification": passage.classification,
                "score": passage.score,
            })
        })
        .collect();
    json!({ "status": result.status, "passages": passages })
}

/// The evidence lookup the tool performs (`JevEvidenceTool.Source`).
#[async_trait]
pub trait EvidenceSource: Send + Sync {
    /// Returns up to `request.candidate_limit` passages for the query.
    async fn search(&self, request: &EvidenceRequest) -> Result<Vec<JevPassage>, JevError>;

    /// Whether this caller may see `passage` (`Source.authorized`).
    fn authorized(&self, scope: &ToolScope, passage: &JevPassage) -> bool;
}

/// What the host is asked for (`JevEvidenceTool.Request`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRequest {
    /// The caller.
    pub scope: ToolScope,
    /// The question to retrieve evidence for.
    pub query: String,
    /// The candidate limit the tool was configured with.
    pub candidate_limit: usize,
}

/// `search_evidence`: authorized evidence only, never filtered passages or ACL metadata.
pub struct EvidenceSearchTool {
    processor: Arc<JevEvidenceProcessor>,
    source: Arc<dyn EvidenceSource>,
    spec: JevEvidenceTool,
}

impl EvidenceSearchTool {
    /// Creates the tool; `1 <= top_k <= candidate_limit <= 1024`.
    pub fn new(
        processor: Arc<JevEvidenceProcessor>,
        source: Arc<dyn EvidenceSource>,
        candidate_limit: usize,
        top_k: usize,
    ) -> Result<Self, JevError> {
        Ok(Self {
            processor,
            source,
            spec: JevEvidenceTool::new(candidate_limit, top_k)?,
        })
    }

    /// The pure projection spec (limits and call validation).
    pub fn spec(&self) -> &JevEvidenceTool {
        &self.spec
    }
}

#[async_trait]
impl Tool for EvidenceSearchTool {
    fn name(&self) -> &str {
        EVIDENCE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Search authorized evidence. Cite each passage id and version. CONFLICTING evidence \
         challenges the query premise and must not be treated as supporting it. Empty or \
         unavailable evidence is not permission to invent an answer."
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
                    "description": "Question to retrieve evidence for"
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> CoreResult<ToolResultBlock> {
        let query = string_arg(&args, "query")?;
        let scope = scope_or("EVIDENCE_SCOPE_REQUIRED", &ctx.runtime)?;
        self.spec.validate_call(&query, true).map_err(tool_error)?;
        let candidates = self
            .source
            .search(&EvidenceRequest {
                scope: scope.clone(),
                query: query.clone(),
                candidate_limit: self.spec.candidate_limit(),
            })
            .await
            .map_err(tool_error)?;
        let authorized = |passage: &JevPassage| self.source.authorized(&scope, passage);
        let decision = self
            .processor
            .process(&query, candidates, &authorized, self.spec.top_k())
            .await
            .map_err(tool_error)?;
        let report = decision
            .value
            .ok_or_else(|| Error::Tool("EVIDENCE_SOURCE_UNAVAILABLE".to_string()))?;
        result_json(&evidence_json(&evidence_result(&report)))
    }
}

/// `search_knowledge`: JEV-decorated retrieval, empty means no supporting evidence.
pub struct KnowledgeSearchTool {
    knowledge: Arc<JevKnowledge>,
    acl: Arc<dyn Fn(&ToolScope, &KnowledgeDocument) -> bool + Send + Sync>,
    top_k: usize,
}

impl KnowledgeSearchTool {
    /// Creates the tool; `1 <= top_k <= 1024` (Java `RetrieveConfig.limit`).
    pub fn new(
        knowledge: Arc<JevKnowledge>,
        acl: Arc<dyn Fn(&ToolScope, &KnowledgeDocument) -> bool + Send + Sync>,
        top_k: usize,
    ) -> Result<Self, JevError> {
        if !(1..=1024).contains(&top_k) {
            return Err(JevError::invalid("candidateLimit must be 1..1024"));
        }
        Ok(Self {
            knowledge,
            acl,
            top_k,
        })
    }
}

#[async_trait]
impl Tool for KnowledgeSearchTool {
    fn name(&self) -> &str {
        KNOWLEDGE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Retrieve authorized evidence passages; an empty result means no supporting evidence \
         was found."
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
                    "description": "Search question"
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> CoreResult<ToolResultBlock> {
        let query = string_arg(&args, "query")?;
        let scope = scope_or("KNOWLEDGE_SCOPE_REQUIRED", &ctx.runtime)?;
        let acl = |document: &KnowledgeDocument| (self.acl)(&scope, document);
        let documents = self
            .knowledge
            .retrieve(&query, self.top_k, &acl)
            .await
            .map_err(tool_error)?;
        let passages: Vec<Value> = knowledge_passages(&documents)
            .into_iter()
            .map(|passage| json!({ "id": passage.id, "text": passage.text }))
            .collect();
        result_json(&Value::Array(passages))
    }
}

/// Loads a host-authorized snapshot (`JevCodeReviewTool.Source`).
///
/// The snapshot id is never a filesystem path: the host resolves it and returns the already
/// loaded [`JevReviewInput`].
#[async_trait]
pub trait CodeReviewSource: Send + Sync {
    /// Resolves `snapshot_id` for this caller, or fails closed.
    async fn load(
        &self,
        scope: &ToolScope,
        snapshot_id: &str,
    ) -> Result<JevReviewInput, JevError>;
}

/// Runs the staged review over a loaded snapshot (`JevCodeReviewer.review`).
///
/// The driver is a trait so a host can substitute its own pipeline; the crate's own driver is
/// [`JevCodeReviewerDriver`], which runs the six stages with
/// [`ReviewDriver`](crate::jev::runtime::review_driver::ReviewDriver).
#[async_trait]
pub trait CodeReviewDriver: Send + Sync {
    /// Reviews a validated snapshot.
    async fn review(&self, input: JevReviewInput) -> Result<Decision<ReviewReport>, JevError>;
}

/// The crate's own driver: delegates to the ported [`JevCodeReviewer`].
///
/// [`JevCodeReviewerDriver::new`] has no System One caller and therefore stays fail-closed with
/// [`CODE_REVIEWER_UNAVAILABLE`](crate::jev::code_review::CODE_REVIEWER_UNAVAILABLE);
/// [`JevCodeReviewerDriver::with_caller`] wires the staged driver.
pub struct JevCodeReviewerDriver {
    reviewer: Arc<JevCodeReviewer>,
    staged: Option<ReviewDriver>,
}

impl JevCodeReviewerDriver {
    /// Wraps a configured reviewer without a System One caller (fail-closed).
    pub fn new(reviewer: Arc<JevCodeReviewer>) -> Self {
        Self {
            reviewer,
            staged: None,
        }
    }

    /// Wraps a configured reviewer and runs the staged driver over `caller`.
    ///
    /// Fails closed for [`Mode::Enforce`](crate::jev::execution::Mode::Enforce), because a review
    /// is advice and never an approval.
    pub fn with_caller(
        reviewer: Arc<JevCodeReviewer>,
        caller: Caller,
        mode: Mode,
    ) -> Result<Self, JevError> {
        let staged = ReviewDriver::from_reviewer(&reviewer, caller, mode)?;
        Ok(Self {
            reviewer,
            staged: Some(staged),
        })
    }

    /// The wrapped reviewer definition.
    pub fn reviewer(&self) -> &Arc<JevCodeReviewer> {
        &self.reviewer
    }

    /// Whether the staged pipeline is wired (rather than fail-closed).
    pub fn is_staged(&self) -> bool {
        self.staged.is_some()
    }
}

#[async_trait]
impl CodeReviewDriver for JevCodeReviewerDriver {
    async fn review(&self, input: JevReviewInput) -> Result<Decision<ReviewReport>, JevError> {
        match &self.staged {
            Some(driver) => driver.review(input).await,
            None => {
                // Fail-closed: without a caller nothing can be reviewed, and silence is never an
                // approval.
                let report = self.reviewer.review()?;
                Ok(Decision::decided(report))
            }
        }
    }
}

/// The maximum snapshot-id length Java accepts (`512`).
pub const MAX_SNAPSHOT_ID_CHARS: usize = 512;

/// `review_code_snapshot`: evidence-bound suggestions, never an approval.
pub struct ReviewCodeTool {
    source: Arc<dyn CodeReviewSource>,
    driver: Arc<dyn CodeReviewDriver>,
}

impl ReviewCodeTool {
    /// Creates the tool over a host snapshot source and a review driver.
    pub fn new(source: Arc<dyn CodeReviewSource>, driver: Arc<dyn CodeReviewDriver>) -> Self {
        Self { source, driver }
    }
}

/// Validates a snapshot id (`JevCodeReviewTool.review` guards).
pub fn validate_snapshot_id(snapshot_id: &str) -> Result<(), JevError> {
    if snapshot_id.trim().is_empty()
        || snapshot_id.chars().count() > MAX_SNAPSHOT_ID_CHARS
        || snapshot_id.chars().any(|c| c.is_control())
    {
        return Err(JevError::invalid("INVALID_SNAPSHOT_ID"));
    }
    Ok(())
}

#[async_trait]
impl Tool for ReviewCodeTool {
    fn name(&self) -> &str {
        CODE_REVIEW_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Review a host-authorized code snapshot. Returns evidence-bound review suggestions, \
         coverage gaps and errors, never approval or proof that code is defect-free. Does not \
         edit or publish."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "snapshotId": {
                    "type": "string",
                    "description": "Identifier of an authorized snapshot, not a filesystem path"
                }
            },
            "required": ["snapshotId"]
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> CoreResult<ToolResultBlock> {
        let snapshot_id = string_arg(&args, "snapshotId")?;
        let scope = scope_or("REVIEW_SCOPE_REQUIRED", &ctx.runtime)?;
        validate_snapshot_id(&snapshot_id).map_err(tool_error)?;
        let input = self
            .source
            .load(&scope, &snapshot_id)
            .await
            .map_err(tool_error)?;
        let decision = self.driver.review(input).await.map_err(tool_error)?;
        let mut value = json!({
            "status": format!("{:?}", decision.status).to_uppercase(),
            "reason": decision.reason,
            "recommendation": decision.recommendation,
        });
        if let Some(report) = decision.value {
            value["report"] = serde_json::to_value(&report)?;
        }
        result_json(&value)
    }
}

/// Everything [`register_jev_tools`] needs.
pub struct JevToolConfig {
    /// The evidence processor and its source (candidate limit / top-k come from the tool).
    pub evidence: Option<EvidenceSearchTool>,
    /// The knowledge tool.
    pub knowledge: Option<KnowledgeSearchTool>,
    /// The code-review tool.
    pub code_review: Option<ReviewCodeTool>,
}

impl JevToolConfig {
    /// An empty configuration.
    pub fn new() -> Self {
        Self {
            evidence: None,
            knowledge: None,
            code_review: None,
        }
    }

    /// Adds the evidence tool.
    pub fn with_evidence(mut self, tool: EvidenceSearchTool) -> Self {
        self.evidence = Some(tool);
        self
    }

    /// Adds the knowledge tool.
    pub fn with_knowledge(mut self, tool: KnowledgeSearchTool) -> Self {
        self.knowledge = Some(tool);
        self
    }

    /// Adds the code-review tool.
    pub fn with_code_review(mut self, tool: ReviewCodeTool) -> Self {
        self.code_review = Some(tool);
        self
    }
}

impl Default for JevToolConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Registers every configured JEV tool on `toolkit`.
///
/// Java registers each tool individually with `Toolkit.registerTool`; the returned names let the
/// host report what was wired without inspecting the toolkit.
pub fn register_jev_tools(toolkit: &Toolkit, config: JevToolConfig) -> Vec<String> {
    let mut registered = Vec::new();
    if let Some(tool) = config.evidence {
        registered.push(tool.name().to_string());
        toolkit.register_tool(Arc::new(tool));
    }
    if let Some(tool) = config.knowledge {
        registered.push(tool.name().to_string());
        toolkit.register_tool(Arc::new(tool));
    }
    if let Some(tool) = config.code_review {
        registered.push(tool.name().to_string());
        toolkit.register_tool(Arc::new(tool));
    }
    registered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::code_review::ReviewConfig;
    use crate::jev::evidence::processor::{Limits, Policy};
    use crate::jev::execution::Options;
    use crate::jev::review::ReviewMode;
    use std::sync::Mutex;

    fn runtime(user: Option<&str>, session: Option<&str>) -> JsonMap {
        let mut map = JsonMap::new();
        if let Some(user) = user {
            map.insert("userId".into(), Value::String(user.into()));
        }
        if let Some(session) = session {
            map.insert("sessionId".into(), Value::String(session.into()));
        }
        map
    }

    fn ctx(user: Option<&str>, session: Option<&str>) -> ToolExecutionContext {
        ToolExecutionContext {
            runtime: runtime(user, session),
            ..Default::default()
        }
    }

    fn query_args(query: &str) -> JsonMap {
        let mut args = JsonMap::new();
        args.insert("query".into(), Value::String(query.into()));
        args
    }

    struct FixedSource {
        passages: Vec<JevPassage>,
        denied: Option<String>,
    }

    #[async_trait]
    impl EvidenceSource for FixedSource {
        async fn search(&self, _request: &EvidenceRequest) -> Result<Vec<JevPassage>, JevError> {
            Ok(self.passages.clone())
        }

        fn authorized(&self, _scope: &ToolScope, passage: &JevPassage) -> bool {
            self.denied.as_deref() != Some(passage.id.as_str())
        }
    }

    fn evidence_tool(denied: Option<&str>) -> EvidenceSearchTool {
        let source = FixedSource {
            passages: vec![
                JevPassage::plain("p1", "first", "v1").unwrap(),
                JevPassage::plain("p2", "second", "v2").unwrap(),
            ],
            denied: denied.map(str::to_string),
        };
        // `OFF` never reaches the caller: the processor still retrieves and applies the ACL.
        let caller: crate::jev::call::Caller = Arc::new(|_request| {
            Box::pin(async { Err(crate::jev::call::CallFailure::backend("unused")) })
        });
        let processor = JevEvidenceProcessor::new(
            caller,
            Policy::demonstration(),
            Limits::defaults(),
            0.0,
            Options::disabled(),
        )
        .unwrap();
        EvidenceSearchTool::new(Arc::new(processor), Arc::new(source), 64, 2).unwrap()
    }

    #[test]
    fn schemas_match_the_java_tool_parameters() {
        let evidence = evidence_tool(None);
        assert_eq!(evidence.name(), "search_evidence");
        assert!(evidence.is_read_only());
        assert_eq!(
            evidence.parameters_schema(),
            json!({
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Question to retrieve evidence for"}},
                "required": ["query"]
            })
        );
        assert!(matches!(
            EvidenceSearchTool::new(
                Arc::new(
                    JevEvidenceProcessor::new(
                        Arc::new(|_request| Box::pin(async { Err(crate::jev::call::CallFailure::backend("unused")) })),
                        Policy::demonstration(),
                        Limits::defaults(),
                        0.0,
                        Options::disabled(),
                    )
                    .unwrap()
                ),
                Arc::new(FixedSource {
                    passages: vec![],
                    denied: None
                }),
                2,
                3
            ),
            Err(_)
        ));
    }

    #[test]
    fn tool_scope_requires_both_ids() {
        assert!(tool_scope(&runtime(Some("u"), Some("s"))).is_some());
        assert!(tool_scope(&runtime(None, Some("s"))).is_none());
        assert!(tool_scope(&runtime(Some("u"), Some(" "))).is_none());
        let mut snake = JsonMap::new();
        snake.insert("user_id".into(), Value::String("u".into()));
        snake.insert("session_id".into(), Value::String("s".into()));
        assert_eq!(
            tool_scope(&snake),
            Some(ToolScope {
                user_id: "u".into(),
                session_id: "s".into()
            })
        );
    }

    #[tokio::test]
    async fn evidence_tool_returns_delivered_passages_only() {
        let tool = evidence_tool(Some("p2"));
        let block = tool
            .call(query_args("why?"), ctx(Some("u"), Some("s")))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&block.text_content()).unwrap();
        assert_eq!(value["status"], json!("RETRIEVED"));
        assert_eq!(value["passages"].as_array().unwrap().len(), 1);
        assert_eq!(value["passages"][0]["id"], json!("p1"));
        assert_eq!(value["passages"][0]["version"], json!("v1"));
        assert_eq!(value["passages"][0]["text"], json!("first"));
    }

    #[tokio::test]
    async fn evidence_tool_fails_closed_without_scope_or_query() {
        let tool = evidence_tool(None);
        assert_eq!(
            tool.call(query_args("why?"), ctx(None, Some("s")))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: EVIDENCE_SCOPE_REQUIRED"
        );
        assert_eq!(
            tool.call(query_args("why?"), ctx(Some("u"), None))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: EVIDENCE_SCOPE_REQUIRED"
        );
        assert_eq!(
            tool.call(JsonMap::new(), ctx(Some("u"), Some("s")))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: invalid tool arguments: query must be a string"
        );
        assert_eq!(
            tool.call(query_args("  "), ctx(Some("u"), Some("s")))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: EVIDENCE_SCOPE_REQUIRED"
        );
    }

    struct FixedKnowledge {
        documents: Mutex<Vec<KnowledgeDocument>>,
    }

    #[async_trait]
    impl crate::jev::integration::knowledge::KnowledgeSource for FixedKnowledge {
        async fn add_documents(
            &self,
            documents: Vec<KnowledgeDocument>,
        ) -> Result<(), JevError> {
            self.documents.lock().unwrap().extend(documents);
            Ok(())
        }

        async fn retrieve(
            &self,
            _query: &str,
            limit: usize,
        ) -> Result<Vec<KnowledgeDocument>, JevError> {
            Ok(self
                .documents
                .lock()
                .unwrap()
                .iter()
                .take(limit)
                .cloned()
                .collect())
        }
    }

    fn knowledge_tool() -> KnowledgeSearchTool {
        let source = Arc::new(FixedKnowledge {
            documents: Mutex::new(vec![
                KnowledgeDocument::new("d1", "alpha", "v1").unwrap(),
                KnowledgeDocument::new("d2", "beta", "v1").unwrap(),
            ]),
        });
        let caller: crate::jev::call::Caller = Arc::new(|_request| {
            Box::pin(async { Err(crate::jev::call::CallFailure::backend("unused")) })
        });
        let selector = Arc::new(
            crate::jev::application::JevCandidateSelector::new(caller, Options::disabled(), 0.3, 0.7)
                .unwrap(),
        );
        let knowledge = Arc::new(
            JevKnowledge::new(source, selector, Options::disabled(), 16).unwrap(),
        );
        KnowledgeSearchTool::new(
            knowledge,
            Arc::new(|_scope: &ToolScope, document: &KnowledgeDocument| document.id != "d2"),
            16,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn knowledge_tool_projects_id_and_text_after_the_acl() {
        let tool = knowledge_tool();
        assert_eq!(tool.name(), "search_knowledge");
        assert_eq!(
            tool.parameters_schema(),
            json!({
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Search question"}},
                "required": ["query"]
            })
        );
        let block = tool
            .call(query_args("alpha"), ctx(Some("u"), Some("s")))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&block.text_content()).unwrap();
        assert_eq!(value, json!([{ "id": "d1", "text": "alpha" }]));
        assert_eq!(
            tool.call(query_args("alpha"), ctx(Some("u"), None))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: KNOWLEDGE_SCOPE_REQUIRED"
        );
    }

    struct FixedReviewSource;

    #[async_trait]
    impl CodeReviewSource for FixedReviewSource {
        async fn load(
            &self,
            _scope: &ToolScope,
            snapshot_id: &str,
        ) -> Result<JevReviewInput, JevError> {
            if snapshot_id != "snap-1" {
                return Err(JevError::invalid("SNAPSHOT_NOT_FOUND"));
            }
            JevReviewInput::new(
                "rev-1",
                ReviewMode::Changes,
                vec![crate::jev::review::ReviewFile {
                    path: "src/lib.rs".to_string(),
                    text: "fn f() {}".to_string(),
                    related_tests: vec![],
                }],
                vec![],
            )
        }
    }

    struct FailClosedDriver;

    #[async_trait]
    impl CodeReviewDriver for FailClosedDriver {
        async fn review(
            &self,
            _input: JevReviewInput,
        ) -> Result<Decision<ReviewReport>, JevError> {
            Err(JevError::protocol("CODE_REVIEWER_UNAVAILABLE"))
        }
    }

    #[tokio::test]
    async fn review_tool_validates_scope_and_snapshot_id() {
        let tool = ReviewCodeTool::new(Arc::new(FixedReviewSource), Arc::new(FailClosedDriver));
        assert_eq!(tool.name(), "review_code_snapshot");
        assert_eq!(
            tool.parameters_schema()["properties"]["snapshotId"]["type"],
            json!("string")
        );
        let mut args = JsonMap::new();
        args.insert("snapshotId".into(), Value::String("snap-1".into()));
        assert_eq!(
            tool.call(args.clone(), ctx(None, Some("s")))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: REVIEW_SCOPE_REQUIRED"
        );
        let mut blank = JsonMap::new();
        blank.insert("snapshotId".into(), Value::String("  ".into()));
        assert_eq!(
            tool.call(blank, ctx(Some("u"), Some("s")))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: INVALID_SNAPSHOT_ID"
        );
        let mut long = JsonMap::new();
        long.insert("snapshotId".into(), Value::String("x".repeat(513)));
        assert_eq!(
            tool.call(long, ctx(Some("u"), Some("s")))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: INVALID_SNAPSHOT_ID"
        );
        let mut missing = JsonMap::new();
        missing.insert("snapshotId".into(), Value::String("other".into()));
        assert_eq!(
            tool.call(missing, ctx(Some("u"), Some("s")))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: SNAPSHOT_NOT_FOUND"
        );
        // The default reviewer is still fail-closed rather than silently approving.
        assert_eq!(
            tool.call(args, ctx(Some("u"), Some("s")))
                .await
                .unwrap_err()
                .to_string(),
            "Tool error: CODE_REVIEWER_UNAVAILABLE"
        );
    }

    #[tokio::test]
    async fn register_jev_tools_wires_every_configured_tool() {
        let toolkit = Toolkit::new();
        let names = register_jev_tools(
            &toolkit,
            JevToolConfig::new()
                .with_evidence(evidence_tool(None))
                .with_knowledge(knowledge_tool())
                .with_code_review(ReviewCodeTool::new(
                    Arc::new(FixedReviewSource),
                    Arc::new(FailClosedDriver),
                )),
        );
        assert_eq!(
            names,
            vec![
                "search_evidence".to_string(),
                "search_knowledge".to_string(),
                "review_code_snapshot".to_string()
            ]
        );
        let mut registered = toolkit.tool_names();
        registered.sort();
        let mut expected = names;
        expected.sort();
        assert_eq!(registered, expected);
        assert!(toolkit.get_tool("search_evidence").is_some());
        assert!(toolkit.get_tool("search_knowledge").is_some());
        assert!(toolkit.get_tool("review_code_snapshot").is_some());
    }

    #[tokio::test]
    async fn reviewer_driver_is_fail_closed_until_the_staged_driver_lands() {
        let reviewer = Arc::new(JevCodeReviewer::new(ReviewConfig::reference_policy()).unwrap());
        let driver = JevCodeReviewerDriver::new(reviewer);
        let scope = ToolScope {
            user_id: "u".into(),
            session_id: "s".into(),
        };
        let input = FixedReviewSource.load(&scope, "snap-1").await.unwrap();
        let error = driver.review(input).await.unwrap_err();
        assert_eq!(
            error.message(),
            crate::jev::code_review::CODE_REVIEWER_UNAVAILABLE
        );
    }
}
