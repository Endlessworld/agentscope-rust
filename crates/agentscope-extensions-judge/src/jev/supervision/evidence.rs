//! Host-supplied supervision evidence (`SupervisionEvidence`).
//!
//! The host enforces access, redaction and revision consistency; JEV only reasons about what it is
//! given, and an absent provider is explicit *missing* evidence, never successful verification.

use serde::{Deserialize, Serialize};

use crate::error::JevError;

/// Scope- and revision-bound verification produced by the verifier, never by the worker
/// (`SupervisionEvidence.Verification`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    /// The user the verification belongs to.
    pub user_id: String,
    /// The session the verification belongs to.
    pub session_id: String,
    /// The run the verification belongs to.
    pub run_id: String,
    /// The revision that was verified.
    pub revision: String,
    /// The verifier that produced it.
    pub source: String,
    /// Whether verification passed.
    pub passed: bool,
    /// A bounded summary.
    pub summary: String,
}

impl Verification {
    /// Creates a verification; every scope/revision field must be present (never blank-checked
    /// beyond presence, matching Java's `Objects.requireNonNull`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        revision: impl Into<String>,
        source: impl Into<String>,
        passed: bool,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            session_id: session_id.into(),
            run_id: run_id.into(),
            revision: revision.into(),
            source: source.into(),
            passed,
            summary: summary.into(),
        }
    }
}

/// Host-supplied evidence (`SupervisionEvidence`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisionEvidence {
    /// The repository revision the evidence describes.
    pub revision: String,
    /// `git status`-style repository status.
    pub repository_status: String,
    /// The bounded diff.
    pub diff: String,
    /// The changed file paths.
    pub changed_files: Vec<String>,
    /// The repository instructions (`AGENTS.md`-style).
    pub instructions: String,
    /// Whether the task requires documentation.
    pub documentation_required: bool,
    /// The verifier's result, when one ran.
    pub verification: Option<Verification>,
}

impl Default for SupervisionEvidence {
    fn default() -> Self {
        Self::missing()
    }
}

impl SupervisionEvidence {
    /// Creates evidence.
    pub fn new(
        revision: impl Into<String>,
        repository_status: impl Into<String>,
        diff: impl Into<String>,
        changed_files: Vec<String>,
        instructions: impl Into<String>,
        documentation_required: bool,
        verification: Option<Verification>,
    ) -> Self {
        Self {
            revision: revision.into(),
            repository_status: repository_status.into(),
            diff: diff.into(),
            changed_files,
            instructions: instructions.into(),
            documentation_required,
            verification,
        }
    }

    /// Explicit missing evidence (`SupervisionEvidence.missing`).
    pub fn missing() -> Self {
        Self {
            revision: String::new(),
            repository_status: String::new(),
            diff: String::new(),
            changed_files: Vec::new(),
            instructions: String::new(),
            documentation_required: false,
            verification: None,
        }
    }

    /// Whether a verification is current, scope-bound and passed (`JevTaskSupervisor.assess`).
    ///
    /// `revision` is the evidence revision; the verification must describe the same revision and
    /// the same `(user, session, run)` scope.
    pub fn verified_for(&self, user_id: &str, session_id: &str, run_id: &str) -> bool {
        match &self.verification {
            None => false,
            Some(verification) => {
                verification.passed
                    && !self.revision.trim().is_empty()
                    && !verification.source.trim().is_empty()
                    && verification.revision == self.revision
                    && verification.user_id == user_id
                    && verification.session_id == session_id
                    && verification.run_id == run_id
            }
        }
    }

    /// Adds a verification, validating that it is non-blank where Java requires it.
    pub fn with_verification(mut self, verification: Verification) -> Result<Self, JevError> {
        if verification.user_id.trim().is_empty()
            || verification.session_id.trim().is_empty()
            || verification.run_id.trim().is_empty()
            || verification.revision.trim().is_empty()
            || verification.source.trim().is_empty()
            || verification.summary.trim().is_empty()
        {
            return Err(JevError::invalid("verification scope required"));
        }
        self.verification = Some(verification);
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verification(passed: bool) -> Verification {
        Verification::new("u", "s", "r", "rev1", "cargo test", passed, "ok")
    }

    #[test]
    fn missing_evidence_is_explicit() {
        let missing = SupervisionEvidence::missing();
        assert_eq!(missing, SupervisionEvidence::default());
        assert!(!missing.verified_for("u", "s", "r"));
        assert!(missing.changed_files.is_empty());
        assert!(!missing.documentation_required);
    }

    #[test]
    fn verification_must_match_scope_revision_and_pass() {
        let evidence = SupervisionEvidence::new(
            "rev1",
            "clean",
            "diff",
            vec!["a.rs".to_string()],
            "rules",
            true,
            Some(verification(true)),
        );
        assert!(evidence.verified_for("u", "s", "r"));
        assert!(!evidence.verified_for("other", "s", "r"));
        assert!(!evidence.verified_for("u", "other", "r"));
        assert!(!evidence.verified_for("u", "s", "other"));

        let failed =
            SupervisionEvidence::new("rev1", "", "", vec![], "", false, Some(verification(false)));
        assert!(!failed.verified_for("u", "s", "r"));

        let stale =
            SupervisionEvidence::new("rev2", "", "", vec![], "", false, Some(verification(true)));
        assert!(!stale.verified_for("u", "s", "r"));

        let no_revision =
            SupervisionEvidence::new("", "", "", vec![], "", false, Some(verification(true)));
        assert!(!no_revision.verified_for("u", "s", "r"));
    }

    #[test]
    fn verification_with_a_blank_scope_is_rejected() {
        let blank = Verification::new("", "s", "r", "rev", "src", true, "ok");
        assert_eq!(
            SupervisionEvidence::missing()
                .with_verification(blank)
                .unwrap_err()
                .message(),
            "verification scope required"
        );
        assert!(SupervisionEvidence::missing()
            .with_verification(verification(true))
            .is_ok());
    }
}
