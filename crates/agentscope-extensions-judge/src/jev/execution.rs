//! Execution budget and metadata-only observation types shared by JEV integrations.
//!
//! Mirrors `io.agentscope.extensions.judge.jev.JevExecution` minus the reactive plumbing: Java's
//! `execute` wraps a `Mono<Decision<T>>` with a timeout, swallows errors into
//! `Decision { status: ERROR, reason: "TIMEOUT" | "CALL_OR_RESPONSE_ERROR" }` and reports a
//! metadata record to an observer that must never change execution. Here the same facts are
//! modelled as data plus pure transitions, so an async runtime can drive them later.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::error::JevError;

/// Whether and how a JEV integration participates in a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Do not call JEV at all.
    #[default]
    Off,
    /// Call JEV, observe, but never change the response.
    Shadow,
    /// Call JEV and let the decision take effect.
    Enforce,
}

/// Why a decision has the value it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The operation produced a value.
    Decided,
    /// The operation produced no usable value.
    Inconclusive,
    /// The operation failed or timed out.
    Error,
    /// The subscription was cancelled.
    Cancelled,
    /// The integration is disabled ([`Mode::Off`]).
    Skipped,
}

/// The reason strings Java puts on a [`Decision`].
pub mod reason {
    /// A decided outcome.
    pub const DECIDED: &str = "DECIDED";
    /// The integration is disabled.
    pub const DISABLED: &str = "DISABLED";
    /// The operation exceeded its budget.
    pub const TIMEOUT: &str = "TIMEOUT";
    /// The call or the response was unusable.
    pub const CALL_OR_RESPONSE_ERROR: &str = "CALL_OR_RESPONSE_ERROR";
    /// The subscription was cancelled.
    pub const CANCELLED: &str = "CANCELLED";
}

/// A typed decision plus its status, reason and optional recommendation.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision<T> {
    /// Why this decision exists.
    pub status: Status,
    /// The value, absent unless `status == Decided`.
    pub value: Option<T>,
    /// Machine readable reason (see [`reason`]).
    pub reason: String,
    /// Extra key/value recommendation data.
    pub recommendation: BTreeMap<String, String>,
}

impl<T> Decision<T> {
    /// Creates a decision with an explicit reason and recommendation.
    pub fn new(
        status: Status,
        value: Option<T>,
        reason: impl Into<String>,
        recommendation: BTreeMap<String, String>,
    ) -> Self {
        Self {
            status,
            value,
            reason: reason.into(),
            recommendation,
        }
    }

    /// A decided outcome (`Decision.decided`).
    pub fn decided(value: T) -> Self {
        Self::new(
            Status::Decided,
            Some(value),
            reason::DECIDED,
            BTreeMap::new(),
        )
    }

    /// An inconclusive outcome (`Decision.uncertain`).
    pub fn uncertain(reason: impl Into<String>) -> Self {
        Self::new(Status::Inconclusive, None, reason, BTreeMap::new())
    }

    /// The decision taken when the integration is disabled.
    pub fn skipped() -> Self {
        Self::new(Status::Skipped, None, reason::DISABLED, BTreeMap::new())
    }

    /// The decision taken when a call failed or timed out.
    ///
    /// `timed_out` selects the reason string, mirroring Java's `TimeoutException` check.
    pub fn error(timed_out: bool) -> Self {
        Self::new(
            Status::Error,
            None,
            if timed_out {
                reason::TIMEOUT
            } else {
                reason::CALL_OR_RESPONSE_ERROR
            },
            BTreeMap::new(),
        )
    }

    /// The decision taken when the subscription was cancelled.
    pub fn cancelled() -> Self {
        Self::new(Status::Cancelled, None, reason::CANCELLED, BTreeMap::new())
    }

    /// Whether a value is present (only possible for [`Status::Decided`]).
    pub fn has_value(&self) -> bool {
        self.value.is_some()
    }
}

/// A metadata-only observation record handed to the observer.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// What the integration was called for.
    pub purpose: String,
    /// Integration version.
    pub version: String,
    /// The configured mode.
    pub mode: Mode,
    /// The observed status.
    pub status: Status,
    /// The observed reason.
    pub reason: String,
    /// Wall-clock time spent.
    pub elapsed: Duration,
    /// The decision's recommendation data.
    pub recommendation: BTreeMap<String, String>,
}

/// Per-subscription execution options.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Whether and how JEV participates.
    pub mode: Mode,
    /// The execution budget.
    pub budget: Duration,
    /// Integration version (non-blank).
    pub version: String,
}

/// Default budget when the integration is disabled (`Options.disabled`).
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(2);

/// Default version when the integration is disabled (`Options.disabled`).
pub const DEFAULT_VERSION: &str = "v1";

impl Options {
    /// Creates and validates options.
    pub fn new(mode: Mode, budget: Duration, version: impl Into<String>) -> Result<Self, JevError> {
        let version = version.into();
        if budget.is_zero() {
            return Err(JevError::invalid("budget must be positive"));
        }
        if version.trim().is_empty() {
            return Err(JevError::invalid("version required"));
        }
        Ok(Self {
            mode,
            budget,
            version,
        })
    }

    /// The disabled options Java ships as `Options.disabled()`.
    pub fn disabled() -> Self {
        Self {
            mode: Mode::Off,
            budget: DEFAULT_BUDGET,
            version: DEFAULT_VERSION.to_string(),
        }
    }
}

/// A per-subscription execution scope: a fixed purpose plus immutable [`Options`].
#[derive(Debug, Clone, PartialEq)]
pub struct JevExecution {
    purpose: String,
    options: Options,
}

impl JevExecution {
    /// Creates an execution scope.
    pub fn new(purpose: impl Into<String>, options: Options) -> Self {
        Self {
            purpose: purpose.into(),
            options,
        }
    }

    /// The purpose this scope was created for.
    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    /// The configured mode.
    pub fn mode(&self) -> Mode {
        self.options.mode
    }

    /// The configured budget.
    pub fn budget(&self) -> Duration {
        self.options.budget
    }

    /// The decision to use when the mode is [`Mode::Off`] (`JevExecution.execute`).
    pub fn disabled_decision<T>(&self) -> Decision<T> {
        Decision::skipped()
    }

    /// Builds the observation record for a decision (`JevExecution.observe`).
    pub fn record<T>(&self, decision: &Decision<T>, elapsed: Duration) -> Record {
        Record {
            purpose: self.purpose.clone(),
            version: self.options.version.clone(),
            mode: self.options.mode,
            status: decision.status,
            reason: decision.reason.clone(),
            elapsed,
            recommendation: decision.recommendation.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decided_and_uncertain_decisions_match_java_constructors() {
        let decided = Decision::decided("value".to_string());
        assert_eq!(decided.status, Status::Decided);
        assert_eq!(decided.value.as_deref(), Some("value"));
        assert_eq!(decided.reason, reason::DECIDED);
        assert!(decided.recommendation.is_empty());
        assert!(decided.has_value());

        let uncertain: Decision<String> = Decision::uncertain("LOW_SIGNAL");
        assert_eq!(uncertain.status, Status::Inconclusive);
        assert!(!uncertain.has_value());
        assert_eq!(uncertain.reason, "LOW_SIGNAL");
    }

    #[test]
    fn error_and_cancelled_decisions_carry_their_reason() {
        let timeout: Decision<u8> = Decision::error(true);
        assert_eq!(timeout.status, Status::Error);
        assert_eq!(timeout.reason, reason::TIMEOUT);
        let call_error: Decision<u8> = Decision::error(false);
        assert_eq!(call_error.reason, reason::CALL_OR_RESPONSE_ERROR);

        let cancelled: Decision<u8> = Decision::cancelled();
        assert_eq!(cancelled.status, Status::Cancelled);
        assert_eq!(cancelled.reason, reason::CANCELLED);
    }

    #[test]
    fn options_validate_budget_and_version() {
        assert_eq!(
            Options::new(Mode::Enforce, Duration::ZERO, "v1")
                .unwrap_err()
                .message(),
            "budget must be positive"
        );
        assert_eq!(
            Options::new(Mode::Enforce, Duration::from_millis(1), "  ")
                .unwrap_err()
                .message(),
            "version required"
        );
        let options = Options::disabled();
        assert_eq!(options.mode, Mode::Off);
        assert_eq!(options.budget, DEFAULT_BUDGET);
        assert_eq!(options.version, DEFAULT_VERSION);
        assert_eq!(Mode::default(), Mode::Off);
    }

    #[test]
    fn execution_skips_when_off_and_records_observations() {
        let disabled = JevExecution::new("guardrail", Options::disabled());
        assert_eq!(disabled.mode(), Mode::Off);
        let decision: Decision<String> = disabled.disabled_decision();
        assert_eq!(decision.status, Status::Skipped);
        assert_eq!(decision.reason, reason::DISABLED);

        let record = disabled.record(&decision, Duration::from_millis(7));
        assert_eq!(record.purpose, "guardrail");
        assert_eq!(record.mode, Mode::Off);
        assert_eq!(record.status, Status::Skipped);
        assert_eq!(record.elapsed, Duration::from_millis(7));
        assert!(record.recommendation.is_empty());

        let enforce = JevExecution::new(
            "quality",
            Options::new(Mode::Shadow, Duration::from_millis(250), "v2").unwrap(),
        );
        let mut decision = Decision::decided(1u8);
        decision
            .recommendation
            .insert("route".to_string(), "fast".to_string());
        let record = enforce.record(&decision, Duration::from_millis(3));
        assert_eq!(record.mode, Mode::Shadow);
        assert_eq!(record.version, "v2");
        assert_eq!(record.status, Status::Decided);
        assert_eq!(record.recommendation["route"], "fast");
    }
}
