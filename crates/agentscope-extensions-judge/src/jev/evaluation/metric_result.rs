//! The per-metric result type (`evaluation/JevMetricResult`).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::JevError;

/// Whether a metric decided anything (`JevMetricResult.Status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricStatus {
    /// The metric produced a score (and maybe a pass/fail verdict).
    Decided,
    /// The evidence was insufficient to decide.
    Inconclusive,
    /// The metric did not run.
    Skipped,
    /// The metric failed.
    Error,
}

impl MetricStatus {
    /// The Java enum name, used as the default `reason` and in runner summaries.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Decided => "DECIDED",
            Self::Inconclusive => "INCONCLUSIVE",
            Self::Skipped => "SKIPPED",
            Self::Error => "ERROR",
        }
    }
}

/// One metric's outcome.
///
/// Classification labels are preserved; `passed` is absent unless a metric defines a policy.
#[derive(Debug, Clone, PartialEq)]
pub struct JevMetricResult {
    /// What kind of outcome this is.
    pub status: MetricStatus,
    /// A score in `[0, 1]`, when the metric produced one.
    pub score: Option<f64>,
    /// A pass/fail verdict; only legal for [`MetricStatus::Decided`].
    pub passed: Option<bool>,
    /// An optional classification label.
    pub label: Option<String>,
    /// Human readable reason; never blank.
    pub reason: String,
    /// Structured evidence backing the result.
    pub evidence: BTreeMap<String, Value>,
}

impl JevMetricResult {
    /// Creates and validates a result (`JevMetricResult` compact constructor).
    pub fn new(
        status: MetricStatus,
        score: Option<f64>,
        passed: Option<bool>,
        label: Option<String>,
        reason: impl Into<String>,
        evidence: BTreeMap<String, Value>,
    ) -> Result<Self, JevError> {
        if let Some(score) = score {
            if !score.is_finite() || !(0.0..=1.0).contains(&score) {
                return Err(JevError::invalid("score must be in [0,1]"));
            }
        }
        if status != MetricStatus::Decided && passed.is_some() {
            return Err(JevError::invalid("non-decisions cannot pass or fail"));
        }
        Ok(Self {
            status,
            score,
            passed,
            label,
            reason: reason.into(),
            evidence,
        })
    }

    /// A skipped result (`JevMetricResult.skipped`).
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self {
            status: MetricStatus::Skipped,
            score: None,
            passed: None,
            label: None,
            reason: reason.into(),
            evidence: BTreeMap::new(),
        }
    }

    /// An error result (`JevMetricResult.error`).
    pub fn error(reason: impl Into<String>) -> Self {
        Self {
            status: MetricStatus::Error,
            score: None,
            passed: None,
            label: None,
            reason: reason.into(),
            evidence: BTreeMap::new(),
        }
    }

    /// Whether the metric decided that the subject passed.
    pub fn is_pass(&self) -> bool {
        self.status == MetricStatus::Decided && self.passed == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_must_be_finite_and_bounded() {
        for score in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            let result = JevMetricResult::new(
                MetricStatus::Decided,
                Some(score),
                None,
                None,
                "DECIDED",
                BTreeMap::new(),
            );
            assert_eq!(result.unwrap_err().message(), "score must be in [0,1]");
        }
        assert!(JevMetricResult::new(
            MetricStatus::Decided,
            Some(1.0),
            Some(true),
            None,
            "DECIDED",
            BTreeMap::new()
        )
        .is_ok());
    }

    #[test]
    fn only_decisions_may_carry_a_verdict() {
        let result = JevMetricResult::new(
            MetricStatus::Inconclusive,
            None,
            Some(false),
            None,
            "INCONCLUSIVE",
            BTreeMap::new(),
        );
        assert_eq!(
            result.unwrap_err().message(),
            "non-decisions cannot pass or fail"
        );
    }

    #[test]
    fn constructors_and_helpers_agree() {
        let skipped = JevMetricResult::skipped("MISSING_FIELDS");
        assert_eq!(skipped.status, MetricStatus::Skipped);
        assert_eq!(skipped.reason, "MISSING_FIELDS");
        assert!(skipped.evidence.is_empty());
        assert!(!skipped.is_pass());

        let error = JevMetricResult::error("TIMEOUT");
        assert_eq!(error.status, MetricStatus::Error);
        assert_eq!(error.status.as_str(), "ERROR");

        let decided = JevMetricResult::new(
            MetricStatus::Decided,
            Some(1.0),
            Some(true),
            Some("correct".to_string()),
            "DECIDED",
            BTreeMap::new(),
        )
        .unwrap();
        assert!(decided.is_pass());
        assert_eq!(decided.label.as_deref(), Some("correct"));
    }
}
