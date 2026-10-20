//! Path constants for the workspace directory structure, mirroring
//! `WorkspaceConstants`.

/// Default workspace root directory.
pub const DEFAULT_WORKSPACE_ROOT: &str = ".agentscope/workspace";

pub const AGENTS_MD: &str = "AGENTS.md";
pub const MEMORY_MD: &str = "MEMORY.md";
pub const TOOLS_JSON: &str = "tools.json";

pub const MEMORY_DIR: &str = "memory";
pub const SKILLS_DIR: &str = "skills";
pub const KNOWLEDGE_DIR: &str = "knowledge";
pub const KNOWLEDGE_MD: &str = "KNOWLEDGE.md";
pub const RULES_DIR: &str = "rules";

pub const AGENTS_DIR: &str = "agents";
pub const SESSIONS_DIR: &str = "sessions";
pub const TASKS_DIR: &str = "tasks";

/// Workspace-relative marker for the curator state sidecar.
pub const CURATOR_STATE_PATH: &str = "skills/.curator_state.json";
/// Workspace-relative directory for curator dry-run reports.
pub const CURATOR_REPORTS_DIR: &str = "skills/.curator_reports";

/// Aggregates the workspace path constants.
pub struct WorkspaceConstants;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_stable() {
        assert_eq!(DEFAULT_WORKSPACE_ROOT, ".agentscope/workspace");
        assert_eq!(AGENTS_MD, "AGENTS.md");
    }
}
