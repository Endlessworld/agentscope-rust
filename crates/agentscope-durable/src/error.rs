//! Stable, transport-independent command failures.

use std::fmt;

/// Mirrors `DurableException.Code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    NotFound,
    Conflict,
    /// The writer lease is stale; the caller lost ownership of the thread.
    Fenced,
    Invalid,
    Incompatible,
    CursorExpired,
    Unavailable,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::Conflict => "CONFLICT",
            ErrorCode::Fenced => "FENCED",
            ErrorCode::Invalid => "INVALID",
            ErrorCode::Incompatible => "INCOMPATIBLE",
            ErrorCode::CursorExpired => "CURSOR_EXPIRED",
            ErrorCode::Unavailable => "UNAVAILABLE",
        }
    }
}

/// A command failure the caller may branch on without parsing the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableError {
    code: ErrorCode,
    message: String,
}

impl DurableError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }

    pub fn fenced(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Fenced, message)
    }

    pub fn incompatible(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Incompatible, message)
    }

    pub fn cursor_expired(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::CursorExpired, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DurableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for DurableError {}

impl From<serde_json::Error> for DurableError {
    fn from(error: serde_json::Error) -> Self {
        Self::invalid(format!("invalid JSON: {error}"))
    }
}

impl From<std::io::Error> for DurableError {
    fn from(error: std::io::Error) -> Self {
        Self::unavailable(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, DurableError>;
