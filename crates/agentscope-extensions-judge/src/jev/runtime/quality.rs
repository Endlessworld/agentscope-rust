//! The quality / reviser branches of `JevResponseMiddleware`.
//!
//! Java's response middleware has two independent policies over the same model turn:
//!
//! * **safety** (`JevGuardrail` screening of the draft) — ported in
//!   [`crate::jev::runtime::middleware`], which owns the buffering and the withholding.
//! * **quality** (`JevJudge` assessment plus bounded revision) — ported here.
//!
//! The assess loop is deliberately a plain async function over the pure
//! [`quality_state`](crate::jev::integration::response::quality_state) projection, a
//! [`JevJudge`] and an injected [`Reviser`], so every branch is testable offline:
//!
//! ```text
//! assess(draft, 0)
//!   trace issues?                      -> INCONCLUSIVE INCOMPLETE_JUDGING_EVIDENCE
//!   judge state > max_chars?           -> INCONCLUSIVE JUDGE_STATE_LIMIT
//!   judge PASS                         -> DECIDED  (<draft>, QUALITY_PASS)
//!   judge FAIL & ENFORCE & room left   -> reviser -> collect_text -> rescreen -> assess(n+1)
//!   judge FAIL & (SHADOW | no room)    -> INCONCLUSIVE QUALITY_FAIL
//!   judge INCONCLUSIVE / ERROR         -> INCONCLUSIVE/ERROR QUALITY_<STATUS>
//!   revised empty or unchanged         -> INCONCLUSIVE UNCHANGED_DRAFT
//! ```
//!
//! Two seams are injected instead of assumed: the [`Screener`] (Java's `JevGuardrail::screen`
//! System One call) and the [`Reviser`] (Java's revision stream). The default reviser is the
//! faithful `reviseWithModel`: one tool-free call on the model that produced the draft, which is
//! why [`Revision`] carries the model handle.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use agentscope_core::model::{GenerateOptions, Model};
use futures::StreamExt;

use crate::error::JevError;
use crate::jev::call::BoxedFuture;
use crate::jev::evaluation::TraceMessage;
use crate::jev::execution::{Decision, Mode, Status};
use crate::jev::guardrail::{Outcome, Verdict};
use crate::jev::integration::response::{quality_state, ResponseLimits};
use crate::jev::judge::{Definition, JevJudge, JudgeResult, JudgeStatus};

/// The async screening call the host provides (`JevGuardrail::screen`).
///
/// Java's `screen` is a reactive System One call; the injected future keeps the middleware
/// offline-testable while the pure [`JevGuardrail::evaluate`](crate::jev::guardrail::JevGuardrail::evaluate)
/// stays the only verdict fold.
pub type Screener = Arc<dyn Fn(&str) -> BoxedFuture<Result<Verdict, JevError>> + Send + Sync>;

/// What one screening round decided.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenOutcome {
    /// Whether the text must be withheld.
    pub blocked: bool,
    /// Why it was blocked (the Java `Rejected` reason).
    pub reason: String,
}

/// One screening policy (Java's `inputPolicy`/`outputPolicy` + `maxChars` + `blockOnReview`).
#[derive(Clone)]
pub struct ScreenPolicy {
    screener: Screener,
    max_chars: usize,
    block_on_review: bool,
}

impl ScreenPolicy {
    /// Creates a policy over an injected screener.
    pub fn new(screener: Screener, max_chars: usize, block_on_review: bool) -> Self {
        Self {
            screener,
            max_chars,
            block_on_review,
        }
    }

    /// Screens `text` (`JevResponseMiddleware.screen`).
    ///
    /// A blank draft is never screened (Java returns `Mono.empty()`), and an over-long draft fails
    /// with `INPUT_LIMIT` before the policy is consulted.
    pub fn check(&self, text: String) -> BoxedFuture<Result<ScreenOutcome, JevError>> {
        let screener = Arc::clone(&self.screener);
        let max_chars = self.max_chars;
        let block_on_review = self.block_on_review;
        Box::pin(async move {
            if text.trim().is_empty() {
                return Ok(ScreenOutcome {
                    blocked: false,
                    reason: String::new(),
                });
            }
            if text.chars().count() > max_chars {
                return Err(JevError::invalid("INPUT_LIMIT"));
            }
            let verdict = screener(&text).await?;
            Ok(ScreenOutcome {
                blocked: verdict.blocked(block_on_review),
                reason: format!("OUTPUT_{}", outcome_name(verdict.outcome)),
            })
        })
    }
}

/// The Java enum name of an [`Outcome`].
pub fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pass => "PASS",
        Outcome::Review => "REVIEW",
        Outcome::Block => "BLOCK",
        Outcome::Support => "SUPPORT",
    }
}

/// The Java enum name of a [`JudgeStatus`].
pub fn judge_status_name(status: JudgeStatus) -> &'static str {
    match status {
        JudgeStatus::Pass => "PASS",
        JudgeStatus::Fail => "FAIL",
        JudgeStatus::Inconclusive => "INCONCLUSIVE",
        JudgeStatus::Error => "ERROR",
    }
}

/// One revision request (`JevResponseMiddleware.Revision`).
#[derive(Clone)]
pub struct Revision {
    /// The conversation that produced the draft.
    pub messages: Vec<Msg>,
    /// The options the original call was made with.
    pub options: GenerateOptions,
    /// The model that produced the draft (the default reviser calls it directly).
    pub model: Arc<dyn Model>,
    /// The draft to revise.
    pub draft: String,
    /// The failed assessment, so the revision knows which criteria to fix.
    pub review: JudgeResult,
}

impl Revision {
    /// Builds the revision request from the draft's judge result.
    pub fn new(
        messages: Vec<Msg>,
        options: GenerateOptions,
        model: Arc<dyn Model>,
        draft: impl Into<String>,
        review: JudgeResult,
    ) -> Self {
        Self {
            messages,
            options,
            model,
            draft: draft.into(),
            review,
        }
    }

    /// The criterion verdicts Java renders from `review.findings()`.
    pub fn findings(&self) -> String {
        let rendered: Vec<String> = self
            .review
            .findings
            .iter()
            .map(|(id, finding)| format!("{}={}", id, judge_status_name(finding.status)))
            .collect();
        format!("{{{}}}", rendered.join(", "))
    }

    /// The revision instruction Java appends as a user message (`reviseWithModel`).
    pub fn prompt(&self) -> String {
        format!(
            "Revise only the following draft. Do not request or execute tools. Retain claims \
             supported by the conversation. Failed criteria: {}\nDraft:\n{}",
            self.findings(),
            self.draft
        )
    }
}

/// The async revision call the host provides (`JevResponseMiddleware.reviser`).
///
/// Java returns a `Flux<String>`; the port returns the same stream as collected chunks so the
/// caller can apply Java's `BUFFER_LIMIT` accounting before using the text.
pub type Reviser = Arc<dyn Fn(Revision) -> BoxedFuture<Result<Vec<String>, JevError>> + Send + Sync>;

/// The default reviser (`JevResponseMiddleware.reviseWithModel`).
///
/// One call on the model that produced the draft, no tools, and a tool call in the reply is a
/// `REVISION_TOOL_CALL` failure — never re-enters the agent.
pub fn revise_with_model(revision: Revision) -> BoxedFuture<Result<Vec<String>, JevError>> {
    Box::pin(async move {
        let mut messages = revision.messages.clone();
        messages.push(Msg::text(MsgRole::User, revision.prompt()));
        let mut stream = revision
            .model
            .stream(messages, Vec::new(), revision.options.clone());
        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            let response = item.map_err(|error| JevError::protocol(error.to_string()))?;
            for block in response.content {
                match block {
                    ContentBlock::Text(text) => chunks.push(text.text),
                    ContentBlock::ToolUse(_) => {
                        return Err(JevError::protocol("REVISION_TOOL_CALL"))
                    }
                    _ => {}
                }
            }
        }
        Ok(chunks)
    })
}

/// The quality policy (`JevResponseMiddleware.quality(...)`).
#[derive(Clone)]
pub struct QualitySettings {
    /// Whether and how the quality policy participates.
    pub mode: Mode,
    /// The judge that assesses the draft.
    pub judge: Arc<JevJudge>,
    /// The definition (criteria) the judge applies.
    pub definition: Definition,
    /// The revision seam.
    pub reviser: Reviser,
    /// The maximum revision rounds (`0..=10`).
    pub max_revisions: usize,
}

impl std::fmt::Debug for QualitySettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QualitySettings")
            .field("mode", &self.mode)
            .field("definition", &self.definition.version)
            .field("max_revisions", &self.max_revisions)
            .finish_non_exhaustive()
    }
}

impl QualitySettings {
    /// Creates the policy (`invalid response limits` when `max_revisions > 10`).
    pub fn new(
        mode: Mode,
        judge: Arc<JevJudge>,
        definition: Definition,
        reviser: Reviser,
        max_revisions: usize,
    ) -> Result<Self, JevError> {
        if max_revisions > 10 {
            return Err(JevError::invalid("invalid response limits"));
        }
        Ok(Self {
            mode,
            judge,
            definition,
            reviser,
            max_revisions,
        })
    }

    /// Creates the policy with the default (`reviseWithModel`) reviser.
    pub fn with_default_reviser(
        mode: Mode,
        judge: Arc<JevJudge>,
        definition: Definition,
        max_revisions: usize,
    ) -> Result<Self, JevError> {
        Self::new(
            mode,
            judge,
            definition,
            Arc::new(revise_with_model),
            max_revisions,
        )
    }
}

/// One captured model-call stream (`JevResponseMiddleware.Capture`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    /// The concatenated text deltas.
    pub text: String,
    /// Whether a tool call started (a tool-free final draft is required for revision).
    pub tools: bool,
}

/// Folds the captured events exactly like Java's `Capture.add` (`EVENT_LIMIT`/`TEXT_LIMIT`).
pub fn capture(
    events: &[agentscope_core::error::Result<agentscope_core::event::AgentEvent>],
    limits: &ResponseLimits,
) -> Result<Capture, JevError> {
    if events.len() > limits.max_events {
        return Err(JevError::invalid("EVENT_LIMIT"));
    }
    let mut capture = Capture::default();
    for event in events {
        match event {
            Ok(agentscope_core::event::AgentEvent::TextBlockDelta(data)) => {
                if let Some(delta) = data.fields.get("delta").and_then(|value| value.as_str()) {
                    if delta.chars().count()
                        > limits.max_chars.saturating_sub(capture.text.chars().count())
                    {
                        return Err(JevError::invalid("TEXT_LIMIT"));
                    }
                    capture.text.push_str(delta);
                }
            }
            Ok(agentscope_core::event::AgentEvent::ToolCallStart(_)) => capture.tools = true,
            _ => {}
        }
    }
    Ok(capture)
}

/// Collects a revision stream with Java's `BUFFER_LIMIT` accounting (`maxEvents`/`maxChars`).
pub fn collect_text(chunks: Vec<String>, limits: &ResponseLimits) -> Result<String, JevError> {
    if chunks.len() > limits.max_events {
        return Err(JevError::invalid("BUFFER_LIMIT"));
    }
    let mut text = String::new();
    for chunk in chunks {
        if chunk.chars().count() > limits.max_chars.saturating_sub(text.chars().count()) {
            return Err(JevError::invalid("BUFFER_LIMIT"));
        }
        text.push_str(&chunk);
    }
    Ok(text)
}

/// Replaces the draft inside a captured stream (`JevResponseMiddleware.replaceText`).
///
/// The first `TEXT_BLOCK_DELTA` becomes one delta carrying the whole revision and every later
/// delta is dropped; a stream without any text delta is `NO_TEXT_BLOCK`.
pub fn replace_text(
    events: Vec<agentscope_core::error::Result<agentscope_core::event::AgentEvent>>,
    text: &str,
) -> Result<Vec<agentscope_core::error::Result<agentscope_core::event::AgentEvent>>, JevError> {
    use agentscope_core::event::AgentEvent;
    let mut replaced = Vec::with_capacity(events.len());
    let mut emitted = false;
    for event in events {
        match event {
            Ok(AgentEvent::TextBlockDelta(mut data)) => {
                if !emitted {
                    data.fields
                        .insert("delta".to_string(), serde_json::Value::String(text.to_string()));
                    replaced.push(Ok(AgentEvent::TextBlockDelta(data)));
                    emitted = true;
                }
            }
            other => replaced.push(other),
        }
    }
    if !emitted {
        return Err(JevError::invalid("NO_TEXT_BLOCK"));
    }
    Ok(replaced)
}

/// The assess/revision loop (`JevResponseMiddleware.assess` + `review`).
///
/// `Decided` carries the text to use (the draft itself on `QUALITY_PASS`, the revision after a
/// successful round). In `ENFORCE` every other status withholds the response with its reason; in
/// `SHADOW` the caller observes the decision and keeps the draft.
pub async fn assess(
    settings: &QualitySettings,
    input: &agentscope_core::middleware::ModelCallInput,
    messages: &[TraceMessage],
    draft: &str,
    limits: &ResponseLimits,
    policy: &ScreenPolicy,
) -> Decision<String> {
    let mut draft = draft.to_string();
    let mut attempt = 0usize;
    loop {
        let state = match quality_state(messages, &draft) {
            Ok(state) => state,
            Err(error) => {
                return Decision::new(
                    Status::Error,
                    None,
                    error.message().to_string(),
                    revisions(attempt),
                )
            }
        };
        if !state.issues.is_empty() {
            return Decision::new(
                Status::Inconclusive,
                None,
                "INCOMPLETE_JUDGING_EVIDENCE",
                revisions(attempt),
            );
        }
        let size = serde_json::Value::Object(state.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .to_string()
            .chars()
            .count();
        if size > limits.max_chars {
            return Decision::new(
                Status::Inconclusive,
                None,
                "JUDGE_STATE_LIMIT",
                revisions(attempt),
            );
        }
        let review = settings
            .judge
            .judge(
                serde_json::Value::Object(
                    state.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                ),
                &settings.definition,
            )
            .await;
        match review.status {
            JudgeStatus::Pass => {
                return Decision::new(Status::Decided, Some(draft), "QUALITY_PASS", revisions(attempt))
            }
            JudgeStatus::Fail if settings.mode == Mode::Enforce && attempt < settings.max_revisions => {
                let revision = Revision::new(
                    input.messages.clone(),
                    input.options.clone(),
                    Arc::clone(&input.model),
                    draft.clone(),
                    review,
                );
                let chunks = match (settings.reviser)(revision).await {
                    Ok(chunks) => chunks,
                    Err(error) => {
                        return Decision::new(
                            Status::Error,
                            None,
                            error.message().to_string(),
                            revisions(attempt),
                        )
                    }
                };
                let revised = match collect_text(chunks, limits) {
                    Ok(text) => text,
                    Err(error) => {
                        return Decision::new(
                            Status::Error,
                            None,
                            error.message().to_string(),
                            revisions(attempt),
                        )
                    }
                };
                if revised.trim().is_empty() || revised == draft {
                    return Decision::new(
                        Status::Inconclusive,
                        None,
                        "UNCHANGED_DRAFT",
                        revisions(attempt),
                    );
                }
                match policy.check(revised.clone()).await {
                    Ok(outcome) if outcome.blocked => {
                        return Decision::new(
                            Status::Inconclusive,
                            None,
                            outcome.reason,
                            revisions(attempt),
                        )
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return Decision::new(
                            Status::Error,
                            None,
                            error.message().to_string(),
                            revisions(attempt),
                        )
                    }
                }
                draft = revised;
                attempt += 1;
            }
            status => {
                let final_status = if status == JudgeStatus::Error {
                    Status::Error
                } else {
                    Status::Inconclusive
                };
                return Decision::new(
                    final_status,
                    None,
                    format!("QUALITY_{}", judge_status_name(status)),
                    revisions(attempt),
                );
            }
        }
    }
}

fn revisions(attempt: usize) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("revisions".to_string(), attempt.to_string());
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::dto::Usage;
    use crate::jev::judge::{Criterion, ErrorCode, Finding};
    use agentscope_core::error::Result as CoreResult;
    use agentscope_core::event::AgentEvent;
    use agentscope_core::message::{ChatUsage, TextBlock, ToolUseBlock};
    use agentscope_core::model::{ChatResponse, ToolSchema};
    use futures::stream::BoxStream;
    use serde_json::Value;
    use std::time::Duration;

    fn text_event(text: &str) -> CoreResult<AgentEvent> {
        let mut fields = serde_json::Map::new();
        fields.insert("delta".to_string(), Value::String(text.to_string()));
        Ok(AgentEvent::TextBlockDelta(
            agentscope_core::event::EventData {
                fields,
                ..Default::default()
            },
        ))
    }

    fn review(status: JudgeStatus) -> JudgeResult {
        let mut findings = BTreeMap::new();
        findings.insert(
            "a".to_string(),
            Finding {
                status,
                yes_probability: 0.1,
                expected_probability: 0.1,
            },
        );
        JudgeResult {
            definition_version: "v1".to_string(),
            status,
            findings,
            error: ErrorCode::None,
            model: Some("jev".to_string()),
            usage: Some(Usage::new(1, 1)),
            elapsed: Duration::ZERO,
        }
    }

    fn definition() -> Definition {
        Definition::new(
            "v1",
            vec![Criterion::new(
                "a",
                crate::jev::dto::NoulQuestion::new(serde_json::json!("right?"), None),
                true,
                0.2,
                0.8,
            )
            .unwrap()],
        )
        .unwrap()
    }

    struct FixedModel {
        content: Vec<ContentBlock>,
    }

    impl Model for FixedModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, CoreResult<ChatResponse>> {
            let content = self.content.clone();
            Box::pin(futures::stream::iter(vec![Ok(ChatResponse::new(
                "r",
                content,
                ChatUsage::default(),
                None,
                None,
            ))]))
        }

        fn model_name(&self) -> &str {
            "fixed"
        }
    }

    fn revision(content: Vec<ContentBlock>, draft: &str) -> Revision {
        Revision::new(
            vec![Msg::text(MsgRole::User, "q")],
            GenerateOptions::default(),
            Arc::new(FixedModel { content }),
            draft,
            review(JudgeStatus::Fail),
        )
    }

    #[test]
    fn collect_text_enforces_the_buffer_limits() {
        let limits = ResponseLimits::defaults();
        assert_eq!(
            collect_text(vec!["a".to_string(), "b".to_string()], &limits).unwrap(),
            "ab"
        );
        let tight = ResponseLimits::new(3, 1, 0, 1).unwrap();
        assert_eq!(
            collect_text(vec!["abcd".to_string()], &tight)
                .unwrap_err()
                .message(),
            "BUFFER_LIMIT"
        );
        assert_eq!(
            collect_text(vec!["a".to_string(), "b".to_string()], &tight)
                .unwrap_err()
                .message(),
            "BUFFER_LIMIT"
        );
        assert_eq!(
            collect_text(vec!["ab".to_string(), "cd".to_string()], &tight)
                .unwrap_err()
                .message(),
            "BUFFER_LIMIT"
        );
    }

    #[test]
    fn replace_text_rewrites_the_first_delta_and_requires_one() {
        let events = vec![
            text_event("a"),
            text_event("b"),
        ];
        let replaced = replace_text(events, "revised").unwrap();
        assert_eq!(replaced.len(), 1);
        match &replaced[0] {
            Ok(AgentEvent::TextBlockDelta(data)) => {
                assert_eq!(data.fields.get("delta").and_then(Value::as_str), Some("revised"))
            }
            other => panic!("unexpected event {other:?}"),
        }
        let no_text = vec![Ok(AgentEvent::ModelCallStart(
            agentscope_core::event::EventData::default(),
        ))];
        assert_eq!(
            replace_text(no_text, "revised").unwrap_err().message(),
            "NO_TEXT_BLOCK"
        );
    }

    #[test]
    fn capture_enforces_event_and_text_limits() {
        let limits = ResponseLimits::defaults();
        let folded = capture(
            &[
                text_event("a"),
                Ok(AgentEvent::ToolCallStart(
                    agentscope_core::event::EventData::default(),
                )),
                text_event("b"),
            ],
            &limits,
        )
        .unwrap();
        assert_eq!(folded.text, "ab");
        assert!(folded.tools);

        let tight = ResponseLimits::new(3, 2, 0, 1).unwrap();
        assert_eq!(
            capture(&[text_event("abcd")], &tight)
                .unwrap_err()
                .message(),
            "TEXT_LIMIT"
        );
        assert_eq!(
            capture(&[text_event("a"), text_event("b"), text_event("c")], &tight)
                .unwrap_err()
                .message(),
            "EVENT_LIMIT"
        );
    }

    #[test]
    fn the_default_reviser_uses_the_model_without_tools() {
        let revised = futures::executor::block_on(revise_with_model(revision(
            vec![ContentBlock::Text(TextBlock::new("revised"))],
            "draft",
        )))
        .unwrap();
        assert_eq!(revised, vec!["revised".to_string()]);

        let rejected = futures::executor::block_on(revise_with_model(revision(
            vec![ContentBlock::ToolUse(ToolUseBlock {
                id: "c1".to_string(),
                name: "read".to_string(),
                input: serde_json::Map::new(),
                content: None,
                metadata: serde_json::Map::new(),
                state: Default::default(),
            })],
            "draft",
        )))
        .unwrap_err();
        assert_eq!(rejected.message(), "REVISION_TOOL_CALL");
    }

    #[test]
    fn the_revision_prompt_lists_the_failed_criteria() {
        let revision = revision(vec![], "draft body");
        assert_eq!(revision.findings(), "{a=FAIL}");
        let prompt = revision.prompt();
        assert!(prompt.contains("Do not request or execute tools"));
        assert!(prompt.contains("Failed criteria: {a=FAIL}"));
        assert!(prompt.ends_with("Draft:\ndraft body"));
    }

    #[test]
    fn quality_settings_validate_the_revision_budget() {
        let caller: crate::jev::call::Caller = Arc::new(|_request| {
            Box::pin(async { Err(crate::jev::call::CallFailure::backend("unused")) })
        });
        let judge = Arc::new(JevJudge::new(caller, Duration::from_millis(10)).unwrap());
        assert_eq!(
            QualitySettings::new(
                Mode::Shadow,
                judge.clone(),
                definition(),
                Arc::new(revise_with_model),
                11,
            )
            .unwrap_err()
            .message(),
            "invalid response limits"
        );
        let settings = QualitySettings::with_default_reviser(
            Mode::Shadow,
            judge,
            definition(),
            1,
        )
        .unwrap();
        assert_eq!(settings.mode, Mode::Shadow);
        assert_eq!(settings.max_revisions, 1);
    }
}
