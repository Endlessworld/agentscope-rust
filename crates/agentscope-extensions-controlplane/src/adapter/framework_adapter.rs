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
    fn detach(&mut self) -> ControlPlaneResult<()>;

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
        Err(ControlPlaneError::Unsupported(CAP_MESSAGE_QUERY.to_string()))
    }

    /// Instance inventory: subagents, workspaces, health.
    async fn list_subagents(&self) -> ControlPlaneResult<Inventory> {
        Err(ControlPlaneError::Unsupported(CAP_SUBAGENT_INVENTORY.to_string()))
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
        fn detach(&mut self) -> ControlPlaneResult<()> {
            Ok(())
        }
        async fn extract_context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
            Ok(ContextSnapshot::builder(session_id).framework("noop").build())
        }
    }

    #[tokio::test]
    async fn optional_ops_default_to_unsupported() {
        let a = NoopAdapter;
        assert!(a.list_messages("s1", 0, 10).await.is_err());
        assert!(a.list_subagents().await.is_err());
        assert!(a.execute_command("s1", COMMAND_ABORT, None).await.is_err());
    }
}
