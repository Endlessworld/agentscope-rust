//! Error type shared by all skill repository backends.

use thiserror::Error;

/// Errors raised by [`crate::AgentSkillRepository`] implementations.
///
/// Mirrors the Java behaviour where a missing skill is an `IllegalArgumentException`,
/// a save conflict is an `IllegalStateException` and driver failures are wrapped
/// `RuntimeException`s — collapsed into one enum so callers can match precisely.
#[derive(Debug, Error)]
pub enum SkillRepositoryError {
    /// The requested skill does not exist (`getSkill` on a missing name).
    #[error("skill not found: {0}")]
    NotFound(String),

    /// Invalid argument: bad skill name / resource path / identifier.
    #[error("invalid {kind}: {message}")]
    InvalidArgument {
        /// Which concept was invalid (e.g. `skill name`).
        kind: String,
        /// Human readable explanation.
        message: String,
    },

    /// Save rejected because `force=false` and the skill already exists.
    #[error("cannot save skills: already exist and force=false: {0}")]
    Conflict(String),

    /// The repository is read-only but a write was attempted.
    #[error("repository is read-only")]
    ReadOnly,

    /// I/O failure (filesystem / git subprocess).
    #[error("io error: {0}")]
    Io(String),

    /// Underlying database / driver failure.
    #[error("database error: {0}")]
    Database(String),

    /// JSON (de)serialization failure for `metadata_json`.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// A remote git operation failed (clone / pull / ls-remote).
    #[error("git error: {0}")]
    Git(String),
}

impl SkillRepositoryError {
    /// Convenience constructor for an invalid argument.
    pub fn invalid(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for SkillRepositoryError {
    fn from(e: std::io::Error) -> Self {
        SkillRepositoryError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for SkillRepositoryError {
    fn from(e: serde_json::Error) -> Self {
        SkillRepositoryError::Serialization(e.to_string())
    }
}
