//! Runtime context shared across agent execution.
use crate::formatter::ResponseFormat;
use crate::message::{JsonMap, Msg};
use crate::tool::Toolkit;

/// Runtime context shared across agent execution, mirroring `RuntimeContext`.
#[derive(Debug, Clone, Default)]
pub struct RuntimeContext {
    /// Agent name for this execution.
    pub agent_name: Option<String>,
    /// Raw user input.
    pub user_input: Option<String>,
    /// The current conversation memory.
    pub memory: Vec<Msg>,
    /// The live toolkit (may be empty).
    pub toolkit: Option<Toolkit>,
    /// Runtime metadata.
    pub metadata: JsonMap,
    /// Working directory for this execution.
    pub cwd: Option<String>,
    /// Optional structured-output schema for this call's reasoning phase.
    pub response_format: Option<ResponseFormat>,
    /// Session id for this execution (used by harness middlewares to scope inbox/async tools).
    pub session_id: Option<String>,
    /// User id for this execution.
    pub user_id: Option<String>,
    /// Agent id (as opposed to agent name) for this execution.
    pub agent_id: Option<String>,
}

impl RuntimeContext {
    /// Sets the session id.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Sets the user id.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Sets the agent id.
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Returns the session id, if any.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Returns the user id, if any.
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// Returns the agent id, if any.
    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }
}

impl RuntimeContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the accumulated text content of the memory.
    pub fn text_content(&self) -> String {
        self.memory
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
