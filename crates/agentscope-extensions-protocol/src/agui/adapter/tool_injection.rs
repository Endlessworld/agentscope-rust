//! Scoped registration of AG-UI frontend tools into an AgentScope [`Toolkit`], mirroring
//! `ToolInjection`.
//!
//! Frontend tool definitions are injected for one run and removed afterwards. If an injected tool
//! temporarily replaced an existing tool with the same name, closing the scope restores the
//! previous tool when the slot is empty. The scope is `Clone` so it can be owned by the event
//! stream and closed when the stream terminates.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::{SchemaOnlyTool, Tool, ToolSchema, Toolkit};

use crate::agui::converter::tool::AguiToolConverter;
use crate::agui::model::{RunAgentInput, ToolMergeMode};

/// An injected frontend tool together with its name, so it can be removed by identity.
type InjectedTool = (String, Arc<dyn Tool>);

/// Scoped registration of AG-UI frontend tools into a toolkit.
#[derive(Clone)]
pub struct ToolInjection {
    toolkit: Option<Toolkit>,
    registered_tools: Vec<InjectedTool>,
    previous_tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolInjection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInjection")
            .field("has_toolkit", &self.toolkit.is_some())
            .field(
                "registered_tools",
                &self
                    .registered_tools
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("previous_tools", &self.previous_tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolInjection {
    /// Creates a scoped tool injection.
    pub fn new(
        toolkit: Option<Toolkit>,
        registered_tools: Vec<InjectedTool>,
        previous_tools: BTreeMap<String, Arc<dyn Tool>>,
    ) -> Self {
        Self {
            toolkit,
            registered_tools,
            previous_tools,
        }
    }

    /// A no-op injection scope.
    pub fn empty() -> Self {
        Self {
            toolkit: None,
            registered_tools: Vec::new(),
            previous_tools: BTreeMap::new(),
        }
    }

    /// Whether this scope injects anything.
    pub fn is_empty(&self) -> bool {
        self.toolkit.is_none()
    }

    /// Whether this scope is currently holding injected tools (i.e. [`Self::close`] is pending).
    pub fn is_active(&self) -> bool {
        self.toolkit.is_some()
    }

    /// The names of the tools registered by this scope.
    pub fn registered_tool_names(&self) -> Vec<&str> {
        self.registered_tools
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Removes injected frontend tools and restores displaced toolkit entries.
    ///
    /// Closing is idempotent: after the first call the scope holds no toolkit.
    pub fn close(&mut self) {
        let Some(toolkit) = self.toolkit.take() else {
            return;
        };
        for (name, tool) in self.registered_tools.iter().rev() {
            toolkit.remove_tool_if_same(name, tool);
        }
        for (name, tool) in std::mem::take(&mut self.previous_tools) {
            if toolkit.get_tool(&name).is_none() {
                toolkit.register_tool(tool);
            }
        }
        self.registered_tools.clear();
    }

    /// Injects the frontend tools of `input` into `toolkit` according to `mode`.
    ///
    /// Returns an empty scope when there is nothing to do: no frontend tools, no toolkit, or
    /// [`ToolMergeMode::AgentOnly`].
    pub fn inject(
        toolkit: Option<&Toolkit>,
        input: &RunAgentInput,
        mode: ToolMergeMode,
    ) -> Self {
        if !input.has_tools() || mode == ToolMergeMode::AgentOnly {
            return Self::empty();
        }
        let Some(toolkit) = toolkit else {
            return Self::empty();
        };

        let mut previous_tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
        if mode == ToolMergeMode::FrontendOnly {
            // The agent's existing toolkit is ignored completely: stash and remove every tool.
            for name in toolkit.tool_names() {
                if let Some(previous) = toolkit.get_tool(&name) {
                    previous_tools.insert(name.clone(), previous);
                    toolkit.remove_tool(&name);
                }
            }
        }

        let converter = AguiToolConverter::new();
        let mut registered_tools: Vec<InjectedTool> = Vec::new();
        for schema in converter.to_tool_schema_list(&input.tools) {
            if let Some(previous) = toolkit.get_tool(&schema.name) {
                previous_tools
                    .entry(schema.name.clone())
                    .or_insert(previous);
            }
            let frontend_tool = SchemaOnlyTool::new(schema);
            let name = frontend_tool.name().to_string();
            let handle: Arc<dyn Tool> = Arc::new(frontend_tool);
            toolkit.register_tool(Arc::clone(&handle));
            registered_tools.push((name, handle));
        }

        Self {
            toolkit: Some(toolkit.clone()),
            registered_tools,
            previous_tools,
        }
    }
}

/// The schema of a frontend tool, exposed for callers that need to inspect injections.
pub fn injected_tool_schema(tool: &SchemaOnlyTool) -> ToolSchema {
    tool.schema()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::model::AguiTool;
    use agentscope_core::Tool;
    use serde_json::{json, Map};
    fn frontend_tool(name: &str) -> AguiTool {
        let mut parameters = Map::new();
        parameters.insert("type".to_string(), json!("object"));
        AguiTool::new(name, &format!("{name} description"), parameters)
    }

    fn input_with_tools(tools: Vec<AguiTool>) -> RunAgentInput {
        RunAgentInput::new(
            "t",
            "r",
            Vec::new(),
            tools,
            Vec::new(),
            Map::new(),
            Map::new(),
            Vec::new(),
        )
    }

    #[test]
    fn empty_scope_closes_without_a_toolkit() {
        let mut injection = ToolInjection::empty();
        assert!(injection.is_empty());
        assert!(injection.registered_tool_names().is_empty());
        injection.close();
        injection.close();
        let rendered = format!("{injection:?}");
        assert!(rendered.contains("has_toolkit: false"));
    }

    #[test]
    fn agent_only_mode_injects_nothing() {
        let toolkit = Toolkit::new();
        let injection = ToolInjection::inject(
            Some(&toolkit),
            &input_with_tools(vec![frontend_tool("search")]),
            ToolMergeMode::AgentOnly,
        );
        assert!(injection.is_empty());
        assert!(toolkit.get_tool("search").is_none());
    }

    #[test]
    fn merge_mode_registers_frontend_tools_and_restores_on_close() {
        let toolkit = Toolkit::new();
        toolkit.register_schema(ToolSchema::new("agent_tool", "agent tool"));
        let input = input_with_tools(vec![frontend_tool("search")]);
        let mut injection = ToolInjection::inject(
            Some(&toolkit),
            &input,
            ToolMergeMode::MergeFrontendPriority,
        );
        assert!(injection.is_active());
        assert_eq!(injection.registered_tool_names(), vec!["search"]);
        let registered = toolkit.get_tool("search").expect("frontend tool injected");
        // Injected frontend tools are external: the client executes them.
        assert!(registered.is_external());
        assert!(toolkit.get_tool("agent_tool").is_some());

        injection.close();
        assert!(!injection.is_active());
        assert!(toolkit.get_tool("search").is_none());
        assert!(toolkit.get_tool("agent_tool").is_some());
    }

    #[test]
    fn frontend_only_mode_removes_agent_tools_then_restores_them() {
        let toolkit = Toolkit::new();
        toolkit.register_schema(ToolSchema::new("agent_tool", "agent tool"));
        let input = input_with_tools(vec![frontend_tool("search")]);
        let mut injection = ToolInjection::inject(
            Some(&toolkit),
            &input,
            ToolMergeMode::FrontendOnly,
        );
        assert!(toolkit.get_tool("agent_tool").is_none());
        assert!(toolkit.get_tool("search").is_some());
        injection.close();
        assert!(toolkit.get_tool("agent_tool").is_some());
        assert!(toolkit.get_tool("search").is_none());
    }

    #[test]
    fn a_frontend_tool_shadowing_an_agent_tool_is_restored_on_close() {
        let toolkit = Toolkit::new();
        toolkit.register_schema(ToolSchema::new("shared", "agent shadowed"));
        let input = input_with_tools(vec![frontend_tool("shared")]);
        let mut injection = ToolInjection::inject(
            Some(&toolkit),
            &input,
            ToolMergeMode::MergeFrontendPriority,
        );
        let shadowing = toolkit.get_tool("shared").expect("frontend shadow wins");
        assert!(shadowing.is_external());
        injection.close();
        let restored = toolkit.get_tool("shared").expect("agent tool restored");
        // The displaced entry was itself a schema-only tool, so it is external by definition;
        // the restore contract is that the *original instance* is back, not the frontend shadow.
        assert!(!std::sync::Arc::ptr_eq(&restored, &shadowing));
        assert_eq!(restored.description(), "agent shadowed");
    }

    #[test]
    fn missing_toolkit_or_tools_yield_an_empty_scope() {
        assert!(ToolInjection::inject(
            None,
            &input_with_tools(vec![frontend_tool("search")]),
            ToolMergeMode::MergeFrontendPriority
        )
        .is_empty());
        let toolkit = Toolkit::new();
        assert!(ToolInjection::inject(
            Some(&toolkit),
            &input_with_tools(Vec::new()),
            ToolMergeMode::MergeFrontendPriority
        )
        .is_empty());
    }

    #[test]
    fn injected_tool_schema_round_trips_parameters() {
        let schema = ToolSchema::new("x", "y").parameters(Map::new());
        let tool = SchemaOnlyTool::new(schema.clone());
        assert_eq!(injected_tool_schema(&tool).name, "x");
        assert_eq!(tool.name(), "x");
    }
}
