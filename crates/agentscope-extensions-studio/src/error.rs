//! Error types for the Studio extension.
use thiserror::Error;

/// Errors produced by the Studio HTTP/WebSocket clients and manager.
#[derive(Debug, Error)]
pub enum StudioError {
    /// A transport-level (network/IO) failure. Retryable.
    #[error("studio transport error: {0}")]
    Transport(String),

    /// The server returned a non-success HTTP status.
    #[error("studio http error: HTTP {status}: {message}")]
    Http { status: u16, message: String },

    /// (De)serialization of a request/response body failed.
    #[error("studio serialization error: {0}")]
    Serialization(String),

    /// Studio integration has not been initialized.
    #[error("studio not initialized")]
    NotInitialized,

    /// Waiting for user input timed out.
    #[error("timed out waiting for studio user input: {0}")]
    InputTimeout(String),

    /// Generic WebSocket/transport failure.
    #[error("studio websocket error: {0}")]
    WebSocket(String),
}

impl StudioError {
    /// Whether this error is retryable by the HTTP retry logic.
    ///
    /// Mirrors Java's `Retry.backoff(...).filter(t -> t instanceof IOException)` —
    /// only transport/network failures are retried, not HTTP status or
    /// serialization errors.
    pub fn is_retryable(&self) -> bool {
        matches!(self, StudioError::Transport(_))
    }
}

/// Convenience alias for `Result` with [`StudioError`].
pub type StudioResult<T> = Result<T, StudioError>;
