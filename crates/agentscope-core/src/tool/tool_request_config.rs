//! Immutable per-call tool request config, mirroring
//! `io.agentscope.core.tool.{ToolRequestConfig, ToolMergeMode}`.
//!
//! A [`ToolRequestConfig`] is composed with — never copied into, never mutated on — the
//! shared, stateless [`Toolkit`], producing the tool surface for one call. Multiple
//! concurrent calls can therefore disagree about which tools exist without touching each
//! other's view.
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::model::ToolSchema;

use super::tool::Tool;
use super::toolkit::Toolkit;

/// Keys under which a [`ToolRequestConfig`] travels in [`RuntimeContext`].
///
/// [`RuntimeContext`]: crate::agent::RuntimeContext
pub const TOOL_REQUEST_CONFIG_CONTEXT_KEY: &str = "tool_request_config";

/// How externally injected tools (e.g. from a frontend) combine with the agent's toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ToolMergeMode {
    /// Use only externally injected tools; the agent toolkit is ignored (not unregistered).
    ExternalOnly,
    /// Use only the agent's toolkit; externally injected tools are ignored.
    AgentOnly,
    /// Merge both; external tools override agent tools on name conflicts.
    #[default]
    MergeExternalPriority,
}

impl ToolMergeMode {
    /// The Java enum constant name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolMergeMode::ExternalOnly => "EXTERNAL_ONLY",
            ToolMergeMode::AgentOnly => "AGENT_ONLY",
            ToolMergeMode::MergeExternalPriority => "MERGE_EXTERNAL_PRIORITY",
        }
    }

    /// Parses a Java enum constant name (case-insensitive).
    pub fn from_wire(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "EXTERNAL_ONLY" => Some(ToolMergeMode::ExternalOnly),
            "AGENT_ONLY" => Some(ToolMergeMode::AgentOnly),
            "MERGE_EXTERNAL_PRIORITY" => Some(ToolMergeMode::MergeExternalPriority),
            _ => None,
        }
    }
}

/// An immutable per-call tool difference: external tools plus a merge mode.
///
/// External tools take priority over the backend regardless of the mode; the mode only
/// governs backend visibility ([`ToolMergeMode::ExternalOnly`] hides it without changing the
/// registry). [`ToolMergeMode::AgentOnly`] therefore only pairs with an empty tool set —
/// that is [`ToolRequestConfig::NONE`].
#[derive(Clone)]
pub struct ToolRequestConfig {
    external_tools: BTreeMap<String, Arc<dyn Tool>>,
    merge_mode: ToolMergeMode,
}

/// Equal to [`ToolRequestConfig::none`]: "use the shared toolkit as-is".
///
/// Not derived: the derive would take [`ToolMergeMode::default`]
/// (`MergeExternalPriority`), while the sentinel must be `AgentOnly`, matching Java
/// `ToolRequestConfig.NONE`.
impl Default for ToolRequestConfig {
    fn default() -> Self {
        Self::none()
    }
}

impl std::fmt::Debug for ToolRequestConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRequestConfig")
            .field("external_tools", &self.external_tools.keys().collect::<Vec<_>>())
            .field("merge_mode", &self.merge_mode)
            .finish()
    }
}

impl ToolRequestConfig {
    /// Sentinel meaning "use the shared toolkit as-is".
    pub fn none() -> Self {
        Self {
            external_tools: BTreeMap::new(),
            merge_mode: ToolMergeMode::AgentOnly,
        }
    }

    /// Builds a config from external tools and a merge mode.
    ///
    /// Rejects [`ToolMergeMode::AgentOnly`] combined with a non-empty tool set, and rejects
    /// external tools whose key does not match the tool's own name (both would silently
    /// produce an unreachable tool).
    pub fn new(
        external_tools: impl IntoIterator<Item = Arc<dyn Tool>>,
        merge_mode: ToolMergeMode,
    ) -> Result<Self> {
        let mut map = BTreeMap::new();
        for tool in external_tools {
            let name = tool.name().to_string();
            if !tool.is_external() {
                return Err(Error::Tool(format!(
                    "Request-config tool '{name}' must be external (schema-only)"
                )));
            }
            map.insert(name, tool);
        }
        if merge_mode == ToolMergeMode::AgentOnly && !map.is_empty() {
            return Err(Error::Tool(
                "AGENT_ONLY cannot include external tools".into(),
            ));
        }
        Ok(Self {
            external_tools: map,
            merge_mode,
        })
    }

    /// Convenience constructor from schemas: every schema becomes a schema-only tool.
    pub fn from_schemas(
        schemas: impl IntoIterator<Item = ToolSchema>,
        merge_mode: ToolMergeMode,
    ) -> Result<Self> {
        Self::new(
            schemas
                .into_iter()
                .map(|s| Arc::new(super::schema_only_tool::SchemaOnlyTool::new(s)) as Arc<dyn Tool>),
            merge_mode,
        )
    }

    /// The configured merge mode.
    pub fn merge_mode(&self) -> ToolMergeMode {
        self.merge_mode
    }

    /// The external tools carried by this config.
    pub fn external_tools(&self) -> &BTreeMap<String, Arc<dyn Tool>> {
        &self.external_tools
    }

    /// Whether this config hides the backend entirely.
    pub fn hides_backend(&self) -> bool {
        self.merge_mode == ToolMergeMode::ExternalOnly
    }

    /// Whether `name` is shadowed by an external tool.
    pub fn overrides(&self, name: &str) -> bool {
        self.external_tools.contains_key(name)
    }

    /// The single home of the "external -> hide-backend -> registry" policy, shared by
    /// [`Toolkit`] lookups and the executor so the two cannot drift.
    pub fn resolve_tool(&self, name: &str, toolkit: &Toolkit) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.external_tools.get(name) {
            return Some(Arc::clone(tool));
        }
        if self.hides_backend() {
            return None;
        }
        toolkit.get_tool(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::SchemaOnlyTool;

    fn external(name: &str) -> Arc<dyn Tool> {
        Arc::new(SchemaOnlyTool::new(ToolSchema::new(name, "frontend tool")))
    }

    #[test]
    fn none_means_use_the_shared_toolkit() {
        let toolkit = Toolkit::new();
        toolkit.register_schema(ToolSchema::new("agent_tool", "agent tool"));
        let cfg = ToolRequestConfig::none();
        assert_eq!(cfg.merge_mode(), ToolMergeMode::AgentOnly);
        assert!(!cfg.hides_backend());
        assert!(cfg.resolve_tool("agent_tool", &toolkit).is_some());
        assert!(cfg.resolve_tool("missing", &toolkit).is_none());
    }

    #[test]
    fn external_tools_override_the_backend() {
        let toolkit = Toolkit::new();
        toolkit.register_schema(ToolSchema::new("shared", "agent shadowed"));
        let cfg = ToolRequestConfig::new(
            vec![external("shared")],
            ToolMergeMode::MergeExternalPriority,
        )
        .unwrap();
        assert!(cfg.overrides("shared"));
        let resolved = cfg.resolve_tool("shared", &toolkit).unwrap();
        assert_eq!(resolved.description(), "frontend tool");
    }

    #[test]
    fn external_only_hides_the_backend_without_unregistering() {
        let toolkit = Toolkit::new();
        toolkit.register_schema(ToolSchema::new("agent_tool", "agent tool"));
        let cfg = ToolRequestConfig::new(vec![external("search")], ToolMergeMode::ExternalOnly)
            .unwrap();
        assert!(cfg.hides_backend());
        assert!(cfg.resolve_tool("agent_tool", &toolkit).is_none());
        assert!(cfg.resolve_tool("search", &toolkit).is_some());
        // The shared registry is untouched.
        assert!(toolkit.get_tool("agent_tool").is_some());
    }

    #[test]
    fn agent_only_with_external_tools_is_rejected() {
        let err = ToolRequestConfig::new(vec![external("search")], ToolMergeMode::AgentOnly)
            .unwrap_err();
        assert!(err.to_string().contains("AGENT_ONLY"), "got {err}");
    }

    #[test]
    fn non_external_tools_are_rejected() {
        let tool = Arc::new(crate::tool::FunctionTool::new(
            "local",
            "a local tool",
            serde_json::json!({"type": "object"}),
            Box::new(|_, _| Ok(crate::message::ToolResultBlock::text("ok"))),
        )) as Arc<dyn Tool>;
        let err = ToolRequestConfig::new(vec![tool], ToolMergeMode::MergeExternalPriority)
            .unwrap_err();
        assert!(err.to_string().contains("must be external"), "got {err}");
    }

    #[test]
    fn merge_mode_wire_names_round_trip() {
        for mode in [
            ToolMergeMode::ExternalOnly,
            ToolMergeMode::AgentOnly,
            ToolMergeMode::MergeExternalPriority,
        ] {
            assert_eq!(ToolMergeMode::from_wire(mode.as_str()), Some(mode));
        }
        assert_eq!(ToolMergeMode::from_wire("external_only"), Some(ToolMergeMode::ExternalOnly));
        assert_eq!(ToolMergeMode::from_wire("NOPE"), None);
        assert_eq!(ToolMergeMode::default(), ToolMergeMode::MergeExternalPriority);
    }
}
