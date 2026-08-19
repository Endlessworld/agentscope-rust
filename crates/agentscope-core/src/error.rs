//! Core error types for AgentScope Rust, mirroring `io.agentscope.core.util` exceptions.

/// Result type alias for the core crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The unified error type for the core crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP transport error.
    #[error("HTTP transport error: {0}")]
    Http(String),

    /// HTTP transport error with status code (retryable-aware).
    #[error("HTTP error {status}: {message}")]
    HttpStatus { status: u16, message: String },

    /// Model invocation error.
    #[error("Model error: {0}")]
    Model(String),

    /// Tool execution error.
    #[error("Tool error: {0}")]
    Tool(String),

    /// Tool execution was suspended for external handling.
    #[error("Tool execution suspended: {0}")]
    ToolSuspended(String),

    /// Agent execution error.
    #[error("Agent error: {0}")]
    Agent(String),

    /// A request was interrupted.
    #[error("Interrupted: {0}")]
    Interrupted(String),

    /// Agent is shutting down.
    #[error("Agent is shutting down: {0}")]
    ShuttingDown(String),

    /// Generic error with a message.
    #[error("{0}")]
    Other(String),

    /// Aggregation of failures from multiple agents.
    #[error("{0}")]
    CompositeAgent(CompositeAgentException),
}

impl Error {
    /// Creates an HTTP error with the given status code.
    pub fn http_status(status: u16, message: impl Into<String>) -> Self {
        Error::HttpStatus { status, message: message.into() }
    }

    /// Creates a tool-suspension error signalling external execution.
    pub fn tool_suspended(reason: impl Into<String>) -> Self {
        Error::ToolSuspended(reason.into())
    }

    /// Whether this error is retryable (429 or 5xx, or IO/network errors).
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::HttpStatus { status, .. } => *status == 429 || *status >= 500,
            Error::Io(_) => true,
            _ => false,
        }
    }

    /// Returns the HTTP status code if this is an HTTP error.
    pub fn http_status_code(&self) -> Option<u16> {
        match self {
            Error::HttpStatus { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Information about a single agent failure within a [`CompositeAgentException`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentExceptionInfo {
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    /// Message of the underlying failure, or `None` when the cause is unknown.
    pub throwable_message: Option<String>,
}

impl AgentExceptionInfo {
    pub fn new(
        agent_id: Option<String>,
        agent_name: Option<String>,
        throwable_message: Option<String>,
    ) -> Self {
        Self {
            agent_id,
            agent_name,
            throwable_message,
        }
    }
}

/// An aggregation of failures raised by multiple agents, mirroring
/// `io.agentscope.core.exception.CompositeAgentException`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeAgentException {
    pub message: String,
    pub causes: Vec<AgentExceptionInfo>,
}

impl CompositeAgentException {
    pub fn new(message: impl Into<String>, causes: Vec<AgentExceptionInfo>) -> Self {
        Self {
            message: message.into(),
            causes,
        }
    }
}

impl std::fmt::Display for CompositeAgentException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.causes.is_empty() {
            return write!(f, "{}", self.message);
        }

        write!(f, "{} (caused by: ", self.message)?;
        for (i, cause) in self.causes.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }

            let has_ident = cause.agent_id.is_some() || cause.agent_name.is_some();
            if let Some(id) = &cause.agent_id {
                write!(f, "agentId={id}")?;
                if let Some(name) = &cause.agent_name {
                    write!(f, "({name})")?;
                }
            } else if let Some(name) = &cause.agent_name {
                write!(f, "agentName={name}")?;
            }

            if has_ident {
                write!(f, ": ")?;
            }

            match &cause.throwable_message {
                Some(msg) if !msg.is_empty() => write!(f, "{msg}")?,
                Some(_) => write!(f, "No message")?,
                None => write!(f, "Unknown error")?,
            }
        }
        write!(f, ")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_message_renders_causes() {
        let ex = CompositeAgentException::new(
            "multiple agents failed",
            vec![
                AgentExceptionInfo::new(
                    Some("a1".into()),
                    Some("Alice".into()),
                    Some("boom".into()),
                ),
                AgentExceptionInfo::new(None, Some("Bob".into()), None),
            ],
        );
        let rendered = ex.to_string();
        assert_eq!(
            rendered,
            "multiple agents failed (caused by: agentId=a1(Alice): boom, agentName=Bob: Unknown error)"
        );
    }

    #[test]
    fn empty_causes_render_base_message() {
        let ex = CompositeAgentException::new("only message", vec![]);
        assert_eq!(ex.to_string(), "only message");
    }
}
