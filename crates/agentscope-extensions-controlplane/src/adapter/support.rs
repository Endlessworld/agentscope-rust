//! Optional adapter extension points, mirroring the sibling SPIs of
//! `AgentScopeAdapter` (`SessionCompactor`, `SessionHistorySource`, `AgentRuntimeSource`,
//! `AgentTaskStarter`).
//!
//! All are optional: when unset the corresponding capability is either not declared in
//! [`AgentScopeAdapter::capabilities`](super::AgentScopeAdapter::capabilities) or the
//! operation reports `Unsupported`, so the control plane never shows a button that fails.

use agentscope_core::state::AgentState;

use crate::error::ControlPlaneResult;
use crate::model::{AgentTaskAssignment, MessageItem, SubagentInfo, ToolInfo};

/// Handles the `compress` session command, mirroring `SessionCompactor`.
///
/// Implementations compact `state.context` in place and return the summary text that is
/// published as a `compaction` event.
#[async_trait::async_trait]
pub trait SessionCompactor: Send + Sync {
    /// Compacts `state` for `session_id`, returning the summary that was injected.
    async fn compact(&self, session_id: &str, state: &mut AgentState)
        -> ControlPlaneResult<String>;
}

/// Reads full session history including messages already compacted away, mirroring
/// `SessionHistorySource` (e.g. the harness `.log.jsonl` transcript).
#[async_trait::async_trait]
pub trait SessionHistorySource: Send + Sync {
    /// Loads the full history for `session_id`; `None` falls back to the live state buffer.
    async fn load_messages(
        &self,
        session_id: &str,
        user_id: Option<&str>,
    ) -> Option<Vec<MessageItem>>;
}

/// Harness-/app hooks for inventory, plan files and subagent tasks, mirroring
/// `AgentRuntimeSource`.
#[async_trait::async_trait]
pub trait AgentRuntimeSource: Send + Sync {
    /// Reachable subagents (drives the `subagent-inventory` capability).
    async fn list_subagents(&self) -> ControlPlaneResult<Vec<SubagentInfo>> {
        Err(crate::error::ControlPlaneError::Unsupported(
            crate::adapter::CAP_SUBAGENT_INVENTORY.to_string(),
        ))
    }

    /// Known workspaces (drives the `workspace-inventory` capability).
    async fn list_workspaces(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
        Err(crate::error::ControlPlaneError::Unsupported(
            crate::adapter::CAP_WORKSPACE_INVENTORY.to_string(),
        ))
    }

    /// Subagent tasks under `session_id`.
    async fn list_subagent_tasks(
        &self,
        _session_id: &str,
    ) -> ControlPlaneResult<Vec<serde_json::Value>> {
        Err(crate::error::ControlPlaneError::Unsupported(
            crate::adapter::CAP_SUBAGENT_TASK_QUERY.to_string(),
        ))
    }

    /// Cancels a subagent task; returns whether it was found and cancelled.
    async fn cancel_subagent_task(
        &self,
        _session_id: &str,
        _task_id: &str,
    ) -> ControlPlaneResult<bool> {
        Err(crate::error::ControlPlaneError::Unsupported(
            crate::adapter::CAP_SUBAGENT_TASK_COMMAND.to_string(),
        ))
    }

    /// Toggles plan mode for a session; returns whether it handled the switch.
    async fn set_plan_mode(
        &self,
        _session_id: &str,
        _user_id: Option<&str>,
        _active: bool,
    ) -> ControlPlaneResult<bool> {
        Ok(false)
    }

    /// Reads a plan-file excerpt for the context snapshot.
    async fn read_plan_excerpt(
        &self,
        _session_id: &str,
        _user_id: Option<&str>,
        _plan_file: &str,
    ) -> Option<String> {
        None
    }

    /// Enriches the agent-config JSON served by `GET /agentscope/info`.
    fn enrich_agent_config(&self, _cfg: &mut serde_json::Map<String, serde_json::Value>) {}

    /// Tools exposed for the given activated groups, when the source overrides them.
    fn tools_for_groups(&self, _activated_groups: &[String]) -> Option<Vec<ToolInfo>> {
        None
    }
}

/// Installs the host hook that materializes ASDP AgentTask deliveries, mirroring
/// `AgentTaskStarter`.
#[async_trait::async_trait]
pub trait AgentTaskStarter: Send + Sync {
    /// Starts an agent-task assignment (drives the `agent-task` capability).
    async fn start(&self, assignment: AgentTaskAssignment) -> ControlPlaneResult<()>;

    /// Whether this starter consumes workspace-definition deliveries.
    fn consumes_workspace_definition(&self) -> bool {
        false
    }
}
