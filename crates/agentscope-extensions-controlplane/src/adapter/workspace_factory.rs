//! Harness workspace agent factory, mirroring
//! `io.agentscope.extensions.controlplane.adapter.{HarnessWorkspaceAgentFactory, WorkspaceAgentFactory}`.
//!
//! Loads a published Workspace definition into a fresh attempt workspace: definition files are
//! materialized under a per-attempt directory and the agent is built from the definition's
//! system prompt, model, iteration budget, tool selection, permissions and skills.
//!
//! The application supplies the fresh builder (its model registry and sandbox policy); runtime
//! files stay local to the application.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agentscope_core::model::Model;
use agentscope_core::permission::{PermissionBehavior, PermissionContextState, PermissionRule};
use agentscope_harness::agent::{HarnessAgent, HarnessAgentBuilder};
use agentscope_harness::SkillFilter;
use serde_json::Value;

use crate::model::AgentTaskAssignment;

/// Maps one canonical definition tool name to its runtime tool name.
///
/// Mirrors `HarnessWorkspaceAgentFactory.toolName`. Unknown names pass through unchanged.
pub fn tool_name(name: &str) -> String {
    match name {
        "bash" | "shell" => "execute",
        "read" => "read_file",
        "write" => "write_file",
        "edit" => "edit_file",
        "glob" => "glob_files",
        "grep" => "grep_files",
        "list_dir" => "list_files",
        other => other,
    }
    .to_string()
}

/// One MCP server selection derived from a definition, mirroring `McpServerConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSelection {
    /// `stdio` when the definition declares a `command`, otherwise `http`.
    pub transport: String,
    /// Command for a `stdio` server.
    pub command: Option<String>,
    /// URL for an `http` server.
    pub url: Option<String>,
    /// Whether a failure to reach this server blocks the attempt.
    pub required: bool,
    /// Whether the server's tools are enabled by default.
    pub default_tools_enabled: bool,
    /// Explicit allowlist (only meaningful when tools are disabled by default).
    pub enable_tools: Option<Vec<String>>,
    /// Explicit denylist.
    pub disable_tools: Vec<String>,
}

/// Tool selection derived from a definition, mirroring Java `ToolsConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsSelection {
    /// Whether the built-in agent toolset is enabled by default.
    pub default_tools_enabled: bool,
    /// Tools explicitly allowed while the default toolset is disabled.
    pub allow: Vec<String>,
    /// Tools explicitly denied while the default toolset is enabled.
    pub deny: Vec<String>,
    /// Declared MCP servers, keyed by name.
    pub mcp_servers: BTreeMap<String, McpSelection>,
}

impl Default for ToolsSelection {
    fn default() -> Self {
        Self {
            default_tools_enabled: true,
            allow: Vec::new(),
            deny: Vec::new(),
            mcp_servers: BTreeMap::new(),
        }
    }
}

/// Whether `name` is a well-formed MCP server name (`[A-Za-z0-9_-]{1,64}`, no `__`).
fn valid_mcp_name(name: &str) -> bool {
    let len = name.chars().count();
    (1..=64).contains(&len)
        && !name.contains("__")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Returns `value`'s string form the way Jackson's `JsonNode.asText()` does:
/// text for scalars, empty for containers.
fn as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(_) | Value::Object(_) => String::new(),
    }
}

/// Derives the tool selection from a definition, mirroring `HarnessWorkspaceAgentFactory.tools`.
pub fn tools(definition: &Value) -> Result<ToolsSelection, String> {
    let mut selection = ToolsSelection::default();

    if let Some(servers) = definition.get("mcpServers").and_then(Value::as_array) {
        for server in servers {
            let name = server.get("name").map(as_text).unwrap_or_default();
            if !valid_mcp_name(&name) || selection.mcp_servers.contains_key(&name) {
                return Err("Invalid or duplicate MCP name".to_string());
            }
            let command = server.get("command").filter(|v| !v.is_null()).map(as_text);
            let transport = server
                .get("transport")
                .map(as_text)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| {
                    if command.is_some() {
                        "stdio".to_string()
                    } else {
                        "http".to_string()
                    }
                });
            let enable_tools = server
                .get("enableTools")
                .and_then(Value::as_array)
                .map(|items| items.iter().map(as_text).collect::<Vec<_>>());
            let disable_tools = server
                .get("disableTools")
                .and_then(Value::as_array)
                .map(|items| items.iter().map(as_text).collect::<Vec<_>>())
                .unwrap_or_default();
            selection.mcp_servers.insert(
                name,
                McpSelection {
                    transport,
                    command,
                    url: server.get("url").filter(|v| !v.is_null()).map(as_text),
                    required: server
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    default_tools_enabled: true,
                    enable_tools,
                    disable_tools,
                },
            );
        }
    }

    if let Some(toolsets) = definition.get("tools").and_then(Value::as_array) {
        for toolset in toolsets {
            let enabled = toolset
                .get("defaultConfig")
                .and_then(|c| c.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let kind = toolset.get("type").map(as_text).unwrap_or_default();
            match kind.as_str() {
                "agent_toolset" => {
                    selection.default_tools_enabled = enabled;
                    for tool in configs(toolset) {
                        let name = tool_name(&tool.get("name").map(as_text).unwrap_or_default());
                        if !tool_enabled(tool, enabled) {
                            selection.deny.push(name);
                        } else if !enabled {
                            selection.allow.push(name);
                        }
                    }
                }
                "mcp_toolset" => {
                    let server_name = toolset
                        .get("mcpServerName")
                        .map(as_text)
                        .unwrap_or_default();
                    let Some(server) = selection.mcp_servers.get_mut(&server_name) else {
                        return Err("Undeclared MCP server".to_string());
                    };
                    server.default_tools_enabled = enabled;
                    let mut include: Vec<String> = Vec::new();
                    let mut exclude: Vec<String> = server.disable_tools.clone();
                    for tool in configs(toolset) {
                        let name = tool.get("name").map(as_text).unwrap_or_default();
                        if !tool_enabled(tool, enabled) {
                            exclude.push(name);
                        } else if !enabled {
                            include.push(name);
                        }
                    }
                    if !enabled {
                        if let Some(declared) = &server.enable_tools {
                            if !declared.is_empty() {
                                include.retain(|n| declared.contains(n));
                            }
                        }
                        server.enable_tools = Some(include);
                    }
                    server.disable_tools = exclude;
                }
                other => return Err(format!("Unsupported toolset: {other}")),
            }
        }
    }

    Ok(selection)
}

/// The `configs` array of a toolset (empty when absent).
fn configs(toolset: &Value) -> &[Value] {
    toolset
        .get("configs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// A tool config's `enabled` flag, defaulting to the toolset's default.
fn tool_enabled(tool: &Value, default: bool) -> bool {
    tool.get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// Builds the permission context from a definition, mirroring
/// `HarnessWorkspaceAgentFactory.permissions`.
pub fn permissions(definition: &Value) -> Result<PermissionContextState, String> {
    let mut builder = PermissionContextState::builder();
    for toolset in definition
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let mcp = toolset.get("type").map(as_text).unwrap_or_default() == "mcp_toolset";
        let prefix = if mcp {
            format!(
                "{}__",
                toolset
                    .get("mcpServerName")
                    .map(as_text)
                    .unwrap_or_default()
            )
        } else {
            String::new()
        };
        let fallback = toolset
            .get("defaultConfig")
            .and_then(|c| c.get("permissionPolicy"))
            .and_then(|p| p.get("type"))
            .map(as_text)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "always_ask".to_string());
        for tool in configs(toolset) {
            let raw = tool.get("name").map(as_text).unwrap_or_default();
            let name = if mcp {
                format!("{prefix}{raw}")
            } else {
                tool_name(&raw)
            };
            let policy = tool
                .get("permissionPolicy")
                .and_then(|p| p.get("type"))
                .map(as_text)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| fallback.clone());
            let behavior = match policy.as_str() {
                "always_allow" => PermissionBehavior::Allow,
                "always_ask" => PermissionBehavior::Ask,
                "deny" => PermissionBehavior::Deny,
                other => return Err(format!("Unsupported permission policy: {other}")),
            };
            let rule = PermissionRule::new(name.clone(), None, behavior, "workspace");
            builder = match behavior {
                PermissionBehavior::Allow => builder.add_allow_rule(name, rule),
                PermissionBehavior::Deny => builder.add_deny_rule(name, rule),
                PermissionBehavior::Ask => builder.add_ask_rule(name, rule),
                // `passthrough` is not reachable from a definition policy.
                PermissionBehavior::Passthrough => builder,
            };
        }
    }
    Ok(builder.build())
}

/// Path segments that can never appear in a Workspace definition.
const FORBIDDEN_SEGMENTS: &[&str] = &[
    "..",
    "sessions",
    "memory",
    "logs",
    ".git",
    "inputs",
    "outputs",
    "artifacts",
];

/// Writes `files` (a `{relative_path: text}` object) under `root`.
///
/// Rejects absolute paths, traversal, runtime state (`sessions`/`memory`/`logs`/...),
/// private memory (`MEMORY.md`, `*.log.jsonl`) and symbolic-link destinations, mirroring
/// `HarnessWorkspaceAgentFactory.materialize`.
pub fn materialize(root: &Path, files: &Value) -> Result<(), String> {
    let Some(fields) = files.as_object() else {
        return Ok(());
    };
    for (key, value) in fields {
        let relative = Path::new(key);
        if relative.is_absolute() {
            return Err("Definition file escapes attempt workspace".to_string());
        }
        for component in relative.components() {
            let segment = match component {
                std::path::Component::Normal(seg) => seg.to_string_lossy().to_lowercase(),
                std::path::Component::ParentDir => "..".to_string(),
                std::path::Component::CurDir => continue,
                _ => return Err("Definition file escapes attempt workspace".to_string()),
            };
            if FORBIDDEN_SEGMENTS.contains(&segment.as_str()) {
                return Err("Runtime state cannot be a Workspace definition".to_string());
            }
        }
        let lower = key.to_lowercase();
        if lower == "memory.md" || lower.ends_with(".log.jsonl") {
            return Err("Private memory cannot be a Workspace definition".to_string());
        }
        let path = root.join(relative);
        for ancestor in ancestors_within(root, &path) {
            if ancestor.is_symlink() {
                return Err("Definition path is a symbolic link".to_string());
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot create Workspace definition directory: {e}"))?;
        }
        std::fs::write(&path, as_text(value))
            .map_err(|e| format!("Cannot write Workspace definition file: {e}"))?;
    }
    Ok(())
}

/// `path`'s ancestors up to and including `root` (nearest first).
fn ancestors_within<'a>(root: &Path, path: &'a Path) -> Vec<&'a Path> {
    let mut out = Vec::new();
    let mut current = Some(path);
    while let Some(candidate) = current {
        out.push(candidate);
        if candidate == root {
            break;
        }
        current = candidate.parent();
    }
    out
}

/// Resolves a model name to a concrete model, mirroring the application-supplied builder's
/// model registry.
pub type ModelResolver = Arc<dyn Fn(&str) -> Option<Arc<dyn Model>> + Send + Sync>;

/// Loads a published definition into a fresh attempt workspace, mirroring
/// `HarnessWorkspaceAgentFactory`.
pub struct HarnessWorkspaceAgentFactory {
    root: PathBuf,
    builders: Box<dyn Fn() -> HarnessAgentBuilder + Send + Sync>,
    model_resolver: Option<ModelResolver>,
}

impl HarnessWorkspaceAgentFactory {
    /// Creates a factory that materializes attempts under `root` and builds agents from
    /// fresh builders supplied by the application.
    pub fn new(
        root: impl Into<PathBuf>,
        builders: impl Fn() -> HarnessAgentBuilder + Send + Sync + 'static,
    ) -> Self {
        Self {
            root: root.into(),
            builders: Box::new(builders),
            model_resolver: None,
        }
    }

    /// Installs the model registry used to resolve a definition's `model` name.
    pub fn with_model_resolver(mut self, resolver: ModelResolver) -> Self {
        self.model_resolver = Some(resolver);
        self
    }

    /// The attempt root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates a fresh attempt-owned agent, including its tools and credential policy.
    pub fn create(
        &self,
        definition: &Value,
        assignment: &AgentTaskAssignment,
    ) -> Result<HarnessAgent, String> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| format!("Cannot prepare Workspace root: {e}"))?;
        let workspace = self.new_attempt_dir()?;
        materialize(&workspace, definition.get("files").unwrap_or(&Value::Null))?;

        let name = definition
            .get("name")
            .map(as_text)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Workspace agent".to_string());
        let system = definition.get("system").map(as_text).unwrap_or_default();
        let mut builder = (self.builders)()
            .workspace(&workspace)
            .agent_id(format!("workspace-{}", assignment.attempt_id))
            .name(name)
            .sys_prompt(system);

        let model = definition
            .get("model")
            .map(as_text)
            .filter(|m| !m.trim().is_empty());
        if let (Some(model_name), Some(resolver)) = (model, &self.model_resolver) {
            if let Some(resolved) = resolver(&model_name) {
                builder = builder.model(resolved);
            }
        }
        let max_iters = definition
            .get("maxIters")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if max_iters > 0 {
            builder = builder.max_iters(max_iters as i32);
        }
        builder = builder.permission_context(permissions(definition)?);
        builder = builder.skill_filter(skill_filter(definition));

        let agent = builder.build();
        apply_tool_selection(&agent, &tools(definition)?);
        Ok(agent)
    }

    /// Creates a unique `attempt-*` directory under the root.
    fn new_attempt_dir(&self) -> Result<PathBuf, String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        for _ in 0..1024 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = self.root.join(format!("attempt-{n}"));
            match std::fs::create_dir(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("Cannot prepare Workspace definition: {e}")),
            }
        }
        Err("Cannot allocate an attempt workspace".to_string())
    }
}

/// The skill filter implied by a definition's `skills` array (`SkillFilter.none()` when empty).
pub fn skill_filter(definition: &Value) -> SkillFilter {
    let mut names = Vec::new();
    if let Some(skills) = definition.get("skills").and_then(Value::as_array) {
        for skill in skills {
            let name = skill
                .get("name")
                .map(as_text)
                .filter(|n| !n.is_empty())
                .or_else(|| skill.get("id").map(as_text))
                .unwrap_or_default();
            if !name.is_empty() {
                names.push(name);
            }
        }
    }
    if names.is_empty() {
        return SkillFilter::none();
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    SkillFilter::only(&refs)
}

/// Removes tools the definition does not permit from the built agent's toolkit.
///
/// Java applies a `ToolsConfig` allow/deny list; Rust's harness has no builder-level tool
/// selection, so the equivalent is applied on the assembled toolkit.
pub fn apply_tool_selection(agent: &HarnessAgent, selection: &ToolsSelection) {
    let toolkit = agent.toolkit();
    for name in toolkit.tool_names() {
        let disabled = if selection.default_tools_enabled {
            selection.deny.contains(&name)
        } else {
            !selection.allow.contains(&name)
        };
        if disabled {
            toolkit.remove_tool(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definition(files: Value) -> Value {
        files
    }

    #[test]
    fn materializes_definition_and_rejects_private_state_and_traversal() {
        let root = std::env::temp_dir().join(format!("controlplane-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        materialize(
            &root,
            &serde_json::json!({"AGENTS.md": "instructions", "skills/review/SKILL.md": "review"}),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("skills/review/SKILL.md")).unwrap(),
            "review"
        );

        for path in [
            "../escape",
            "/tmp/escape",
            "sessions/log",
            "memory/private",
            "MEMORY.md",
            "logs/run",
            "outputs/result",
        ] {
            let files = json!({ path: "private" });
            assert!(
                materialize(&root, &files).is_err(),
                "{path} must be rejected"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_symlink_destinations() {
        let root = std::env::temp_dir().join(format!("controlplane-sym-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("controlplane-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // Creating a symlink needs privilege on Windows; the check is verified only when the
        // platform allows it.
        let link = root.join("reference");
        let created = std::os::windows::fs::symlink_dir(&outside, &link).is_ok();
        if created {
            assert!(materialize(&root, &json!({"reference/secret": "overwrite"})).is_err());
            assert!(!outside.join("secret").exists());
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn applies_tool_selection_mcp_allowlist_and_permissions() {
        let def = json!({
            "mcpServers": [{"name": "repo", "command": "server", "enableTools": ["read"]}],
            "tools": [
                {
                    "type": "agent_toolset",
                    "defaultConfig": {"enabled": false},
                    "configs": [
                        {"name": "read", "enabled": true, "permissionPolicy": {"type": "always_ask"}},
                        {"name": "bash", "enabled": false, "permissionPolicy": {"type": "deny"}}
                    ]
                },
                {
                    "type": "mcp_toolset",
                    "mcpServerName": "repo",
                    "defaultConfig": {"enabled": false},
                    "configs": [
                        {"name": "read", "enabled": true},
                        {"name": "write", "enabled": true}
                    ]
                }
            ]
        });
        let tools = tools(&def).unwrap();
        assert!(!tools.default_tools_enabled);
        assert!(tools.allow.contains(&"read_file".to_string()));
        assert!(tools.deny.contains(&"execute".to_string()));
        assert_eq!(
            tools.mcp_servers["repo"].enable_tools.as_deref(),
            Some(["read".to_string()].as_slice())
        );
        assert_eq!(tools.mcp_servers["repo"].transport, "stdio");

        let permissions = permissions(&def).unwrap();
        assert!(permissions.deny_rules.contains_key("execute"));
        assert!(permissions.ask_rules.contains_key("read_file"));
        assert!(permissions.ask_rules.contains_key("repo__write"));
    }

    #[test]
    fn unsupported_toolset_and_undeclared_mcp_are_rejected() {
        assert!(tools(&json!({"tools": [{"type": "unknown"}]})).is_err());
        assert!(
            tools(&json!({"tools": [{"type": "mcp_toolset", "mcpServerName": "missing"}]}))
                .is_err()
        );
        assert!(tools(&json!({"mcpServers": [{"name": "a__b"}]})).is_err());
        assert!(permissions(&json!({
            "tools": [{"type": "agent_toolset",
                       "configs": [{"name": "read", "permissionPolicy": {"type": "maybe"}}]}]
        }))
        .is_err());
    }

    #[test]
    fn empty_skills_select_nothing() {
        assert!(matches!(skill_filter(&json!({})), SkillFilter::None));
        match skill_filter(&json!({"skills": [{"name": "review"}]})) {
            SkillFilter::Whitelist(names) => assert_eq!(names, vec!["review".to_string()]),
            other => panic!("expected a whitelist, got {other:?}"),
        }
    }
}
