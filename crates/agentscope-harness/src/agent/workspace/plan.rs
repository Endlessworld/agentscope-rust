//! Coordinates plan mode for a single agent, mirroring `PlanModeManager`.
//!
//! State (whether plan mode is active and which markdown file holds the current
//! plan) lives in `PlanModeContextState` inside the agent's `AgentState`, so it
//! is persisted with the session and survives restarts / cross-node hand-offs —
//! this is the mechanism that makes dynamic (runtime) plan-mode switching work
//! in a distributed setting.
//!
//! The plan markdown file is written exclusively through the configured
//! `Filesystem`, never directly, so it lands on whatever backend (local, sandbox,
//! remote) the agent's filesystem is configured with. The logical path stays
//! `<planDir>/PLAN.md` for every isolation scope.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::state::AgentState;

use crate::filesystem::Filesystem;

/// Default workspace-relative directory for plan files.
pub const DEFAULT_PLAN_DIR: &str = "plans";

/// Persisted plan-mode coordinator bound to a `Filesystem`.
pub struct PlanModeManager {
    filesystem: Arc<dyn Filesystem>,
    plan_dir: String,
}

impl PlanModeManager {
    pub fn new(filesystem: Arc<dyn Filesystem>, plan_dir: Option<&str>) -> Self {
        let mut dir = plan_dir.map(str::trim).unwrap_or(DEFAULT_PLAN_DIR);
        if dir.is_empty() {
            dir = DEFAULT_PLAN_DIR;
        }
        // Normalise: strip leading/trailing slashes.
        while dir.starts_with('/') {
            dir = &dir[1..];
        }
        while dir.ends_with('/') {
            dir = &dir[..dir.len() - 1];
        }
        let plan_dir = if dir.is_empty() { DEFAULT_PLAN_DIR.to_string() } else { dir.to_string() };
        Self { filesystem, plan_dir }
    }

    pub fn plan_dir(&self) -> &str {
        &self.plan_dir
    }

    pub fn is_plan_active(&self, state: &AgentState) -> bool {
        state.plan_mode_context.plan_active
    }

    /// Enters plan mode. Idempotent. Ensures a plan file path is recorded
    /// (default if none set). Returns the workspace-relative plan file path.
    pub fn enter(&self, state: &mut AgentState) -> String {
        state.plan_mode_context.set_plan_active(true);
        let file = state.plan_mode_context.current_plan_file().map(str::to_string);
        if file.as_deref().map(str::trim).unwrap_or("").is_empty() {
            let default = self.default_plan_file();
            state.plan_mode_context.current_plan_file = Some(default.clone());
            return default;
        }
        file.unwrap()
    }

    /// Exits plan mode (back to BUILD). Idempotent. Keeps `currentPlanFile`.
    pub fn exit(&self, state: &mut AgentState) {
        state.plan_mode_context.set_plan_active(false);
    }

    /// Workspace-relative path of the plan file currently associated with this agent.
    pub fn plan_file_path(&self, state: &AgentState) -> String {
        if let Some(current) = state.plan_mode_context.current_plan_file.as_deref() {
            if !current.trim().is_empty() {
                return current.to_string();
            }
        }
        self.default_plan_file()
    }

    /// Writes (creates or overwrites) the plan markdown file through the
    /// workspace filesystem and records its path in state.
    pub async fn write_plan(
        &self,
        rc: &RuntimeContext,
        state: &mut AgentState,
        content: &str,
    ) -> Result<String> {
        let path = self.plan_file_path(state);
        self.filesystem.write(rc, &path, content).await?;
        state.plan_mode_context.current_plan_file = Some(path.clone());
        Ok(path)
    }

    fn default_plan_file(&self) -> String {
        format!("{}/PLAN.md", self.plan_dir)
    }

    /// Writes `content` to an explicit workspace-relative plan path through the
    /// configured filesystem (the caller owns state bookkeeping). Used by the
    /// `plan_write` tool so it need not hold the shared state lock across the await.
    pub async fn write_plan_file(
        &self,
        rc: &RuntimeContext,
        path: &str,
        content: &str,
    ) -> Result<()> {
        self.filesystem.write(rc, path, content).await
    }
}
