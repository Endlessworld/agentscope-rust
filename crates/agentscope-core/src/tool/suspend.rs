//! Tool suspension semantics, mirroring `io.agentscope.core.tool.ToolSuspendException`.
//!
//! A tool that requires external execution (e.g. a schema-only / MCP-registered
//! tool) signals this by throwing a `ToolSuspendException`. The framework converts
//! it to a pending [`ToolResultBlock`] flagged as suspended, returns a message to the
//! user with `GenerateReason::ToolSuspended`, and waits for the user result.
use crate::error::Error;

/// Signals that a tool execution should be suspended and handled externally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSuspendException {
    reason: String,
}

impl ToolSuspendException {
    /// Creates a new suspension signal with the given reason (defaults to a
    /// generic message when empty).
    pub fn new(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            reason: if reason.is_empty() {
                "Tool execution suspended".to_string()
            } else {
                reason
            },
        }
    }

    /// The user-facing reason for the suspension.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Converts this signal to a framework [`Error::ToolSuspended`].
    pub fn to_error(&self) -> Error {
        Error::tool_suspended(self.reason.clone())
    }
}

impl std::fmt::Display for ToolSuspendException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ToolSuspendException {}
