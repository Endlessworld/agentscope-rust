//! Subagent declarations, factories, and background task repository, mirroring
//! `io.agentscope.harness.agent.subagent`.
use std::collections::BTreeMap;
use std::sync::Arc;

use agentscope_core::agent::{CallableAgent, RuntimeContext};
use serde::{Deserialize, Serialize};

use self::protocol::RemoteStreamDetail;

pub mod entry;
pub mod loader;
pub mod manager;
pub mod materialize;
pub mod protocol;
pub mod task;

pub use entry::{SubagentEntry, SubagentFactory};
pub use materialize::{
    allowlisted_inherited_toolkit, build_subagent_sys_prompt, declared_generate_options,
    declared_skill_filter, derive_child_session_id, resolve_declared_model, SubagentBlueprint,
    SUBAGENT_CONTEXT_SECTION,
};
pub use loader::{
    load_from_directory, load_from_file, load_from_filesystem, parse, SUBAGENTS_DIR, SUBAGENT_GLOB,
};
pub use manager::DefaultAgentManager;
pub use task::{
    BackgroundTask, TaskCompletionCallback, TaskDelivery, TaskRecord, TaskRepository,
    TaskRunSpec, TaskStatus, WorkspaceTaskRepository, SUPPRESS_COMPLETION_CALLBACK,
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

/// Policy for resolving a remote subagent's tool-confirmation (HITL) requests when the
/// parent cannot forward the decision to an interactive human within this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RemoteAskPolicy {
    /// Auto-deny every pending remote confirmation (safe default).
    #[default]
    Deny,
    /// Forward the confirmation request upstream instead of auto-denying.
    Propagate,
}

fn default_true() -> bool {
    true
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
    /// Whether the parent's DENY permission rules propagate to this subagent.
    #[serde(default = "default_true")]
    pub inherit_parent_permissions: bool,
    /// Tri-state override for orphaned tool-call recovery in local subagents;
    /// `None` inherits the parent setting.
    #[serde(default)]
    pub enable_pending_tool_recovery: Option<bool>,
    /// Base URL of a remote task server (agent-protocol transport).
    #[serde(default)]
    pub url: Option<String>,
    /// Optional HTTP headers for the remote task server.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Whether remote subagent events stream back into the parent's event stream.
    /// `None` defaults to `true`; see [`SubagentDeclaration::is_remote_streaming`].
    #[serde(default)]
    pub remote_streaming: Option<bool>,
    /// How much of the remote event stream to request. `None` defaults to
    /// [`RemoteStreamDetail::Full`]; see [`SubagentDeclaration::remote_stream_detail_or_default`].
    #[serde(default)]
    pub remote_stream_detail: Option<RemoteStreamDetail>,
    /// Policy for resolving remote HITL confirmations.
    #[serde(default)]
    pub remote_ask_policy: RemoteAskPolicy,
    /// Static attributes sent as `context.attributes` on every remote submission.
    #[serde(default)]
    pub remote_context_attributes: Option<BTreeMap<String, serde_json::Value>>,

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
            inherit_parent_permissions: true,
            enable_pending_tool_recovery: None,
            url: None,
            headers: BTreeMap::new(),
            remote_streaming: None,
            remote_stream_detail: None,
            remote_ask_policy: RemoteAskPolicy::default(),
            remote_context_attributes: None,
            workspace_mode: SubagentWorkspaceMode::Isolated,
            workspace_path: None,
            model: None,
            // Matches the Java builder default (`steps = 10`) for programmatic construction.
            steps: Some(10),
            temperature: None,
            top_p: None,
            variant: None,
            expose_to_user: None,
            tools: Vec::new(),
            skills: Vec::new(),
        }
    }

    /// Whether this declaration carries a definition workspace path.
    pub fn has_definition_workspace(&self) -> bool {
        self.workspace_path.is_some()
    }

    /// Whether remote events should stream back into the parent event stream (default `true`).
    pub fn is_remote_streaming(&self) -> bool {
        self.remote_streaming.unwrap_or(true)
    }

    /// Requested remote stream detail, defaulting to [`RemoteStreamDetail::Full`].
    pub fn remote_stream_detail_or_default(&self) -> RemoteStreamDetail {
        self.remote_stream_detail.unwrap_or(RemoteStreamDetail::Full)
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

/// A caller-supplied subagent factory (`Builder::subagent_factory`), mirroring Java
/// `HarnessAgentBuilderSupport.SubagentFactoryEntry`.
pub struct CustomSubagentFactory {
    pub name: String,
    pub description: String,
    pub factory: Arc<dyn Fn(String) -> Arc<dyn CallableAgent> + Send + Sync>,
}

impl CustomSubagentFactory {
    /// Description shown to the orchestrator, falling back to the name when unset.
    pub fn display_description(&self) -> &str {
        if self.description.trim().is_empty() {
            &self.name
        } else {
            &self.description
        }
    }
}

impl SubagentFactory for CustomSubagentFactory {
    fn create(&self, _parent_rc: &RuntimeContext) -> Arc<dyn CallableAgent> {
        (self.factory)(self.name.clone())
    }
}
