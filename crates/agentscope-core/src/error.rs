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

    /// The execution host paused this invocation at a recoverable boundary.
    ///
    /// This is control flow, not a failure: the invocation is recorded as suspended and may be
    /// resumed from its committed position. Never convert it into a tool error result.
    #[error("Invocation paused by execution host")]
    HostPaused,

    /// Agent execution error.
    #[error("Agent error: {0}")]
    Agent(String),

    /// A request was interrupted.
    #[error("Interrupted: {0}")]
    Interrupted(String),

    /// An execution was cancelled by its caller.
    ///
    /// Distinct from [`Error::Interrupted`]: an interrupt asks a running loop to stop and may
    /// still produce a result, while a cancellation aborts the execution outright. Mirrors
    /// Java's `CancellationException`.
    #[error("Cancelled: {0}")]
    Cancelled(String),

    /// Journal persistence failure.
    ///
    /// Persistence failures must propagate and must never become a successful
    /// model/tool result. Mirrors Java `JournalException`.
    #[error("{0}")]
    Journal(String),

    /// An action observation could not be acknowledged.
    ///
    /// Never convert this into a retryable tool result: replaying an action whose
    /// lifecycle fact was not persisted is unsafe. Mirrors Java
    /// `ActionObservationException`.
    #[error("Unable to persist action observation; do not replay the action automatically: {0}")]
    ActionObservation(String),

    /// The `action/start` fact could not be acknowledged, so the action **never ran**.
    ///
    /// Unlike [`Error::ActionObservation`] this is safe to surface as a retryable tool
    /// failure: no side effect happened, and the journal has no dispatch record to
    /// reconcile. Callers downgrade it to a normal tool error result.
    #[error("Unable to record the action start; the action did not run and may be retried: {0}")]
    ActionDispatch(String),

    /// An `agent_state` save lost an optimistic-concurrency race under
    /// [`ConflictPolicy::Fail`](crate::state::ConflictPolicy::Fail).
    ///
    /// Mirrors Java `ConcurrentSessionModificationException`: the write was rejected and
    /// nothing was persisted, so the caller may retry from a freshly loaded baseline.
    #[error(
        "Concurrent modification of session state (userId={user_id}, sessionId={session_id}, key={key}, expectedVersion={expected_version})"
    )]
    ConcurrentSessionModification {
        user_id: String,
        session_id: String,
        key: String,
        expected_version: i64,
    },

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
        Error::HttpStatus {
            status,
            message: message.into(),
        }
    }

    /// Creates a tool-suspension error signalling external execution.
    pub fn tool_suspended(reason: impl Into<String>) -> Self {
        Error::ToolSuspended(reason.into())
    }

    /// Whether this error is retryable (429 or 5xx, or IO/network errors).
    ///
    /// Transport-level failures ([`Error::Http`], e.g. a dropped Wi-Fi
    /// connection or a read timeout) are always retryable so the model call is
    /// re-established once the network returns.
    ///
    /// A timeout or a dropped connection also surfaces as a plain
    /// [`Error::Tool`] / [`Error::Model`] error (the framework's own execution
    /// timeout, a provider SDK reporting a stalled stream), so a *transient*
    /// failure message is retryable too. Classification is deliberately narrow:
    /// business failures ("Tool not found", a schema mismatch, a refused call)
    /// never match, because retrying them either repeats a side effect or fails
    /// identically.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::HttpStatus { status, .. } => *status == 429 || *status >= 500,
            Error::Http(_) => true,
            Error::Io(_) => true,
            Error::Tool(message) | Error::Model(message) => is_transient_failure(message),
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

/// Markers of a transient failure inside a [`Error::Tool`] / [`Error::Model`] message.
///
/// Covers the framework's own timeout wrapper (`Tool execution timeout after …`),
/// the wording of `tokio`/`reqwest` (`timed out`, `deadline has elapsed`) and the
/// socket failures providers re-wrap (`connection reset`, `broken pipe`).
const TRANSIENT_FAILURE_MARKERS: &[&str] = &[
    "timed out",
    "timeout",
    "deadline has elapsed",
    "connection reset",
    "connection refused",
    "connection aborted",
    "connection closed",
    "broken pipe",
    "unexpected eof",
    "temporarily unavailable",
];

/// Whether `message` describes a transient failure worth retrying.
pub(crate) fn is_transient_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    TRANSIENT_FAILURE_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
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

    #[test]
    fn a_timeout_wrapped_as_a_tool_or_model_error_is_retryable() {
        // The execution timeout is raised as `Error::Tool` and a stalled provider
        // stream as `Error::Model`; neither carries an HTTP status, so without the
        // message classification the retry loops never fired for them.
        assert!(
            Error::Tool("Tool execution timeout after 5s for 'search'".to_string()).is_retryable()
        );
        assert!(Error::Model("request timed out".to_string()).is_retryable());
        assert!(Error::Tool("connection reset by peer".to_string()).is_retryable());
        assert!(Error::Model("stream deadline has elapsed".to_string()).is_retryable());
        // The existing classifications are untouched.
        assert!(Error::http_status(429, "slow down").is_retryable());
        assert!(Error::Http("dropped".to_string()).is_retryable());
    }

    #[test]
    fn a_business_tool_failure_is_not_retryable() {
        // Retrying these would repeat a side effect or fail identically.
        for message in [
            "Tool not found: nope",
            "Parameter validation failed: 'a' is a required property",
            "Unauthorized tool call: x is not available",
            "Tool 'x' is external and cannot be executed by the framework",
        ] {
            assert!(
                !Error::Tool(message.to_string()).is_retryable(),
                "{message}"
            );
        }
        assert!(!Error::Model("model 'x' does not accept image input".into()).is_retryable());
        assert!(!Error::http_status(400, "bad request").is_retryable());
    }
}
