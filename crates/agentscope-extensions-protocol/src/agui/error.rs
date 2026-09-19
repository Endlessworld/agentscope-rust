//! AG-UI specific errors, mirroring `io.agentscope.core.agui.AguiException`.
//!
//! Java uses a small `RuntimeException` hierarchy; Rust uses one enum with the same
//! three semantic cases so callers can branch on them.

/// Errors produced while handling AG-UI protocol traffic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AguiError {
    /// No agent is registered under the requested id (`AguiException.AgentNotFoundException`).
    #[error("Agent not found: {0}")]
    AgentNotFound(String),
    /// An AG-UI event could not be encoded (`AguiException.EncodingException`).
    #[error("Failed to encode AG-UI event: {0}")]
    Encoding(String),
    /// The incoming message/input does not satisfy the AG-UI contract.
    #[error("Invalid AG-UI input: {0}")]
    InvalidInput(String),
    /// A structurally impossible state was reached (mirrors `IllegalStateException`).
    #[error("Invalid AG-UI state: {0}")]
    IllegalState(String),
}

impl AguiError {
    /// Builds an [`AguiError::Encoding`] from any displayable cause.
    pub fn encoding(err: impl std::fmt::Display) -> Self {
        AguiError::Encoding(err.to_string())
    }

    /// Builds an [`AguiError::InvalidInput`] from any displayable detail.
    pub fn invalid_input(detail: impl std::fmt::Display) -> Self {
        AguiError::InvalidInput(detail.to_string())
    }
}

/// Convenience alias for AG-UI fallible operations.
pub type AguiResult<T> = Result<T, AguiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_not_found_matches_the_java_message() {
        let err = AguiError::AgentNotFound("assistant".to_string());
        assert_eq!(err.to_string(), "Agent not found: assistant");
    }

    #[test]
    fn encoding_wraps_any_displayable_cause() {
        let err = AguiError::encoding("bad json");
        assert_eq!(err, AguiError::Encoding("bad json".to_string()));
        assert!(err.to_string().contains("bad json"));
    }
}
