//! Shared error type for the protocol extension crate.

/// Errors produced by the protocol extension crate.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// The JSON (de)serialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// A core framework error bubbled up (agent streaming, model, toolkit).
    #[error("core error: {0}")]
    Core(#[from] agentscope_core::Error),
    /// The HTTP transport failed (connect/timeout/decode).
    #[error("transport error: {0}")]
    Transport(String),
    /// The remote peer answered with a non-success status.
    #[error("http status {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Human-readable detail.
        message: String,
    },
    /// The peer returned a JSON-RPC error object.
    #[error("json-rpc error {code}: {message}")]
    JsonRpc {
        /// JSON-RPC error code.
        code: i64,
        /// JSON-RPC error message.
        message: String,
    },
    /// A task with the given id does not exist.
    #[error("task not found: {0}")]
    TaskNotFound(String),
    /// The requested method is not supported by this handler.
    #[error("unsupported method: {0}")]
    UnsupportedMethod(String),
    /// The payload was structurally invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// An operation did not complete within its deadline.
    #[error("timed out: {0}")]
    Timeout(String),
}

impl ProtocolError {
    /// Wraps a `reqwest` error as [`ProtocolError::Transport`].
    pub fn transport(err: reqwest::Error) -> Self {
        ProtocolError::Transport(err.to_string())
    }

    /// Wraps a `serde_json` error as [`ProtocolError::Json`].
    pub fn json(err: serde_json::Error) -> Self {
        ProtocolError::Json(err)
    }
}

/// Convenience alias for fallible protocol operations.
pub type ProtocolResult<T> = Result<T, ProtocolError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_errors_convert_through_from() {
        let err: ProtocolError = serde_json::from_str::<serde_json::Value>("{oops")
            .unwrap_err()
            .into();
        assert!(matches!(err, ProtocolError::Json(_)));
        assert!(err.to_string().starts_with("json error:"));
    }

    #[test]
    fn jsonrpc_error_renders_code_and_message() {
        let err = ProtocolError::JsonRpc {
            code: -32001,
            message: "task not found".to_string(),
        };
        assert_eq!(err.to_string(), "json-rpc error -32001: task not found");
    }
}
