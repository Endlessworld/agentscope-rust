//! The thin, explicitly-invoked application workflows.
//!
//! Each type wraps [`super::selector::JevCandidateSelector`] and/or [`crate::jev::judge::JevJudge`]
//! and adds only deterministic policy: validating the input, choosing what an inconclusive verdict
//! means, and shaping an advice-only report. None of them dispatches, executes, persists or sends.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::dto::NoulQuestion;
use crate::jev::execution::{Decision, Status};
use crate::jev::judge::{Criterion, Definition, JevJudge, JudgeResult, JudgeStatus};

use super::selector::{JevCandidateSelector, Selection};

/// A browser interaction a planner may propose (`JevBrowserPlanner.Operation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserOperation {
    /// Read the visible page.
    Read,
    /// Scroll the page.
    Scroll,
    /// Wait for the page to change.
    Wait,
    /// The goal is complete.
    Done,
    /// The page blocks the goal.
    Blocked,
}

impl BrowserOperation {
    /// The Java enum name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "READ",
            Self::Scroll => "SCROLL",
            Self::Wait => "WAIT",
            Self::Done => "DONE",
            Self::Blocked => "BLOCKED",
        }
    }
}

/// One observed browser action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserAction {
    /// The action id.
    pub id: String,
    /// What the action does.
    pub operation: BrowserOperation,
    /// The human-visible target text.
    pub visible_target: String,
}

impl BrowserAction {
    /// Creates and validates an action.
    pub fn new(
        id: impl Into<String>,
        operation: BrowserOperation,
        visible_target: impl Into<String>,
    ) -> Result<Self, JevError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(JevError::invalid("invalid action"));
        }
        Ok(Self {
            id,
            operation,
            visible_target: visible_target.into(),
        })
    }
}

/// The planner's advice for the next step.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserProposal {
    /// The page version the proposal was made for.
    pub page_version: String,
    /// The proposed action, absent when the planner abstained.
    pub action: Option<BrowserAction>,
    /// Why the proposal has this shape.
    pub reason: String,
}

impl BrowserProposal {
    /// Whether the proposal still applies to `current_version` (`Proposal.validFor`).
    pub fn is_valid_for(&self, current_version: &str) -> bool {
        self.action.is_some() && self.page_version == current_version
    }
}

/// Bounded, read-only browser experiment (`JevBrowserPlanner`).
#[derive(Debug)]
pub struct JevBrowserPlanner {
    selector: Arc<JevCandidateSelector>,
}

impl JevBrowserPlanner {
    /// Creates a planner over a selector.
    pub fn new(selector: Arc<JevCandidateSelector>) -> Self {
        Self { selector }
    }

    /// Proposes one action.
    ///
    /// Java reads the current page version through a `Supplier`; the port takes the value, so a
    /// caller that needs liveness re-reads it before calling.
    pub async fn propose(
        &self,
        goal: &str,
        page_version: &str,
        observed: Vec<BrowserAction>,
        current_version: &str,
        independently_complete: bool,
    ) -> Result<BrowserProposal, JevError> {
        if page_version.trim().is_empty() {
            return Err(JevError::invalid("page version required"));
        }
        let mut candidates: BTreeMap<String, Value> = BTreeMap::new();
        for action in &observed {
            let key = action.id.clone();
            if candidates
                .insert(key, json!({ "operation": action.operation.as_str(), "target": action.visible_target }))
                .is_some()
            {
                return Err(JevError::invalid("duplicate action"));
            }
        }
        let actions: BTreeMap<&str, &BrowserAction> = observed
            .iter()
            .map(|action| (action.id.as_str(), action))
            .collect();
        let decision = self
            .selector
            .select(
                json!({ "goal": goal, "pageVersion": page_version }),
                "Is this observed action appropriate as the next step towards the goal?",
                candidates,
            )
            .await?;
        if page_version != current_version {
            return Ok(BrowserProposal {
                page_version: page_version.to_string(),
                action: None,
                reason: "STALE_PAGE".to_string(),
            });
        }
        let Some(selection) = decision
            .value
            .as_ref()
            .filter(|_| decision.status == Status::Decided)
        else {
            return Ok(BrowserProposal {
                page_version: page_version.to_string(),
                action: None,
                reason: "ABSTAIN".to_string(),
            });
        };
        if selection.selected.is_empty() || !selection.uncertain.is_empty() {
            return Ok(BrowserProposal {
                page_version: page_version.to_string(),
                action: None,
                reason: "ABSTAIN".to_string(),
            });
        }
        let Some(action) = actions.get(selection.selected[0].as_str()) else {
            return Ok(BrowserProposal {
                page_version: page_version.to_string(),
                action: None,
                reason: "ABSTAIN".to_string(),
            });
        };
        if action.operation == BrowserOperation::Done && !independently_complete {
            return Ok(BrowserProposal {
                page_version: page_version.to_string(),
                action: None,
                reason: "COMPLETION_NOT_VERIFIED".to_string(),
            });
        }
        Ok(BrowserProposal {
            page_version: page_version.to_string(),
            action: Some((*action).clone()),
            reason: "PROPOSAL_ONLY".to_string(),
        })
    }
}

/// One call/result exchange in a context plan.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextExchange {
    /// The exchange id.
    pub id: String,
    /// The tool call.
    pub call: Value,
    /// The tool result.
    pub result: Value,
    /// Whether the host pinned the exchange.
    pub pinned: bool,
}

impl ContextExchange {
    /// Creates and validates an exchange (`Exchange` compact constructor).
    pub fn new(
        id: impl Into<String>,
        call: Value,
        result: Value,
        pinned: bool,
    ) -> Result<Self, JevError> {
        let id = id.into();
        if id.trim().is_empty() || call.is_null() || result.is_null() {
            return Err(JevError::invalid("complete exchange required"));
        }
        Ok(Self {
            id,
            call,
            result,
            pinned,
        })
    }
}

/// A reversible offloading proposal (`JevContextPlanner.Plan`).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPlan {
    /// The exchanges that stay in the active context.
    pub retained: Vec<ContextExchange>,
    /// The exchanges the host may archive.
    pub archive: BTreeMap<String, ContextExchange>,
    /// The original list, for restoring.
    pub original: Vec<ContextExchange>,
    /// Why the plan has this shape.
    pub reason: String,
}

impl ContextPlan {
    /// The original exchanges (`Plan.restore`).
    pub fn restore(&self) -> Vec<ContextExchange> {
        self.original.clone()
    }
}

/// Proposes reversible pair-level offloading (`JevContextPlanner`).
#[derive(Debug)]
pub struct JevContextPlanner {
    selector: Arc<JevCandidateSelector>,
}

impl JevContextPlanner {
    /// Creates a planner over a selector.
    pub fn new(selector: Arc<JevCandidateSelector>) -> Self {
        Self { selector }
    }

    /// Plans which exchanges may leave the active context.
    pub async fn plan(
        &self,
        task: Value,
        exchanges: Vec<ContextExchange>,
    ) -> Result<ContextPlan, JevError> {
        let original = exchanges.clone();
        let mut seen = std::collections::BTreeSet::new();
        if exchanges
            .iter()
            .any(|exchange| !seen.insert(exchange.id.clone()))
        {
            return Err(JevError::invalid("duplicate exchange id"));
        }
        let candidates: BTreeMap<String, Value> = exchanges
            .iter()
            .filter(|exchange| !exchange.pinned)
            .map(|exchange| {
                (
                    exchange.id.clone(),
                    json!({ "call": exchange.call, "result": exchange.result }),
                )
            })
            .collect();
        let decision = self
            .selector
            .select(
                task,
                "Must this tool call and its result remain available in active context to complete the task?",
                candidates,
            )
            .await?;
        let Some(selection) = decision
            .value
            .as_ref()
            .filter(|_| decision.status == Status::Decided)
        else {
            return Ok(ContextPlan {
                retained: original.clone(),
                archive: BTreeMap::new(),
                original,
                reason: decision.reason.clone(),
            });
        };
        let mut archive: BTreeMap<String, ContextExchange> = BTreeMap::new();
        for exchange in &original {
            if !exchange.pinned && selection.rejected.contains(&exchange.id) {
                archive.insert(exchange.id.clone(), exchange.clone());
            }
        }
        let retained = original
            .iter()
            .filter(|exchange| !archive.contains_key(&exchange.id))
            .cloned()
            .collect();
        Ok(ContextPlan {
            retained,
            archive,
            original,
            reason: "PROPOSAL_ONLY".to_string(),
        })
    }
}

/// One team member a planner may recommend (`JevTeamPlanner.Member`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamMember {
    /// The member id.
    pub id: String,
    /// The member's capabilities.
    pub capabilities: String,
}

/// Stage-scoped team recommendations (`JevTeamPlanner`); never dispatches.
#[derive(Debug)]
pub struct JevTeamPlanner {
    selector: Arc<JevCandidateSelector>,
}

impl JevTeamPlanner {
    /// Creates a planner over a selector.
    pub fn new(selector: Arc<JevCandidateSelector>) -> Self {
        Self { selector }
    }

    /// Recommends members for an explicit stage.
    pub async fn recommend(
        &self,
        stage: &str,
        task: Value,
        members: Vec<TeamMember>,
        eligible: &dyn Fn(&TeamMember) -> bool,
    ) -> Result<Decision<Selection>, JevError> {
        if stage.trim().is_empty() {
            return Err(JevError::invalid("explicit stage required"));
        }
        let mut candidates: BTreeMap<String, Value> = BTreeMap::new();
        for member in members.iter().filter(|member| eligible(member)) {
            if candidates
                .insert(member.id.clone(), json!(member.capabilities))
                .is_some()
            {
                return Err(JevError::invalid("duplicate member"));
            }
        }
        self.selector
            .select(
                json!({ "stage": stage, "task": task }),
                "Is this member suitable for the explicit current stage and task?",
                candidates,
            )
            .await
    }
}

/// A retrieval document (`JevRag.Document`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeDocument {
    /// The document id.
    pub id: String,
    /// The document text.
    pub text: String,
    /// The document version.
    pub version: String,
}

impl KnowledgeDocument {
    /// Creates and validates a document (`Document` compact constructor).
    pub fn new(
        id: impl Into<String>,
        text: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, JevError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(JevError::invalid("document id/text/version required"));
        }
        Ok(Self {
            id,
            text: text.into(),
            version: version.into(),
        })
    }
}

/// The outcome of an ACL-first retrieval (`JevRag.Retrieval`).
#[derive(Debug, Clone, PartialEq)]
pub struct RagRetrieval {
    /// The decision status the retrieval inherited.
    pub status: Status,
    /// The selected evidence.
    pub evidence: Vec<KnowledgeDocument>,
    /// Documents the classifier could not score.
    pub unscored: Vec<String>,
    /// Why the retrieval has this shape.
    pub reason: String,
}

/// ACL-first application RAG selection (`JevRag`); no dependency on a core Knowledge API.
#[derive(Debug)]
pub struct JevRag {
    selector: Arc<JevCandidateSelector>,
    judge: Arc<JevJudge>,
}

impl JevRag {
    /// Creates the workflow.
    pub fn new(selector: Arc<JevCandidateSelector>, judge: Arc<JevJudge>) -> Self {
        Self { selector, judge }
    }

    /// Selects evidence for `query` after applying the caller's ACL.
    pub async fn retrieve(
        &self,
        query: &str,
        documents: Vec<KnowledgeDocument>,
        authorized: &dyn Fn(&KnowledgeDocument) -> bool,
        limit: usize,
    ) -> Result<RagRetrieval, JevError> {
        if limit < 1 {
            return Err(JevError::invalid("positive limit required"));
        }
        let visible: Vec<KnowledgeDocument> = documents
            .into_iter()
            .filter(|document| authorized(document))
            .collect();
        let mut candidates: BTreeMap<String, Value> = BTreeMap::new();
        for document in &visible {
            if candidates
                .insert(document.id.clone(), json!(document.text))
                .is_some()
            {
                return Err(JevError::invalid("duplicate document id"));
            }
        }
        let decision = self
            .selector
            .select(
                json!({ "query": query }),
                "Does this passage supply relevant evidence for the query?",
                candidates,
            )
            .await?;
        if decision.status != Status::Decided {
            return Ok(RagRetrieval {
                status: decision.status,
                evidence: Vec::new(),
                unscored: visible.iter().map(|document| document.id.clone()).collect(),
                reason: decision.reason,
            });
        }
        let selection = decision.value.unwrap_or(Selection {
            selected: Vec::new(),
            rejected: Vec::new(),
            uncertain: Vec::new(),
            scores: BTreeMap::new(),
        });
        let evidence: Vec<KnowledgeDocument> = selection
            .selected
            .iter()
            .take(limit)
            .filter_map(|id| visible.iter().find(|document| &document.id == id).cloned())
            .collect();
        Ok(RagRetrieval {
            status: decision.status,
            unscored: selection.uncertain.clone(),
            reason: if evidence.is_empty() {
                "NO_EVIDENCE".to_string()
            } else {
                "EVIDENCE_SELECTED".to_string()
            },
            evidence,
        })
    }

    /// The grounding definition (`JevRag.verify`).
    pub fn grounding_definition() -> Result<Definition, JevError> {
        Definition::new(
            "rag-grounding-v1",
            vec![Criterion::new(
                "grounded",
                NoulQuestion::new(
                    json!(
                        "Are all factual claims in the answer supported by the supplied evidence? If evidence is empty answer no."
                    ),
                    None,
                ),
                true,
                0.2,
                0.8,
            )?],
        )
    }

    /// Verifies that `answer` is grounded in `evidence`.
    ///
    /// Empty evidence fails without calling the backend, exactly like Java.
    pub async fn verify(
        &self,
        query: &str,
        answer: &str,
        evidence: Vec<KnowledgeDocument>,
    ) -> Result<JudgeResult, JevError> {
        let definition = Self::grounding_definition()?;
        if evidence.is_empty() {
            let mut findings = BTreeMap::new();
            findings.insert(
                "grounded".to_string(),
                crate::jev::judge::Finding {
                    status: JudgeStatus::Fail,
                    yes_probability: 0.0,
                    expected_probability: 0.0,
                },
            );
            return Ok(JudgeResult {
                definition_version: definition.version,
                status: JudgeStatus::Fail,
                findings,
                error: crate::jev::judge::ErrorCode::None,
                model: None,
                usage: None,
                elapsed: std::time::Duration::ZERO,
            });
        }
        Ok(self
            .judge
            .judge(
                json!({ "query": query, "answer": answer, "evidence": evidence }),
                &definition,
            )
            .await)
    }
}

/// A memory candidate (`JevMemoryGate.Fact`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFact {
    /// The fact owner; every fact in one assessment must share it.
    pub owner: String,
    /// The fact text.
    pub text: String,
    /// Where the fact came from.
    pub source: String,
}

impl MemoryFact {
    /// Creates and validates a fact (`Fact` compact constructor).
    pub fn new(
        owner: impl Into<String>,
        text: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Self, JevError> {
        let owner = owner.into();
        let text = text.into();
        let source = source.into();
        if owner.trim().is_empty() || text.trim().is_empty() || source.trim().is_empty() {
            return Err(JevError::invalid("fact owner/text/source required"));
        }
        Ok(Self {
            owner,
            text,
            source,
        })
    }
}

/// Memory admission advice bound to an explicit owner and source (`JevMemoryGate`).
#[derive(Debug)]
pub struct JevMemoryGate {
    judge: Arc<JevJudge>,
}

impl JevMemoryGate {
    /// Creates the gate.
    pub fn new(judge: Arc<JevJudge>) -> Self {
        Self { judge }
    }

    /// The admission definition (`memory-admission-v1`).
    pub fn definition() -> Result<Definition, JevError> {
        Definition::new(
            "memory-admission-v1",
            vec![
                Criterion::new(
                    "durable",
                    NoulQuestion::new(
                        json!(
                            "Is the candidate a useful durable fact supported by its source rather than an instruction or temporary speculation?"
                        ),
                        None,
                    ),
                    true,
                    0.2,
                    0.8,
                )?,
                Criterion::new(
                    "conflict",
                    NoulQuestion::new(
                        json!(
                            "Does the candidate contradict an existing fact without clear evidence that it supersedes it?"
                        ),
                        None,
                    ),
                    false,
                    0.2,
                    0.8,
                )?,
            ],
        )
    }

    /// Assesses whether `candidate` should be remembered.
    pub async fn assess(
        &self,
        owner: &str,
        candidate: MemoryFact,
        existing: Vec<MemoryFact>,
    ) -> Result<JudgeResult, JevError> {
        if candidate.owner != owner || existing.iter().any(|fact| fact.owner != owner) {
            return Err(JevError::invalid("cross-owner memory input"));
        }
        let definition = Self::definition()?;
        Ok(self
            .judge
            .judge(
                json!({ "candidate": candidate, "existing": existing }),
                &definition,
            )
            .await)
    }
}

/// Read-only supervision advice (`JevSupervisor.Advice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorAdvice {
    /// The worker may continue.
    Continue,
    /// An independent verification is needed.
    RequestVerification,
    /// The completion is ready for review.
    ReviewCompletion,
    /// A human must look at it.
    ManualReview,
}

impl SupervisorAdvice {
    /// The Java enum name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Continue => "CONTINUE",
            Self::RequestVerification => "REQUEST_VERIFICATION",
            Self::ReviewCompletion => "REVIEW_COMPLETION",
            Self::ManualReview => "MANUAL_REVIEW",
        }
    }
}

/// The supervision report.
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorReport {
    /// The advice; never an actuated command.
    pub advice: SupervisorAdvice,
    /// The judge result behind the advice.
    pub assessment: JudgeResult,
}

/// Read-only supervision: emits advice, never interrupts or finishes a worker (`JevSupervisor`).
#[derive(Debug)]
pub struct JevSupervisor {
    judge: Arc<JevJudge>,
}

impl JevSupervisor {
    /// Creates the supervisor.
    pub fn new(judge: Arc<JevJudge>) -> Self {
        Self { judge }
    }

    /// The supervision definition (`supervision-v1`).
    pub fn definition() -> Result<Definition, JevError> {
        Definition::new(
            "supervision-v1",
            vec![
                Criterion::new(
                    "complete",
                    NoulQuestion::new(
                        json!(
                            "Does the supplied evidence establish all task requirements are implemented? Do not treat a worker's unsupported completion claim as evidence."
                        ),
                        None,
                    ),
                    true,
                    0.2,
                    0.8,
                )?,
                Criterion::new(
                    "off_track",
                    NoulQuestion::new(
                        json!(
                            "Is the worker stuck or materially off the requested task? Normal investigation alone is not being stuck."
                        ),
                        None,
                    ),
                    false,
                    0.2,
                    0.8,
                )?,
            ],
        )
    }

    /// Assesses the worker and derives advice from the findings.
    pub async fn assess(
        &self,
        task: Value,
        bounded_evidence: Value,
        verification_succeeded: bool,
    ) -> Result<SupervisorReport, JevError> {
        let definition = Self::definition()?;
        let assessment = self
            .judge
            .judge(
                json!({
                    "task": task,
                    "evidence": bounded_evidence,
                    "verificationSucceeded": verification_succeeded,
                }),
                &definition,
            )
            .await;
        let advice = match assessment.status {
            JudgeStatus::Error | JudgeStatus::Inconclusive => SupervisorAdvice::ManualReview,
            _ => {
                let off_track = assessment
                    .findings
                    .get("off_track")
                    .map(|finding| finding.status)
                    .unwrap_or(JudgeStatus::Error);
                let complete = assessment
                    .findings
                    .get("complete")
                    .map(|finding| finding.status)
                    .unwrap_or(JudgeStatus::Error);
                if off_track != JudgeStatus::Pass {
                    SupervisorAdvice::ManualReview
                } else if complete != JudgeStatus::Pass {
                    SupervisorAdvice::Continue
                } else if verification_succeeded {
                    SupervisorAdvice::ReviewCompletion
                } else {
                    SupervisorAdvice::RequestVerification
                }
            }
        };
        Ok(SupervisorReport { advice, assessment })
    }
}

/// The customer-support report (`JevCustomerSupport.Report`).
#[derive(Debug, Clone, PartialEq)]
pub struct CustomerSupportReport {
    /// The shadow triage.
    pub triage: Decision<Selection>,
    /// The draft review.
    pub review: JudgeResult,
}

/// Explicit shadow triage and draft review (`JevCustomerSupport`); never issues refunds or sends.
#[derive(Debug)]
pub struct JevCustomerSupport {
    selector: Arc<JevCandidateSelector>,
    judge: Arc<JevJudge>,
}

impl JevCustomerSupport {
    /// Creates the workflow.
    pub fn new(selector: Arc<JevCandidateSelector>, judge: Arc<JevJudge>) -> Self {
        Self { selector, judge }
    }

    /// The draft review definition (`support-draft-v1`).
    pub fn definition() -> Result<Definition, JevError> {
        Definition::new(
            "support-draft-v1",
            vec![
                Criterion::new(
                    "covers_request",
                    NoulQuestion::new(
                        json!(
                            "Does the draft address the customer's request and its conditions?"
                        ),
                        None,
                    ),
                    true,
                    0.2,
                    0.8,
                )?,
                Criterion::new(
                    "unsupported_commitment",
                    NoulQuestion::new(
                        json!(
                            "Does the draft claim an action has completed without a successful business record?"
                        ),
                        None,
                    ),
                    false,
                    0.2,
                    0.8,
                )?,
            ],
        )
    }

    /// Triages the request and reviews the draft in one bounded round.
    pub async fn review(
        &self,
        request: &str,
        draft: &str,
        business_record: Value,
    ) -> Result<CustomerSupportReport, JevError> {
        let state = json!({
            "request": request,
            "draft": draft,
            "business_record": business_record,
        });
        let candidates = BTreeMap::from([
            ("orders".to_string(), json!("Order status and shipping")),
            (
                "billing".to_string(),
                json!("Payments and conditional refund requests"),
            ),
            ("technical".to_string(), json!("Technical failures")),
        ]);
        let definition = Self::definition()?;
        let triage_future = self.selector.select(
            state.clone(),
            "Does the customer request require this support queue? Preserve conditional requests as conditional.",
            candidates,
        );
        let review_future = self.judge.judge(state, &definition);
        let (triage, review) = tokio::join!(triage_future, review_future);
        Ok(CustomerSupportReport {
            triage: triage?,
            review,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::dto::{Answer, NoulAnswer, SystemOneResult, Usage};
    use crate::jev::execution::Mode;
    use std::collections::BTreeMap as Map;
    use std::time::Duration;

    fn options() -> crate::jev::execution::Options {
        crate::jev::execution::Options::new(Mode::Shadow, Duration::from_secs(2), "v1").unwrap()
    }

    fn noul_response(values: &[(&str, f64)]) -> SystemOneResult {
        let mut answers = Map::new();
        for (id, value) in values {
            answers.insert(
                (*id).to_string(),
                Answer::Noul(NoulAnswer { noul: Some(*value) }),
            );
        }
        SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(answers),
            usage: Some(Usage::new(1, 1)),
        }
    }

    fn selector(values: &[f64]) -> Arc<JevCandidateSelector> {
        selector_calls(&[values])
    }

    /// One scripted response per `select` call, each with its own answer count.
    fn selector_calls(calls: &[&[f64]]) -> Arc<JevCandidateSelector> {
        let responses: Vec<SystemOneResult> = calls
            .iter()
            .map(|values| {
                let pairs: Vec<(String, f64)> = values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| (format!("item_{index}"), *value))
                    .collect();
                let borrowed: Vec<(&str, f64)> = pairs
                    .iter()
                    .map(|(id, value)| (id.as_str(), *value))
                    .collect();
                noul_response(&borrowed)
            })
            .collect();
        let scripted = Arc::new(ScriptedCaller::new(responses.into_iter().map(Ok).collect()));
        Arc::new(JevCandidateSelector::new(scripted.as_caller(), options(), 0.2, 0.8).unwrap())
    }

    fn judge(answers: Vec<SystemOneResult>) -> Arc<JevJudge> {
        let scripted = Arc::new(ScriptedCaller::new(answers.into_iter().map(Ok).collect()));
        Arc::new(JevJudge::new(scripted.as_caller(), Duration::from_secs(1)).unwrap())
    }

    fn action(id: &str, operation: BrowserOperation) -> BrowserAction {
        BrowserAction::new(id, operation, "target").unwrap()
    }

    #[tokio::test]
    async fn browser_planner_proposes_only_verified_non_done_actions() {
        let planner = JevBrowserPlanner::new(selector(&[0.9, 0.1]));
        let proposal = planner
            .propose(
                "find the price",
                "v1",
                vec![
                    action("a", BrowserOperation::Read),
                    action("b", BrowserOperation::Scroll),
                ],
                "v1",
                false,
            )
            .await
            .unwrap();
        assert_eq!(proposal.reason, "PROPOSAL_ONLY");
        assert_eq!(proposal.action.as_ref().unwrap().id, "a");
        assert!(proposal.is_valid_for("v1"));
        assert!(!proposal.is_valid_for("v2"));

        let stale = planner
            .propose(
                "g",
                "v1",
                vec![action("a", BrowserOperation::Read)],
                "v2",
                false,
            )
            .await
            .unwrap();
        assert_eq!(stale.reason, "STALE_PAGE");
        assert!(stale.action.is_none());

        // A `Done` action needs independent completion evidence.
        let planner = JevBrowserPlanner::new(selector_calls(&[&[0.9], &[0.9]]));
        let unverified = planner
            .propose(
                "g",
                "v1",
                vec![action("a", BrowserOperation::Done)],
                "v1",
                false,
            )
            .await
            .unwrap();
        assert_eq!(unverified.reason, "COMPLETION_NOT_VERIFIED");
        let verified = planner
            .propose(
                "g",
                "v1",
                vec![action("a", BrowserOperation::Done)],
                "v1",
                true,
            )
            .await
            .unwrap();
        assert_eq!(verified.reason, "PROPOSAL_ONLY");

        assert_eq!(
            planner
                .propose("g", " ", vec![], "v1", false)
                .await
                .unwrap_err()
                .message(),
            "page version required"
        );
        assert_eq!(
            planner
                .propose(
                    "g",
                    "v1",
                    vec![
                        action("a", BrowserOperation::Read),
                        action("a", BrowserOperation::Wait)
                    ],
                    "v1",
                    false,
                )
                .await
                .unwrap_err()
                .message(),
            "duplicate action"
        );
        assert_eq!(BrowserOperation::Blocked.as_str(), "BLOCKED");
    }

    #[tokio::test]
    async fn browser_planner_abstains_when_anything_is_uncertain() {
        let planner = JevBrowserPlanner::new(selector(&[0.9, 0.5]));
        let proposal = planner
            .propose(
                "g",
                "v1",
                vec![
                    action("a", BrowserOperation::Read),
                    action("b", BrowserOperation::Read),
                ],
                "v1",
                false,
            )
            .await
            .unwrap();
        assert_eq!(proposal.reason, "ABSTAIN");
        assert!(proposal.action.is_none());
    }

    #[tokio::test]
    async fn context_planner_archives_only_rejected_unpinned_exchanges() {
        let planner = JevContextPlanner::new(selector(&[0.9, 0.1]));
        let exchanges = vec![
            ContextExchange::new("e1", json!({"name": "read"}), json!("body"), false).unwrap(),
            ContextExchange::new("e2", json!({"name": "read"}), json!("body"), false).unwrap(),
        ];
        let plan = planner.plan(json!({"task": "t"}), exchanges).await.unwrap();
        assert_eq!(plan.reason, "PROPOSAL_ONLY");
        assert_eq!(plan.retained.len(), 1);
        assert_eq!(plan.retained[0].id, "e1");
        assert!(plan.archive.contains_key("e2"));
        assert_eq!(plan.restore().len(), 2);

        // Pinned exchanges are never even classified.
        let planner = JevContextPlanner::new(selector(&[0.1]));
        let pinned = ContextExchange::new("e1", json!({}), json!({}), true).unwrap();
        let plan = planner.plan(json!({}), vec![pinned]).await.unwrap();
        assert!(plan.archive.is_empty());
        assert_eq!(plan.retained.len(), 1);

        let duplicated = vec![
            ContextExchange::new("e1", json!({}), json!({}), false).unwrap(),
            ContextExchange::new("e1", json!({}), json!({}), false).unwrap(),
        ];
        assert_eq!(
            planner
                .plan(json!({}), duplicated)
                .await
                .unwrap_err()
                .message(),
            "duplicate exchange id"
        );
        assert_eq!(
            ContextExchange::new("", json!({}), json!({}), false)
                .unwrap_err()
                .message(),
            "complete exchange required"
        );
    }

    #[tokio::test]
    async fn team_planner_filters_by_eligibility_and_rejects_duplicates() {
        let planner = JevTeamPlanner::new(selector(&[0.9, 0.1]));
        let members = vec![
            TeamMember {
                id: "m1".to_string(),
                capabilities: "rust".to_string(),
            },
            TeamMember {
                id: "m2".to_string(),
                capabilities: "docs".to_string(),
            },
        ];
        let decision = planner
            .recommend("build", json!({}), members.clone(), &|member| {
                member.capabilities == "rust" || member.capabilities == "docs"
            })
            .await
            .unwrap();
        assert_eq!(decision.value.unwrap().selected, vec!["m1"]);

        let planner = JevTeamPlanner::new(selector(&[0.9]));
        let decision = planner
            .recommend("build", json!({}), members, &|member| member.id == "m1")
            .await
            .unwrap();
        assert_eq!(decision.value.unwrap().selected, vec!["m1"]);

        let planner = JevTeamPlanner::new(selector(&[0.1]));

        let duplicates = vec![
            TeamMember {
                id: "m1".to_string(),
                capabilities: "a".to_string(),
            },
            TeamMember {
                id: "m1".to_string(),
                capabilities: "b".to_string(),
            },
        ];
        assert_eq!(
            planner
                .recommend("build", json!({}), duplicates, &|_| true)
                .await
                .unwrap_err()
                .message(),
            "duplicate member"
        );
        assert_eq!(
            planner
                .recommend(" ", json!({}), vec![], &|_| true)
                .await
                .unwrap_err()
                .message(),
            "explicit stage required"
        );
    }

    #[tokio::test]
    async fn rag_applies_the_acl_before_classification() {
        let selector = selector(&[0.9]);
        let judge = judge(vec![]);
        let rag = JevRag::new(selector, judge);
        let documents = vec![
            KnowledgeDocument::new("d1", "alpha", "v1").unwrap(),
            KnowledgeDocument::new("d2", "secret", "v1").unwrap(),
        ];
        let retrieval = rag
            .retrieve("q", documents, &|document| document.id != "d2", 10)
            .await
            .unwrap();
        assert_eq!(retrieval.status, Status::Decided);
        assert_eq!(retrieval.evidence.len(), 1);
        assert_eq!(retrieval.evidence[0].id, "d1");
        assert_eq!(retrieval.reason, "EVIDENCE_SELECTED");

        assert_eq!(
            rag.retrieve("q", vec![], &|_| true, 0)
                .await
                .unwrap_err()
                .message(),
            "positive limit required"
        );
        assert_eq!(
            KnowledgeDocument::new(" ", "t", "v").unwrap_err().message(),
            "document id/text/version required"
        );
    }

    #[tokio::test]
    async fn rag_verification_fails_closed_on_empty_evidence() {
        let rag = JevRag::new(selector(&[]), judge(vec![]));
        let result = rag.verify("q", "a", vec![]).await.unwrap();
        assert_eq!(result.status, JudgeStatus::Fail);
        assert_eq!(result.definition_version, "rag-grounding-v1");
        assert_eq!(result.findings["grounded"].status, JudgeStatus::Fail);

        let rag = JevRag::new(
            selector(&[]),
            judge(vec![noul_response(&[("grounded", 0.9)])]),
        );
        let result = rag
            .verify(
                "q",
                "a",
                vec![KnowledgeDocument::new("d1", "t", "v").unwrap()],
            )
            .await
            .unwrap();
        assert_eq!(result.status, JudgeStatus::Pass);
    }

    #[tokio::test]
    async fn memory_gate_rejects_cross_owner_inputs() {
        let gate = JevMemoryGate::new(judge(vec![noul_response(&[
            ("durable", 0.9),
            ("conflict", 0.1),
        ])]));
        let candidate = MemoryFact::new("u1", "likes rust", "chat").unwrap();
        let result = gate.assess("u1", candidate.clone(), vec![]).await.unwrap();
        assert_eq!(result.definition_version, "memory-admission-v1");
        assert_eq!(result.status, JudgeStatus::Pass);

        assert_eq!(
            gate.assess("u2", candidate, vec![])
                .await
                .unwrap_err()
                .message(),
            "cross-owner memory input"
        );
        assert_eq!(
            MemoryFact::new("", "t", "s").unwrap_err().message(),
            "fact owner/text/source required"
        );
    }

    #[tokio::test]
    async fn supervisor_maps_findings_onto_advice() {
        let supervisor = JevSupervisor::new(judge(vec![
            noul_response(&[("complete", 0.9), ("off_track", 0.1)]),
            noul_response(&[("complete", 0.1), ("off_track", 0.1)]),
            noul_response(&[("complete", 0.9), ("off_track", 0.9)]),
            noul_response(&[("complete", 0.5), ("off_track", 0.1)]),
            noul_response(&[("complete", 0.9), ("off_track", 0.1)]),
        ]));
        let report = supervisor.assess(json!({}), json!({}), true).await.unwrap();
        assert_eq!(report.advice, SupervisorAdvice::ReviewCompletion);
        assert_eq!(report.advice.as_str(), "REVIEW_COMPLETION");

        let report = supervisor
            .assess(json!({}), json!({}), false)
            .await
            .unwrap();
        assert_eq!(report.advice, SupervisorAdvice::Continue);

        let report = supervisor.assess(json!({}), json!({}), true).await.unwrap();
        assert_eq!(
            report.advice,
            SupervisorAdvice::ManualReview,
            "off track is manual review"
        );

        let report = supervisor.assess(json!({}), json!({}), true).await.unwrap();
        assert_eq!(
            report.advice,
            SupervisorAdvice::ManualReview,
            "inconclusive is manual review"
        );

        // Verification pending on a complete, on-track job asks for verification.
        let report = supervisor
            .assess(json!({}), json!({}), false)
            .await
            .unwrap();
        assert_eq!(report.advice, SupervisorAdvice::RequestVerification);
    }

    #[tokio::test]
    async fn customer_support_reviews_triage_and_draft_together() {
        let selector = selector(&[0.9, 0.1, 0.1]);
        let judge = judge(vec![noul_response(&[
            ("covers_request", 0.9),
            ("unsupported_commitment", 0.1),
        ])]);
        let support = JevCustomerSupport::new(selector, judge);
        let report = support
            .review("where is my order?", "It ships today.", json!({}))
            .await
            .unwrap();
        assert_eq!(report.triage.status, Status::Decided);
        assert_eq!(report.review.status, JudgeStatus::Pass);
        assert_eq!(report.review.definition_version, "support-draft-v1");
        assert_eq!(JevCustomerSupport::definition().unwrap().criteria.len(), 2);
    }
}
