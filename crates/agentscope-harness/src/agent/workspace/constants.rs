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

/// Per-agent session store filename under `agents/<agentId>/sessions/`.
pub const SESSIONS_STORE: &str = "sessions.json";

/// JSONL session context file extension (LLM-facing, may be compacted).
pub const SESSION_CONTEXT_EXT: &str = ".jsonl";

/// JSONL session log file extension (full history, append-only, never compacted).
pub const SESSION_LOG_EXT: &str = ".log.jsonl";

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
        assert_eq!(SESSION_CONTEXT_EXT, ".jsonl");
        assert_eq!(SESSION_LOG_EXT, ".log.jsonl");
    }
}
