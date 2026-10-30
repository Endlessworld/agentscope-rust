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
    /// No agent card could be resolved for the agent, so the call fails before any
    /// request is sent.
    ///
    /// Mirrors Java `A2aAgent#startCall`, which throws an `IllegalStateException` naming the
    /// agent and the resolver instead of building a client from a `null` card. Rust resolvers
    /// report the missing card as an error already (there is no `null`), so the client wraps
    /// that error with the agent name and the resolver that produced it.
    #[error(
        "No AgentCard found for A2A agent '{agent_name}' via {resolver}. \
         Check that the agent name matches the name registered on the server ({detail})"
    )]
    CardUnavailable {
        /// Name of the agent whose card was requested.
        agent_name: String,
        /// Concrete resolver implementation (Java `getClass().getSimpleName()`).
        resolver: String,
        /// The resolver's own failure text.
        detail: String,
    },
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

    /// Wraps a resolver failure as a fail-fast [`A2aError::CardUnavailable`].
    pub fn card_unavailable(
        agent_name: &str,
        resolver: &str,
        error: A2aError,
    ) -> Self {
        A2aError::CardUnavailable {
            agent_name: agent_name.to_string(),
            resolver: resolver.to_string(),
            detail: error.to_string(),
        }
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

    // Java: A2aAgent#startCall (1524b3191) — the failure names the agent and the resolver class.
    #[test]
    fn card_unavailable_names_the_agent_and_the_resolver() {
        let err = A2aError::card_unavailable(
            "remote",
            "FixedAgentCardResolver",
            A2aError::Transport("no card".to_string()),
        );
        assert!(matches!(err, A2aError::CardUnavailable { .. }));
        let text = err.to_string();
        assert_eq!(
            text,
            "No AgentCard found for A2A agent 'remote' via FixedAgentCardResolver. \
             Check that the agent name matches the name registered on the server (transport error: no card)"
        );
    }
}
