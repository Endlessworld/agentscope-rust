//! The application-level, all-required-criteria judge (`io.agentscope.extensions.judge.jev.JevJudge`).
//!
//! Java's `JevJudge` sits between the raw System One client and the application/evaluation
//! packages: it turns one [`Definition`] into one System One request, requires *every* criterion
//! to be answered, and folds per-criterion probabilities into an overall status. It never
//! authorizes or executes actions, and findings carry probabilities only (no generated
//! explanations).
//!
//! Deviation: Java validates a definition by throwing from the record constructors, which the
//! caller can only catch at assembly time. Here [`Criterion::new`] and [`Definition::new`] return
//! `Result`, which is the same contract expressed in Rust.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::error::JevError;

use super::call::Caller;
use super::client::JevClient;
use super::dto::{Answer, NoulQuestion, Question, SystemOneRequest, SystemOneResult, Usage};

/// Overall and per-criterion verdict (`JevJudge.Status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeStatus {
    /// Every criterion passed.
    Pass,
    /// At least one criterion failed.
    Fail,
    /// At least one criterion was between its two thresholds.
    Inconclusive,
    /// The judge could not produce a verdict.
    Error,
}

/// Why a result is an error (`JevJudge.ErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Not an error.
    None,
    /// The budget expired.
    Timeout,
    /// The reply violated the response contract.
    InvalidResponse,
    /// The caller failed.
    Backend,
}

/// One required criterion.
///
/// The two thresholds apply to the **expected** answer, so `expected == false` criteria pass on a
/// low `noul` probability.
#[derive(Debug, Clone, PartialEq)]
pub struct Criterion {
    /// Criterion id; also the System One question id.
    pub id: String,
    /// The yes/no question asked about this criterion.
    pub question: NoulQuestion,
    /// Whether the criterion is expected to be true.
    pub expected: bool,
    /// Below or equal to this probability the criterion fails.
    pub fail_at_or_below: f64,
    /// At or above this probability the criterion passes.
    pub pass_at_or_above: f64,
}

impl Criterion {
    /// Creates and validates a criterion (`JevJudge.Criterion` compact constructor).
    pub fn new(
        id: impl Into<String>,
        question: NoulQuestion,
        expected: bool,
        fail_at_or_below: f64,
        pass_at_or_above: f64,
    ) -> Result<Self, JevError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(JevError::invalid("criterion id is required"));
        }
        if !fail_at_or_below.is_finite()
            || !pass_at_or_above.is_finite()
            || fail_at_or_below < 0.0
            || pass_at_or_above > 1.0
            || fail_at_or_below >= pass_at_or_above
        {
            return Err(JevError::invalid("require 0 <= fail < pass <= 1"));
        }
        Ok(Self {
            id,
            question,
            expected,
            fail_at_or_below,
            pass_at_or_above,
        })
    }
}

/// An immutable, versioned evaluation definition; all criteria are required.
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    /// Definition version reported back on every result.
    pub version: String,
    /// The criteria, in evaluation order.
    pub criteria: Vec<Criterion>,
}

impl Definition {
    /// Creates and validates a definition (`JevJudge.Definition` compact constructor).
    pub fn new(version: impl Into<String>, criteria: Vec<Criterion>) -> Result<Self, JevError> {
        let version = version.into();
        if version.trim().is_empty() {
            return Err(JevError::invalid("version is required"));
        }
        if criteria.is_empty() {
            return Err(JevError::invalid("criteria must not be empty"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for criterion in &criteria {
            if !seen.insert(criterion.id.as_str()) {
                return Err(JevError::invalid("duplicate criterion id"));
            }
        }
        Ok(Self { version, criteria })
    }
}

/// One criterion's verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Finding {
    /// The criterion status.
    pub status: JudgeStatus,
    /// The raw probability that the answer to the question is "yes".
    pub yes_probability: f64,
    /// The probability of the *expected* answer (`yes_probability` or its complement).
    pub expected_probability: f64,
}

/// The full judge result (`JevJudge.Result`).
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeResult {
    /// The definition version that produced this result.
    pub definition_version: String,
    /// The overall status.
    pub status: JudgeStatus,
    /// Per-criterion findings, keyed by criterion id.
    pub findings: BTreeMap<String, Finding>,
    /// Why the result is an error.
    pub error: ErrorCode,
    /// The model reported by the backend, when available.
    pub model: Option<String>,
    /// Token usage, when available.
    pub usage: Option<Usage>,
    /// Wall-clock time spent.
    pub elapsed: Duration,
}

impl JudgeResult {
    /// Creates an error result with no findings (`new JevJudge.Result(..., ERROR, Map.of(), ...)`).
    pub fn error(
        definition_version: impl Into<String>,
        code: ErrorCode,
        elapsed: Duration,
    ) -> Self {
        Self {
            definition_version: definition_version.into(),
            status: JudgeStatus::Error,
            findings: BTreeMap::new(),
            error: code,
            model: None,
            usage: None,
            elapsed,
        }
    }
}

/// Evaluates one [`Definition`] against arbitrary state.
pub struct JevJudge {
    caller: Caller,
    budget: Duration,
}

impl std::fmt::Debug for JevJudge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevJudge")
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl JevJudge {
    /// Builds a judge over an injected caller (Java's `Function<...>` constructor).
    ///
    /// The budget covers the caller's own retries and backoff; it must be positive.
    pub fn new(caller: Caller, budget: Duration) -> Result<Self, JevError> {
        if budget.is_zero() {
            return Err(JevError::invalid("budget must be positive"));
        }
        Ok(Self { caller, budget })
    }

    /// Builds a judge over a [`JevClient`].
    pub fn from_client(client: JevClient, budget: Duration) -> Result<Self, JevError> {
        Self::new(super::call::caller_from_client(client), budget)
    }

    /// The configured budget.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// The System One request a definition produces for `state` (`JevJudge.judge` request build).
    pub fn request(&self, state: Value, definition: &Definition) -> SystemOneRequest {
        let mut builder = SystemOneRequest::builder().state(state);
        for criterion in &definition.criteria {
            builder = builder.question(
                criterion.id.clone(),
                Question::Noul(criterion.question.clone()),
            );
        }
        builder.build()
    }

    /// Evaluates once. Backend problems become an error result; this never fails.
    pub async fn judge(&self, state: Value, definition: &Definition) -> JudgeResult {
        let request = self.request(state, definition);
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(self.budget, (self.caller)(request.clone())).await;
        match outcome {
            Err(_elapsed) => JudgeResult::error(
                definition.version.clone(),
                ErrorCode::Timeout,
                started.elapsed(),
            ),
            Ok(Err(failure)) => JudgeResult::error(
                definition.version.clone(),
                if failure.is_invalid_reply() {
                    ErrorCode::InvalidResponse
                } else {
                    ErrorCode::Backend
                },
                started.elapsed(),
            ),
            Ok(Ok(response)) => match evaluate(definition, &response, started) {
                Ok(result) => result,
                Err(SchemaProblem::InvalidResponse) => JudgeResult::error(
                    definition.version.clone(),
                    ErrorCode::InvalidResponse,
                    started.elapsed(),
                ),
            },
        }
    }
}

/// Why [`evaluate`] rejected a usable-looking reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaProblem {
    InvalidResponse,
}

fn evaluate(
    definition: &Definition,
    response: &SystemOneResult,
    started: std::time::Instant,
) -> Result<JudgeResult, SchemaProblem> {
    let Some(answers) = &response.answers else {
        return Err(SchemaProblem::InvalidResponse);
    };
    if answers.len() != definition.criteria.len() {
        return Err(SchemaProblem::InvalidResponse);
    }
    let mut findings = BTreeMap::new();
    let mut overall = JudgeStatus::Pass;
    for criterion in &definition.criteria {
        let Some(Answer::Noul(answer)) = answers.get(&criterion.id) else {
            return Err(SchemaProblem::InvalidResponse);
        };
        let Some(noul) = answer.noul else {
            return Err(SchemaProblem::InvalidResponse);
        };
        if !noul.is_finite() || !(0.0..=1.0).contains(&noul) {
            return Err(SchemaProblem::InvalidResponse);
        }
        let probability = if criterion.expected { noul } else { 1.0 - noul };
        let status = if probability >= criterion.pass_at_or_above {
            JudgeStatus::Pass
        } else if probability <= criterion.fail_at_or_below {
            JudgeStatus::Fail
        } else {
            JudgeStatus::Inconclusive
        };
        findings.insert(
            criterion.id.clone(),
            Finding {
                status,
                yes_probability: noul,
                expected_probability: probability,
            },
        );
        if status == JudgeStatus::Fail {
            overall = JudgeStatus::Fail;
        } else if status == JudgeStatus::Inconclusive && overall == JudgeStatus::Pass {
            overall = JudgeStatus::Inconclusive;
        }
    }
    Ok(JudgeResult {
        definition_version: definition.version.clone(),
        status: overall,
        findings,
        error: ErrorCode::None,
        model: response.model.clone(),
        usage: response.usage,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::call::CallFailure;
    use crate::jev::dto::NoulAnswer;
    use std::sync::Arc;

    fn question(text: &str) -> NoulQuestion {
        NoulQuestion::new(serde_json::json!(text), None)
    }

    fn definition() -> Definition {
        Definition::new(
            "v1",
            vec![
                Criterion::new("a", question("is a true?"), true, 0.2, 0.8).unwrap(),
                Criterion::new("b", question("is b true?"), false, 0.2, 0.8).unwrap(),
            ],
        )
        .unwrap()
    }

    fn response(a: f64, b: f64) -> SystemOneResult {
        let mut answers = BTreeMap::new();
        answers.insert(
            "a".to_string(),
            Answer::Noul(NoulAnswer { noul: Some(a) }),
        );
        answers.insert(
            "b".to_string(),
            Answer::Noul(NoulAnswer { noul: Some(b) }),
        );
        SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(answers),
            usage: Some(Usage::new(10, 5)),
        }
    }

    fn judge_with(
        result: Result<SystemOneResult, CallFailure>,
    ) -> (JevJudge, Arc<ScriptedCaller>) {
        let scripted = Arc::new(ScriptedCaller::new(vec![result]));
        let judge = JevJudge::new(
            scripted.as_caller(),
            Duration::from_millis(200),
        )
        .unwrap();
        (judge, scripted)
    }

    #[tokio::test]
    async fn passes_only_when_every_criterion_passes() {
        let (judge, _) = judge_with(Ok(response(0.9, 0.1)));
        let result = judge.judge(serde_json::json!({"k":1}), &definition()).await;
        assert_eq!(result.status, JudgeStatus::Pass);
        assert_eq!(result.error, ErrorCode::None);
        assert_eq!(result.definition_version, "v1");
        assert_eq!(result.model.as_deref(), Some("jev"));
        assert_eq!(result.usage, Some(Usage::new(10, 5)));
        assert_eq!(result.findings["a"].expected_probability, 0.9);
        assert_eq!(result.findings["b"].expected_probability, 0.9);
    }

    #[tokio::test]
    async fn a_single_failure_dominates_and_negated_criteria_flip_the_probability() {
        let (judge, _) = judge_with(Ok(response(0.9, 0.9)));
        let result = judge.judge(serde_json::json!({}), &definition()).await;
        assert_eq!(result.status, JudgeStatus::Fail);
        assert_eq!(result.findings["b"].status, JudgeStatus::Fail);
        assert_eq!(result.findings["b"].expected_probability, 1.0 - 0.9);
    }

    #[tokio::test]
    async fn band_between_thresholds_is_inconclusive() {
        let (judge, _) = judge_with(Ok(response(0.5, 0.1)));
        let result = judge.judge(serde_json::json!({}), &definition()).await;
        assert_eq!(result.status, JudgeStatus::Inconclusive);
        assert_eq!(result.findings["a"].status, JudgeStatus::Inconclusive);
    }

    #[tokio::test]
    async fn contract_violations_and_backend_errors_are_error_results() {
        let (judge, _) = judge_with(Err(CallFailure::invalid_reply("llm:x", None)));
        assert_eq!(
            judge.judge(serde_json::json!({}), &definition()).await.error,
            ErrorCode::InvalidResponse
        );

        let (judge, _) = judge_with(Err(CallFailure::backend("down")));
        assert_eq!(
            judge.judge(serde_json::json!({}), &definition()).await.error,
            ErrorCode::Backend
        );

        let mut missing = response(0.9, 0.1);
        missing.answers.as_mut().unwrap().remove("b");
        let (judge, _) = judge_with(Ok(missing));
        let result = judge.judge(serde_json::json!({}), &definition()).await;
        assert_eq!(result.status, JudgeStatus::Error);
        assert_eq!(result.error, ErrorCode::InvalidResponse);
        assert!(result.findings.is_empty());

        let (judge, _) = judge_with(Ok(response(f64::NAN, 0.1)));
        assert_eq!(
            judge.judge(serde_json::json!({}), &definition()).await.error,
            ErrorCode::InvalidResponse
        );
    }

    #[tokio::test]
    async fn budget_expiry_is_reported_as_timeout() {
        let caller: Caller = Arc::new(|_request| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Err(CallFailure::backend("late"))
            })
        });
        let judge = JevJudge::new(caller, Duration::from_millis(10)).unwrap();
        let result = judge.judge(serde_json::json!({}), &definition()).await;
        assert_eq!(result.error, ErrorCode::Timeout);
        assert_eq!(result.status, JudgeStatus::Error);
    }

    #[tokio::test]
    async fn the_request_carries_every_criterion_as_a_noul_question() {
        let (judge, scripted) = judge_with(Ok(response(0.9, 0.1)));
        judge.judge(serde_json::json!({"k":1}), &definition()).await;
        let requests = scripted.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].questions.len(), 2);
        assert!(matches!(
            requests[0].questions.get("a"),
            Some(Question::Noul(_))
        ));
        assert_eq!(requests[0].state, Some(serde_json::json!({"k":1})));
    }

    #[test]
    fn definitions_validate_versions_ids_and_thresholds() {
        assert_eq!(
            Definition::new(" ", vec![]).unwrap_err().message(),
            "version is required"
        );
        assert_eq!(
            Definition::new("v", vec![]).unwrap_err().message(),
            "criteria must not be empty"
        );
        let criterion = Criterion::new("", question("q"), true, 0.2, 0.8);
        assert_eq!(criterion.unwrap_err().message(), "criterion id is required");
        for (fail, pass) in [(0.8, 0.2), (-0.1, 0.5), (0.2, 1.5)] {
            assert_eq!(
                Criterion::new("a", question("q"), true, fail, pass)
                    .unwrap_err()
                    .message(),
                "require 0 <= fail < pass <= 1"
            );
        }
        let duplicated = Definition::new(
            "v",
            vec![
                Criterion::new("a", question("q"), true, 0.2, 0.8).unwrap(),
                Criterion::new("a", question("q"), true, 0.2, 0.8).unwrap(),
            ],
        );
        assert_eq!(duplicated.unwrap_err().message(), "duplicate criterion id");
        assert_eq!(
            JevJudge::new(Arc::new(|_r| Box::pin(async { Err(CallFailure::backend("x")) })), Duration::ZERO)
                .unwrap_err()
                .message(),
            "budget must be positive"
        );
    }
}
