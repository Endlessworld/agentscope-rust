//! Tool group management: grouping, activation and scoping of tools.
//!
//! Mirrors `io.agentscope.core.tool.ToolGroupManager`, `ToolGroup` and
//! `ToolGroupScope`. Groups let the agent (or host) enable/disable subsets of
//! tools at runtime; an ungrouped tool is always considered active.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Scope of a tool group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolGroupScope {
    /// Managed by the agent via the `reset_equipped_tools` meta tool. Uses
    /// replacement semantics: only the groups in the new active list stay on.
    #[default]
    Meta,
    /// Managed by host code; invisible to the meta tool and unaffected by it.
    External,
}

/// A named collection of tools with an activation flag and a scope.
#[derive(Debug, Clone)]
pub struct ToolGroup {
    name: String,
    description: String,
    active: bool,
    scope: ToolGroupScope,
    tools: HashSet<String>,
}

impl ToolGroup {
    pub fn name(&self) -> &str { &self.name }
    pub fn description(&self) -> &str { &self.description }
    pub fn is_active(&self) -> bool { self.active }
    pub fn set_active(&mut self, active: bool) { self.active = active; }
    pub fn scope(&self) -> ToolGroupScope { self.scope }
    pub fn tools(&self) -> &HashSet<String> { &self.tools }
    pub fn add_tool(&mut self, tool: impl Into<String>) { self.tools.insert(tool.into()); }
    pub fn remove_tool(&mut self, tool: &str) { self.tools.remove(tool); }
    pub fn contains_tool(&self, tool: &str) -> bool { self.tools.contains(tool) }
    /// Returns a deep copy of this group.
    pub fn copy(&self) -> ToolGroup { self.clone() }

    pub fn builder() -> ToolGroupBuilder { ToolGroupBuilder::default() }
}

/// Builder for [`ToolGroup`].
#[derive(Default)]
pub struct ToolGroupBuilder {
    name: Option<String>,
    description: String,
    active: bool,
    scope: ToolGroupScope,
    tools: HashSet<String>,
}

impl ToolGroupBuilder {
    pub fn name(mut self, name: impl Into<String>) -> Self { self.name = Some(name.into()); self }
    pub fn description(mut self, description: impl Into<String>) -> Self { self.description = description.into(); self }
    pub fn active(mut self, active: bool) -> Self { self.active = active; self }
    pub fn scope(mut self, scope: ToolGroupScope) -> Self { self.scope = scope; self }
    pub fn tools(mut self, tools: HashSet<String>) -> Self { self.tools = tools; self }
    pub fn add_tool(mut self, tool: impl Into<String>) -> Self { self.tools.insert(tool.into()); self }
    pub fn build(self) -> ToolGroup {
        ToolGroup {
            name: self.name.expect("ToolGroup requires a name"),
            description: self.description,
            active: self.active,
            scope: self.scope,
            tools: self.tools,
        }
    }
}

/// Thread-safe manager for tool groups, mirroring `ToolGroupManager`.
#[derive(Clone, Default)]
pub struct ToolGroupManager {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    tool_groups: HashMap<String, ToolGroup>,
    /// Reverse index: tool name -> set of group names containing it.
    tools_to_groups: HashMap<String, HashSet<String>>,
    /// Currently active group names.
    active_groups: Vec<String>,
}

impl ToolGroupManager {
    pub fn new() -> Self { Self::default() }

    /// Creates a group and registers it as active/inactive.
    pub fn create_tool_group(&self, name: &str, description: &str, active: bool) {
        self.create_tool_group_with_scope(name, description, active, ToolGroupScope::Meta);
    }

    pub fn create_tool_group_with_scope(&self, name: &str, description: &str, active: bool, scope: ToolGroupScope) {
        let group = ToolGroup::builder().name(name).description(description).active(active).scope(scope).build();
        self.register_tool_group(group);
    }

    /// Registers a pre-built group. Duplicate names are ignored.
    pub fn register_tool_group(&self, group: ToolGroup) {
        let mut inner = self.inner.lock().unwrap();
        if inner.tool_groups.contains_key(group.name()) {
            return;
        }
        for t in group.tools().iter() {
            inner.tools_to_groups.entry(t.clone()).or_default().insert(group.name().to_string());
        }
        let name = group.name().to_string();
        if group.is_active() && !inner.active_groups.contains(&name) {
            inner.active_groups.push(name.clone());
        }
        inner.tool_groups.insert(name, group);
    }

    /// Sets the active flag on several groups.
    pub fn update_tool_groups(&self, groups: &[String], active: bool) {
        let mut inner = self.inner.lock().unwrap();
        for g in groups {
            if let Some(grp) = inner.tool_groups.get_mut(g) {
                grp.set_active(active);
                if active {
                    if !inner.active_groups.contains(g) { inner.active_groups.push(g.clone()); }
                } else {
                    inner.active_groups.retain(|a| a != g);
                }
            }
        }
    }

    /// Removes several groups and returns tool names that lost all membership.
    pub fn remove_tool_groups(&self, groups: &[String]) -> HashSet<String> {
        let mut inner = self.inner.lock().unwrap();
        let mut orphans = HashSet::new();
        for g in groups {
            if let Some(grp) = inner.tool_groups.remove(g) {
                for t in grp.tools().iter() {
                    if let Some(gs) = inner.tools_to_groups.get_mut(t) {
                        gs.remove(g);
                        if gs.is_empty() {
                            inner.tools_to_groups.remove(t);
                            orphans.insert(t.clone());
                        }
                    }
                }
                inner.active_groups.retain(|a| a != g);
            }
        }
        orphans
    }

    pub fn add_tool_to_group(&self, group: &str, tool: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(grp) = inner.tool_groups.get_mut(group) {
            grp.add_tool(tool);
        }
        inner.tools_to_groups.entry(tool.to_string()).or_default().insert(group.to_string());
    }

    pub fn remove_tool_from_group(&self, group: &str, tool: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(grp) = inner.tool_groups.get_mut(group) {
            grp.remove_tool(tool);
        }
        if let Some(gs) = inner.tools_to_groups.get_mut(tool) {
            gs.remove(group);
            if gs.is_empty() { inner.tools_to_groups.remove(tool); }
        }
    }

    /// Whether the given group exists and is active.
    pub fn is_active_group(&self, name: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.active_groups.iter().any(|a| a == name)
    }

    /// Whether the given tool may be invoked (ungrouped tools are always active).
    pub fn is_active_tool(&self, name: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        match inner.tools_to_groups.get(name) {
            None => true,
            Some(groups) => groups.iter().any(|g| inner.active_groups.contains(g)),
        }
    }

    /// Whether the tool is active within the given (call-slot) group set.
    pub fn is_active_tool_for(&self, name: &str, active_groups: &[String]) -> bool {
        let inner = self.inner.lock().unwrap();
        match inner.tools_to_groups.get(name) {
            None => true,
            Some(groups) => groups.iter().any(|g| active_groups.contains(g)),
        }
    }

    /// Whether the tool belongs to at least one group.
    pub fn is_grouped_tool(&self, name: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.tools_to_groups.contains_key(name)
    }

    pub fn get_tool_group(&self, name: &str) -> Option<ToolGroup> {
        self.inner.lock().unwrap().tool_groups.get(name).cloned()
    }

    pub fn get_tool_group_names(&self) -> Vec<String> {
        self.inner.lock().unwrap().tool_groups.keys().cloned().collect()
    }

    pub fn get_active_groups(&self) -> Vec<String> {
        self.inner.lock().unwrap().active_groups.clone()
    }

    /// Fully replaces the active groups (used for state restore).
    pub fn set_active_groups(&self, groups: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        for g in inner.tool_groups.values_mut() {
            g.set_active(groups.iter().any(|x| x == g.name()));
        }
        inner.active_groups = groups.to_vec();
    }
    /// Names of all groups with [`ToolGroupScope::Meta`] scope (invisible to the
    /// meta tool are EXTERNAL groups).
    pub fn get_meta_group_names(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner
            .tool_groups
            .values()
            .filter(|g| g.scope() == ToolGroupScope::Meta)
            .map(|g| g.name().to_string())
            .collect()
    }

    /// Returns an error if the group does not exist.
    pub fn validate_group_exists(&self, name: &str) -> crate::error::Result<()> {
        let inner = self.inner.lock().unwrap();
        if inner.tool_groups.contains_key(name) {
            Ok(())
        } else {
            Err(crate::error::Error::Tool(format!(
                "Tool group '{}' does not exist",
                name
            )))
        }
    }

    /// Human-readable summary of META-scoped groups, mirroring `ToolGroupManager.getNotes()`.
    pub fn get_notes(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let mut activated = String::from("Activated tool groups:\n");
        let mut inactive = String::from("Inactive tool groups:\n");
        let mut has_activated = false;
        let mut has_inactive = false;
        for g in inner.tool_groups.values() {
            if g.scope() != ToolGroupScope::Meta {
                continue;
            }
            let line = format!("- {}: {}\n", g.name(), g.description());
            if g.is_active() {
                has_activated = true;
                activated.push_str(&line);
            } else {
                has_inactive = true;
                inactive.push_str(&line);
            }
        }
        if !has_activated {
            activated.push_str("No tool groups are currently activated.\n");
        }
        if !has_inactive {
            inactive.push_str("No tool groups are currently inactive.\n");
        }
        activated.push_str(&inactive);
        activated
    }

    /// Replaces the active set of META-scoped groups with `to_activate`.
    ///
    /// All META groups not in the list are deactivated; EXTERNAL groups are
    /// unaffected. Mirrors `ToolGroupManager.replaceMetaActiveGroups`.
    pub fn replace_meta_active_groups(&self, to_activate: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        // Deactivate all active META-scoped groups.
        for g in inner.tool_groups.values_mut() {
            if g.scope() == ToolGroupScope::Meta && g.is_active() {
                g.set_active(false);
            }
        }
        let external_groups: HashSet<String> = inner
            .tool_groups
            .iter()
            .filter(|(_, g)| g.scope() == ToolGroupScope::External)
            .map(|(n, _)| n.clone())
            .collect();
        inner.active_groups.retain(|a| external_groups.contains(a));
        // Activate the specified META groups.
        for name in to_activate {
            if let Some(g) = inner.tool_groups.get_mut(name) {
                if g.scope() == ToolGroupScope::Meta {
                    g.set_active(true);
                    if !inner.active_groups.contains(name) {
                        inner.active_groups.push(name.clone());
                    }
                }
            }
        }
    }
}
