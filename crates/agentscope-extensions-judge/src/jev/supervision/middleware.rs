//! `JevSupervisionMiddleware`: the schedule, the observation record and the bounded tail helper.
//!
//! Java's middleware runs a per-invocation observer loop: it subscribes to the worker event stream
//! once, buffers only event **type names** plus a bounded output tail, and on both a minimum and a
//! periodic interval assesses the captured snapshot. That loop needs the core `MiddlewareBase`,
//! `AgentEvent`, `TextBlockDeltaEvent` and operator/timer plumbing, which this crate does not depend
//! on, so it is **not** ported.
//!
//! What *is* ported: the validated [`Schedule`], the [`Observation`] record, the evidence request
//! shape and [`append_tail`] (the bounded, truncation-aware tail accumulation the capture uses).

use std::sync::Arc;

use crate::error::JevError;
use crate::jev::execution::Decision;

use super::evidence::SupervisionEvidence;
use super::supervisor::{Report, Scope, TaskAdvice};

/// Why the observer loop cannot be driven from this crate.
pub const SUPERVISION_MIDDLEWARE_UNAVAILABLE: &str = "JevSupervisionMiddleware needs the core middleware event stream (onAgent, AgentEvent, bounded-elastic scheduling); the observer loop is not ported";

/// The jittered assessment schedule (`JevSupervisionMiddleware.Schedule`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// The minimum gap between two assessments, in milliseconds.
    pub min_interval_millis: u64,
    /// The gap required when nothing changed, in milliseconds.
    pub periodic_interval_millis: u64,
    /// The maximum number of assessments per invocation.
    pub max_assessments: usize,
    /// How many consecutive failures stop the loop.
    pub max_consecutive_failures: usize,
}

impl Schedule {
    /// Creates and validates a schedule (`positive bounded supervision schedule required`).
    pub fn new(
        min_interval_millis: u64,
        periodic_interval_millis: u64,
        max_assessments: usize,
        max_consecutive_failures: usize,
    ) -> Result<Self, JevError> {
        if min_interval_millis == 0
            || periodic_interval_millis < min_interval_millis
            || max_assessments < 1
            || max_consecutive_failures < 1
        {
            return Err(JevError::invalid(
                "positive bounded supervision schedule required",
            ));
        }
        Ok(Self {
            min_interval_millis,
            periodic_interval_millis,
            max_assessments,
            max_consecutive_failures,
        })
    }

    /// The Java defaults: 5s minimum, 30s periodic, 60 assessments, 3 failures.
    pub fn defaults() -> Self {
        Self {
            min_interval_millis: 5_000,
            periodic_interval_millis: 30_000,
            max_assessments: 60,
            max_consecutive_failures: 3,
        }
    }
}

/// What the host asks its evidence provider for (`JevSupervisionMiddleware.EvidenceRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRequest {
    /// The run scope.
    pub scope: Scope,
    /// The observation sequence number.
    pub sequence: u64,
    /// Whether the worker is still running.
    pub worker_active: bool,
}

/// A host-registered evidence reader (`EvidenceSource`).
pub type EvidenceSource =
    Arc<dyn Fn(&EvidenceRequest) -> Option<SupervisionEvidence> + Send + Sync>;

/// One published observation (`JevSupervisionMiddleware.Observation`).
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// The run scope.
    pub scope: Scope,
    /// The 1-based assessment number.
    pub assessment: usize,
    /// The supervisor's decision.
    pub decision: Decision<Report>,
    /// Whether newer evidence arrived while the assessment ran.
    pub superseded: bool,
    /// Consecutive failures at publication time.
    pub consecutive_failures: usize,
    /// The advice the host should record; never an actuated command.
    pub advice: TaskAdvice,
}

/// The middleware's structural skeleton.
#[derive(Debug, Clone)]
pub struct SupervisionMiddlewareUnavailable {
    reason: String,
}

impl SupervisionMiddlewareUnavailable {
    /// Creates the placeholder with an explicit reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The default placeholder.
    pub fn observer_loop_missing() -> Self {
        Self::new(SUPERVISION_MIDDLEWARE_UNAVAILABLE)
    }

    /// The Java middleware order (`order()`).
    pub fn order(&self) -> i32 {
        310
    }

    /// Fails closed with the documented reason.
    pub fn start(&self) -> Result<Schedule, JevError> {
        Err(JevError::protocol(self.reason.clone()))
    }
}

/// Appends `value` to the bounded tail, reporting whether anything was clipped
/// (`JevSupervisionMiddleware.Capture.append`).
///
/// The tail keeps at most `limit` characters and always keeps the **end** of the stream, which is
/// what a supervisor needs; `truncated` is sticky across calls.
pub fn append_tail(output: &mut String, value: &str, limit: usize) -> bool {
    let mut clipped = false;
    if value.chars().count() >= limit {
        clipped = value.chars().count() > limit || !output.is_empty();
        *output = value
            .chars()
            .skip(value.chars().count() - limit)
            .collect();
        return clipped;
    }
    output.push_str(value);
    let count = output.chars().count();
    if count > limit {
        clipped = true;
        *output = output.chars().skip(count - limit).collect();
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_are_bounded_and_ordered() {
        assert_eq!(
            Schedule::new(0, 30_000, 1, 1).unwrap_err().message(),
            "positive bounded supervision schedule required"
        );
        assert_eq!(
            Schedule::new(30_000, 5_000, 1, 1)
                .unwrap_err()
                .message(),
            "positive bounded supervision schedule required"
        );
        assert_eq!(
            Schedule::new(5_000, 30_000, 0, 1)
                .unwrap_err()
                .message(),
            "positive bounded supervision schedule required"
        );
        assert_eq!(
            Schedule::new(5_000, 30_000, 1, 0)
                .unwrap_err()
                .message(),
            "positive bounded supervision schedule required"
        );
        let schedule = Schedule::defaults();
        assert_eq!(schedule.min_interval_millis, 5_000);
        assert_eq!(schedule.periodic_interval_millis, 30_000);
        assert_eq!(schedule.max_assessments, 60);
        assert_eq!(schedule.max_consecutive_failures, 3);
        assert!(Schedule::new(5_000, 5_000, 1, 1).is_ok());
    }

    #[test]
    fn the_observer_loop_fails_closed() {
        let placeholder = SupervisionMiddlewareUnavailable::observer_loop_missing();
        assert_eq!(placeholder.order(), 310);
        let error = placeholder.start().unwrap_err();
        assert_eq!(error.message(), SUPERVISION_MIDDLEWARE_UNAVAILABLE);
        assert!(error.message().contains("middleware"));
        assert_ne!(
            SupervisionMiddlewareUnavailable::new("custom").start().unwrap_err().message(),
            SUPERVISION_MIDDLEWARE_UNAVAILABLE
        );
    }

    #[test]
    fn the_tail_keeps_the_end_and_reports_clipping() {
        let mut output = String::new();
        assert!(!append_tail(&mut output, "abc", 10));
        assert_eq!(output, "abc");

        assert!(!append_tail(&mut output, "def", 10));
        assert_eq!(output, "abcdef");

        assert!(append_tail(&mut output, "ghi", 7));
        assert_eq!(output, "cdefghi");

        // A single chunk longer than the limit replaces the tail.
        let mut long = String::from("old");
        assert!(append_tail(&mut long, "0123456789", 4));
        assert_eq!(long, "6789");

        // Exactly the limit is not clipping on its own.
        let mut exact = String::new();
        assert!(!append_tail(&mut exact, "1234", 4));
        assert_eq!(exact, "1234");
    }
}
