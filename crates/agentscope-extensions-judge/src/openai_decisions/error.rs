//! Error type mirroring Java `OpenAIDecisionsException` (+ the `IllegalArgumentException`s the
//! SDK wrapper lets through).
//!
//! Java sanitizes every failure: the exception carries a status and a retry hint but **never**
//! the provider body (its test asserts `!message.contains("private")`). The same rule applies
//! here, which is why [`DecisionsError::Api`] has no `response_body` field.

use thiserror::Error;

/// Errors raised by the Decisions DTO/validation/client layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DecisionsError {
    /// The caller supplied an invalid argument (Java `IllegalArgumentException`).
    #[error("{0}")]
    InvalidArgument(String),

    /// A transport, HTTP, timeout or invalid-response failure (Java
    /// `OpenAIDecisionsException`). The message is sanitized: it never contains the provider body.
    #[error("{message}")]
    Api {
        /// Sanitized message.
        message: String,
        /// The HTTP status, or `None` when the request failed before a response arrived.
        status_code: Option<u16>,
        /// Whether the configured retry policy may retry this failure.
        retryable: bool,
    },
}

impl DecisionsError {
    /// Creates an invalid-argument error.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    /// Creates a sanitized API/transport error.
    pub fn api(message: impl Into<String>, status_code: Option<u16>, retryable: bool) -> Self {
        Self::Api {
            message: message.into(),
            status_code,
            retryable,
        }
    }

    /// The error message, independent of the variant.
    pub fn message(&self) -> &str {
        match self {
            DecisionsError::InvalidArgument(message) => message,
            DecisionsError::Api { message, .. } => message,
        }
    }

    /// The HTTP status, or `None` when there was no response.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            DecisionsError::InvalidArgument(_) => None,
            DecisionsError::Api { status_code, .. } => *status_code,
        }
    }

    /// Whether the configured retry policy may retry this error.
    pub fn is_retryable(&self) -> bool {
        match self {
            DecisionsError::InvalidArgument(_) => false,
            DecisionsError::Api { retryable, .. } => *retryable,
        }
    }

    /// Whether this is an argument error (Java `IllegalArgumentException`).
    pub fn is_invalid_argument(&self) -> bool {
        matches!(self, DecisionsError::InvalidArgument(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_errors_carry_status_and_retry_hint() {
        let error = DecisionsError::api("Decisions request failed with HTTP 503", Some(503), true);
        assert_eq!(error.message(), "Decisions request failed with HTTP 503");
        assert_eq!(error.status_code(), Some(503));
        assert!(error.is_retryable());
        assert!(!error.is_invalid_argument());
        assert_eq!(error.to_string(), "Decisions request failed with HTTP 503");
    }

    #[test]
    fn argument_errors_are_never_retryable() {
        let error = DecisionsError::invalid("model must not be blank");
        assert!(error.is_invalid_argument());
        assert!(!error.is_retryable());
        assert_eq!(error.status_code(), None);
        assert_eq!(error.message(), "model must not be blank");
    }
}
