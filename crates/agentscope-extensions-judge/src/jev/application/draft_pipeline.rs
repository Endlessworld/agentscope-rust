//! Buffers draft text before release; revision functions must never re-execute side-effectful
//! tools (`JevDraftPipeline`).
//!
//! Deviation: Java consumes a `Flux<String>` chunk stream with a per-chunk size guard. The port
//! seam returns the whole generated text, and the same `max_chars` limit is enforced on it (and on
//! every revision). The observable outcomes (`DRAFT_TOO_LARGE`, `UNCHANGED_DRAFT`, ...) are
//! identical.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::call::BoxedFuture;
use crate::jev::judge::{Definition, JevJudge, JudgeResult, JudgeStatus};

/// The maximum number of revisions Java allows.
pub const MAX_REVISIONS: usize = 10;

/// A draft source: produces the full text.
pub type DraftSource = Arc<dyn Fn() -> BoxedFuture<Result<String, JevError>> + Send + Sync>;

/// A revision source: produces the full revised text.
pub type ReviseSource =
    Arc<dyn Fn(Revision) -> BoxedFuture<Result<String, JevError>> + Send + Sync>;

/// One revision input (`JevDraftPipeline.Revision`).
#[derive(Debug, Clone, PartialEq)]
pub struct Revision {
    /// The draft that failed review.
    pub draft: String,
    /// The review that rejected it.
    pub review: JudgeResult,
    /// The 1-based attempt number of the revision.
    pub attempt: usize,
}

/// The pipeline's outcome (`JevDraftPipeline.Outcome`); never a partially accepted draft.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftOutcome {
    /// The text that may be published, absent when nothing was accepted.
    pub published_text: Option<String>,
    /// The last draft that was considered.
    pub draft_text: Option<String>,
    /// The last review, when one ran.
    pub review: Option<JudgeResult>,
    /// How many revisions were attempted.
    pub revisions: usize,
    /// `ACCEPTED`, `INPUT_NOT_ACCEPTED`, `DRAFT_NOT_ACCEPTED`, `UNCHANGED_DRAFT`,
    /// `DRAFT_TOO_LARGE`, `TIMEOUT` or `PIPELINE_ERROR`.
    pub reason: String,
}

impl DraftOutcome {
    fn rejected(reason: &str) -> Self {
        Self {
            published_text: None,
            draft_text: None,
            review: None,
            revisions: 0,
            reason: reason.to_string(),
        }
    }
}

/// Generates, reviews and revises a draft under fixed limits (`JevDraftPipeline`).
pub struct JevDraftPipeline {
    judge: Arc<JevJudge>,
    max_chars: usize,
    max_revisions: usize,
    budget: Duration,
}

impl std::fmt::Debug for JevDraftPipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevDraftPipeline")
            .field("max_chars", &self.max_chars)
            .field("max_revisions", &self.max_revisions)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl JevDraftPipeline {
    /// Creates the pipeline (`invalid pipeline limits` on bad sizes/budgets).
    pub fn new(
        judge: Arc<JevJudge>,
        max_chars: usize,
        max_revisions: usize,
        budget: Duration,
    ) -> Result<Self, JevError> {
        if max_chars < 1 || max_revisions > MAX_REVISIONS || budget.is_zero() {
            return Err(JevError::invalid("invalid pipeline limits"));
        }
        Ok(Self {
            judge,
            max_chars,
            max_revisions,
            budget,
        })
    }

    /// The character limit.
    pub fn max_chars(&self) -> usize {
        self.max_chars
    }

    /// The revision limit.
    pub fn max_revisions(&self) -> usize {
        self.max_revisions
    }

    /// Runs the pipeline within the configured budget.
    pub async fn run(
        &self,
        request: Value,
        evidence: Value,
        input_checks: &Definition,
        output_checks: &Definition,
        generate: DraftSource,
        revise: ReviseSource,
    ) -> DraftOutcome {
        match tokio::time::timeout(
            self.budget,
            self.run_inner(
                request,
                evidence,
                input_checks,
                output_checks,
                generate,
                revise,
            ),
        )
        .await
        {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_error)) => DraftOutcome::rejected("PIPELINE_ERROR"),
            Err(_elapsed) => DraftOutcome::rejected("TIMEOUT"),
        }
    }

    async fn run_inner(
        &self,
        request: Value,
        evidence: Value,
        input_checks: &Definition,
        output_checks: &Definition,
        generate: DraftSource,
        revise: ReviseSource,
    ) -> Result<DraftOutcome, JevError> {
        let input = self.judge.judge(request.clone(), input_checks).await;
        if input.status != JudgeStatus::Pass {
            return Ok(DraftOutcome {
                published_text: None,
                draft_text: None,
                review: Some(input),
                revisions: 0,
                reason: "INPUT_NOT_ACCEPTED".to_string(),
            });
        }
        let mut text = (generate)().await?;
        if text.chars().count() > self.max_chars {
            return Ok(DraftOutcome::rejected("DRAFT_TOO_LARGE"));
        }
        let mut attempt = 0usize;
        loop {
            let review = self
                .judge
                .judge(
                    json!({ "request": request, "evidence": evidence, "draft": text }),
                    output_checks,
                )
                .await;
            if review.status == JudgeStatus::Pass {
                return Ok(DraftOutcome {
                    published_text: Some(text.clone()),
                    draft_text: Some(text),
                    review: Some(review),
                    revisions: attempt,
                    reason: "ACCEPTED".to_string(),
                });
            }
            if review.status != JudgeStatus::Fail || attempt >= self.max_revisions {
                return Ok(DraftOutcome {
                    published_text: None,
                    draft_text: Some(text),
                    review: Some(review),
                    revisions: attempt,
                    reason: "DRAFT_NOT_ACCEPTED".to_string(),
                });
            }
            let revision = Revision {
                draft: text.clone(),
                review: review.clone(),
                attempt: attempt + 1,
            };
            let next = (revise)(revision).await?;
            if next == text {
                return Ok(DraftOutcome {
                    published_text: None,
                    draft_text: Some(text),
                    review: Some(review),
                    revisions: attempt + 1,
                    reason: "UNCHANGED_DRAFT".to_string(),
                });
            }
            if next.chars().count() > self.max_chars {
                return Ok(DraftOutcome {
                    published_text: None,
                    draft_text: Some(text),
                    review: Some(review),
                    revisions: attempt + 1,
                    reason: "DRAFT_TOO_LARGE".to_string(),
                });
            }
            text = next;
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::dto::{Answer, NoulAnswer, NoulQuestion, SystemOneResult, Usage};
    use crate::jev::judge::Criterion;
    use std::collections::BTreeMap;

    fn definition(id: &str) -> Definition {
        Definition::new(
            "v1",
            vec![
                Criterion::new(id, NoulQuestion::new(json!("ok?"), None), true, 0.2, 0.8).unwrap(),
            ],
        )
        .unwrap()
    }

    /// One judge answer per call, in order.
    fn judge(values: Vec<f64>) -> Arc<JevJudge> {
        let responses = values
            .into_iter()
            .map(|value| {
                Ok(SystemOneResult {
                    model: Some("jev".to_string()),
                    answers: Some(BTreeMap::from([(
                        "ok".to_string(),
                        Answer::Noul(NoulAnswer { noul: Some(value) }),
                    )])),
                    usage: Some(Usage::new(1, 1)),
                })
            })
            .collect();
        let scripted = Arc::new(ScriptedCaller::new(responses));
        Arc::new(JevJudge::new(scripted.as_caller(), Duration::from_secs(1)).unwrap())
    }

    fn source(text: &'static str) -> DraftSource {
        Arc::new(move || Box::pin(async move { Ok(text.to_string()) }))
    }

    fn draft_pipeline_with(values: Vec<f64>) -> JevDraftPipeline {
        JevDraftPipeline::new(judge(values), 100, 2, Duration::from_secs(2)).unwrap()
    }

    fn unchanged() -> ReviseSource {
        Arc::new(|revision: Revision| Box::pin(async move { Ok(revision.draft) }))
    }

    #[tokio::test]
    async fn an_accepted_input_and_draft_publish_immediately() {
        let pipeline = draft_pipeline_with(vec![0.9, 0.9]);
        let outcome = pipeline
            .run(
                json!({}),
                json!({}),
                &definition("ok"),
                &definition("ok"),
                source("draft"),
                unchanged(),
            )
            .await;
        assert_eq!(outcome.reason, "ACCEPTED");
        assert_eq!(outcome.published_text.as_deref(), Some("draft"));
        assert_eq!(outcome.revisions, 0);
        assert!(outcome.review.is_some());
    }

    #[tokio::test]
    async fn a_rejected_input_stops_before_generation() {
        let pipeline = draft_pipeline_with(vec![0.1]);
        let outcome = pipeline
            .run(
                json!({}),
                json!({}),
                &definition("ok"),
                &definition("ok"),
                source("draft"),
                unchanged(),
            )
            .await;
        assert_eq!(outcome.reason, "INPUT_NOT_ACCEPTED");
        assert!(outcome.published_text.is_none());
        assert!(outcome.draft_text.is_none());
    }

    #[tokio::test]
    async fn failed_drafts_are_revised_up_to_the_limit() {
        let pipeline = draft_pipeline_with(vec![0.9, 0.1, 0.1, 0.9]);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&calls);
        let revise: ReviseSource = Arc::new(move |revision: Revision| {
            sink.lock().unwrap().push(revision.attempt);
            let text = format!("draft{}", revision.attempt + 1);
            Box::pin(async move { Ok(text) })
        });
        let outcome = pipeline
            .run(
                json!({}),
                json!({}),
                &definition("ok"),
                &definition("ok"),
                source("draft0"),
                revise,
            )
            .await;
        assert_eq!(outcome.reason, "ACCEPTED");
        assert_eq!(outcome.published_text.as_deref(), Some("draft3"));
        assert_eq!(outcome.revisions, 2);
        assert_eq!(*calls.lock().unwrap(), vec![1, 2]);

        // Two failures at max_revisions=2 stop at DRAFT_NOT_ACCEPTED.
        let pipeline = draft_pipeline_with(vec![0.9, 0.1, 0.1]);
        let revise: ReviseSource = Arc::new(|revision: Revision| {
            let text = format!("draft{}", revision.attempt + 1);
            Box::pin(async move { Ok(text) })
        });
        let outcome = pipeline
            .run(
                json!({}),
                json!({}),
                &definition("ok"),
                &definition("ok"),
                source("draft0"),
                revise,
            )
            .await;
        assert_eq!(outcome.reason, "DRAFT_NOT_ACCEPTED");
        assert_eq!(outcome.revisions, 2);
        assert!(outcome.published_text.is_none());
    }

    #[tokio::test]
    async fn unchanged_oversized_and_failed_revisions_are_reported() {
        let pipeline = draft_pipeline_with(vec![0.9, 0.1]);
        let outcome = pipeline
            .run(
                json!({}),
                json!({}),
                &definition("ok"),
                &definition("ok"),
                source("draft"),
                unchanged(),
            )
            .await;
        assert_eq!(outcome.reason, "UNCHANGED_DRAFT");
        assert_eq!(outcome.revisions, 1);

        let pipeline =
            JevDraftPipeline::new(judge(vec![0.9, 0.1]), 5, 2, Duration::from_secs(2)).unwrap();
        let outcome = pipeline
            .run(
                json!({}),
                json!({}),
                &definition("ok"),
                &definition("ok"),
                source("too long draft"),
                unchanged(),
            )
            .await;
        assert_eq!(outcome.reason, "DRAFT_TOO_LARGE");

        let pipeline =
            JevDraftPipeline::new(judge(vec![0.9, 0.1]), 10, 2, Duration::from_secs(2)).unwrap();
        let outcome = pipeline
            .run(
                json!({}),
                json!({}),
                &definition("ok"),
                &definition("ok"),
                source("abc"),
                Arc::new(|_revision: Revision| Box::pin(async { Err(JevError::invalid("boom")) })),
            )
            .await;
        assert_eq!(outcome.reason, "PIPELINE_ERROR");
    }

    #[test]
    fn limits_are_validated() {
        for (chars, revisions, budget) in [
            (0, 1, Duration::from_secs(1)),
            (10, 11, Duration::from_secs(1)),
            (10, 1, Duration::ZERO),
        ] {
            assert_eq!(
                JevDraftPipeline::new(judge(vec![]), chars, revisions, budget)
                    .unwrap_err()
                    .message(),
                "invalid pipeline limits"
            );
        }
        let pipeline = JevDraftPipeline::new(judge(vec![]), 10, 1, Duration::from_secs(1)).unwrap();
        assert_eq!(pipeline.max_chars(), 10);
        assert_eq!(pipeline.max_revisions(), 1);
        assert!(format!("{pipeline:?}").contains("JevDraftPipeline"));
    }
}
