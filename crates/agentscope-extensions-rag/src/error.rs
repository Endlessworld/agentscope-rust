//! Error types for the RAG extension.
//!
//! Mirrors the Java exception homes (`EmbeddingException`, `VectorStoreException`,
//! `ReaderException`, `DifyApiException`, `DifyAuthException`) as one `RagError` enum so
//! every backend can share the same error surface.

use thiserror::Error;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, RagError>;

/// Unified error type for RAG operations.
#[derive(Debug, Error)]
pub enum RagError {
    /// Invalid argument (mirrors Java `IllegalArgumentException`).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Embedding generation failure (mirrors `EmbeddingException`).
    #[error("embedding error: {0}")]
    Embedding(String),

    /// Vector store failure (mirrors `VectorStoreException`).
    #[error("vector store error: {0}")]
    VectorStore(String),

    /// Document reading failure (mirrors `ReaderException`).
    #[error("reader error: {0}")]
    Reader(String),

    /// Operation intentionally not supported by the backend.
    #[error("unsupported operation: {0}")]
    Unsupported(String),

    /// Transport-level failure (connect / timeout / body read).
    #[error("HTTP transport error: {0}")]
    Http(String),

    /// Non-auth HTTP error returned by a remote backend (mirrors `DifyApiException`).
    #[error("API error [{status_code}]: {message}")]
    Api {
        /// HTTP status code.
        status_code: u16,
        /// Backend specific error code, when present.
        error_code: Option<String>,
        /// Human-readable message.
        message: String,
    },

    /// Authentication / authorization failure (mirrors `DifyAuthException`).
    #[error("authentication failed [{status_code}]: {message}")]
    Auth {
        /// HTTP status code (401 / 403).
        status_code: u16,
        /// Human-readable message.
        message: String,
    },

    /// JSON (de)serialization failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl RagError {
    /// Whether the error is worth retrying (429, 5xx, or transport errors).
    ///
    /// Mirrors the Java `RetryInterceptor`: 4xx (except 429) are *not* retried.
    pub fn is_retryable(&self) -> bool {
        match self {
            RagError::Http(_) | RagError::Io(_) => true,
            RagError::Api { status_code, .. } => *status_code == 429 || *status_code >= 500,
            _ => false,
        }
    }

    /// Returns the HTTP status code when available.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            RagError::Api { status_code, .. } | RagError::Auth { status_code, .. } => {
                Some(*status_code)
            }
            _ => None,
        }
    }

    /// Returns the backend error code when available.
    pub fn error_code(&self) -> Option<&str> {
        match self {
            RagError::Api { error_code, .. } => error_code.as_deref(),
            _ => None,
        }
    }

    /// Whether this is a 4xx client error.
    pub fn is_client_error(&self) -> bool {
        self.status_code().is_some_and(|c| (400..500).contains(&c))
    }

    /// Whether this is a 5xx server error.
    pub fn is_server_error(&self) -> bool {
        self.status_code().is_some_and(|c| (500..600).contains(&c))
    }

    /// Whether this is a 429 rate-limit error.
    pub fn is_rate_limit_error(&self) -> bool {
        self.status_code() == Some(429)
    }
}
