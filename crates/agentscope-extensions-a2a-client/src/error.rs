//! Error type for the A2A client protocol extension.

/// Errors produced by the A2A client crate.
#[derive(Debug, thiserror::Error)]
pub enum A2aError {
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
    /// The requested method is not supported by this handler.
    #[error("unsupported method: {0}")]
    UnsupportedMethod(String),
    /// The payload was structurally invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl A2aError {
    /// Wraps a `reqwest` error as [`A2aError::Transport`].
    pub fn transport(err: reqwest::Error) -> Self {
        A2aError::Transport(err.to_string())
    }

    /// Wraps a `serde_json` error as [`A2aError::Json`].
    pub fn json(err: serde_json::Error) -> Self {
        A2aError::Json(err)
    }
}

/// Convenience alias for fallible A2A operations.
pub type A2aResult<T> = Result<T, A2aError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_errors_convert_through_from() {
        let err: A2aError = serde_json::from_str::<serde_json::Value>("{oops")
            .unwrap_err()
            .into();
        assert!(matches!(err, A2aError::Json(_)));
        assert!(err.to_string().starts_with("json error:"));
    }

    #[test]
    fn jsonrpc_error_renders_code_and_message() {
        let err = A2aError::JsonRpc {
            code: -32001,
            message: "task not found".to_string(),
        };
        assert_eq!(err.to_string(), "json-rpc error -32001: task not found");
    }
}
