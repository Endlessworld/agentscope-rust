//! Contract data provider SPI, mirroring `ContractProvider`.
//!
//! The in-process `/agentscope/*` contract server ([`ContractHttpServer`](super::ContractHttpServer))
//! resolves each incoming request through a provider. The typed Level-4/3 methods
//! ([`ContractProvider::inventory`] / [`ContractProvider::context`] / [`ContractProvider::messages`])
//! are implemented by [`ControlPlaneBridge`](crate::bridge::ControlPlaneBridge); the remaining routes have
//! honest defaults so a provider only implements what it actually supports (over-declaring shows
//! up as capability-mismatched buttons in the control plane).
//!
//! Error mapping mirrors the Java server: [`ControlPlaneError::NotFound`] -> 404,
//! [`ControlPlaneError::Busy`] -> 409, [`ControlPlaneError::Unauthorized`] -> 401,
//! [`ControlPlaneError::Unsupported`] -> 501, anything else -> 500.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::model::{ContextSnapshot, Inventory, MessagePage};

/// Supplies contract responses for a single instrumented agent.
#[async_trait]
pub trait ContractProvider: Send + Sync {
    /// `GET /agentscope/inventory`.
    async fn inventory(&self) -> ControlPlaneResult<Inventory>;

    /// `GET /agentscope/sessions/{id}/context`.
    async fn context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot>;

    /// `GET /agentscope/sessions/{id}/messages`.
    async fn messages(
        &self,
        session_id: &str,
        offset: i64,
        limit: i64,
    ) -> ControlPlaneResult<MessagePage>;

    /// List of session ids currently tracked by this instance.
    async fn session_ids(&self) -> ControlPlaneResult<Vec<String>>;

    // ─── contract routes with honest defaults ───

    /// `GET /agentscope/info`: the agent definition snapshot.
    async fn info(&self) -> ControlPlaneResult<Value> {
        Ok(json!({}))
    }

    /// `GET /agentscope/sessions`: session summaries (server caps the list at 500).
    async fn sessions(&self) -> ControlPlaneResult<Vec<Value>> {
        let ids = self.session_ids().await?;
        Ok(ids
            .into_iter()
            .map(|id| {
                let phase = self.session_phase(&id);
                json!({ "sessionId": id, "phase": phase })
            })
            .collect())
    }

    /// `GET /agentscope/sessions/{id}/state`.
    async fn session_state(&self, session_id: &str) -> ControlPlaneResult<Value> {
        Ok(json!({
            "sessionId": session_id,
            "phase": self.session_phase(session_id),
            "busy": false,
        }))
    }

    /// Current phase string used in command-success responses.
    fn session_phase(&self, _session_id: &str) -> String {
        "running".to_string()
    }

    /// `GET /agentscope/subagents`.
    async fn subagents(&self) -> ControlPlaneResult<Vec<Value>> {
        Ok(Vec::new())
    }

    /// `GET /agentscope/workspaces`.
    async fn workspaces(&self) -> ControlPlaneResult<Vec<Value>> {
        Ok(Vec::new())
    }

    /// `GET /agentscope/sessions/{id}/tasks` (capability `task-query`).
    async fn tasks(&self, _session_id: &str) -> ControlPlaneResult<Value> {
        Err(ControlPlaneError::Unsupported("task-query".to_string()))
    }

    /// `GET /agentscope/sessions/{id}/subagent-tasks` (capability `subagent-task-query`).
    async fn subagent_tasks(&self, _session_id: &str) -> ControlPlaneResult<Value> {
        Err(ControlPlaneError::Unsupported("subagent-task-query".to_string()))
    }

    /// `DELETE /agentscope/sessions/{id}/subagent-tasks/{taskId}` (capability
    /// `subagent-task-command`).
    async fn cancel_subagent_task(&self, _session_id: &str, _task_id: &str) -> ControlPlaneResult<()> {
        Err(ControlPlaneError::Unsupported(
            "subagent-task-command".to_string(),
        ))
    }

    /// `POST /agentscope/sessions/{id}/compress`.
    async fn compress(&self, _session_id: &str) -> ControlPlaneResult<()> {
        Err(ControlPlaneError::Unsupported("compress".to_string()))
    }

    /// `POST /agentscope/sessions/{id}/terminate`.
    async fn terminate(&self, _session_id: &str) -> ControlPlaneResult<()> {
        Ok(())
    }

    /// `POST /agentscope/sessions/{id}/abort`: aborts the current turn without ending the session.
    async fn abort(&self, _session_id: &str) -> ControlPlaneResult<()> {
        Ok(())
    }

    /// `POST /agentscope/sessions/{id}/plan-mode`; body is `{"active":bool}` (capability
    /// `plan-mode`).
    async fn plan_mode(&self, _session_id: &str, _body: Value) -> ControlPlaneResult<()> {
        Err(ControlPlaneError::Unsupported("plan-mode".to_string()))
    }

    /// `POST /agentscope/sessions/{id}/messages`: inject a user message (capability
    /// `conversation-inbound`). Busy sessions should return [`ControlPlaneError::Busy`].
    async fn post_message(&self, _session_id: &str, _body: Value) -> ControlPlaneResult<()> {
        Err(ControlPlaneError::Unsupported(
            "conversation-inbound".to_string(),
        ))
    }

    /// `GET /agentscope/sessions/{id}/export-transcript` (capability `export-transcript`).
    async fn export_transcript(&self, _session_id: &str) -> ControlPlaneResult<Value> {
        Err(ControlPlaneError::Unsupported("export-transcript".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Minimal;

    #[async_trait]
    impl ContractProvider for Minimal {
        async fn inventory(&self) -> ControlPlaneResult<Inventory> {
            Ok(Inventory::health_only(1))
        }
        async fn context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
            Ok(ContextSnapshot::builder(session_id).build())
        }
        async fn messages(
            &self,
            session_id: &str,
            offset: i64,
            limit: i64,
        ) -> ControlPlaneResult<MessagePage> {
            Ok(MessagePage::of(session_id, Vec::new(), offset, limit))
        }
        async fn session_ids(&self) -> ControlPlaneResult<Vec<String>> {
            Ok(vec!["s1".to_string()])
        }
    }

    #[tokio::test]
    async fn defaults_are_honest_about_unsupported_capabilities() {
        let p = Minimal;
        assert!(p.tasks("s1").await.is_err());
        assert!(p.compress("s1").await.is_err());
        assert!(p.post_message("s1", json!({})).await.is_err());
        assert!(p.plan_mode("s1", json!({})).await.is_err());
        assert!(p.export_transcript("s1").await.is_err());
        assert!(p.subagent_tasks("s1").await.is_err());
        assert!(p.cancel_subagent_task("s1", "t1").await.is_err());
        // terminate/abort are always safe no-ops by default.
        assert!(p.terminate("s1").await.is_ok());
        assert!(p.abort("s1").await.is_ok());
    }

    #[tokio::test]
    async fn default_sessions_derives_from_session_ids() {
        let p = Minimal;
        let sessions = p.sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["sessionId"], "s1");
        assert_eq!(sessions[0]["phase"], "running");
    }
}
