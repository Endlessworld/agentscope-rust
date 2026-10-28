//! Fixed trace datasets with separate metric/scenario denominators and request-level accounting
//! (`evaluation/JevTraceRunner`).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures::StreamExt;

use crate::error::JevError;

use super::evaluator::{JevTraceEvaluator, TraceReport};
use super::metric_result::MetricStatus;
use super::trace::JevTrace;

/// What a case expects for one metric (`JevTraceRunner.Expected`).
///
/// Exactly one of `passed`/`label` is set: a verdict or a classification label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    /// The expected pass/fail verdict.
    pub passed: Option<bool>,
    /// The expected classification label.
    pub label: Option<String>,
}

impl Expected {
    /// Creates and validates an expectation.
    pub fn new(passed: Option<bool>, label: Option<String>) -> Result<Self, JevError> {
        if passed.is_some() == label.is_some() {
            return Err(JevError::invalid("exactly one expected value required"));
        }
        Ok(Self { passed, label })
    }

    /// An expectation of a pass/fail verdict.
    pub fn passed(passed: bool) -> Self {
        Self {
            passed: Some(passed),
            label: None,
        }
    }

    /// An expectation of a classification label.
    pub fn label(label: impl Into<String>) -> Self {
        Self {
            passed: None,
            label: Some(label.into()),
        }
    }

    /// Whether the metric's result matches this expectation.
    pub fn matches(&self, status: MetricStatus, passed: Option<bool>, label: Option<&str>) -> Option<bool> {
        if status != MetricStatus::Decided {
            return None;
        }
        let (actual, target): (Option<String>, Option<String>) = if self.label.is_some() {
            (label.map(str::to_string), self.label.clone())
        } else {
            (
                passed.map(|value| value.to_string()),
                self.passed.map(|value| value.to_string()),
            )
        };
        match actual {
            None => None,
            Some(actual) => Some(Some(&actual) == target.as_ref()),
        }
    }
}

/// One labelled trace case.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceCase {
    /// The scenario this case belongs to.
    pub scenario: String,
    /// The trace to evaluate.
    pub trace: JevTrace,
    /// Expectations per metric id.
    pub expected: BTreeMap<String, Expected>,
}

impl TraceCase {
    /// Creates and validates a case (`Case` compact constructor).
    pub fn new(
        scenario: impl Into<String>,
        trace: JevTrace,
        expected: BTreeMap<String, Expected>,
    ) -> Result<Self, JevError> {
        let scenario = scenario.into();
        if scenario.trim().is_empty() {
            return Err(JevError::invalid("scenario required"));
        }
        Ok(Self {
            scenario,
            trace,
            expected,
        })
    }
}

/// One evaluated case.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceRow {
    /// The scenario this row belongs to.
    pub scenario: String,
    /// The trace id.
    pub trace_id: String,
    /// The case's expectations.
    pub expected: BTreeMap<String, Expected>,
    /// The full evaluation report.
    pub report: TraceReport,
}

/// Per-metric counts (`Summary`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// How many results were observed.
    pub total: usize,
    /// How many carried an expectation.
    pub labelled: usize,
    /// How many matched their expectation.
    pub correct: usize,
    /// How many contradicted their expectation.
    pub incorrect: usize,
    /// How many were inconclusive.
    pub inconclusive: usize,
    /// How many were skipped.
    pub skipped: usize,
    /// How many errored.
    pub errors: usize,
    /// How many decisions carried no comparable value.
    pub unscored: usize,
}

impl Summary {
    /// `correct / labelled`, or `0` when nothing was labelled.
    pub fn accuracy(&self) -> f64 {
        if self.labelled == 0 {
            0.0
        } else {
            self.correct as f64 / self.labelled as f64
        }
    }
}

/// Latency percentiles over the per-trace evaluation time (`Latency`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Latency {
    /// Median evaluation time, in milliseconds.
    pub p50_millis: f64,
    /// 95th percentile evaluation time, in milliseconds.
    pub p95_millis: f64,
}

/// Request-level accounting (`Usage`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceUsage {
    /// How many System One requests were sent.
    pub requests: usize,
    /// How many of them reported usable counters.
    pub responses_with_usage: usize,
    /// Summed input tokens of the usable counters.
    pub input_tokens: i64,
    /// Summed output tokens of the usable counters.
    pub output_tokens: i64,
}

/// The runner report (`JevTraceRunner.Report`).
#[derive(Debug, Clone, PartialEq)]
pub struct RunnerReport {
    /// One row per case, in case order.
    pub rows: Vec<TraceRow>,
    /// Per-metric summaries across all scenarios.
    pub metrics: BTreeMap<String, Summary>,
    /// Per-scenario, per-metric summaries.
    pub scenarios: BTreeMap<String, BTreeMap<String, Summary>>,
    /// Evaluation-time percentiles.
    pub latency: Latency,
    /// Request-level usage.
    pub usage: TraceUsage,
    /// Total wall-clock time.
    pub elapsed: Duration,
}

/// Runs a fixed trace dataset through a [`JevTraceEvaluator`].
#[derive(Debug, Default, Clone, Copy)]
pub struct JevTraceRunner;

impl JevTraceRunner {
    /// Evaluates every case, keeping case order (`flatMapSequential` with `concurrency`).
    ///
    /// Rejects more than 10000 cases, duplicate trace ids, a concurrency outside `1..=32` and
    /// expectations that do not refer to a configured metric.
    pub async fn run(
        &self,
        evaluator: &JevTraceEvaluator,
        cases: Vec<TraceCase>,
        concurrency: usize,
    ) -> Result<RunnerReport, JevError> {
        if !(1..=32).contains(&concurrency)
            || cases.len() > 10_000
            || has_duplicate_trace_ids(&cases)
        {
            return Err(JevError::invalid(
                "<=10000 unique trace IDs and concurrency 1..32 required",
            ));
        }
        let metric_ids = evaluator.metric_ids();
        for case in &cases {
            if !case
                .expected
                .keys()
                .all(|key| metric_ids.contains(key))
            {
                return Err(JevError::invalid(
                    "expectations must refer to configured metrics",
                ));
            }
        }

        let started = Instant::now();
        let evaluated = futures::stream::iter(cases)
            .map(|case| async move {
                let report = evaluator.evaluate(&case.trace).await;
                TraceRow {
                    scenario: case.scenario,
                    trace_id: case.trace.id,
                    expected: case.expected,
                    report,
                }
            })
            .buffered(concurrency)
            .collect::<Vec<TraceRow>>()
            .await;
        Ok(report(evaluated, started.elapsed()))
    }
}

fn has_duplicate_trace_ids(cases: &[TraceCase]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    cases.iter().any(|case| !seen.insert(case.trace.id.as_str()))
}

fn report(rows: Vec<TraceRow>, elapsed: Duration) -> RunnerReport {
    let mut grouped: Vec<(String, Vec<TraceRow>)> = Vec::new();
    for row in &rows {
        match grouped.iter_mut().find(|(key, _)| key == &row.scenario) {
            Some((_, bucket)) => bucket.push(row.clone()),
            None => grouped.push((row.scenario.clone(), vec![row.clone()])),
        }
    }
    let scenarios: BTreeMap<String, BTreeMap<String, Summary>> = grouped
        .into_iter()
        .map(|(scenario, bucket)| (scenario, summarize(&bucket)))
        .collect();

    let mut times: Vec<f64> = rows
        .iter()
        .map(|row| row.report.elapsed.as_secs_f64() * 1000.0)
        .collect();
    times.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));

    let mut usage = TraceUsage::default();
    for row in &rows {
        usage.requests += row.report.requests;
        for call in &row.report.usage {
            if let Some(counters) = call.usage {
                if counters.input_tokens >= 0 && counters.output_tokens >= 0 {
                    usage.responses_with_usage += 1;
                    usage.input_tokens += counters.input_tokens;
                    usage.output_tokens += counters.output_tokens;
                }
            }
        }
    }

    RunnerReport {
        metrics: summarize(&rows),
        scenarios,
        latency: Latency {
            p50_millis: percentile(&times, 0.5),
            p95_millis: percentile(&times, 0.95),
        },
        usage,
        elapsed,
        rows,
    }
}

fn summarize(rows: &[TraceRow]) -> BTreeMap<String, Summary> {
    let mut summaries: BTreeMap<String, Summary> = BTreeMap::new();
    for row in rows {
        for entry in &row.report.results {
            let summary = summaries.entry(entry.id.clone()).or_default();
            summary.total += 1;
            let expected = row.expected.get(&entry.id);
            if expected.is_some() {
                summary.labelled += 1;
            }
            let Some(result) = &entry.result else {
                summary.errors += 1;
                continue;
            };
            match result.status {
                MetricStatus::Inconclusive => summary.inconclusive += 1,
                MetricStatus::Skipped => summary.skipped += 1,
                MetricStatus::Error => summary.errors += 1,
                MetricStatus::Decided => {
                    if let Some(expected) = expected {
                        match expected.matches(
                            result.status,
                            result.passed,
                            result.label.as_deref(),
                        ) {
                            None => summary.unscored += 1,
                            Some(true) => summary.correct += 1,
                            Some(false) => summary.incorrect += 1,
                        }
                    }
                }
            }
        }
    }
    summaries
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((p * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::dto::{Answer, NoulAnswer, SystemOneResult, Usage};
    use crate::jev::evaluation::evaluator::Limits;
    use crate::jev::evaluation::metrics::{self, Thresholds};
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn sample_trace(id: &str, fields: Value) -> JevTrace {
        let map = fields
            .as_object()
            .expect("object")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        JevTrace::without_issues(id, map).unwrap()
    }

    fn noul_response(value: f64, usage: Option<Usage>) -> SystemOneResult {
        let mut answers = BTreeMap::new();
        answers.insert(
            "used_tool_result.q".to_string(),
            Answer::Noul(NoulAnswer {
                noul: Some(value),
            }),
        );
        SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(answers),
            usage,
        }
    }

    fn evaluator(scripted: &Arc<ScriptedCaller>) -> JevTraceEvaluator {
        JevTraceEvaluator::new(
            scripted.as_caller(),
            vec![metrics::used_tool_result(
                Thresholds::new(0.2, 0.8).unwrap(),
            )],
            Limits::defaults(),
        )
        .unwrap()
    }

    fn fields() -> Value {
        json!({
            "request": "q",
            "final_answer": "a",
            "tool_results": [{"result": "x"}],
        })
    }

    #[tokio::test]
    async fn per_metric_and_per_scenario_summaries_are_separate() {
        let scripted = Arc::new(ScriptedCaller::new(vec![
            Ok(noul_response(0.9, Some(Usage::new(10, 2)))),
            Ok(noul_response(0.1, Some(Usage::new(20, 3)))),
        ]));
        let evaluator = evaluator(&scripted);
        let mut expected = BTreeMap::new();
        expected.insert("used_tool_result".to_string(), Expected::passed(true));
        let cases = vec![
            TraceCase::new("good", sample_trace("t1", fields()), expected.clone()).unwrap(),
            TraceCase::new("bad", sample_trace("t2", fields()), expected).unwrap(),
        ];
        let report = JevTraceRunner.run(&evaluator, cases, 2).await.unwrap();

        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].trace_id, "t1");
        assert_eq!(report.rows[1].trace_id, "t2");

        let overall = &report.metrics["used_tool_result"];
        assert_eq!(overall.total, 2);
        assert_eq!(overall.labelled, 2);
        assert_eq!(overall.correct, 1);
        assert_eq!(overall.incorrect, 1);
        assert_eq!(overall.accuracy(), 0.5);

        assert_eq!(report.scenarios["good"]["used_tool_result"].correct, 1);
        assert_eq!(report.scenarios["bad"]["used_tool_result"].incorrect, 1);
        assert_eq!(report.usage.requests, 2);
        assert_eq!(report.usage.responses_with_usage, 2);
        assert_eq!(report.usage.input_tokens, 30);
        assert_eq!(report.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn unlabelled_inconclusive_and_label_expectations_are_counted_separately() {
        let scripted = Arc::new(ScriptedCaller::new(vec![
            Ok(noul_response(0.5, Some(Usage::new(0, 0)))),
            Ok(noul_response(0.9, Some(Usage::new(10, 2)))),
        ]));
        let evaluator = evaluator(&scripted);
        let cases = vec![
            TraceCase::new("s", sample_trace("t1", fields()), BTreeMap::new()).unwrap(),
            TraceCase::new(
                "s",
                sample_trace("t2", fields()),
                BTreeMap::from([("used_tool_result".to_string(), Expected::passed(true))]),
            )
            .unwrap(),
        ];
        let report = JevTraceRunner.run(&evaluator, cases, 1).await.unwrap();
        let summary = &report.metrics["used_tool_result"];
        assert_eq!(summary.total, 2);
        assert_eq!(summary.labelled, 1);
        assert_eq!(summary.inconclusive, 1);
        assert_eq!(summary.correct, 1);
        assert_eq!(report.usage.responses_with_usage, 2);
        assert_eq!(report.usage.input_tokens, 10);
        assert_eq!(report.usage.output_tokens, 2);
    }

    #[tokio::test]
    async fn invalid_datasets_are_rejected_before_any_call() {
        let scripted = Arc::new(ScriptedCaller::new(vec![]));
        let evaluator = evaluator(&scripted);
        let case = TraceCase::new("s", sample_trace("t1", fields()), BTreeMap::new()).unwrap();

        assert_eq!(
            JevTraceRunner
                .run(&evaluator, vec![case.clone()], 0)
                .await
                .unwrap_err()
                .message(),
            "<=10000 unique trace IDs and concurrency 1..32 required"
        );
        assert_eq!(
            JevTraceRunner
                .run(&evaluator, vec![case.clone(), case], 1)
                .await
                .unwrap_err()
                .message(),
            "<=10000 unique trace IDs and concurrency 1..32 required"
        );
        let unknown = TraceCase::new(
            "s",
            sample_trace("t3", fields()),
            BTreeMap::from([("other".to_string(), Expected::passed(true))]),
        )
        .unwrap();
        assert_eq!(
            JevTraceRunner
                .run(&evaluator, vec![unknown], 1)
                .await
                .unwrap_err()
                .message(),
            "expectations must refer to configured metrics"
        );
    }

    #[test]
    fn expectations_require_exactly_one_value() {
        assert_eq!(
            Expected::new(None, None).unwrap_err().message(),
            "exactly one expected value required"
        );
        assert_eq!(
            Expected::new(Some(true), Some("x".to_string()))
                .unwrap_err()
                .message(),
            "exactly one expected value required"
        );
        let passed = Expected::passed(true);
        assert_eq!(
            passed.matches(MetricStatus::Decided, Some(true), None),
            Some(true)
        );
        assert_eq!(
            passed.matches(MetricStatus::Decided, Some(false), None),
            Some(false)
        );
        assert_eq!(passed.matches(MetricStatus::Inconclusive, None, None), None);
        assert_eq!(passed.matches(MetricStatus::Decided, None, None), None);
        let label = Expected::label("correct");
        assert_eq!(
            label.matches(MetricStatus::Decided, Some(false), Some("correct")),
            Some(true)
        );
        assert_eq!(
            Expected::label("a").matches(MetricStatus::Decided, None, None),
            None
        );
        assert_eq!(
            TraceCase::new(" ", sample_trace("t", fields()), BTreeMap::new())
                .unwrap_err()
                .message(),
            "scenario required"
        );
    }

    #[test]
    fn percentiles_and_accuracy_follow_the_java_definition() {
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[7.0], 0.5), 7.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.5), 2.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.95), 4.0);

        let empty = Summary::default();
        assert_eq!(empty.accuracy(), 0.0);
        let summary = Summary {
            labelled: 4,
            correct: 3,
            ..Summary::default()
        };
        assert_eq!(summary.accuracy(), 0.75);
    }
}
