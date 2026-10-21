//! Task requirement records, mirroring `io.agentscope.core.state.TaskRequirement`.
//!
//! A requirement is a *claim about the task*, never evidence that it is satisfied.
//! [`RequirementStatus::Candidate`] is an unconfirmed proposal (typically mined from user
//! text), [`RequirementStatus::Confirmed`] means trusted caller code authorized it, and
//! [`RequirementStatus::Rejected`] makes it inactive. Confirmation establishes *authority*
//! only; satisfaction is bounded evidence carried by
//! [`TaskVerification`](super::TaskVerification).
//!
//! Every field is bounded and validated on construction *and* on deserialization, so a
//! malformed persisted record is an error rather than a partially trusted requirement.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Upper bound on the number of requirements one task context may carry.
pub(crate) const MAX_REQUIREMENTS: usize = 64;

/// Maximum length (in characters) of an identifier field such as `id`.
pub(crate) const MAX_ID_CHARS: usize = 200;

/// Maximum length (in characters) of a free-text/source field such as `text`.
pub(crate) const MAX_TEXT_CHARS: usize = 2000;

/// Mirrors the Java `TaskRequirement.requireText` guard: non-blank and bounded length.
pub(crate) fn require_text(value: &str, label: &str, max_length: usize) -> Result<()> {
    if value.trim().is_empty() || value.chars().count() > max_length {
        return Err(Error::Other(format!(
            "{label} must be nonblank and at most {max_length} characters"
        )));
    }
    Ok(())
}

/// What a requirement constrains. Wire names follow the Java enum constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RequirementKind {
    /// A rule the solution must not violate.
    Constraint,
    /// A criterion that must be satisfied before the task counts as done.
    AcceptanceCriterion,
}

impl RequirementKind {
    /// The Java enum constant name (`CONSTRAINT`, `ACCEPTANCE_CRITERION`).
    pub fn as_str(&self) -> &'static str {
        match self {
            RequirementKind::Constraint => "CONSTRAINT",
            RequirementKind::AcceptanceCriterion => "ACCEPTANCE_CRITERION",
        }
    }
}

impl std::fmt::Display for RequirementKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle of a requirement. Wire names follow the Java enum constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RequirementStatus {
    /// Unconfirmed proposal; never actionable on its own.
    Candidate,
    /// Authorized by trusted caller code.
    Confirmed,
    /// Explicitly refused; kept for auditability but inactive.
    Rejected,
}

impl RequirementStatus {
    /// The Java enum constant name (`CANDIDATE`, `CONFIRMED`, `REJECTED`).
    pub fn as_str(&self) -> &'static str {
        match self {
            RequirementStatus::Candidate => "CANDIDATE",
            RequirementStatus::Confirmed => "CONFIRMED",
            RequirementStatus::Rejected => "REJECTED",
        }
    }
}

impl std::fmt::Display for RequirementStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who made the decision about a requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RequirementAuthority {
    /// The end user.
    User,
    /// Trusted application code (the caller), not the model.
    Caller,
}

impl RequirementAuthority {
    /// The Java enum constant name (`USER`, `CALLER`).
    pub fn as_str(&self) -> &'static str {
        match self {
            RequirementAuthority::User => "USER",
            RequirementAuthority::Caller => "CALLER",
        }
    }
}

impl std::fmt::Display for RequirementAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The explicit decision that turned a candidate into a confirmed/rejected requirement.
///
/// Supplied by trusted application code after a real decision; never by a model tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "RequirementDecisionWire")]
pub struct RequirementDecision {
    /// Who decided.
    pub authority: RequirementAuthority,
    /// Where the decision can be audited (user message id, ticket, ...). Untrusted text.
    pub source_ref: String,
}

impl RequirementDecision {
    /// Creates a decision, validating the source reference.
    pub fn new(authority: RequirementAuthority, source_ref: impl Into<String>) -> Result<Self> {
        let decision = Self { authority, source_ref: source_ref.into() };
        decision.validate()?;
        Ok(decision)
    }

    /// Re-validates the invariants (useful for values built by struct literal).
    pub fn validate(&self) -> Result<()> {
        require_text(&self.source_ref, "decision source", MAX_TEXT_CHARS)
    }
}

impl TryFrom<RequirementDecisionWire> for RequirementDecision {
    type Error = String;

    fn try_from(wire: RequirementDecisionWire) -> std::result::Result<Self, Self::Error> {
        Self::new(wire.authority, wire.source_ref).map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequirementDecisionWire {
    authority: RequirementAuthority,
    source_ref: String,
}

/// An immutable requirement. Mirrors `io.agentscope.core.state.TaskRequirement`.
///
/// The invariant `(status == CANDIDATE) == (decision == None)` is enforced by the
/// constructor: a candidate carries no decision, a decided requirement always carries one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "TaskRequirementWire")]
pub struct TaskRequirement {
    /// Stable identifier, unique within one task context.
    pub id: String,
    /// What the requirement constrains.
    pub kind: RequirementKind,
    /// The requirement itself. Untrusted text until confirmed.
    pub text: String,
    /// Where the proposal came from (user message id, plan step, ...). Untrusted text.
    pub proposed_source_ref: String,
    /// Lifecycle status.
    pub status: RequirementStatus,
    /// Present exactly when `status` is not [`RequirementStatus::Candidate`].
    pub decision: Option<RequirementDecision>,
}

impl TaskRequirement {
    /// Creates a requirement, validating every bounded field and the decision invariant.
    pub fn new(
        id: impl Into<String>,
        kind: RequirementKind,
        text: impl Into<String>,
        proposed_source_ref: impl Into<String>,
        status: RequirementStatus,
        decision: Option<RequirementDecision>,
    ) -> Result<Self> {
        let requirement = Self {
            id: id.into(),
            kind,
            text: text.into(),
            proposed_source_ref: proposed_source_ref.into(),
            status,
            decision,
        };
        requirement.validate()?;
        Ok(requirement)
    }

    /// Creates an unconfirmed candidate with a freshly allocated identifier.
    ///
    /// Mirrors `new TaskRequirement(UUID.randomUUID().toString(), kind, text, sourceRef,
    /// CANDIDATE, null)`.
    pub fn candidate(
        kind: RequirementKind,
        text: impl Into<String>,
        proposed_source_ref: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            uuid::Uuid::new_v4().to_string(),
            kind,
            text,
            proposed_source_ref,
            RequirementStatus::Candidate,
            None,
        )
    }

    /// Re-validates the invariants (useful for values built by struct literal).
    pub fn validate(&self) -> Result<()> {
        require_text(&self.id, "id", MAX_ID_CHARS)?;
        require_text(&self.text, "requirement", MAX_TEXT_CHARS)?;
        require_text(&self.proposed_source_ref, "proposed source", MAX_TEXT_CHARS)?;
        if let Some(decision) = &self.decision {
            decision.validate()?;
        }
        if (self.status == RequirementStatus::Candidate) != self.decision.is_none() {
            return Err(Error::Other(
                "Only decided requirements must have a decision source".to_string(),
            ));
        }
        Ok(())
    }

    /// Whether this requirement is still an unconfirmed proposal.
    pub fn is_candidate(&self) -> bool {
        self.status == RequirementStatus::Candidate
    }
}

impl TryFrom<TaskRequirementWire> for TaskRequirement {
    type Error = String;

    fn try_from(wire: TaskRequirementWire) -> std::result::Result<Self, Self::Error> {
        Self::new(
            wire.id,
            wire.kind,
            wire.text,
            wire.proposed_source_ref,
            wire.status,
            wire.decision,
        )
        .map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskRequirementWire {
    id: String,
    kind: RequirementKind,
    text: String,
    proposed_source_ref: String,
    status: RequirementStatus,
    #[serde(default)]
    decision: Option<RequirementDecision>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn confirmed() -> TaskRequirement {
        TaskRequirement::new(
            "req-1",
            RequirementKind::AcceptanceCriterion,
            "the suite is green",
            "user message 3",
            RequirementStatus::Confirmed,
            Some(RequirementDecision::new(RequirementAuthority::User, "user message 3").unwrap()),
        )
        .unwrap()
    }

    #[test]
    fn serde_uses_java_wire_names() {
        let value = serde_json::to_value(confirmed()).unwrap();
        assert_eq!(value["id"], json!("req-1"));
        assert_eq!(value["kind"], json!("ACCEPTANCE_CRITERION"));
        assert_eq!(value["status"], json!("CONFIRMED"));
        assert_eq!(value["proposedSourceRef"], json!("user message 3"));
        assert_eq!(value["decision"]["authority"], json!("USER"));
        assert_eq!(value["decision"]["sourceRef"], json!("user message 3"));
        assert_eq!(
            value["kind"].as_str().unwrap(),
            RequirementKind::AcceptanceCriterion.as_str()
        );
    }

    #[test]
    fn wire_round_trip_is_lossless() {
        let candidate = TaskRequirement::candidate(
            RequirementKind::Constraint,
            "do not touch main",
            "user message 4",
        )
        .unwrap();
        for requirement in [confirmed(), candidate] {
            let json = serde_json::to_string(&requirement).unwrap();
            let back: TaskRequirement = serde_json::from_str(&json).unwrap();
            assert_eq!(back, requirement);
        }
    }

    #[test]
    fn candidate_with_decision_is_rejected() {
        let error = TaskRequirement::new(
            "req-1",
            RequirementKind::Constraint,
            "do not touch main",
            "user message 4",
            RequirementStatus::Candidate,
            Some(RequirementDecision::new(RequirementAuthority::Caller, "ticket-7").unwrap()),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("Only decided requirements must have a decision source"),
            "{error}"
        );
    }

    #[test]
    fn decided_without_decision_is_rejected_on_the_wire_too() {
        let mut value = serde_json::to_value(confirmed()).unwrap();
        value["decision"] = serde_json::Value::Null;
        let error = serde_json::from_value::<TaskRequirement>(value).unwrap_err();
        assert!(
            error.to_string().contains("Only decided requirements must have a decision source"),
            "{error}"
        );
    }

    #[test]
    fn blank_and_oversized_fields_are_rejected() {
        let blank = TaskRequirement::new(
            "req-1",
            RequirementKind::Constraint,
            "   ",
            "user message 4",
            RequirementStatus::Candidate,
            None,
        )
        .unwrap_err();
        assert!(blank.to_string().contains("requirement"), "{blank}");

        let oversized = TaskRequirement::new(
            "req-1",
            RequirementKind::Constraint,
            "x".repeat(MAX_TEXT_CHARS + 1),
            "user message 4",
            RequirementStatus::Candidate,
            None,
        )
        .unwrap_err();
        assert!(oversized.to_string().contains("requirement"), "{oversized}");

        let blank_source = RequirementDecision::new(RequirementAuthority::User, "  ").unwrap_err();
        assert!(blank_source.to_string().contains("decision source"), "{blank_source}");
    }

    #[test]
    fn unknown_status_is_a_deserialization_error() {
        let mut value = serde_json::to_value(confirmed()).unwrap();
        value["status"] = json!("CONFIRMED_ISH");
        let error = serde_json::from_value::<TaskRequirement>(value).unwrap_err();
        assert!(error.to_string().contains("CONFIRMED_ISH"), "{error}");
    }
}
