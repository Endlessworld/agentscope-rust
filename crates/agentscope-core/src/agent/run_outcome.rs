//! Semantic result of one invocation, mirroring `io.agentscope.core.agent.RunOutcome`.
//!
//! A normally completed publisher can carry a suspended, interrupted or failed outcome. This
//! value never decides the terminal state of a durable task; it only describes what one native
//! invocation did.

use serde::{Deserialize, Serialize};

use crate::message::{GenerateReason, Msg};

/// Semantic status of one invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunOutcomeStatus {
    Completed,
    Suspended,
    Interrupted,
    Failed,
    Cancelled,
}

/// The semantic result of one invocation: a status plus the native reason that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunOutcome {
    status: RunOutcomeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<GenerateReason>,
}

impl RunOutcome {
    /// Builds an outcome from an explicit status and reason.
    pub fn new(status: RunOutcomeStatus, reason: Option<GenerateReason>) -> Self {
        Self { status, reason }
    }

    /// Maps a native [`GenerateReason`] to its invocation status.
    pub fn from_reason(reason: Option<GenerateReason>) -> Self {
        let status = match reason {
            None => RunOutcomeStatus::Completed,
            Some(
                GenerateReason::ToolSuspended
                | GenerateReason::HostPaused
                | GenerateReason::PermissionAsking
                | GenerateReason::ReasoningStopRequested
                | GenerateReason::ActingStopRequested
                | GenerateReason::MiddlewareStopRequested,
            ) => RunOutcomeStatus::Suspended,
            Some(GenerateReason::Interrupted) => RunOutcomeStatus::Interrupted,
            Some(GenerateReason::MaxIterations | GenerateReason::AllToolsDenied) => {
                RunOutcomeStatus::Failed
            }
            Some(_) => RunOutcomeStatus::Completed,
        };
        Self { status, reason }
    }

    /// Maps the reason carried by `result` to its invocation status.
    pub fn from_result(result: &Msg) -> Self {
        Self::from_reason(Some(result.generate_reason()))
    }

    /// The semantic status of the invocation.
    pub fn status(&self) -> RunOutcomeStatus {
        self.status
    }

    /// The native reason that produced the status, when one was recorded.
    pub fn reason(&self) -> Option<GenerateReason> {
        self.reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_pause_and_suspension_reasons_are_suspended() {
        for reason in [
            GenerateReason::HostPaused,
            GenerateReason::ToolSuspended,
            GenerateReason::PermissionAsking,
            GenerateReason::ReasoningStopRequested,
            GenerateReason::ActingStopRequested,
            GenerateReason::MiddlewareStopRequested,
        ] {
            let outcome = RunOutcome::from_reason(Some(reason));
            assert_eq!(outcome.status(), RunOutcomeStatus::Suspended, "{reason:?}");
            assert_eq!(outcome.reason(), Some(reason));
        }
    }

    #[test]
    fn interruption_and_failures_are_distinguished() {
        assert_eq!(
            RunOutcome::from_reason(Some(GenerateReason::Interrupted)).status(),
            RunOutcomeStatus::Interrupted
        );
        for reason in [
            GenerateReason::MaxIterations,
            GenerateReason::AllToolsDenied,
        ] {
            assert_eq!(
                RunOutcome::from_reason(Some(reason)).status(),
                RunOutcomeStatus::Failed,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn ordinary_completion_has_no_suspension() {
        assert_eq!(
            RunOutcome::from_reason(None).status(),
            RunOutcomeStatus::Completed
        );
        assert_eq!(RunOutcome::from_reason(None).reason(), None);
        for reason in [
            GenerateReason::ModelStop,
            GenerateReason::ToolCalls,
            GenerateReason::StructuredOutput,
            GenerateReason::ToolReturnDirect,
        ] {
            assert_eq!(
                RunOutcome::from_reason(Some(reason)).status(),
                RunOutcomeStatus::Completed,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn status_serializes_lowercase_like_java() {
        let outcome = RunOutcome::from_reason(Some(GenerateReason::HostPaused));
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["status"], "suspended");
        assert_eq!(json["reason"], "HOST_PAUSED");
        let back: RunOutcome = serde_json::from_value(json).unwrap();
        assert_eq!(back, outcome);
    }
}
