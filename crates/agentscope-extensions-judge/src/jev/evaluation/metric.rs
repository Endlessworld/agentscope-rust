//! The reusable metric contract (`evaluation/JevTraceMetric`).
//!
//! A metric packs three steps — deterministic `precheck`, the `state`/`questions` handed to
//! System One, and the `reduce` that turns answers into a [`JevMetricResult`]. Java returns `null`
//! from `precheck` to mean "continue with the questions"; here that is `None`.
//!
//! Deviation: Java's `reduce` may throw, which the evaluator maps to `REDUCE_ERROR`. Silently
//! swallowing a panic in Rust is worse than a typed error, so `reduce` returns `Result`.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::JevError;
use crate::jev::dto::{Answer, Question};

use super::metric_result::JevMetricResult;
use super::trace::JevTrace;

/// A reusable state/questions/reduce definition, following jevals' `Eval` contract.
pub trait JevTraceMetric: Send + Sync {
    /// The metric id; a simple identifier used as a question-id prefix.
    fn id(&self) -> &str;

    /// The metric version, reported next to every entry.
    fn version(&self) -> String;

    /// Returns a skipped or deterministic result, or `None` to continue with the questions.
    fn precheck(&self, _trace: &JevTrace) -> Option<JevMetricResult> {
        None
    }

    /// The bounded state sent to System One.
    fn state(&self, trace: &JevTrace) -> BTreeMap<String, Value>;

    /// The questions to ask; empty means `reduce` decides without a call.
    fn questions(&self, trace: &JevTrace) -> BTreeMap<String, Question>;

    /// Folds the answers into a result.
    fn reduce(
        &self,
        answers: &BTreeMap<String, Answer>,
        trace: &JevTrace,
    ) -> Result<JevMetricResult, JevError>;
}
