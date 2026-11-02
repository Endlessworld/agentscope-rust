//! Workspace-level tool governance, mirroring `io.agentscope.harness.agent.tools`.
//!
//! `workspace/tools.json` has two jobs (Java `ToolsConfig`):
//!
//! * `allow` / `deny` narrow the *product catalog* (filesystem, shell, web, memory, …).
//!   Harness platform tools (subagents, teams, tasks, plan, skills admin) always survive
//!   `allow`, because an `allow` list describes a product toolset and must not disable the
//!   coordination runtime; an explicit `deny` still removes them.
//! * `mcpServers` declares external MCP servers, registered through a [`Registrar`]
//!   (`McpServerRegistrar` in production).
//!
//! [`Registrar`]: super::mcp::Registrar
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use agentscope_core::error::{Error, Result};
use agentscope_core::tool::Toolkit;
use serde::{Deserialize, Serialize};

use super::mcp_client_manager::{McpServerConfig, McpTransport};
use super::workspace::constants::TOOLS_JSON;
use super::workspace::WorkspaceManager;

/// Harness platform / orchestration tool names that [`ToolFilter`] must not strip through
/// an `allow` list. Mirrors Java `HarnessPlatformTools.NAMES`.
pub struct HarnessPlatformTools;

impl HarnessPlatformTools {
    /// Names registered by the harness agent (middleware / builder paths) that are not
    /// part of the product "agent toolset" catalog.
    pub const NAMES: &'static [&'static str] = &[
        // AgentTeams (unified + per-action aliases models often invent)
        "team",
        "listTasks",
        "listClaimableTasks",
        "createTask",
        "assignTask",
        "claimTask",
        "unclaimTask",
        "completeTask",
        "failTask",
        "sendMessage",
        "broadcastMessage",
        "listMessages",
        "listMembers",
        "spawnMember",
        "shutdownMember",
        "submitPlan",
        "approvePlan",
        "rejectPlan",
        "completeTeam",
        // Subagents (session-mode aliases included)
        "agent_spawn",
        "agent_send",
        "agent_list",
        "agent_generate",
        "sessions_spawn",
        "sessions_send",
        "sessions_list",
        "sessions_history",
        "sessions_pending_completions",
        // Background task lifecycle
        "task_output",
        "task_cancel",
        "task_list",
        "wait_async_results",
        // Plan mode
        "plan_enter",
        "plan_write",
        "plan_exit",
        // Skills self-learning
        "skill_manage",
        "propose_skill",
    ];

    /// `true` when `tool_name` is a harness platform tool.
    pub fn is_platform_tool(tool_name: &str) -> bool {
        Self::NAMES.contains(&tool_name)
    }
}

/// One `mcpServers.<name>` entry of `tools.json`, mirroring Java
/// `tools/McpServerConfig`.
///
/// Only the fields the Rust side can honour are modelled; unknown keys are ignored exactly
/// like Java (`@JsonIgnoreProperties(ignoreUnknown = true)`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpServerEntry {
    /// Transport discriminator: `stdio`, `sse` or `http`.
    pub transport: Option<String>,
    /// stdio: executable command.
    pub command: Option<String>,
    /// stdio: command arguments (folded into the command line).
    pub args: Vec<String>,
    /// stdio: environment variables. Not supported by the harness stdio transport yet.
    pub env: BTreeMap<String, String>,
    /// sse / http: server URL.
    pub url: Option<String>,
    /// sse / http: headers attached to every request.
    pub headers: BTreeMap<String, String>,
    /// Optional tool allowlist for this server (not applied yet; see the tracker).
    pub enable_tools: Vec<String>,
    /// Connection-level request-metadata propagation switch.
    pub propagate_meta: Option<bool>,
    /// Register bridged tools as `<server>__<tool>`. Absent keeps the bare remote name.
    pub prefix_tool_names: Option<bool>,
}

impl McpServerEntry {
    /// Converts this entry into the manager's [`McpServerConfig`].
    ///
    /// A field the Rust transport cannot honour is an explicit error rather than a silent
    /// drop: a server that "connects" without its `env` would run with the wrong
    /// credentials, and that is exactly the kind of failure a workspace file should surface
    /// at build time.
    pub fn to_server_config(&self, name: &str) -> Result<McpServerConfig> {
        let transport = match &self.transport {
            Some(raw) => Some(McpTransport::parse(raw).ok_or_else(|| {
                Error::Other(format!(
                    "mcpServers.{name}.transport '{raw}' is not one of stdio|sse|http"
                ))
            })?),
            None => None,
        };
        if !self.env.is_empty() {
            return Err(Error::Other(format!(
                "mcpServers.{name}.env is not supported by the harness stdio transport"
            )));
        }
        if !self.enable_tools.is_empty() {
            tracing::warn!(
                "mcpServers.{name}.enableTools is not applied yet; all advertised tools are registered"
            );
        }

        let command = match (&self.command, self.args.is_empty()) {
            (Some(command), true) => Some(command.clone()),
            (Some(command), false) => Some(format!("{} {}", command, self.args.join(" "))),
            (None, _) => None,
        };

        let mut config = McpServerConfig::new(name);
        config.command = command;
        config.url = self.url.clone();
        config.headers = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        config.propagate_meta = self.propagate_meta.unwrap_or(true);
        config.transport = transport;
        config.prefix_tool_names = self.prefix_tool_names.unwrap_or(false);
        Ok(config)
    }
}

/// Workspace-level tool configuration loaded from `workspace/tools.json`.
///
/// Java's `ToolsConfig` also carries `strictAllow` / `defaultToolsEnabled`, but those are
/// programmatic-only there (no `@JsonProperty`), so the JSON surface is exactly `allow`,
/// `deny` and `mcpServers`. They are therefore not modelled here: the Rust side has no
/// equivalent programmatic caller yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolsConfig {
    /// When non-empty, catalogued tools not listed here are hidden (platform tools exempt).
    pub allow: Vec<String>,
    /// Tools whose name appears here are removed regardless of `allow`.
    pub deny: Vec<String>,
    /// Declarative MCP servers, keyed by server name.
    pub mcp_servers: BTreeMap<String, McpServerEntry>,
}

impl ToolsConfig {
    /// A config that filters nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// `true` when neither `allow` nor `deny` is set (a no-op filter).
    pub fn has_no_filter(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
}

/// Applies `allow` / `deny` to a toolkit. Mirrors Java `ToolFilter`.
pub struct ToolFilter;

impl ToolFilter {
    /// Whether a tool named `name` survives `config`.
    ///
    /// `deny` always wins; platform tools survive `allow`; an empty `allow` list means "no
    /// `allow` narrowing". A blank name is never allowed.
    pub fn is_allowed(name: &str, config: Option<&ToolsConfig>) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        let Some(config) = config else {
            return true;
        };
        if config.deny.iter().any(|denied| denied == name) {
            return false;
        }
        if HarnessPlatformTools::is_platform_tool(name) {
            return true;
        }
        if config.allow.is_empty() {
            return true;
        }
        config.allow.iter().any(|allowed| allowed == name)
    }

    /// Removes every tool `config` excludes. A missing config or one without `allow`/`deny`
    /// is a no-op.
    ///
    /// Names that match no registered tool are logged at WARN and otherwise ignored: a typo
    /// in the workspace file must not abort the agent (Java does the same).
    pub fn apply(toolkit: &Toolkit, config: &ToolsConfig) {
        if config.has_no_filter() {
            return;
        }
        let registered: Vec<String> = toolkit.tool_names();
        let mut removed: HashSet<String> = HashSet::new();
        let mut kept_platform: Vec<String> = Vec::new();
        for name in &registered {
            if !Self::is_allowed(name, Some(config)) {
                removed.insert(name.clone());
            } else if !config.allow.is_empty()
                && !config.allow.iter().any(|allowed| allowed == name)
                && HarnessPlatformTools::is_platform_tool(name)
            {
                kept_platform.push(name.clone());
            }
        }
        if !removed.is_empty() {
            toolkit.remove_tools(&removed);
        }

        let mut removed: Vec<String> = removed.into_iter().collect();
        removed.sort_unstable();
        let mut kept_platform = kept_platform;
        kept_platform.sort_unstable();
        tracing::info!(
            "tools.json filter applied: removed {:?}; kept {} platform tool(s) outside allow {:?}",
            removed,
            kept_platform.len(),
            kept_platform
        );
        Self::warn_unknown(&config.allow, &registered, "allow");
        Self::warn_unknown(&config.deny, &registered, "deny");
    }

    fn warn_unknown(declared: &[String], registered: &[String], which: &str) {
        for name in declared {
            if !registered.iter().any(|r| r == name) {
                tracing::warn!(
                    "tools.json '{}' references unknown tool '{}' (not currently registered)",
                    which,
                    name
                );
            }
        }
    }
}

/// Loads `workspace/tools.json` into a [`ToolsConfig`]. Mirrors Java `ToolsConfigLoader`.
pub struct ToolsConfigLoader;

impl ToolsConfigLoader {
    /// Reads `tools.json` from the workspace root.
    ///
    /// A missing, blank or unparseable file returns `None` so the agent still builds with
    /// its default toolkit (Java logs and continues).
    ///
    /// The read is synchronous because `HarnessAgentBuilder::build()` is synchronous: an
    /// overlay/remote filesystem must therefore pass an explicit `tools_config(...)`
    /// instead of relying on this path.
    pub fn load(workspace: &WorkspaceManager) -> Option<ToolsConfig> {
        Self::load_from_root(workspace.workspace())
    }

    /// Reads `<root>/tools.json`.
    pub fn load_from_root(root: &Path) -> Option<ToolsConfig> {
        let path = root.join(TOOLS_JSON);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("could not read {}: {e}", path.display());
                return None;
            }
        };
        Self::parse(&raw)
    }

    /// Parses raw JSON, applying `${VAR}` environment substitution first.
    pub fn parse(raw: &str) -> Option<ToolsConfig> {
        if raw.trim().is_empty() {
            return None;
        }
        let substituted = Self::substitute_env(raw);
        match serde_json::from_str::<ToolsConfig>(&substituted) {
            Ok(config) => Some(config),
            Err(e) => {
                tracing::warn!("failed to parse {TOOLS_JSON}: {e}; using the default toolkit");
                None
            }
        }
    }

    /// Replaces `${VAR}` occurrences with the environment value.
    ///
    /// Unset variables resolve to an empty string so a secret can stay out of the workspace
    /// file (`"Authorization": "Bearer ${GITHUB_TOKEN}"`). Mirrors Java
    /// `ToolsConfigLoader.substituteEnv`.
    pub fn substitute_env(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        let mut warned: HashSet<String> = HashSet::new();
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let name = &after[..end];
            let valid = !name.is_empty()
                && name.chars().enumerate().all(|(i, c)| {
                    c == '_' || c.is_ascii_alphanumeric() && (i > 0 || !c.is_ascii_digit())
                });
            if !valid {
                out.push_str(&rest[start..start + 2 + end + 1]);
            } else {
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => {
                        if warned.insert(name.to_string()) {
                            tracing::warn!(
                                "tools.json references env var ${{{name}}} which is not set; substituting an empty string"
                            );
                        }
                    }
                }
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn toolkit_with(names: &[&str]) -> Toolkit {
        let toolkit = Toolkit::default();
        for name in names {
            toolkit.register_tool(Arc::new(NamedTool((*name).to_string())));
        }
        toolkit
    }

    /// Minimal tool whose only job is to exist under a given name.
    #[derive(Debug)]
    struct NamedTool(String);

    #[async_trait::async_trait]
    impl agentscope_core::tool::Tool for NamedTool {
        fn name(&self) -> &str {
            &self.0
        }
        fn description(&self) -> &str {
            "named tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn call(
            &self,
            _args: agentscope_core::message::JsonMap,
            _ctx: agentscope_core::tool::ToolExecutionContext,
        ) -> Result<agentscope_core::message::ToolResultBlock> {
            Ok(agentscope_core::message::ToolResultBlock::builder()
                .id("call")
                .name(self.0.clone())
                .output(agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("ok"),
                ))
                .build())
        }
    }

    #[test]
    fn deny_wins_over_allow_and_platform_tools_survive_allow() {
        let config = ToolsConfig {
            allow: vec!["read_file".into(), "agent_spawn".into()],
            deny: vec!["agent_spawn".into()],
            mcp_servers: Default::default(),
        };
        assert!(ToolFilter::is_allowed("read_file", Some(&config)));
        assert!(
            !ToolFilter::is_allowed("agent_spawn", Some(&config)),
            "an explicit deny must remove even a platform tool"
        );
        assert!(
            ToolFilter::is_allowed("task_list", Some(&config)),
            "platform tools survive an allow list"
        );
        assert!(!ToolFilter::is_allowed("write_file", Some(&config)));
        assert!(!ToolFilter::is_allowed("  ", Some(&config)));
        assert!(ToolFilter::is_allowed("anything", None));
    }

    #[test]
    fn apply_removes_only_excluded_tools() {
        let toolkit = toolkit_with(&["read_file", "write_file", "task_list"]);
        let config = ToolsConfig {
            allow: vec!["read_file".into()],
            deny: vec![],
            mcp_servers: Default::default(),
        };
        ToolFilter::apply(&toolkit, &config);
        let mut names = toolkit.tool_names();
        names.sort();
        assert_eq!(
            names,
            vec!["read_file".to_string(), "task_list".to_string()]
        );
    }

    #[test]
    fn loader_reads_the_workspace_file_and_substitutes_env() {
        let root =
            std::env::temp_dir().join(format!("agentscope_tools_json_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(TOOLS_JSON),
            r#"{
                "deny": ["shell"],
                "mcpServers": {
                    "fs": { "command": "npx", "args": ["-y", "server-filesystem"] },
                    "remote": { "transport": "http", "url": "https://example.test/mcp" }
                }
            }"#,
        )
        .unwrap();

        let config = ToolsConfigLoader::load_from_root(&root).expect("tools.json must parse");
        assert_eq!(config.deny, vec!["shell".to_string()]);
        let fs = config.mcp_servers.get("fs").unwrap();
        let server = fs.to_server_config("fs").unwrap();
        assert_eq!(server.command.as_deref(), Some("npx -y server-filesystem"));
        assert_eq!(server.transport, None, "no transport key keeps sniffing");
        let remote = config.mcp_servers.get("remote").unwrap();
        assert_eq!(
            remote.to_server_config("remote").unwrap().transport,
            Some(McpTransport::StreamableHttp)
        );

        // A missing file is not an error.
        assert!(ToolsConfigLoader::load_from_root(&root.join("nope")).is_none());
        // Blank / invalid documents degrade to "no config".
        assert!(ToolsConfigLoader::parse("   ").is_none());
        assert!(ToolsConfigLoader::parse("{ not json").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn env_references_are_substituted_and_unknown_shapes_left_alone() {
        std::env::set_var("AGENTSCOPE_TOOLS_TEST_TOKEN", "secret");
        let rendered = ToolsConfigLoader::substitute_env(
            r#"{"a": "${AGENTSCOPE_TOOLS_TEST_TOKEN}", "b": "${}"}"#,
        );
        assert!(rendered.contains("\"secret\""), "{rendered}");
        assert!(
            rendered.contains("${}"),
            "invalid references stay verbatim: {rendered}"
        );
        std::env::remove_var("AGENTSCOPE_TOOLS_TEST_TOKEN");
    }
}
