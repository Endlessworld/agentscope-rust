//! Error type for JEV calls, mirroring the Java `JevException` / `IllegalArgumentException`
//! split: argument problems are the caller's fault, everything after the request is
//! serialized is a protocol error carrying the HTTP context and retryability.

use thiserror::Error;

/// Errors raised by the JEV DTO/validation layer.
///
/// Mirrors the Java behaviour where bad *input* throws `IllegalArgumentException` while the
/// client/transport/API path throws `JevException` (which carries `statusCode`,
/// `responseBody` and `retryable`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JevError {
    /// The caller supplied an invalid argument (Java `IllegalArgumentException`).
    #[error("{0}")]
    InvalidArgument(String),

    /// A client, transport, validation or API error (Java `JevException`).
    #[error("{message}")]
    Protocol {
        /// Human readable message.
        message: String,
        /// The HTTP status, or `None` when the request failed before a response arrived.
        status_code: Option<u16>,
        /// The raw HTTP error body, or `None`.
        response_body: Option<String>,
        /// Whether the configured retry policy may retry this error.
        retryable: bool,
    },
}

impl JevError {
    /// Creates an invalid-argument error.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    /// Creates a protocol error without HTTP context (e.g. response-shape validation).
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
            status_code: None,
            response_body: None,
            retryable: false,
        }
    }

    /// Creates an HTTP error, deriving retryability the way `JevClient.httpException` does
    /// (`429`, `529` or `5xx`).
    pub fn http(
        message: impl Into<String>,
        status_code: u16,
        response_body: Option<String>,
    ) -> Self {
        Self::protocol_with_http(
            message,
            Some(status_code),
            response_body,
            status_code == 429 || status_code == 529 || status_code >= 500,
        )
    }

    /// Creates a protocol error with explicit HTTP context.
    pub fn protocol_with_http(
        message: impl Into<String>,
        status_code: Option<u16>,
        response_body: Option<String>,
        retryable: bool,
    ) -> Self {
        Self::Protocol {
            message: message.into(),
            status_code,
            response_body,
            retryable,
        }
    }

    /// The error message, independent of the variant.
    pub fn message(&self) -> &str {
        match self {
            JevError::InvalidArgument(message) => message,
            JevError::Protocol { message, .. } => message,
        }
    }

    /// The HTTP status, or `None` when there was no response (always `None` for argument errors).
    pub fn status_code(&self) -> Option<u16> {
        match self {
            JevError::InvalidArgument(_) => None,
            JevError::Protocol { status_code, .. } => *status_code,
        }
    }

    /// The raw HTTP error body, if any.
    pub fn response_body(&self) -> Option<&str> {
        match self {
            JevError::InvalidArgument(_) => None,
            JevError::Protocol { response_body, .. } => response_body.as_deref(),
        }
    }

    /// Whether the configured retry policy may retry this error.
    pub fn is_retryable(&self) -> bool {
        match self {
            JevError::InvalidArgument(_) => false,
            JevError::Protocol { retryable, .. } => *retryable,
        }
    }

    /// Whether this is an argument error (Java `IllegalArgumentException`).
    pub fn is_invalid_argument(&self) -> bool {
        matches!(self, JevError::InvalidArgument(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_errors_are_retryable_only_for_429_529_and_5xx() {
        for status in [429u16, 529, 500, 503] {
            let error = JevError::http("boom", status, None);
            assert!(error.is_retryable(), "status {status} should retry");
            assert_eq!(error.status_code(), Some(status));
            assert_eq!(error.message(), "boom");
            assert!(!error.is_invalid_argument());
        }
        for status in [400u16, 401, 404, 422] {
            let error = JevError::http("boom", status, Some("body".to_string()));
            assert!(!error.is_retryable(), "status {status} should not retry");
            assert_eq!(error.response_body(), Some("body"));
            assert_eq!(error.to_string(), "boom");
        }
    }

    #[test]
    fn argument_errors_carry_no_http_context() {
        let error = JevError::invalid("state must not be null");
        assert!(error.is_invalid_argument());
        assert!(!error.is_retryable());
        assert_eq!(error.status_code(), None);
        assert_eq!(error.response_body(), None);
        assert_eq!(error.to_string(), "state must not be null");
    }
}
