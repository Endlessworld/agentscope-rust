//! Bounded task-verification facts, mirroring `io.agentscope.core.state.TaskVerification`
//! and `io.agentscope.core.state.EvidenceBinding`.
//!
//! These are pure data records plus their wire codec: [`crate::session::SessionProjection`]
//! replays committed `verification/result` facts into this shape. Nothing here decides *when*
//! a check runs or *what* it asserts — that orchestration belongs to the caller.
//!
//! A `PASSED` outcome is one bounded check result, never a verdict on overall task
//! completion.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Maximum length (in characters) of an identifier field such as `id` or `verifierId`.
const MAX_ID_CHARS: usize = 200;

/// Maximum length (in characters) of a free-text field such as `reason`.
const MAX_REASON_CHARS: usize = 2000;

/// Maximum length (in characters) of `EvidenceBinding.subjectVersion`.
const MAX_SUBJECT_VERSION_CHARS: usize = 2000;

fn invalid(message: impl Into<String>) -> Error {
    Error::Other(message.into())
}

/// Mirrors the Java `TaskRequirement.requireText` guard: non-blank and bounded length.
fn require_text(value: &str, label: &str, max_length: usize) -> Result<()> {
    if value.trim().is_empty() || value.chars().count() > max_length {
        return Err(invalid(format!(
            "{label} must be nonblank and at most {max_length} characters"
        )));
    }
    Ok(())
}

/// Outcome of one bounded check.
///
/// Mirrors `TaskVerification.Outcome`; serialized as the uppercase Java enum constant name so
/// the wire format stays byte-compatible. An unknown name fails deserialization instead of
/// being coerced to a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VerificationOutcome {
    /// The check observed the expected state.
    Passed,
    /// The check observed a mismatch.
    Failed,
    /// The check could not decide.
    Unknown,
    /// The check itself errored.
    Error,
}

impl VerificationOutcome {
    /// The Java enum constant name (`PASSED`, `FAILED`, `UNKNOWN`, `ERROR`).
    pub fn as_str(&self) -> &'static str {
        match self {
            VerificationOutcome::Passed => "PASSED",
            VerificationOutcome::Failed => "FAILED",
            VerificationOutcome::Unknown => "UNKNOWN",
            VerificationOutcome::Error => "ERROR",
        }
    }

    /// Parses a Java enum constant name (case-insensitive).
    pub fn from_wire(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "PASSED" => Some(VerificationOutcome::Passed),
            "FAILED" => Some(VerificationOutcome::Failed),
            "UNKNOWN" => Some(VerificationOutcome::Unknown),
            "ERROR" => Some(VerificationOutcome::Error),
            _ => None,
        }
    }
}

impl std::fmt::Display for VerificationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Trusted application version captured before execution; never inferred from tool output.
///
/// Mirrors `io.agentscope.core.state.EvidenceBinding`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "EvidenceBindingWire")]
pub struct EvidenceBinding {
    pub task_id: String,
    pub contract_version: i64,
    pub subject_version: String,
}

impl EvidenceBinding {
    /// Creates a binding, rejecting a blank/oversized id, a negative contract version and a
    /// blank/oversized subject version.
    pub fn new(
        task_id: impl Into<String>,
        contract_version: i64,
        subject_version: impl Into<String>,
    ) -> Result<Self> {
        let binding = Self {
            task_id: task_id.into(),
            contract_version,
            subject_version: subject_version.into(),
        };
        binding.validate()?;
        Ok(binding)
    }

    /// Re-validates the invariants (useful for values built by struct literal).
    pub fn validate(&self) -> Result<()> {
        require_text(&self.task_id, "task ID", MAX_ID_CHARS)?;
        if self.contract_version < 0 {
            return Err(invalid("Negative contract version"));
        }
        require_text(
            &self.subject_version,
            "subject version",
            MAX_SUBJECT_VERSION_CHARS,
        )
    }
}

impl TryFrom<EvidenceBindingWire> for EvidenceBinding {
    type Error = String;

    fn try_from(wire: EvidenceBindingWire) -> std::result::Result<Self, Self::Error> {
        Self::new(wire.task_id, wire.contract_version, wire.subject_version)
            .map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceBindingWire {
    task_id: String,
    contract_version: i64,
    subject_version: String,
}

/// One bounded check result. Mirrors `io.agentscope.core.state.TaskVerification`.
///
/// Field names follow the Java wire format (`requirementId`, `actionId`, `verifierId`,
/// `recordedAt`, ...). Deserialization validates the same invariants as the Java canonical
/// constructor, so a malformed persisted fact is an error rather than a partially trusted
/// record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "TaskVerificationWire")]
pub struct TaskVerification {
    pub id: String,
    pub requirement_id: String,
    pub action_id: String,
    pub verifier_id: String,
    pub outcome: VerificationOutcome,
    pub reason: String,
    pub binding: EvidenceBinding,
    pub recorded_at: i64,
}

impl TaskVerification {
    /// Creates a verification record, validating every bounded field.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        requirement_id: impl Into<String>,
        action_id: impl Into<String>,
        verifier_id: impl Into<String>,
        outcome: VerificationOutcome,
        reason: impl Into<String>,
        binding: EvidenceBinding,
        recorded_at: i64,
    ) -> Result<Self> {
        let verification = Self {
            id: id.into(),
            requirement_id: requirement_id.into(),
            action_id: action_id.into(),
            verifier_id: verifier_id.into(),
            outcome,
            reason: reason.into(),
            binding,
            recorded_at,
        };
        verification.validate()?;
        Ok(verification)
    }

    /// Re-validates the invariants (useful for values built by struct literal).
    pub fn validate(&self) -> Result<()> {
        require_text(&self.id, "verification ID", MAX_ID_CHARS)?;
        require_text(&self.requirement_id, "requirement ID", MAX_ID_CHARS)?;
        require_text(&self.action_id, "action ID", MAX_ID_CHARS)?;
        require_text(&self.verifier_id, "verifier ID", MAX_ID_CHARS)?;
        require_text(&self.reason, "reason", MAX_REASON_CHARS)?;
        self.binding.validate()
    }
}

impl TryFrom<TaskVerificationWire> for TaskVerification {
    type Error = String;

    fn try_from(wire: TaskVerificationWire) -> std::result::Result<Self, Self::Error> {
        Self::new(
            wire.id,
            wire.requirement_id,
            wire.action_id,
            wire.verifier_id,
            wire.outcome,
            wire.reason,
            wire.binding,
            wire.recorded_at,
        )
        .map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskVerificationWire {
    id: String,
    requirement_id: String,
    action_id: String,
    verifier_id: String,
    outcome: VerificationOutcome,
    reason: String,
    binding: EvidenceBinding,
    recorded_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding() -> EvidenceBinding {
        EvidenceBinding::new("task-1", 3, "v7").unwrap()
    }

    fn verification() -> TaskVerification {
        TaskVerification::new(
            "ver-1",
            "req-1",
            "call-1",
            "verifier-a",
            VerificationOutcome::Passed,
            "order ORD-42 is confirmed",
            binding(),
            1_700_000_000_000,
        )
        .unwrap()
    }

    #[test]
    fn serde_uses_java_camel_case_wire_names() {
        let value = serde_json::to_value(verification()).unwrap();
        assert_eq!(value["requirementId"], json!("req-1"));
        assert_eq!(value["actionId"], json!("call-1"));
        assert_eq!(value["verifierId"], json!("verifier-a"));
        assert_eq!(value["recordedAt"], json!(1_700_000_000_000i64));
        assert_eq!(value["outcome"], json!("PASSED"));
        assert_eq!(value["binding"]["taskId"], json!("task-1"));
        assert_eq!(value["binding"]["contractVersion"], json!(3));
        assert_eq!(value["binding"]["subjectVersion"], json!("v7"));
    }

    #[test]
    fn wire_round_trip_is_lossless() {
        let original = verification();
        let json = serde_json::to_string(&original).unwrap();
        let back: TaskVerification = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn unknown_outcome_is_a_deserialization_error() {
        let mut value = serde_json::to_value(verification()).unwrap();
        value["outcome"] = json!("SUCCEDED");
        let error = serde_json::from_value::<TaskVerification>(value).unwrap_err();
        assert!(error.to_string().contains("SUCCEDED"), "{error}");
    }

    #[test]
    fn blank_id_is_rejected() {
        let error = TaskVerification::new(
            "   ",
            "req-1",
            "call-1",
            "verifier-a",
            VerificationOutcome::Passed,
            "ok",
            binding(),
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("verification ID"), "{error}");
    }

    #[test]
    fn oversized_reason_is_rejected() {
        let error = TaskVerification::new(
            "ver-1",
            "req-1",
            "call-1",
            "verifier-a",
            VerificationOutcome::Failed,
            "x".repeat(MAX_REASON_CHARS + 1),
            binding(),
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("reason"), "{error}");
    }

    #[test]
    fn negative_contract_version_is_rejected() {
        let error = EvidenceBinding::new("task-1", -1, "v7").unwrap_err();
        assert!(
            error.to_string().contains("Negative contract version"),
            "{error}"
        );
    }

    #[test]
    fn outcome_wire_helpers_round_trip() {
        for outcome in [
            VerificationOutcome::Passed,
            VerificationOutcome::Failed,
            VerificationOutcome::Unknown,
            VerificationOutcome::Error,
        ] {
            assert_eq!(
                VerificationOutcome::from_wire(outcome.as_str()),
                Some(outcome)
            );
        }
        assert_eq!(
            VerificationOutcome::from_wire("passed"),
            Some(VerificationOutcome::Passed)
        );
        assert_eq!(VerificationOutcome::from_wire("NOPE"), None);
    }
}
