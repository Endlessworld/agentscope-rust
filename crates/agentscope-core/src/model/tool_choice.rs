//! Tool choice control for model calls, mirroring the Java sealed `ToolChoice` hierarchy.
use serde::{Deserialize, Serialize};

/// Controls how the model should handle tool calling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolChoice {
    /// Let the model decide whether and which tools to call (default).
    Auto,
    /// Prevent the model from calling any tools.
    None,
    /// Force the model to call at least one tool (model chooses which).
    Required,
    /// Force the model to call a specific tool by name.
    Specific { tool_name: String },
}

impl Default for ToolChoice {
    fn default() -> Self {
        ToolChoice::Auto
    }
}

impl ToolChoice {
    /// Creates a `Specific` tool choice, validating the tool name is non-empty.
    pub fn specific(tool_name: impl Into<String>) -> Self {
        let name = tool_name.into();
        if name.trim().is_empty() {
            panic!("Tool name must not be null or empty");
        }
        ToolChoice::Specific { tool_name: name }
    }
}
