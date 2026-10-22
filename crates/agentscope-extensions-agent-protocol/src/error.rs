//! Error type for the agent-protocol extension.

/// Errors produced by the agent-protocol crate.
#[derive(Debug, thiserror::Error)]
pub enum AgentProtocolError {
    /// The JSON (de)serialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// A core framework error bubbled up (agent streaming, model, toolkit).
    #[error("core error: {0}")]
    Core(#[from] agentscope_core::Error),
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

/// Convenience alias for fallible agent-protocol operations.
pub type AgentProtocolResult<T> = Result<T, AgentProtocolError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_errors_convert_through_from() {
        let err: AgentProtocolError = serde_json::from_str::<serde_json::Value>("{oops")
            .unwrap_err()
            .into();
        assert!(matches!(err, AgentProtocolError::Json(_)));
        assert!(err.to_string().starts_with("json error:"));
    }

    #[test]
    fn invalid_request_renders_message() {
        let err = AgentProtocolError::InvalidRequest("missing task id".to_string());
        assert_eq!(err.to_string(), "invalid request: missing task id");
    }
}
