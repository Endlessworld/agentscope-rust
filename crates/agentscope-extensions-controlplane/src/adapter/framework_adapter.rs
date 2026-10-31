//! Framework adapter SPI, mirroring `io.agentscope.extensions.controlplane.FrameworkAdapter`.
//!
//! **Bypass principle:** interception copies data, it never replaces or blocks the
//! framework's own path, and a failed report is swallowed rather than propagated. An
//! instrumented agent must behave exactly as an uninstrumented one.
//!
//! Optional operations default to failing with [`ControlPlaneError::Unsupported`]. Declare only
//! what you actually implement in [`FrameworkAdapter::capabilities`] — the control plane
//! gates its UI on that list, so over-declaring produces buttons that fail.

use async_trait::async_trait;

use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::model::{ContextSnapshot, Inventory, MessagePage, SessionEvent};

/// `context-query` capability.
pub const CAP_CONTEXT_QUERY: &str = "context-query";
/// `message-query` capability.
pub const CAP_MESSAGE_QUERY: &str = "message-query";
/// `subagent-inventory` capability.
pub const CAP_SUBAGENT_INVENTORY: &str = "subagent-inventory";
/// `workspace-inventory` capability.
pub const CAP_WORKSPACE_INVENTORY: &str = "workspace-inventory";
/// `session-command` capability.
pub const CAP_SESSION_COMMAND: &str = "session-command";
/// `session-abort` capability.
pub const CAP_SESSION_ABORT: &str = "session-abort";
/// `task-query` capability.
pub const CAP_TASK_QUERY: &str = "task-query";
/// `subagent-task-query` capability.
pub const CAP_SUBAGENT_TASK_QUERY: &str = "subagent-task-query";
/// `subagent-task-command` capability.
pub const CAP_SUBAGENT_TASK_COMMAND: &str = "subagent-task-command";
/// `plan-mode` capability.
pub const CAP_PLAN_MODE: &str = "plan-mode";
/// `agent-task` capability.
pub const CAP_AGENT_TASK: &str = "agent-task";
/// `export-transcript` capability.
pub const CAP_EXPORT_TRANSCRIPT: &str = "export-transcript";
/// `conversation-inbound` capability.
pub const CAP_CONVERSATION_INBOUND: &str = "conversation-inbound";

/// `compress` command.
pub const COMMAND_COMPRESS: &str = "compress";
/// `terminate` command.
pub const COMMAND_TERMINATE: &str = "terminate";
/// `abort` command.
pub const COMMAND_ABORT: &str = "abort";

/// The outcome of a session command.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommandOutcome {
    /// Whether the command succeeded.
    pub ok: bool,
    /// Human-readable detail.
    pub message: String,
}

/// One adapter per agent framework, presenting a uniform view of that framework's
/// sessions to [`ControlPlaneBridge`](crate::bridge::ControlPlaneBridge).
#[async_trait]
pub trait FrameworkAdapter: Send + Sync {
    /// Framework identifier reported on every snapshot, e.g. `agentscope-rust`.
    fn framework_name(&self) -> &str;

    /// Framework version, or an empty string when it cannot be determined.
    fn framework_version(&self) -> &str {
        ""
    }

    /// Capability identifiers this adapter implements.
    fn capabilities(&self) -> Vec<String>;

    /// Whether this adapter can instrument `target`. Must be side-effect free.
    fn can_handle(&self, target: &(dyn std::any::Any + Send + Sync)) -> bool;

    /// Attaches to the framework object. Session activity is published through `emit`,
    /// which must never be allowed to break the framework's main path.
    fn attach(
        &mut self,
        target: std::sync::Arc<dyn std::any::Any + Send + Sync>,
        emit: std::sync::Arc<dyn Fn(SessionEvent) + Send + Sync>,
    ) -> ControlPlaneResult<()>;

    /// Removes the interception and restores the framework object.
    ///
    /// Takes `&self` rather than `&mut self` so the contract HTTP server's provider handle can
    /// share the adapter with the bridge (mirrors Java `FrameworkAdapter.detach()`).
    fn detach(&self) -> ControlPlaneResult<()>;

    /// Called when the owning bridge is attached, so the adapter can obtain a reporter
    /// handle for publishing events out-of-band. Default: no-op.
    fn on_bridge_attached(&mut self, _reporter: crate::bridge::BridgeReporter) {}

    /// Level 4: the context that the session's next model call will actually see.
    async fn extract_context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot>;

    /// Level 3: full history including messages already compacted away.
    async fn list_messages(
        &self,
        session_id: &str,
        offset: i64,
        limit: i64,
    ) -> ControlPlaneResult<MessagePage> {
        let _ = (session_id, offset, limit);
        Err(ControlPlaneError::Unsupported(
            CAP_MESSAGE_QUERY.to_string(),
        ))
    }

    /// Instance inventory: subagents, workspaces, health.
    async fn list_subagents(&self) -> ControlPlaneResult<Inventory> {
        Err(ControlPlaneError::Unsupported(
            CAP_SUBAGENT_INVENTORY.to_string(),
        ))
    }

    /// All session ids this adapter can report on.
    async fn session_ids(&self) -> ControlPlaneResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// Executes a session command (`compress` / `terminate` / `abort`).
    async fn execute_command(
        &self,
        session_id: &str,
        command: &str,
        payload: Option<serde_json::Value>,
    ) -> ControlPlaneResult<CommandOutcome> {
        let _ = (session_id, payload);
        Err(ControlPlaneError::Unsupported(format!(
            "{CAP_SESSION_COMMAND}:{command}"
        )))
    }

    // ─── optional operations added for Java `FrameworkAdapter` parity ───

    /// Known workspaces (`GET /agentscope/workspaces`, capability `workspace-inventory`).
    async fn list_workspaces(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
        Err(ControlPlaneError::Unsupported(
            CAP_WORKSPACE_INVENTORY.to_string(),
        ))
    }

    /// Session todo list (`GET .../tasks`, capability `task-query`).
    async fn list_tasks(&self, _session_id: &str) -> ControlPlaneResult<Vec<serde_json::Value>> {
        Err(ControlPlaneError::Unsupported(CAP_TASK_QUERY.to_string()))
    }

    /// Background subagent tasks (`GET .../subagent-tasks`, capability
    /// `subagent-task-query`).
    async fn list_subagent_tasks(
        &self,
        _session_id: &str,
    ) -> ControlPlaneResult<Vec<serde_json::Value>> {
        Err(ControlPlaneError::Unsupported(
            CAP_SUBAGENT_TASK_QUERY.to_string(),
        ))
    }

    /// Cancels one background subagent task (`DELETE .../subagent-tasks/{taskId}`, capability
    /// `subagent-task-command`).
    async fn cancel_subagent_task(&self, _session_id: &str, _task_id: &str) -> ControlPlaneResult<()> {
        Err(ControlPlaneError::Unsupported(
            CAP_SUBAGENT_TASK_COMMAND.to_string(),
        ))
    }

    /// Enters or leaves plan mode (`POST .../plan-mode`, capability `plan-mode`).
    ///
    /// `params` is the request body; `{"active": true|false}` is the canonical shape
    /// (absent `active` means "enter").
    async fn set_plan_mode(
        &self,
        _session_id: &str,
        _params: serde_json::Value,
    ) -> ControlPlaneResult<()> {
        Err(ControlPlaneError::Unsupported(CAP_PLAN_MODE.to_string()))
    }

    /// Injects a control-plane user message into the live session (`POST .../messages`,
    /// capability `conversation-inbound`).
    async fn inject_user_message(&self, _session_id: &str, _content: &str) -> ControlPlaneResult<()> {
        Err(ControlPlaneError::Unsupported(
            CAP_CONVERSATION_INBOUND.to_string(),
        ))
    }

    /// Runs a control-plane conversation turn, returning the terminal assistant reply
    /// (capability `conversation-inbound`).
    async fn run_conversation_turn(
        &self,
        _session_id: &str,
        _content: &str,
    ) -> ControlPlaneResult<String> {
        Err(ControlPlaneError::Unsupported(
            CAP_CONVERSATION_INBOUND.to_string(),
        ))
    }

    /// Effective definition snapshot merged into `GET /agentscope/info` → `agentConfig`.
    fn build_agent_config(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    /// Materializes a downstream AgentTask delivery (capability `agent-task`).
    async fn handle_agent_task(
        &self,
        _assignment: crate::model::AgentTaskAssignment,
    ) -> ControlPlaneResult<()> {
        Err(ControlPlaneError::Unsupported(CAP_AGENT_TASK.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopAdapter;

    #[async_trait]
    impl FrameworkAdapter for NoopAdapter {
        fn framework_name(&self) -> &str {
            "noop"
        }
        fn capabilities(&self) -> Vec<String> {
            vec![CAP_CONTEXT_QUERY.to_string()]
        }
        fn can_handle(&self, _t: &(dyn std::any::Any + Send + Sync)) -> bool {
            false
        }
        fn attach(
            &mut self,
            _t: std::sync::Arc<dyn std::any::Any + Send + Sync>,
            _e: std::sync::Arc<dyn Fn(SessionEvent) + Send + Sync>,
        ) -> ControlPlaneResult<()> {
            Ok(())
        }
        fn detach(&self) -> ControlPlaneResult<()> {
            Ok(())
        }
        async fn extract_context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
            Ok(ContextSnapshot::builder(session_id)
                .framework("noop")
                .build())
        }
    }

    #[tokio::test]
    async fn optional_ops_default_to_unsupported() {
        let a = NoopAdapter;
        assert!(a.list_messages("s1", 0, 10).await.is_err());
        assert!(a.list_subagents().await.is_err());
        assert!(a.execute_command("s1", COMMAND_ABORT, None).await.is_err());
    }

    /// Every one of the nine methods Java's `FrameworkAdapter` adds beyond the Rust skeleton
    /// must default to an honest `Unsupported(capability)` instead of silently succeeding.
    #[tokio::test]
    async fn java_parity_defaults_report_unsupported_capabilities() {
        let a = NoopAdapter;
        let unsupported = |result: ControlPlaneResult<()>, cap: &str| match result {
            Err(ControlPlaneError::Unsupported(reason)) => assert_eq!(reason, cap),
            other => panic!("expected Unsupported({cap}), got {other:?}"),
        };
        unsupported(a.list_workspaces().await.map(|_| ()), CAP_WORKSPACE_INVENTORY);
        unsupported(a.list_tasks("s1").await.map(|_| ()), CAP_TASK_QUERY);
        unsupported(
            a.list_subagent_tasks("s1").await.map(|_| ()),
            CAP_SUBAGENT_TASK_QUERY,
        );
        unsupported(
            a.cancel_subagent_task("s1", "t1").await,
            CAP_SUBAGENT_TASK_COMMAND,
        );
        unsupported(
            a.set_plan_mode("s1", serde_json::json!({ "active": true }))
                .await,
            CAP_PLAN_MODE,
        );
        unsupported(
            a.inject_user_message("s1", "hi").await,
            CAP_CONVERSATION_INBOUND,
        );
        unsupported(
            a.run_conversation_turn("s1", "hi").await.map(|_| ()),
            CAP_CONVERSATION_INBOUND,
        );
        let assignment = crate::model::AgentTaskAssignment {
            attempt_id: "attempt-1".into(),
            agent_task_id: "task-1".into(),
            run_id: "run-1".into(),
            node_id: "node-1".into(),
            generation: 1,
            command: "dispatch".into(),
            context_url: String::new(),
            task_token: String::new(),
            attempt_token: String::new(),
            session_id: "s1".into(),
            payload: Vec::new(),
            timestamp: 0,
        };
        unsupported(a.handle_agent_task(assignment).await, CAP_AGENT_TASK);
        // `build_agent_config` is a benign empty map, not an error (Java `Map.of()`).
        assert!(a.build_agent_config().is_empty());
    }
}
