//! Toolkit manages the registration, retrieval, grouping, validation and
//! execution of agent tools. Mirrors `io.agentscope.core.tool.Toolkit`.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::Result;
use crate::message::{JsonMap, ToolResultBlock, ToolUseBlock};
use crate::model::{ExecutionConfig, ToolSchema};

use super::config::ToolkitConfig;
use super::executor::ToolExecutor;
use super::group::{ToolGroup, ToolGroupManager, ToolGroupScope};
use super::tool::Tool;

/// Manages tool registration, grouping, schema generation and dispatch.
#[derive(Clone)]
pub struct Toolkit {
    tools: Arc<std::sync::Mutex<HashMap<String, Arc<dyn Tool>>>>,
    groups: ToolGroupManager,
    config: ToolkitConfig,
    executor: ToolExecutor,
    presets: Arc<std::sync::Mutex<HashMap<String, JsonMap>>>,
    /// Tool name → originating MCP client (Java `RegisteredToolFunction.mcpClientName`).
    mcp_client_names: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Tool name → sub-agent entry point (Java `RegisteredToolFunction.subAgent`).
    sub_agent_names: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

impl Default for Toolkit {
    fn default() -> Self {
        Self {
            tools: Arc::new(std::sync::Mutex::new(HashMap::new())),
            groups: ToolGroupManager::new(),
            config: ToolkitConfig::default(),
            executor: ToolExecutor::new(),
            presets: Arc::new(std::sync::Mutex::new(HashMap::new())),
            mcp_client_names: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sub_agent_names: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }
}

impl std::fmt::Debug for Toolkit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toolkit")
            .field("tools", &self.tool_names())
            .field("active_groups", &self.groups.get_active_groups())
            .finish()
    }
}

impl Toolkit {
    /// Creates a new empty toolkit.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new empty toolkit with the given configuration.
    pub fn with_config(config: ToolkitConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Returns the current configuration.
    pub fn config(&self) -> &ToolkitConfig {
        &self.config
    }

    /// Returns the current configuration as a value.
    pub fn get_config(&self) -> ToolkitConfig {
        self.config.clone()
    }

    /// Replaces the toolkit configuration.
    pub fn set_config(&mut self, config: ToolkitConfig) {
        self.config = config;
    }

    /// Returns a copy of this toolkit (shared tool instances, independent
    /// grouping and configuration).
    ///
    /// Toolkit-aware tools (Java `ToolkitAware`) are deep-copy rebound to the
    /// new copy via [`Tool::rebind_toolkit`], so meta tools such as
    /// `reset_equipped_tools` mutate the owning snapshot, never the original.
    pub fn copy(&self) -> Toolkit {
        let tools = self.tools.lock().unwrap().clone();
        let presets = self.presets.lock().unwrap().clone();
        let mcp_client_names = self.mcp_client_names.lock().unwrap().clone();
        let sub_agent_names = self.sub_agent_names.lock().unwrap().clone();
        let copied = Toolkit {
            tools: Arc::new(std::sync::Mutex::new(tools)),
            groups: self.groups.copy(),
            config: self.config.clone(),
            executor: self.executor.clone(),
            presets: Arc::new(std::sync::Mutex::new(presets)),
            mcp_client_names: Arc::new(std::sync::Mutex::new(mcp_client_names)),
            sub_agent_names: Arc::new(std::sync::Mutex::new(sub_agent_names)),
        };
        let rebinds: Vec<(String, Arc<dyn Tool>)> = copied
            .tools
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(name, tool)| tool.rebind_toolkit(&copied).map(|t| (name.clone(), t)))
            .collect();
        for (name, tool) in rebinds {
            copied.tools.lock().unwrap().insert(name, tool);
        }
        copied
    }

    // ------------------------------------------------------------------
    // Registration
    // ------------------------------------------------------------------

    /// Registers a tool by name.
    pub fn register_tool(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.lock().unwrap().insert(name, tool);
    }

    /// Starts a fluent [`ToolRegistration`] against this toolkit.
    pub fn register(&self) -> super::registration::ToolRegistration<'_> {
        super::registration::ToolRegistration::new(self)
    }
    /// Registers a schema-only (external) tool.
    pub fn register_schema(&self, schema: ToolSchema) {
        let tool = Arc::new(crate::tool::SchemaOnlyTool::new(schema));
        self.register_tool(tool);
    }

    /// Registers several tools at once.
    pub fn register_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        for t in tools {
            self.register_tool(t);
        }
    }

    /// Returns the names of all registered tools.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.lock().unwrap().keys().cloned().collect()
    }

    /// Retrieves a tool by name.
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.lock().unwrap().get(name).cloned()
    }

    /// Resolves a tool by name under a per-call [`ToolRequestConfig`], composing it with
    /// this shared registry. External tools from the config take priority; in
    /// [`ToolMergeMode::ExternalOnly`] the backend is hidden entirely. Neither the
    /// registry nor the config is mutated.
    pub fn get_tool_with(
        &self,
        name: &str,
        config: Option<&super::tool_request_config::ToolRequestConfig>,
    ) -> Option<Arc<dyn Tool>> {
        match config {
            Some(config) => config.resolve_tool(name, self),
            None => self.get_tool(name),
        }
    }

    /// Returns the schemas of all currently active tools.
    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        self.tool_schemas_for(&self.groups.get_active_groups())
    }

    /// Returns the schemas of the tools active for the given group set.
    ///
    /// The result is sorted by tool name: the registry is a `HashMap`, whose iteration
    /// order is unspecified and changes after rehashing or in a new process. The schema
    /// list renders at the head of every request, so an unstable order would invalidate
    /// the provider's prompt cache for the whole conversation.
    pub fn tool_schemas_for(&self, active_groups: &[String]) -> Vec<ToolSchema> {
        let tools = self.tools.lock().unwrap();
        let mut out = Vec::new();
        for (name, tool) in tools.iter() {
            if self.groups.is_active_tool_for(name, active_groups) {
                out.push(tool.schema());
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Whether a tool is registered as external.
    pub fn is_external_tool(&self, name: &str) -> bool {
        self.tools
            .lock()
            .unwrap()
            .get(name)
            .map(|t| t.is_external())
            .unwrap_or(false)
    }

    /// Whether `name` resolves to an external tool under a per-call
    /// [`ToolRequestConfig`]. Externally injected (schema-only) tools are recognised here in
    /// addition to backend-registered external tools.
    pub fn is_external_tool_with(
        &self,
        name: &str,
        config: Option<&super::tool_request_config::ToolRequestConfig>,
    ) -> bool {
        self.get_tool_with(name, config)
            .map(|t| t.is_external())
            .unwrap_or(false)
    }

    /// Returns the schemas visible for the active groups under a per-call config.
    ///
    /// Backend schemas shadowed by an external tool are dropped, and
    /// [`ToolMergeMode::ExternalOnly`](super::tool_request_config::ToolMergeMode::ExternalOnly)
    /// skips the backend entirely — both without unregistering anything.
    pub fn tool_schemas_with(
        &self,
        config: Option<&super::tool_request_config::ToolRequestConfig>,
    ) -> Vec<ToolSchema> {
        let Some(config) = config else {
            return self.tool_schemas();
        };
        let active_groups = self.groups.get_active_groups();
        let mut out = Vec::new();
        if !config.hides_backend() {
            let tools = self.tools.lock().unwrap();
            for (name, tool) in tools.iter() {
                if config.overrides(name) {
                    continue;
                }
                if self.groups.is_active_tool_for(name, &active_groups) {
                    out.push(tool.schema());
                }
            }
        }
        for tool in config.external_tools().values() {
            out.push(tool.schema());
        }
        // Same stability requirement as `tool_schemas_for`: backend and external tools are
        // merged from two maps, so the combined list must be ordered deterministically.
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Whether a tool may run in parallel with itself. Unknown tools default to
    /// safe, mirroring the Java behaviour for legacy `AgentTool` instances that
    /// do not opt out via `isConcurrencySafe`.
    pub fn is_concurrency_safe_tool(&self, name: &str) -> bool {
        self.tools
            .lock()
            .unwrap()
            .get(name)
            .map(|t| t.is_concurrency_safe())
            .unwrap_or(true)
    }

    /// Whether a tool may be invoked (respecting the active groups).
    pub fn is_tool_active(&self, name: &str) -> bool {
        self.groups.is_active_tool(name)
    }

    // ------------------------------------------------------------------
    // Execution
    // ------------------------------------------------------------------

    /// Executes a single tool call through the governance pipeline.
    pub async fn call_tool(
        &self,
        call: &ToolUseBlock,
        ctx: crate::tool::ToolExecutionContext,
    ) -> Result<ToolResultBlock> {
        self.executor
            .execute(self, call, ctx, self.config.execution_config.as_ref())
            .await
    }

    /// Executes a single tool call with an explicit config.
    pub async fn call_tool_with_config(
        &self,
        call: &ToolUseBlock,
        ctx: crate::tool::ToolExecutionContext,
        config: Option<&ExecutionConfig>,
    ) -> Result<ToolResultBlock> {
        self.executor.execute(self, call, ctx, config).await
    }

    /// Executes multiple tool calls, honoring the toolkit's parallel flag.
    pub async fn call_tools(
        &self,
        calls: &[ToolUseBlock],
        ctx: crate::tool::ToolExecutionContext,
    ) -> Vec<Result<ToolResultBlock>> {
        self.executor
            .execute_all(
                self,
                calls,
                ctx,
                self.config.execution_config.as_ref(),
                self.config.parallel,
            )
            .await
    }

    /// Executes multiple tool calls with an explicit config and parallel flag.
    pub async fn call_tools_with_config(
        &self,
        calls: &[ToolUseBlock],
        ctx: crate::tool::ToolExecutionContext,
        config: Option<&ExecutionConfig>,
        parallel: bool,
    ) -> Vec<Result<ToolResultBlock>> {
        self.executor
            .execute_all(self, calls, ctx, config, parallel)
            .await
    }

    // ------------------------------------------------------------------
    // Preset parameters
    // ------------------------------------------------------------------

    /// Sets preset parameters for a tool (LLM input cannot override these).
    pub fn set_preset_parameters(&self, name: &str, presets: JsonMap) {
        self.presets
            .lock()
            .unwrap()
            .insert(name.to_string(), presets);
    }

    /// Returns a copy of the preset parameters for a tool.
    pub fn preset_parameters(&self, name: &str) -> JsonMap {
        self.presets
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Merges preset parameters over the given input (presets take precedence).
    pub fn merge_presets(&self, name: &str, input: JsonMap) -> JsonMap {
        let presets = self.presets.lock().unwrap().get(name).cloned();
        match presets {
            Some(p) => {
                let mut merged = input;
                for (k, v) in p {
                    merged.insert(k, v);
                }
                merged
            }
            None => input,
        }
    }

    // ------------------------------------------------------------------
    // Registration metadata (mcpClientName / subAgent)
    // ------------------------------------------------------------------

    /// Tags a tool with its originating MCP client name.
    pub fn set_mcp_client_name(&self, tool: &str, client: String) {
        self.mcp_client_names
            .lock()
            .unwrap()
            .insert(tool.to_string(), client);
    }

    /// Returns the MCP client a tool originates from, if any.
    pub fn mcp_client_name(&self, tool: &str) -> Option<String> {
        self.mcp_client_names.lock().unwrap().get(tool).cloned()
    }

    /// Tags a tool as a sub-agent entry point.
    pub fn set_sub_agent_name(&self, tool: &str, agent: String) {
        self.sub_agent_names
            .lock()
            .unwrap()
            .insert(tool.to_string(), agent);
    }

    /// Returns the sub-agent behind a tool, if any.
    pub fn sub_agent_name(&self, tool: &str) -> Option<String> {
        self.sub_agent_names.lock().unwrap().get(tool).cloned()
    }

    // ------------------------------------------------------------------
    // Groups
    // ------------------------------------------------------------------

    pub fn create_tool_group(&self, name: &str, description: &str, active: bool) {
        self.groups.create_tool_group(name, description, active);
    }

    pub fn create_tool_group_with_scope(
        &self,
        name: &str,
        description: &str,
        active: bool,
        scope: ToolGroupScope,
    ) {
        self.groups
            .create_tool_group_with_scope(name, description, active, scope);
    }

    pub fn register_tool_group(&self, group: ToolGroup) {
        self.groups.register_tool_group(group);
    }

    pub fn update_tool_groups(&self, groups: &[String], active: bool) {
        self.groups.update_tool_groups(groups, active);
    }

    pub fn get_active_groups(&self) -> Vec<String> {
        self.groups.get_active_groups()
    }

    pub fn set_active_groups(&self, groups: &[String]) {
        self.groups.set_active_groups(groups);
    }

    pub fn get_tool_group(&self, name: &str) -> Option<ToolGroup> {
        self.groups.get_tool_group(name)
    }

    pub fn get_tool_group_names(&self) -> Vec<String> {
        self.groups.get_tool_group_names()
    }

    /// Names of all META-scoped tool groups (manageable by `reset_equipped_tools`).
    pub fn meta_group_names(&self) -> Vec<String> {
        self.groups.get_meta_group_names()
    }

    /// Human-readable summary of the META-scoped tool groups (tool instructions).
    pub fn groups_notes(&self) -> String {
        self.groups.get_notes()
    }

    /// Implements the `reset_equipped_tools` meta tool logic: validates that all
    /// requested groups exist and are META-scoped, then replaces the active set.
    /// Returns a human-readable description of the resulting state.
    pub fn reset_equipped_tools(&self, to_activate: &[String]) -> Result<String> {
        for group in to_activate {
            self.groups.validate_group_exists(group)?;
            if let Some(g) = self.groups.get_tool_group(group) {
                if g.scope() != ToolGroupScope::Meta {
                    return Err(crate::error::Error::Tool(format!(
                        "Group '{}' is not manageable by this tool.",
                        group
                    )));
                }
            }
        }

        self.groups.replace_meta_active_groups(to_activate);

        if to_activate.is_empty() {
            return Ok("All tool groups are currently deactivated.".to_string());
        }

        let group_names = to_activate.join(", ");
        let mut result = format!("The currently activated tool group(s): {}.\n", group_names);

        let has_instructions = to_activate.iter().any(|g| {
            self.groups
                .get_tool_group(g)
                .map(|grp| !grp.description().is_empty())
                .unwrap_or(false)
        });
        if has_instructions {
            result.push_str("<tool-instructions>\n");
            for g in to_activate {
                if let Some(grp) = self.groups.get_tool_group(g) {
                    if !grp.description().is_empty() {
                        result.push_str(&format!(
                            "<group name=\"{}\">{}</group>\n",
                            grp.name(),
                            grp.description()
                        ));
                    }
                }
            }
            result.push_str("</tool-instructions>");
        }
        Ok(result)
    }

    /// Registers the built-in `reset_equipped_tools` meta tool onto this toolkit.
    pub fn register_meta_tools(&self) {
        let tool = super::meta::MetaToolFactory::new(self).create_reset_equipped_tools();
        self.register_tool(Arc::new(tool));
    }

    /// Adds an existing tool to a group.
    pub fn add_tool_to_group(&self, group: &str, tool: &str) {
        self.groups.add_tool_to_group(group, tool);
    }

    /// Removes a tool from a group.
    pub fn remove_tool_from_group(&self, group: &str, tool: &str) {
        self.groups.remove_tool_from_group(group, tool);
    }

    // ------------------------------------------------------------------
    // Deletion (honours allow_tool_deletion)
    // ------------------------------------------------------------------

    pub fn remove_tool(&self, name: &str) {
        if !self.config.allow_tool_deletion {
            tracing::warn!("Tool deletion is disabled; ignoring remove of '{}'", name);
            return;
        }
        self.tools.lock().unwrap().remove(name);
    }

    pub fn remove_tool_if_same(&self, name: &str, expected: &Arc<dyn Tool>) -> bool {
        if !self.config.allow_tool_deletion {
            tracing::warn!("Tool deletion is disabled; ignoring remove of '{}'", name);
            return false;
        }
        let mut tools = self.tools.lock().unwrap();
        let removed = match tools.get(name) {
            Some(cur) => Arc::ptr_eq(cur, expected),
            None => false,
        };
        if removed {
            tools.remove(name);
        }
        removed
    }

    pub fn remove_tools(&self, names: &HashSet<String>) {
        if !self.config.allow_tool_deletion {
            tracing::warn!("Tool deletion is disabled; ignoring bulk remove");
            return;
        }
        let mut tools = self.tools.lock().unwrap();
        for n in names {
            tools.remove(n);
        }
    }

    pub fn remove_tool_groups(&self, groups: &[String]) {
        let orphans = self.groups.remove_tool_groups(groups);
        if !self.config.allow_tool_deletion {
            tracing::warn!("Tool deletion is disabled; ignoring removal of groups");
            return;
        }
        if !orphans.is_empty() {
            self.remove_tools(&orphans);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_schemas_are_sorted_by_name() {
        let toolkit = Toolkit::new();
        // Eight deliberately reverse-ordered registrations: a `HashMap` walk would match
        // the expected order by chance only 1/8! of the time.
        for name in [
            "hotel", "golf", "foxtrot", "echo", "delta", "charlie", "bravo", "alpha",
        ] {
            toolkit.register_schema(ToolSchema::new(name, "test tool"));
        }
        let schemas = toolkit.tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|schema| schema.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]
        );
    }
}
