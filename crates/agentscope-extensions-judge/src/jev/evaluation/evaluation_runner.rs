//! The labelled single-question evaluation SPI and its runner (`evaluation/Evaluator`,
//! `JevEvaluator`, `JevEvaluationRunner`).
//!
//! Unlike [`super::runner`], this runs *labelled cases* through any [`Evaluator`] and reports
//! accuracy plus per-case latency. Java's `Evaluator` is a JEV-local SPI, not a core interface.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;

use crate::error::JevError;
use crate::jev::call::CallFailure;
use crate::jev::dto::{NoulQuestion, Question};
use crate::jev::judge::{Definition, ErrorCode, JevJudge, JudgeResult, JudgeStatus};

use super::metric_result::MetricStatus;

/// One case handed to an [`Evaluator`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The case id.
    pub id: String,
    /// The question being evaluated.
    pub question: String,
    /// Supporting context passages.
    pub supporting_context: Vec<String>,
    /// The answer to evaluate.
    pub answer: String,
}

impl Request {
    /// Creates a request (`Evaluator.Request` compact constructor accepts null context lists as
    /// an empty list here).
    pub fn new(
        id: impl Into<String>,
        question: impl Into<String>,
        supporting_context: Vec<String>,
        answer: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            question: question.into(),
            supporting_context,
            answer: answer.into(),
        }
    }
}

/// One evaluation outcome; the verdict is preserved alongside the scalar score.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// The case id.
    pub id: String,
    /// Whether the answer passed.
    pub pass: bool,
    /// The fraction of criteria that passed.
    pub score: f64,
    /// The full judge result.
    pub verdict: JudgeResult,
}

/// A JEV-local evaluation SPI.
#[async_trait]
pub trait Evaluator: Send + Sync {
    /// Evaluates one request.
    async fn evaluate(&self, request: Request) -> Result<Response, CallFailure>;
}

/// Preserves the full judge result; the scalar score is the fraction of criteria that passed.
pub struct JevEvaluator {
    judge: Arc<JevJudge>,
    definition: Arc<Definition>,
}

impl std::fmt::Debug for JevEvaluator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevEvaluator")
            .field("definition", &self.definition.version)
            .finish_non_exhaustive()
    }
}

impl JevEvaluator {
    /// Creates an evaluator over a judge and a definition (`JevEvaluator` constructor).
    pub fn new(judge: Arc<JevJudge>, definition: Arc<Definition>) -> Self {
        Self { judge, definition }
    }

    /// The state a request becomes (`JevEvaluator.evaluate`).
    pub fn state(request: &Request) -> serde_json::Value {
        json!({
            "user_question": request.question,
            "supporting_context": request.supporting_context,
            "assistant_answer": request.answer,
        })
    }
}

#[async_trait]
impl Evaluator for JevEvaluator {
    async fn evaluate(&self, request: Request) -> Result<Response, CallFailure> {
        let verdict = self
            .judge
            .judge(Self::state(&request), &self.definition)
            .await;
        let passing = verdict
            .findings
            .values()
            .filter(|finding| finding.status == JudgeStatus::Pass)
            .count();
        let total = self.definition.criteria.len();
        Ok(Response {
            id: request.id,
            pass: verdict.status == JudgeStatus::Pass,
            score: passing as f64 / total as f64,
            verdict,
        })
    }
}

/// One labelled case.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationCase {
    /// The scenario this case belongs to.
    pub scenario: String,
    /// The request handed to the evaluator.
    pub request: Request,
    /// Whether the answer is expected to pass.
    pub expected_pass: bool,
}

impl EvaluationCase {
    /// Creates a case (`JevEvaluationRunner.Case`).
    pub fn new(scenario: impl Into<String>, request: Request, expected_pass: bool) -> Self {
        Self {
            scenario: scenario.into(),
            request,
            expected_pass,
        }
    }
}

/// One evaluated case.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationRow {
    /// The scenario this row belongs to.
    pub scenario: String,
    /// The case id.
    pub id: String,
    /// The expected verdict.
    pub expected_pass: bool,
    /// The evaluator's response.
    pub response: Response,
}

/// Per-scenario counts and latency percentiles (`JevEvaluationRunner.Summary`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EvaluationSummary {
    /// How many rows the summary covers.
    pub total: usize,
    /// Rows whose verdict matched the expectation.
    pub correct: usize,
    /// Rows whose verdict contradicted the expectation.
    pub incorrect: usize,
    /// Inconclusive evaluations (Java `abstained`).
    pub abstained: usize,
    /// Errored evaluations.
    pub errors: usize,
    /// `correct / total`.
    pub accuracy: f64,
    /// Median evaluation time, in milliseconds.
    pub p50_millis: f64,
    /// 95th percentile evaluation time, in milliseconds.
    pub p95_millis: f64,
}

/// The runner report (`JevEvaluationRunner.Report`).
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationReport {
    /// One row per case, in case order.
    pub rows: Vec<EvaluationRow>,
    /// The overall summary.
    pub overall: EvaluationSummary,
    /// Per-scenario summaries.
    pub scenarios: BTreeMap<String, EvaluationSummary>,
}

/// Runs fixed labelled cases with bounded concurrency and metadata-only per-case reports.
#[derive(Debug, Default, Clone, Copy)]
pub struct JevEvaluationRunner;

impl JevEvaluationRunner {
    /// Evaluates every case, keeping case order.
    ///
    /// Rejects duplicate ids, more than 10000 cases and a concurrency outside `1..=32`.
    pub async fn run(
        &self,
        evaluator: &dyn Evaluator,
        cases: Vec<EvaluationCase>,
        concurrency: usize,
    ) -> Result<EvaluationReport, JevError> {
        if !(1..=32).contains(&concurrency) || cases.len() > 10_000 {
            return Err(JevError::invalid(
                "unique IDs, <=10000 cases and concurrency 1..32 required",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        if cases
            .iter()
            .any(|case| !seen.insert(case.request.id.clone()))
        {
            return Err(JevError::invalid(
                "unique IDs, <=10000 cases and concurrency 1..32 required",
            ));
        }

        let rows = futures::stream::iter(cases)
            .map(|case| async move {
                let response = evaluator.evaluate(case.request.clone()).await;
                let response = match response {
                    Ok(response) => response,
                    // A failed case is still reported, with an explicit error verdict.
                    Err(_error) => Response {
                        id: case.request.id.clone(),
                        pass: false,
                        score: 0.0,
                        verdict: JudgeResult::error(
                            "evaluation-error",
                            ErrorCode::Backend,
                            Duration::ZERO,
                        ),
                    },
                };
                EvaluationRow {
                    scenario: case.scenario,
                    id: case.request.id,
                    expected_pass: case.expected_pass,
                    response,
                }
            })
            .buffered(concurrency)
            .collect::<Vec<EvaluationRow>>()
            .await;

        let mut scenarios: BTreeMap<String, EvaluationSummary> = BTreeMap::new();
        let mut grouped: Vec<(String, Vec<EvaluationRow>)> = Vec::new();
        for row in &rows {
            match grouped.iter_mut().find(|(key, _)| key == &row.scenario) {
                Some((_, bucket)) => bucket.push(row.clone()),
                None => grouped.push((row.scenario.clone(), vec![row.clone()])),
            }
        }
        for (scenario, bucket) in grouped {
            scenarios.insert(scenario, summarize(&bucket));
        }
        Ok(EvaluationReport {
            overall: summarize(&rows),
            scenarios,
            rows,
        })
    }
}

fn summarize(rows: &[EvaluationRow]) -> EvaluationSummary {
    let mut correct = 0usize;
    let mut incorrect = 0usize;
    let mut abstained = 0usize;
    let mut errors = 0usize;
    let mut durations = Vec::with_capacity(rows.len());
    for row in rows {
        durations.push(row.response.verdict.elapsed.as_secs_f64() * 1000.0);
        match row.response.verdict.status {
            JudgeStatus::Error => errors += 1,
            JudgeStatus::Inconclusive => abstained += 1,
            status => {
                if (status == JudgeStatus::Pass) == row.expected_pass {
                    correct += 1;
                } else {
                    incorrect += 1;
                }
            }
        }
    }
    durations.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    EvaluationSummary {
        total: rows.len(),
        correct,
        incorrect,
        abstained,
        errors,
        accuracy: if rows.is_empty() {
            0.0
        } else {
            correct as f64 / rows.len() as f64
        },
        p50_millis: percentile(&durations, 0.5),
        p95_millis: percentile(&durations, 0.95),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((p * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

/// Builds the one question a JEV evaluation definition uses (helper for offline callers).
pub fn noul_question(instructions: &str) -> Question {
    Question::Noul(NoulQuestion::new(json!(instructions), None))
}

/// The judge's passing-criteria count, ignoring non-decided findings.
pub fn passing_findings(verdict: &JudgeResult) -> Vec<&str> {
    verdict
        .findings
        .iter()
        .filter(|(_, finding)| finding.status == JudgeStatus::Pass)
        .map(|(id, _)| id.as_str())
        .collect()
}

/// Whether a metric-style status agrees with the Java abstention rule (kept for symmetry with
/// [`MetricStatus`] consumers).
pub fn abstained(status: MetricStatus) -> bool {
    status == MetricStatus::Inconclusive
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::{Caller, CallFailure};
    use crate::jev::dto::{Answer, NoulAnswer, SystemOneResult, Usage};
    use crate::jev::judge::Criterion;

    fn definition() -> Arc<Definition> {
        Arc::new(
            Definition::new(
                "v1",
                vec![
                    Criterion::new(
                        "a",
                        NoulQuestion::new(json!("is a?"), None),
                        true,
                        0.2,
                        0.8,
                    )
                    .unwrap(),
                    Criterion::new(
                        "b",
                        NoulQuestion::new(json!("is b?"), None),
                        true,
                        0.2,
                        0.8,
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        )
    }

    /// Answers `a` and `b` with fixed probabilities, one script entry per judge call.
    fn judge_over(probabilities: Vec<(f64, f64)>) -> Arc<JevJudge> {
        let caller: Caller = Arc::new(move |_request| {
            let (a, b) = probabilities[0];
            Box::pin(async move {
                let mut answers = BTreeMap::new();
                answers.insert(
                    "a".to_string(),
                    Answer::Noul(NoulAnswer {
                        noul: Some(a),
                    }),
                );
                answers.insert(
                    "b".to_string(),
                    Answer::Noul(NoulAnswer {
                        noul: Some(b),
                    }),
                );
                Ok(SystemOneResult {
                    model: Some("jev".to_string()),
                    answers: Some(answers),
                    usage: Some(Usage::new(1, 1)),
                })
            })
        });
        Arc::new(JevJudge::new(caller, Duration::from_millis(200)).unwrap())
    }

    struct Fixed(Vec<(String, bool, f64, JudgeResult)>);

    #[async_trait]
    impl Evaluator for Fixed {
        async fn evaluate(&self, request: Request) -> Result<Response, CallFailure> {
            let (_, pass, score, verdict) = self
                .0
                .iter()
                .find(|(id, ..)| *id == request.id)
                .cloned()
                .expect("case");
            Ok(Response {
                id: request.id,
                pass,
                score,
                verdict,
            })
        }
    }

    fn verdict(status: JudgeStatus) -> JudgeResult {
        JudgeResult {
            definition_version: "v1".to_string(),
            status,
            findings: BTreeMap::new(),
            error: ErrorCode::None,
            model: None,
            usage: None,
            elapsed: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn jev_evaluator_scores_the_fraction_of_passing_criteria() {
        let evaluator = JevEvaluator::new(judge_over(vec![(0.9, 0.5)]), definition());
        let response = evaluator
            .evaluate(Request::new("c1", "q", vec!["ctx".to_string()], "answer"))
            .await
            .unwrap();
        assert!(!response.pass);
        assert_eq!(response.score, 0.5);
        assert_eq!(response.id, "c1");
        assert_eq!(response.verdict.status, JudgeStatus::Inconclusive);
        assert_eq!(passing_findings(&response.verdict), vec!["a"]);
        assert_eq!(
            JevEvaluator::state(&Request::new("c1", "q", vec!["ctx".to_string()], "answer")),
            json!({
                "user_question": "q",
                "supporting_context": ["ctx"],
                "assistant_answer": "answer",
            })
        );

        let evaluator = JevEvaluator::new(judge_over(vec![(0.9, 0.9)]), definition());
        let response = evaluator
            .evaluate(Request::new("c2", "q", vec![], "answer"))
            .await
            .unwrap();
        assert!(response.pass);
        assert_eq!(response.score, 1.0);
    }

    #[tokio::test]
    async fn runner_reports_accuracy_per_scenario_and_counts_errors() {
        let evaluator = Fixed(vec![
            ("c1".to_string(), true, 1.0, verdict(JudgeStatus::Pass)),
            ("c2".to_string(), false, 0.0, verdict(JudgeStatus::Fail)),
            ("c3".to_string(), false, 0.0, verdict(JudgeStatus::Inconclusive)),
            (
                "c4".to_string(),
                false,
                0.0,
                JudgeResult::error("evaluation-error", ErrorCode::Backend, Duration::ZERO),
            ),
        ]);
        let cases = vec![
            EvaluationCase::new(
                "good",
                Request::new("c1", "q", vec![], "a"),
                true,
            ),
            EvaluationCase::new(
                "good",
                Request::new("c2", "q", vec![], "a"),
                true,
            ),
            EvaluationCase::new("edge", Request::new("c3", "q", vec![], "a"), false),
            EvaluationCase::new("edge", Request::new("c4", "q", vec![], "a"), false),
        ];
        let report = JevEvaluationRunner.run(&evaluator, cases, 4).await.unwrap();
        assert_eq!(report.rows.len(), 4);
        assert_eq!(report.overall.total, 4);
        assert_eq!(report.overall.correct, 1);
        assert_eq!(report.overall.incorrect, 1);
        assert_eq!(report.overall.abstained, 1);
        assert_eq!(report.overall.errors, 1);
        assert_eq!(report.overall.accuracy, 0.25);
        assert_eq!(report.scenarios["good"].correct, 1);
        assert_eq!(report.scenarios["good"].incorrect, 1);
        assert_eq!(report.scenarios["edge"].abstained, 1);
        assert_eq!(report.scenarios["edge"].errors, 1);
        assert_eq!(report.rows[0].id, "c1");
    }

    #[tokio::test]
    async fn failed_calls_become_an_explicit_error_verdict() {
        struct Failing;
        #[async_trait]
        impl Evaluator for Failing {
            async fn evaluate(&self, _request: Request) -> Result<Response, CallFailure> {
                Err(CallFailure::backend("down"))
            }
        }
        let cases = vec![EvaluationCase::new(
            "s",
            Request::new("c1", "q", vec![], "a"),
            true,
        )];
        let report = JevEvaluationRunner.run(&Failing, cases, 1).await.unwrap();
        assert_eq!(report.overall.errors, 1);
        assert_eq!(report.rows[0].response.verdict.definition_version, "evaluation-error");
        assert!(!report.rows[0].response.pass);
        assert_eq!(report.rows[0].response.score, 0.0);
    }

    #[tokio::test]
    async fn duplicate_ids_and_bad_concurrency_are_rejected() {
        let evaluator = Fixed(vec![(
            "c1".to_string(),
            true,
            1.0,
            verdict(JudgeStatus::Pass),
        )]);
        let case = EvaluationCase::new("s", Request::new("c1", "q", vec![], "a"), true);
        assert_eq!(
            JevEvaluationRunner
                .run(&evaluator, vec![case.clone()], 0)
                .await
                .unwrap_err()
                .message(),
            "unique IDs, <=10000 cases and concurrency 1..32 required"
        );
        assert_eq!(
            JevEvaluationRunner
                .run(&evaluator, vec![case.clone(), case], 1)
                .await
                .unwrap_err()
                .message(),
            "unique IDs, <=10000 cases and concurrency 1..32 required"
        );
    }

    #[test]
    fn percentile_and_helpers_match_the_java_definitions() {
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[1.0, 4.0], 0.5), 1.0);
        assert_eq!(percentile(&[1.0, 2.0], 0.95), 2.0);
        assert!(abstained(MetricStatus::Inconclusive));
        assert!(!abstained(MetricStatus::Decided));
        assert!(matches!(noul_question("q"), Question::Noul(_)));
    }
}
