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

    /// The underlying core/harness agent failed while driving a conversation round.
    ///
    /// Not retryable: the agent already applied its own model/tool retry policy.
    #[error("studio agent error: {0}")]
    Agent(String),

    /// An argument was rejected before any I/O happened.
    ///
    /// Mirrors Java's `IllegalArgumentException` checks (e.g. a blank
    /// `StudioUserAgent` name).
    #[error("invalid studio argument: {0}")]
    InvalidArgument(String),
}

impl StudioError {
    /// Whether this error is retryable by the HTTP retry logic.
    ///
    /// Only [`StudioError::Transport`] is retried: this crate separates network failures from
    /// HTTP-status failures, and a blind retry of a rejected request would re-submit
    /// `pushMessage` / `requestUserInput` for nothing.
    ///
    /// **Deviation from Java**: `StudioClient.executeWithRetry` filters on
    /// `t instanceof IOException`, and Java's client turns *every* non-2xx response into an
    /// `IOException` (`StudioClient.java:116/161/209`), so Java does retry 4xx/5xx. Rust keeps
    /// the status visible in [`StudioError::Http`] instead (tracker §6.4 P2-1).
    pub fn is_retryable(&self) -> bool {
        matches!(self, StudioError::Transport(_))
    }
}

/// Convenience alias for `Result` with [`StudioError`].
pub type StudioResult<T> = Result<T, StudioError>;
