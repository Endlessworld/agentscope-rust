//! JEV evaluation: traces, metrics and runners (`io.agentscope.extensions.judge.jev.evaluation`).
//!
//! Sub-modules, in dependency order:
//!
//! * [`trace_data`] — JSON snapshot/size helpers for evaluation state.
//! * [`metric_result`] — the per-metric result type and its invariants.
//! * [`metric`] — the reusable `state/questions/reduce` metric contract.
//! * [`trace`] — the invocation-scoped trace and its message fold.
//! * [`metrics`] — the thresholds and the concrete agent/quality metrics.
//! * [`evaluator`] — request packing, grouping and per-metric failure states.
//! * [`runner`] — labelled trace datasets, per-metric summaries, latency and usage.
//! * [`evaluation_runner`] — the labelled single-question `Evaluator` SPI and its runner.
//! * [`text_backend`] — the chat-completion JSON adapter for offline evaluation.
//! * [`capture`] — the middleware capture state machine.
//!
//! Java additionally contains `JevTraceEvaluationMiddleware`, whose reactive `MiddlewareBase`
//! hooks are not ported (see `docs/extensions/JUDGE__GAP_TRACKER.md` §4.2); the state machine it
//! drives *is* ported as [`capture::TraceCapture`].

pub mod capture;
pub mod evaluation_runner;
pub mod evaluator;
pub mod metric;
pub mod metric_result;
pub mod metrics;
pub mod runner;
pub mod text_backend;
pub mod trace;
pub mod trace_data;

pub use capture::{JevTraceEvaluationMiddleware, TraceCapture};
pub use evaluation_runner::{
    EvaluationCase, EvaluationReport, EvaluationRow, EvaluationSummary, Evaluator, JevEvaluationRunner,
    JevEvaluator, Request, Response,
};
pub use evaluator::{Entry, JevTraceEvaluator, Limits, RequestUsage, TraceReport};
pub use metric::JevTraceMetric;
pub use metric_result::{JevMetricResult, MetricStatus};
pub use metrics::{
    agent_metrics, answer_relevancy, claims, completeness, grounded, indirect_injection,
    split_sentences, stayed_in_scope, tool_choice, trajectory_match, used_tool_result, Thresholds,
    TrajectoryMatchMode,
};
pub use runner::{
    Expected, JevTraceRunner, Latency, RunnerReport, TraceCase, TraceRow, TraceUsage,
};
pub use text_backend::{JevTextBackend, TextBackendError};
pub use trace::{
    JevTrace, TraceBlock, TraceMessage, TraceResultPart, TraceToolResult,
    TraceToolSchema, TraceToolUse,
};
