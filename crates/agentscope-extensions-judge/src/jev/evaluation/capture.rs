//! The capture state machine behind `evaluation/JevTraceEvaluationMiddleware`.
//!
//! Java's middleware hooks (`onAgent`/`onModelCall`) belong to the core `MiddlewareBase` trait,
//! which this crate deliberately does not depend on. Everything the hooks *do* is ported here as a
//! plain state machine: [`TraceCapture`] observes model-call message snapshots, stops on the same
//! reasons and never retains mutable agent arguments. A host adapts its event stream onto these
//! methods (see `docs/extensions/JUDGE__GAP_TRACKER.md` §4.2).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::execution::{Decision, JevExecution, Mode, Options, Status};

use super::evaluator::{JevTraceEvaluator, TraceReport};
use super::trace::{JevTrace, TraceMessage, TraceToolSchema};
use super::trace_data;

/// The default capture limit Java uses (`JevTraceEvaluationMiddleware` default constructor).
pub const DEFAULT_MAX_CAPTURE_CHARS: usize = 100_000;

/// The middleware order Java declares (`order()`).
pub const MIDDLEWARE_ORDER: i32 = 300;

/// Captures one invocation's trace across model calls (`JevTraceEvaluationMiddleware.Capture`).
#[derive(Debug, Clone)]
pub struct TraceCapture {
    id: String,
    input_ids: Vec<Option<String>>,
    tool_snapshots: Vec<Value>,
    latest: Option<JevTrace>,
    stopped: Option<String>,
    finished: bool,
}

impl TraceCapture {
    /// Creates a capture for the messages the invocation started with.
    pub fn new(id: impl Into<String>, input: &[TraceMessage]) -> Self {
        Self {
            id: id.into(),
            input_ids: input.iter().map(|message| message.id.clone()).collect(),
            tool_snapshots: Vec::new(),
            latest: None,
            stopped: None,
            finished: false,
        }
    }

    /// The capture id (the trace id).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the capture stopped and why (Java `stopped`).
    pub fn stopped(&self) -> Option<&str> {
        self.stopped.as_deref()
    }

    /// Whether the invocation finished (Java `finished`).
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Observes the message list of one model call.
    ///
    /// The invocation boundary is the first message whose id the invocation started with; if it is
    /// gone (a compaction rewrote history) the trace is no longer evidence.
    pub fn observe(
        &mut self,
        messages: &[TraceMessage],
        tools: Option<&[TraceToolSchema]>,
        max_capture_chars: usize,
    ) {
        if self.stopped.is_some() {
            return;
        }
        let Some(begin) = messages
            .iter()
            .position(|message| self.input_ids.contains(&message.id))
        else {
            self.stop("INVOCATION_BOUNDARY_MISSING");
            return;
        };
        let window = &messages[begin..];
        let mut snapshot = match JevTrace::from_messages(self.id.clone(), window, tools) {
            Ok(snapshot) => snapshot,
            Err(_error) => {
                self.stop("CAPTURE_ERROR");
                return;
            }
        };
        if let Some(latest) = &self.latest {
            let previous = latest.list("tool_calls");
            let current = snapshot.list("tool_calls");
            if !previous.iter().all(|call| current.contains(call)) {
                self.stop("TRACE_CHANGED_OR_COMPACTED");
                return;
            }
        }
        self.tool_snapshots.push(json!({
            "after_message_count": window.len(),
            "tools": snapshot.fields.get("available_tools").cloned().unwrap_or(Value::Null),
        }));
        snapshot = snapshot.with("available_tool_snapshots", Value::Array(self.tool_snapshots.clone()));
        match trace_data::size(&Value::Object(
            snapshot
                .fields
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )) {
            Ok(size) if size <= max_capture_chars => self.latest = Some(snapshot),
            Ok(_) => self.stop("CAPTURE_LIMIT"),
            Err(_) => self.stop("CAPTURE_ERROR"),
        }
    }

    /// Records the final result message.
    pub fn finish(&mut self, result: &TraceMessage, max_capture_chars: usize) {
        self.finished = true;
        if self.stopped.is_some() || self.latest.is_none() {
            return;
        }
        let final_part = match JevTrace::from_messages(self.id.clone(), &[result.clone()], None) {
            Ok(part) => part,
            Err(_error) => {
                self.stop("CAPTURE_ERROR");
                return;
            }
        };
        if !final_part.issues.is_empty() {
            self.stop("UNSUPPORTED_FINAL_RESULT");
            return;
        }
        let latest = self
            .latest
            .take()
            .expect("checked above")
            .with("final_answer", json!(result.text));
        let size = trace_data::size(&Value::Object(
            latest
                .fields
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ));
        match size {
            Ok(size) if size <= max_capture_chars => self.latest = Some(latest),
            Ok(_) => self.stop("CAPTURE_LIMIT"),
            Err(_) => self.stop("CAPTURE_ERROR"),
        }
    }

    fn stop(&mut self, reason: &str) {
        self.stopped = Some(reason.to_string());
        self.latest = None;
        self.tool_snapshots.clear();
    }

    /// The trace to evaluate; a stopped or unfinished capture becomes an explicit issue.
    pub fn trace(&self) -> JevTrace {
        let empty = BTreeMap::new();
        if let Some(reason) = &self.stopped {
            return JevTrace::new(self.id.clone(), empty, vec![reason.clone()])
                .expect("capture ids are non-blank");
        }
        match (&self.finished, &self.latest) {
            (true, Some(latest)) => latest.clone(),
            _ => JevTrace::new(self.id.clone(), empty, vec!["INCOMPLETE_INVOCATION".to_string()])
                .expect("capture ids are non-blank"),
        }
    }
}

/// Post-run observation configuration (`JevTraceEvaluationMiddleware` constructor).
///
/// `ENFORCE` is rejected: post-run observation cannot revoke already executed actions.
pub struct JevTraceEvaluationMiddleware {
    evaluator: Arc<JevTraceEvaluator>,
    execution: JevExecution,
    max_capture_chars: usize,
    observer: Arc<dyn Fn(&TraceReport) + Send + Sync>,
}

impl std::fmt::Debug for JevTraceEvaluationMiddleware {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevTraceEvaluationMiddleware")
            .field("mode", &self.execution.mode())
            .field("max_capture_chars", &self.max_capture_chars)
            .finish_non_exhaustive()
    }
}

impl JevTraceEvaluationMiddleware {
    /// Creates the middleware over an evaluator and execution options.
    pub fn new(
        evaluator: Arc<JevTraceEvaluator>,
        options: Options,
        max_capture_chars: usize,
        observer: Arc<dyn Fn(&TraceReport) + Send + Sync>,
    ) -> Result<Self, JevError> {
        if options.mode == Mode::Enforce {
            return Err(JevError::invalid(
                "post-run trace evaluation supports OFF/SHADOW; use pre-execution guards for enforcement",
            ));
        }
        if max_capture_chars < 1 {
            return Err(JevError::invalid("positive capture limit required"));
        }
        Ok(Self {
            evaluator,
            execution: JevExecution::new("trace_evaluation", options),
            max_capture_chars,
            observer,
        })
    }

    /// The Java default: shadow mode? No — Java's single-argument constructor uses
    /// `Options.disabled()` (OFF) and a no-op observer.
    pub fn with_defaults(evaluator: Arc<JevTraceEvaluator>) -> Result<Self, JevError> {
        Self::new(
            evaluator,
            Options::disabled(),
            DEFAULT_MAX_CAPTURE_CHARS,
            Arc::new(|_report| {}),
        )
    }

    /// The middleware order Java declares.
    pub fn order(&self) -> i32 {
        MIDDLEWARE_ORDER
    }

    /// Whether and how the middleware participates.
    pub fn mode(&self) -> Mode {
        self.execution.mode()
    }

    /// The capture limit.
    pub fn max_capture_chars(&self) -> usize {
        self.max_capture_chars
    }

    /// Starts a capture for an invocation's input messages.
    pub fn capture(&self, id: impl Into<String>, input: &[TraceMessage]) -> TraceCapture {
        TraceCapture::new(id, input)
    }

    /// Observes a model call on an existing capture.
    pub fn observe(
        &self,
        capture: &mut TraceCapture,
        messages: &[TraceMessage],
        tools: Option<&[TraceToolSchema]>,
    ) {
        capture.observe(messages, tools, self.max_capture_chars);
    }

    /// Records the final result on an existing capture.
    pub fn finish(&self, capture: &mut TraceCapture, result: &TraceMessage) {
        capture.finish(result, self.max_capture_chars);
    }

    /// Evaluates the captured trace and reports it (`onAgent`'s bounded post-run step).
    ///
    /// `OFF` returns a skipped decision without evaluating. Observer failures are swallowed:
    /// observation must never change the agent's outcome.
    pub async fn evaluate(&self, capture: &TraceCapture) -> Decision<TraceReport> {
        if self.execution.mode() == Mode::Off {
            return self.execution.disabled_decision();
        }
        let trace = capture.trace();
        let report = self.evaluator.evaluate(&trace).await;
        (self.observer)(&report);
        let mut recommendation = BTreeMap::new();
        recommendation.insert("metrics".to_string(), report.results.len().to_string());
        recommendation.insert("passed".to_string(), report.passed().to_string());
        Decision::new(
            Status::Decided,
            Some(report),
            "EVALUATED",
            recommendation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::dto::{Answer, NoulAnswer, SystemOneResult, Usage};
    use crate::jev::evaluation::evaluator::Limits;
    use crate::jev::evaluation::metrics::{self, Thresholds};
    use crate::jev::evaluation::trace::{TraceBlock, TraceToolResult, TraceToolUse, TraceResultPart};
    use std::sync::Mutex;

    fn tool_use(id: &str) -> TraceBlock {
        TraceBlock::ToolUse(TraceToolUse {
            id: Some(id.to_string()),
            name: "read".to_string(),
            arguments: json!({"path": "a.rs"}),
        })
    }

    fn tool_result(id: &str) -> TraceBlock {
        TraceBlock::ToolResult(TraceToolResult {
            id: Some(id.to_string()),
            name: "read".to_string(),
            parts: vec![TraceResultPart::Text("body".to_string())],
            state: Some("SUCCESS".to_string()),
            metadata: BTreeMap::new(),
        })
    }

    fn base_messages() -> Vec<TraceMessage> {
        vec![TraceMessage::user("read a.rs").with_id("m1")]
    }

    fn call_messages() -> Vec<TraceMessage> {
        let mut assistant = TraceMessage::new("ASSISTANT", "");
        assistant.id = Some("m2".to_string());
        assistant.blocks = vec![tool_use("c1"), tool_result("c1")];
        let mut messages = base_messages();
        messages.push(assistant);
        messages
    }

    fn capture_with_history() -> TraceCapture {
        let mut capture = TraceCapture::new("trace-1", &base_messages());
        capture.observe(&call_messages(), None, DEFAULT_MAX_CAPTURE_CHARS);
        capture
    }

    #[test]
    fn a_complete_invocation_produces_evidence() {
        let mut capture = capture_with_history();
        capture.finish(
            &TraceMessage::assistant("done").with_id("m3"),
            DEFAULT_MAX_CAPTURE_CHARS,
        );
        let trace = capture.trace();
        assert!(trace.issues.is_empty(), "{:?}", trace.issues);
        assert_eq!(trace.text("request"), "read a.rs");
        assert_eq!(trace.text("final_answer"), "done");
        assert_eq!(trace.list("tool_calls").len(), 1);
        assert_eq!(trace.list("available_tool_snapshots").len(), 1);
        assert_eq!(
            trace.list("available_tool_snapshots")[0]["after_message_count"],
            json!(2)
        );
        assert!(capture.finished());
        assert_eq!(capture.stopped(), None);
    }

    #[test]
    fn a_missing_boundary_stops_the_capture() {
        let mut capture = TraceCapture::new("trace-1", &base_messages());
        capture.observe(
            &[TraceMessage::user("other").with_id("zzz")],
            None,
            DEFAULT_MAX_CAPTURE_CHARS,
        );
        assert_eq!(capture.stopped(), Some("INVOCATION_BOUNDARY_MISSING"));
        assert_eq!(
            capture.trace().issues,
            vec!["INVOCATION_BOUNDARY_MISSING".to_string()]
        );
    }

    #[test]
    fn a_compacted_trace_stops_the_capture() {
        let mut capture = capture_with_history();
        let mut compacted = base_messages();
        let mut assistant = TraceMessage::new("ASSISTANT", "");
        assistant.id = Some("m2".to_string());
        assistant.blocks = vec![tool_use("c2"), tool_result("c2")];
        compacted.push(assistant);
        capture.observe(&compacted, None, DEFAULT_MAX_CAPTURE_CHARS);
        assert_eq!(capture.stopped(), Some("TRACE_CHANGED_OR_COMPACTED"));
    }

    #[test]
    fn capture_limits_and_unsupported_final_results_are_reported() {
        let mut capture = TraceCapture::new("trace-1", &base_messages());
        capture.observe(&call_messages(), None, 10);
        assert_eq!(capture.stopped(), Some("CAPTURE_LIMIT"));

        let mut capture = capture_with_history();
        let mut final_message = TraceMessage::new("ASSISTANT", "");
        final_message.id = Some("m3".to_string());
        final_message.blocks = vec![TraceBlock::Other];
        capture.finish(&final_message, DEFAULT_MAX_CAPTURE_CHARS);
        assert_eq!(capture.stopped(), Some("UNSUPPORTED_FINAL_RESULT"));
    }

    #[test]
    fn an_unfinished_capture_is_incomplete_evidence() {
        let capture = capture_with_history();
        assert_eq!(
            capture.trace().issues,
            vec!["INCOMPLETE_INVOCATION".to_string()]
        );
        assert!(!capture.finished());
    }

    fn evaluator() -> Arc<JevTraceEvaluator> {
        let scripted = Arc::new(ScriptedCaller::new(vec![Ok(SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(BTreeMap::from([(
                "used_tool_result.q".to_string(),
                Answer::Noul(NoulAnswer { noul: Some(0.9) }),
            )])),
            usage: Some(Usage::new(3, 3)),
        })]));
        Arc::new(
            JevTraceEvaluator::new(
                scripted.as_caller(),
                vec![metrics::used_tool_result(
                    Thresholds::new(0.2, 0.8).unwrap(),
                )],
                Limits::defaults(),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn shadow_middleware_evaluates_and_reports() {
        let observed: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&observed);
        let options = Options::new(Mode::Shadow, std::time::Duration::from_secs(2), "v1").unwrap();
        let middleware = JevTraceEvaluationMiddleware::new(
            evaluator(),
            options,
            DEFAULT_MAX_CAPTURE_CHARS,
            Arc::new(move |report| sink.lock().unwrap().push(report.passed())),
        )
        .unwrap();
        assert_eq!(middleware.order(), 300);
        assert_eq!(middleware.mode(), Mode::Shadow);
        assert_eq!(middleware.max_capture_chars(), DEFAULT_MAX_CAPTURE_CHARS);

        let mut capture = middleware.capture("trace-1", &base_messages());
        middleware.observe(&mut capture, &call_messages(), None);
        middleware.finish(&mut capture, &TraceMessage::assistant("done").with_id("m3"));
        let decision = middleware.evaluate(&capture).await;
        assert_eq!(decision.status, Status::Decided);
        assert_eq!(decision.reason, "EVALUATED");
        assert_eq!(decision.recommendation["metrics"], "1");
        assert_eq!(decision.recommendation["passed"], "true");
        assert_eq!(observed.lock().unwrap().as_slice(), &[true]);
    }

    #[tokio::test]
    async fn off_middleware_never_evaluates() {
        let middleware = JevTraceEvaluationMiddleware::with_defaults(evaluator()).unwrap();
        assert_eq!(middleware.mode(), Mode::Off);
        let capture = middleware.capture("trace-1", &base_messages());
        let decision = middleware.evaluate(&capture).await;
        assert_eq!(decision.status, Status::Skipped);
        assert!(decision.value.is_none());
    }

    #[test]
    fn enforce_and_non_positive_limits_are_rejected() {
        let options = Options::new(
            Mode::Enforce,
            std::time::Duration::from_secs(1),
            "v1",
        )
        .unwrap();
        assert_eq!(
            JevTraceEvaluationMiddleware::new(
                evaluator(),
                options,

                DEFAULT_MAX_CAPTURE_CHARS,
                Arc::new(|_report| {}),
            )
            .unwrap_err()
            .message(),
            "post-run trace evaluation supports OFF/SHADOW; use pre-execution guards for enforcement"
        );
        assert_eq!(
            JevTraceEvaluationMiddleware::new(
                evaluator(),
                Options::disabled(),
                0,
                Arc::new(|_report| {}),
            )
            .unwrap_err()
            .message(),
            "positive capture limit required"
        );
    }
}
