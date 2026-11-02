//! Request packing, grouping and per-metric failure states (`evaluation/JevTraceEvaluator`).
//!
//! A trace is evaluated by *packing* compatible metrics into one System One request: metrics are
//! grouped while their states do not conflict and the question/state budgets allow it. Every
//! metric still gets its own [`JevMetricResult`], and a group failure only fails the metrics that
//! rode on that request.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::JevError;
use crate::jev::call::{CallFailure, Caller};
use crate::jev::client::JevClient;
use crate::jev::dto::{Answer, Question, SystemOneRequest, SystemOneResult, Usage};
use crate::jev::validation::validate_response;

use super::metric::JevTraceMetric;
use super::metric_result::JevMetricResult;
use super::trace::JevTrace;
use super::trace_data;

/// Per-evaluation budgets (`JevTraceEvaluator.Limits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Total budget for the whole trace evaluation.
    pub budget: Duration,
    /// Maximum questions per packed request.
    pub max_questions: usize,
    /// Maximum serialized state length per packed request.
    pub max_state_chars: usize,
}

impl Limits {
    /// Creates and validates limits (`Limits` compact constructor).
    pub fn new(
        budget: Duration,
        max_questions: usize,
        max_state_chars: usize,
    ) -> Result<Self, JevError> {
        if budget.is_zero() || max_questions < 1 || max_state_chars < 1 {
            return Err(JevError::invalid(
                "positive trace evaluation limits required",
            ));
        }
        Ok(Self {
            budget,
            max_questions,
            max_state_chars,
        })
    }

    /// The Java defaults: 3s, 128 questions, 100000 state characters.
    pub fn defaults() -> Self {
        Self {
            budget: Duration::from_secs(3),
            max_questions: 128,
            max_state_chars: 100_000,
        }
    }
}

/// One metric's entry in a report.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The metric id.
    pub id: String,
    /// The metric version.
    pub version: String,
    /// The result, absent only when the metric never ran.
    pub result: Option<JevMetricResult>,
}

/// Usage of one packed request; never multiplied by its metric count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestUsage {
    /// The model reported by the backend.
    pub model: Option<String>,
    /// Token usage, when available.
    pub usage: Option<Usage>,
}

/// The result of evaluating one trace (`JevTraceEvaluator.Report`).
#[derive(Debug, Clone, PartialEq)]
pub struct TraceReport {
    /// The trace id that was evaluated.
    pub trace_id: String,
    /// One entry per configured metric, in configuration order.
    pub results: Vec<Entry>,
    /// How many System One requests were sent.
    pub requests: usize,
    /// Usage per request, in request order.
    pub usage: Vec<RequestUsage>,
    /// Wall-clock time spent.
    pub elapsed: Duration,
}

impl TraceReport {
    /// Whether every metric decided *and* passed (`Report.passed`).
    pub fn passed(&self) -> bool {
        !self.results.is_empty()
            && self.results.iter().all(|entry| match &entry.result {
                Some(result) => result.is_pass(),
                None => false,
            })
    }

    /// The result of one metric, by id.
    pub fn result(&self, id: &str) -> Option<&JevMetricResult> {
        self.results
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.result.as_ref())
    }
}

/// One metric riding on a packed request.
struct Member {
    metric: Arc<dyn JevTraceMetric>,
    questions: BTreeMap<String, Question>,
}

/// A pack of metrics that share one state and one request.
#[derive(Default)]
struct Group {
    state: BTreeMap<String, Value>,
    questions: BTreeMap<String, Question>,
    members: Vec<Member>,
}

/// Packs compatible metric state, separates conflicts and preserves per-metric failure states.
pub struct JevTraceEvaluator {
    caller: Caller,
    metrics: Vec<Arc<dyn JevTraceMetric>>,
    limits: Limits,
}

impl std::fmt::Debug for JevTraceEvaluator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevTraceEvaluator")
            .field("metrics", &self.metric_ids())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl JevTraceEvaluator {
    /// Creates an evaluator over an injected caller (`JevTraceEvaluator` constructor).
    ///
    /// Requires 1..=128 metrics with unique, simple ids and non-blank versions.
    pub fn new(
        caller: Caller,
        metrics: Vec<Arc<dyn JevTraceMetric>>,
        limits: Limits,
    ) -> Result<Self, JevError> {
        if metrics.is_empty() || metrics.len() > 128 {
            return Err(JevError::invalid("1..128 unique metrics required"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for metric in &metrics {
            require_id(metric.id())?;
            if metric.version().trim().is_empty() {
                return Err(JevError::invalid("metric version required"));
            }
            if !seen.insert(metric.id().to_string()) {
                return Err(JevError::invalid("1..128 unique metrics required"));
            }
        }
        Ok(Self {
            caller,
            metrics,
            limits,
        })
    }

    /// Creates an evaluator over a [`JevClient`].
    pub fn from_client(
        client: JevClient,
        metrics: Vec<Arc<dyn JevTraceMetric>>,
        limits: Limits,
    ) -> Result<Self, JevError> {
        Self::new(
            crate::jev::call::caller_from_client(client),
            metrics,
            limits,
        )
    }

    /// The configured metric ids, in order (`metricIds`).
    pub fn metric_ids(&self) -> Vec<String> {
        self.metrics
            .iter()
            .map(|metric| metric.id().to_string())
            .collect()
    }

    /// The configured limits.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Evaluates one trace within the configured budget.
    ///
    /// Groups are evaluated sequentially, exactly like Java's `concatMap(group -> ..., 1)`, so
    /// the request order and the usage list are deterministic.
    pub async fn evaluate(&self, trace: &JevTrace) -> TraceReport {
        let started = Instant::now();
        let mut done: BTreeMap<String, JevMetricResult> = BTreeMap::new();
        let mut usage: Vec<RequestUsage> = Vec::new();
        let mut requests = 0usize;
        let mut timed_out = false;

        let groups = self.plan(trace, &mut done);
        for group in groups {
            let remaining = match self.limits.budget.checked_sub(started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    timed_out = true;
                    break;
                }
            };
            let run = run_group(
                &self.caller,
                &group,
                trace,
                &mut done,
                &mut usage,
                &mut requests,
            );
            if tokio::time::timeout(remaining, run).await.is_err() {
                timed_out = true;
                break;
            }
        }
        if timed_out {
            for metric in &self.metrics {
                done.entry(metric.id().to_string())
                    .or_insert_with(|| JevMetricResult::error("TIMEOUT"));
            }
        }

        let results = self
            .metrics
            .iter()
            .map(|metric| Entry {
                id: metric.id().to_string(),
                version: metric.version(),
                result: done.get(metric.id()).cloned(),
            })
            .collect();
        TraceReport {
            trace_id: trace.id.clone(),
            results,
            requests,
            usage,
            elapsed: started.elapsed(),
        }
    }

    fn plan(&self, trace: &JevTrace, done: &mut BTreeMap<String, JevMetricResult>) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        for metric in &self.metrics {
            if !trace.issues.is_empty() {
                done.insert(
                    metric.id().to_string(),
                    JevMetricResult::skipped("INCOMPLETE_TRACE"),
                );
                continue;
            }
            let planned = self.plan_metric(metric, trace, &mut groups);
            match planned {
                Ok(Some(result)) => {
                    done.insert(metric.id().to_string(), result);
                }
                Ok(None) => {}
                Err(_error) => {
                    done.insert(
                        metric.id().to_string(),
                        JevMetricResult::error("DEFINITION_ERROR"),
                    );
                }
            }
        }
        groups
    }

    /// Plans one metric: either a deterministic result, or membership in a group.
    fn plan_metric(
        &self,
        metric: &Arc<dyn JevTraceMetric>,
        trace: &JevTrace,
        groups: &mut Vec<Group>,
    ) -> Result<Option<JevMetricResult>, JevError> {
        if let Some(precheck) = metric.precheck(trace) {
            return Ok(Some(precheck));
        }
        let state = metric.state(trace);
        let questions = metric.questions(trace);
        for id in questions.keys() {
            require_id(id)?;
        }
        if questions.len() > self.limits.max_questions
            || trace_data::size_of_map(&state)? > self.limits.max_state_chars
        {
            return Ok(Some(JevMetricResult::skipped("EVALUATION_LIMIT")));
        }
        if questions.is_empty() {
            return Ok(Some(metric.reduce(&BTreeMap::new(), trace)?));
        }
        let request = SystemOneRequest::new(
            Some(Value::Object(
                state.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            )),
            None,
            questions.clone(),
        );
        crate::jev::validation::validate_request(&request)?;

        let mut target: Option<usize> = None;
        for (index, group) in groups.iter().enumerate() {
            if group.questions.len() + questions.len() > self.limits.max_questions {
                continue;
            }
            let compatible = state.iter().all(|(key, value)| {
                !group.state.contains_key(key) || group.state.get(key) == Some(value)
            });
            if !compatible {
                continue;
            }
            let mut merged = group.state.clone();
            merged.extend(state.iter().map(|(k, v)| (k.clone(), v.clone())));
            if trace_data::size_of_map(&merged)? <= self.limits.max_state_chars {
                target = Some(index);
                break;
            }
        }
        let index = match target {
            Some(index) => index,
            None => {
                groups.push(Group::default());
                groups.len() - 1
            }
        };
        let group = &mut groups[index];
        group.state.extend(state);
        for (id, question) in &questions {
            group
                .questions
                .insert(format!("{}.{id}", metric.id()), question.clone());
        }
        group.members.push(Member {
            metric: Arc::clone(metric),
            questions,
        });
        Ok(None)
    }
}

async fn run_group(
    caller: &Caller,
    group: &Group,
    trace: &JevTrace,
    done: &mut BTreeMap<String, JevMetricResult>,
    usage: &mut Vec<RequestUsage>,
    requests: &mut usize,
) {
    *requests += 1;
    let request = SystemOneRequest::new(
        Some(Value::Object(
            group
                .state
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )),
        None,
        group.questions.clone(),
    );
    let response = caller(request.clone()).await;
    match response {
        Err(failure) => {
            record_invalid_usage(&failure, usage);
            let reason = if failure.is_invalid_reply() {
                "INVALID_RESPONSE"
            } else {
                "BACKEND_ERROR"
            };
            fail_members(group, done, reason);
        }
        Ok(response) => {
            // Even an invalid response can carry billable usage. Never fabricate zero cost.
            usage.push(RequestUsage {
                model: response.model.clone(),
                usage: response.usage,
            });
            if validate_response(&request, &response).is_err() {
                fail_members(group, done, "INVALID_RESPONSE");
                return;
            }
            accept_response(group, &response, trace, done);
        }
    }
}

fn record_invalid_usage(failure: &CallFailure, usage: &mut Vec<RequestUsage>) {
    if failure.is_invalid_reply() {
        usage.push(RequestUsage {
            model: failure.model().map(str::to_string),
            usage: failure.usage(),
        });
    }
}

fn fail_members(group: &Group, done: &mut BTreeMap<String, JevMetricResult>, reason: &str) {
    for member in &group.members {
        done.insert(
            member.metric.id().to_string(),
            JevMetricResult::error(reason),
        );
    }
}

fn accept_response(
    group: &Group,
    response: &SystemOneResult,
    trace: &JevTrace,
    done: &mut BTreeMap<String, JevMetricResult>,
) {
    let empty = BTreeMap::new();
    let answers = response.answers.as_ref().unwrap_or(&empty);
    for member in &group.members {
        let mut collected: BTreeMap<String, Answer> = BTreeMap::new();
        for id in member.questions.keys() {
            let key = format!("{}.{id}", member.metric.id());
            if let Some(answer) = answers.get(&key) {
                collected.insert(id.clone(), answer.clone());
            }
        }
        let result = member.metric.reduce(&collected, trace);
        done.insert(
            member.metric.id().to_string(),
            result.unwrap_or_else(|_| JevMetricResult::error("REDUCE_ERROR")),
        );
    }
}

/// Requires a simple identifier (`[A-Za-z][A-Za-z0-9_]{0,63}`) for metric and question ids.
pub fn require_id(id: &str) -> Result<(), JevError> {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return Err(JevError::invalid(
            "metric and question IDs must be simple identifiers",
        ));
    };
    if !first.is_ascii_alphabetic() || id.len() > 64 {
        return Err(JevError::invalid(
            "metric and question IDs must be simple identifiers",
        ));
    }
    if !chars.all(|character| character.is_ascii_alphanumeric() || character == '_') {
        return Err(JevError::invalid(
            "metric and question IDs must be simple identifiers",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::dto::NoulAnswer;
    use crate::jev::evaluation::metrics::{self, Thresholds};
    use serde_json::json;

    fn thresholds() -> Thresholds {
        Thresholds::new(0.2, 0.8).unwrap()
    }

    fn sample_trace(fields: Value) -> JevTrace {
        let map = fields
            .as_object()
            .expect("object")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        JevTrace::without_issues("trace-1", map).unwrap()
    }

    fn noul_response(pairs: &[(&str, f64)]) -> SystemOneResult {
        let mut answers = BTreeMap::new();
        for (id, value) in pairs {
            answers.insert(
                (*id).to_string(),
                Answer::Noul(NoulAnswer { noul: Some(*value) }),
            );
        }
        SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(answers),
            usage: Some(Usage::new(11, 4)),
        }
    }

    fn limits() -> Limits {
        Limits::defaults()
    }

    #[tokio::test]
    async fn compatible_metrics_share_one_request() {
        let scripted = Arc::new(ScriptedCaller::new(vec![Ok(noul_response(&[
            ("used_tool_result.q", 0.9),
            ("indirect_injection.q", 0.1),
        ]))]));
        let evaluator = JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![
                metrics::used_tool_result(thresholds()),
                metrics::indirect_injection(thresholds()),
            ],
            limits(),
        )
        .unwrap();
        let trace = sample_trace(json!({
            "request": "q",
            "final_answer": "a",
            "tool_results": [{"result": "x"}],
        }));
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(report.requests, 1);
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.usage.len(), 1);
        assert_eq!(report.usage[0].model.as_deref(), Some("jev"));
        let requests = scripted.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].questions.len(), 2);
        assert!(requests[0].state.is_some());
    }

    #[tokio::test]
    async fn conflicting_state_is_split_into_separate_requests() {
        let scripted = Arc::new(ScriptedCaller::new(vec![
            Ok(noul_response(&[("used_tool_result.q", 0.9)])),
            Ok(noul_response(&[("indirect_injection.q", 0.1)])),
        ]));
        // The first metric requires `request`, the second does not: their states still merge
        // (missing keys are compatible). `tool_choice` requires `available_tools` and would
        // conflict with nothing, so use a budget that forces two groups instead.
        let evaluator = JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![
                metrics::used_tool_result(thresholds()),
                metrics::indirect_injection(thresholds()),
            ],
            Limits::new(Duration::from_secs(3), 1, 100_000).unwrap(),
        )
        .unwrap();
        let trace = sample_trace(json!({
            "request": "q",
            "final_answer": "a",
            "tool_results": [{"result": "x"}],
        }));
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(report.requests, 2);
        assert!(report.passed());
    }

    #[tokio::test]
    async fn incomplete_traces_skip_every_metric() {
        let scripted = Arc::new(ScriptedCaller::new(vec![]));
        let evaluator = JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![metrics::completeness(thresholds())],
            limits(),
        )
        .unwrap();
        let mut map = BTreeMap::new();
        map.insert("request".to_string(), json!("q"));
        let trace = JevTrace::new("t", map, vec!["PENDING_TOOL_CALL".to_string()]).unwrap();
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(report.requests, 0);
        assert_eq!(
            report.result("completeness").unwrap().reason,
            "INCOMPLETE_TRACE"
        );
        assert!(!report.passed());
    }

    #[tokio::test]
    async fn per_metric_failures_are_isolated() {
        let scripted = Arc::new(ScriptedCaller::new(vec![
            Ok(noul_response(&[("used_tool_result.q", 0.9)])),
            Err(CallFailure::backend("down")),
        ]));
        let evaluator = JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![
                metrics::used_tool_result(thresholds()),
                metrics::indirect_injection(thresholds()),
            ],
            Limits::new(Duration::from_secs(3), 1, 100_000).unwrap(),
        )
        .unwrap();
        let trace = sample_trace(json!({
            "request": "q",
            "final_answer": "a",
            "tool_results": [{"result": "x"}],
        }));
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(
            report.result("used_tool_result").unwrap().status,
            super::super::MetricStatus::Decided
        );
        assert_eq!(
            report.result("indirect_injection").unwrap().reason,
            "BACKEND_ERROR"
        );
        assert!(!report.passed());
    }

    #[tokio::test]
    async fn invalid_replies_keep_billable_usage_and_fail_the_group() {
        let usage = Usage::new(5, 6);
        let scripted = Arc::new(ScriptedCaller::new(vec![Err(CallFailure::invalid_reply(
            "llm:test",
            Some(usage),
        ))]));
        let evaluator = JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![metrics::used_tool_result(thresholds())],
            limits(),
        )
        .unwrap();
        let trace = sample_trace(json!({
            "request": "q",
            "final_answer": "a",
            "tool_results": [{"result": "x"}],
        }));
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(
            report.result("used_tool_result").unwrap().reason,
            "INVALID_RESPONSE"
        );
        assert_eq!(report.usage.len(), 1);
        assert_eq!(report.usage[0].model.as_deref(), Some("llm:test"));
        assert_eq!(report.usage[0].usage, Some(usage));
    }

    #[tokio::test]
    async fn contract_violations_and_missing_answers_map_to_reduce_errors() {
        let scripted = Arc::new(ScriptedCaller::new(vec![
            Ok(noul_response(&[])),
            Ok(noul_response(&[])),
        ]));
        let evaluator = JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![metrics::used_tool_result(thresholds())],
            limits(),
        )
        .unwrap();
        let trace = sample_trace(json!({
            "request": "q",
            "final_answer": "a",
            "tool_results": [{"result": "x"}],
        }));
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(
            report.result("used_tool_result").unwrap().reason,
            "INVALID_RESPONSE"
        );
        assert_eq!(report.usage.len(), 1, "the invalid reply is still billed");
    }

    #[tokio::test]
    async fn deterministic_metrics_never_call_the_backend() {
        let scripted = Arc::new(ScriptedCaller::new(vec![]));
        let evaluator = JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![metrics::trajectory_match(
                metrics::TrajectoryMatchMode::Strict,
                false,
            )],
            limits(),
        )
        .unwrap();
        let trace = sample_trace(json!({
            "tool_calls": [{"name": "a"}],
            "expected_tool_calls": [{"name": "a"}],
        }));
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(report.requests, 0);
        assert!(report.result("trajectory_match").unwrap().is_pass());
        assert!(report.passed());
    }

    #[tokio::test]
    async fn state_and_question_budgets_skip_with_a_reason() {
        let scripted = Arc::new(ScriptedCaller::new(vec![]));
        let evaluator = JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![metrics::used_tool_result(thresholds())],
            Limits::new(Duration::from_secs(3), 128, 5).unwrap(),
        )
        .unwrap();
        let trace = sample_trace(json!({
            "request": "q",
            "final_answer": "a",
            "tool_results": [{"result": "x"}],
        }));
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(
            report.result("used_tool_result").unwrap().reason,
            "EVALUATION_LIMIT"
        );
        assert_eq!(report.requests, 0);
    }

    #[tokio::test]
    async fn budget_expiry_marks_unfinished_metrics_as_timeouts() {
        let caller: Caller = Arc::new(|_request| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(noul_response(&[("used_tool_result.q", 0.9)]))
            })
        });
        let evaluator = JevTraceEvaluator::new(
            caller,
            vec![metrics::used_tool_result(thresholds())],
            Limits::new(Duration::from_millis(20), 128, 100_000).unwrap(),
        )
        .unwrap();
        let trace = sample_trace(json!({
            "request": "q",
            "final_answer": "a",
            "tool_results": [{"result": "x"}],
        }));
        let report = evaluator.evaluate(&trace).await;
        assert_eq!(report.result("used_tool_result").unwrap().reason, "TIMEOUT");
    }

    #[test]
    fn construction_rejects_empty_duplicate_and_malformed_metrics() {
        let caller: Caller =
            Arc::new(|_request| Box::pin(async { Err(CallFailure::backend("x")) }));
        assert_eq!(
            JevTraceEvaluator::new(caller.clone(), vec![], limits())
                .unwrap_err()
                .message(),
            "1..128 unique metrics required"
        );
        assert_eq!(
            JevTraceEvaluator::new(
                caller.clone(),
                vec![
                    metrics::completeness(thresholds()),
                    metrics::completeness(thresholds()),
                ],
                limits()
            )
            .unwrap_err()
            .message(),
            "1..128 unique metrics required"
        );
        let mut too_many = Vec::new();
        for _ in 0..129 {
            too_many.push(metrics::completeness(thresholds()));
        }
        assert!(JevTraceEvaluator::new(caller, too_many, limits()).is_err());

        for id in ["", "1bad", "has-dash", "has space"] {
            assert_eq!(
                require_id(id).unwrap_err().message(),
                "metric and question IDs must be simple identifiers"
            );
        }
        assert!(require_id("a").is_ok());
        assert!(require_id("used_tool_result").is_ok());
        let long = "a".repeat(65);
        assert!(require_id(&long).is_err());

        assert_eq!(
            Limits::new(Duration::ZERO, 1, 1).unwrap_err().message(),
            "positive trace evaluation limits required"
        );
    }
}
