//! How a model receives the tools found by a client-side search strategy.
//!
//! Mirrors `io.agentscope.core.tool.search.ToolSearchMode`. The enum lives in the model
//! module because the *provider* owns the loading protocol: [`Model::tool_search_mode`]
//! tells core which shape it may put discovered schemas in.
//!
//! [`Model::tool_search_mode`]: crate::model::Model::tool_search_mode

use serde::{Deserialize, Serialize};

/// The loading protocol a provider uses for tools discovered by `tool_search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ToolSearchMode {
    /// Discovered schemas are exposed through ordinary function calling (the default).
    #[default]
    Function,
    /// Discovered definitions are loaded through native client-search output items.
    Client,
    /// The eligible catalog is sent as deferred definitions; matches come back as references.
    Deferred,
}

impl ToolSearchMode {
    /// The wire/metadata spelling (Java `ToolSearchMode.name()`).
    pub fn as_str(self) -> &'static str {
        match self {
            ToolSearchMode::Function => "FUNCTION",
            ToolSearchMode::Client => "CLIENT",
            ToolSearchMode::Deferred => "DEFERRED",
        }
    }

    /// Parses the wire/metadata spelling, case-insensitively.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "FUNCTION" => Some(ToolSearchMode::Function),
            "CLIENT" => Some(ToolSearchMode::Client),
            "DEFERRED" => Some(ToolSearchMode::Deferred),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_function_and_round_trips_its_wire_name() {
        assert_eq!(ToolSearchMode::default(), ToolSearchMode::Function);
        for mode in [
            ToolSearchMode::Function,
            ToolSearchMode::Client,
            ToolSearchMode::Deferred,
        ] {
            assert_eq!(ToolSearchMode::from_wire(mode.as_str()), Some(mode));
            assert_eq!(
                ToolSearchMode::from_wire(&mode.as_str().to_ascii_lowercase()),
                Some(mode)
            );
            // The metadata form must match the wire spelling Java records.
            let json = serde_json::to_value(mode).unwrap();
            assert_eq!(json, serde_json::json!(mode.as_str()));
        }
        assert_eq!(ToolSearchMode::from_wire("nope"), None);
    }
}
