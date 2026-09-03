//! AgentEventEmitter + ConfirmResult, mirroring `io.agentscope.core.event`.
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::event::AgentEvent;
use crate::message::ToolUseBlock;
use crate::permission::PermissionRule;

/// Reactor-context-style key under which the emitter is stored.
pub const CONTEXT_KEY: &str = "agentscope.agent.event.emitter";
/// Forwarding emitter key injected into a child agent's context.
pub const FORWARDING_CONTEXT_KEY: &str = "agentscope.agent.event.forwarding.emitter";

/// Emits [`AgentEvent`]s into the parent agent's stream, optionally tagging a source path.
#[derive(Clone)]
pub struct AgentEventEmitter {
    sink: Arc<dyn Fn(AgentEvent) + Send + Sync>,
    source: Option<String>,
}


impl AgentEventEmitter {
    /// Creates an emitter backed by the given sink callback.
    pub fn new(sink: Arc<dyn Fn(AgentEvent) + Send + Sync>) -> Self {
        Self { sink, source: None }
    }

    /// Returns a copy that tags every emitted event with a source path (subagent forwarding).
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Emits an event, applying the source tag if configured.
    pub fn emit(&self, event: AgentEvent) {
        let event = match &self.source {
            Some(s) => event.with_source(s.clone()),
            None => event,
        };
        (self.sink)(event);
    }
}

/// The result of a single user confirmation decision for a tool call.
///
/// When confirmed, the caller may supply a modified `tool_call` (allowing the user to tweak
/// input) and/or new `rules` that the permission engine should remember for future calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfirmResult {
    pub confirmed: bool,
    #[serde(rename = "tool_call", default)]
    pub tool_call: Option<ToolUseBlock>,
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
    /// Optional human-readable message explaining the decision.
    #[serde(default)]
    pub message: String,
}

impl ConfirmResult {
    /// Creates a new confirmation result.
    pub fn new(confirmed: bool, tool_call: Option<ToolUseBlock>, rules: Vec<PermissionRule>) -> Self {
        Self { confirmed, tool_call, rules, message: String::new() }
    }

    /// Creates a denied result with a message.
    pub fn denied(message: impl Into<String>) -> Self {
        Self { confirmed: false, tool_call: None, rules: Vec::new(), message: message.into() }
    }

    /// Creates an allowed result.
    pub fn allowed() -> Self {
        Self { confirmed: true, tool_call: None, rules: Vec::new(), message: String::new() }
    }

    /// Creates an allowed result with a modified tool call.
    pub fn allowed_with(tool_call: ToolUseBlock) -> Self {
        Self { confirmed: true, tool_call: Some(tool_call), rules: Vec::new(), message: String::new() }
    }

    /// Whether the tool call was confirmed.
    pub fn is_confirmed(&self) -> bool {
        self.confirmed
    }
}
