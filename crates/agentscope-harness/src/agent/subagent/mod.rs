//! Subagent declarations, factories, and background task repository, mirroring
//! `io.agentscope.harness.agent.subagent`.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod entry;
pub mod loader;
pub mod manager;
pub mod task;

pub use entry::{SubagentEntry, SubagentFactory};
pub use loader::{
    load_from_directory, load_from_filesystem, parse, SUBAGENTS_DIR, SUBAGENT_GLOB,
};
pub use manager::DefaultAgentManager;
pub use task::{
    BackgroundTask, TaskCompletionCallback, TaskDelivery, TaskRecord, TaskRepository,
    TaskRunSpec, TaskStatus, WorkspaceTaskRepository,
};

/// How a declared subagent may be used: as a top-level primary agent, only as a
/// delegated subagent, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubagentMode {
    Primary,
    Subagent,
    #[default]
    All,
}

/// Workspace isolation strategy for a declared subagent, parsed from the
/// `workspace.mode` front-matter key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentWorkspaceMode {
    /// Each subagent gets its own isolated workspace slice.
    #[default]
    Isolated,
    /// The subagent shares the parent workspace.
    Shared,
}

/// A subagent declaration that can be materialized into a child agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentDeclaration {
    pub name: String,
    pub description: String,
    /// System-prompt body used when no remote URL is set.
    pub prompt: String,
    #[serde(default)]
    pub mode: SubagentMode,
    /// Hide this declaration from the LLM-facing list of available subagents.
    #[serde(default)]
    pub hidden: bool,
    /// Derive the spawn key deterministically from (parentSessionId, agentId, label).
    #[serde(default)]
    pub persist_session: bool,
    /// Base URL of a remote task server (agent-protocol transport).
    #[serde(default)]
    pub url: Option<String>,
    /// Optional HTTP headers for the remote task server.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    // ---- front-matter fields parsed by `AgentSpecLoader` (all optional) ----
    #[serde(default)]
    pub workspace_mode: SubagentWorkspaceMode,
    /// Resolved absolute (or workspace-relative) path; when set the system prompt
    /// is read from `<workspace_path>/AGENTS.md` at runtime instead of `prompt`.
    #[serde(default)]
    pub workspace_path: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub expose_to_user: Option<bool>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
}

impl SubagentDeclaration {
    /// Creates a declaration from the three core fields, defaults for the rest.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            prompt: prompt.into(),
            mode: SubagentMode::All,
            hidden: false,
            persist_session: false,
            url: None,
            headers: BTreeMap::new(),
            workspace_mode: SubagentWorkspaceMode::Isolated,
            workspace_path: None,
            model: None,
            steps: None,
            temperature: None,
            top_p: None,
            variant: None,
            expose_to_user: None,
            tools: Vec::new(),
            skills: Vec::new(),
        }
    }

    /// Whether this declaration targets a remote task HTTP server.
    pub fn is_remote(&self) -> bool {
        self.url.as_deref().map(|u| !u.trim().is_empty()).unwrap_or(false)
    }

    /// Whether this entry should appear in the LLM-facing available-subagent list.
    pub fn is_visible_to_llm(&self) -> bool {
        !self.hidden && self.mode != SubagentMode::Primary
    }
}
