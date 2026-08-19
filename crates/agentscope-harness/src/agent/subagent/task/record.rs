//! Persistent metadata for a background subagent task, mirroring `TaskRecord`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::now_millis;
use super::status::TaskStatus;

/// Persistent metadata for a background subagent task, stored as JSON in the workspace.
/// This is the authoritative truth source for task state; in-memory `BackgroundTask` handles
/// are a local performance overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskRecord {
    pub task_id: String,
    pub sub_agent_id: String,
    pub parent_agent_id: String,
    pub parent_session_id: String,
    pub sub_session_id: Option<String>,
    pub status: TaskStatus,
    pub result: Option<String>,
    pub error_message: Option<String>,
    pub cancel_requested: bool,
    pub created_at: u64,
    pub last_checked_at: u64,
    pub last_updated_at: u64,
    /// When this terminal-state task's result was pushed back to the parent agent.
    pub delivered_at: Option<u64>,
    /// When `"agent-protocol"`, this task is driven by a remote agent-protocol client.
    pub transport_type: Option<String>,
    pub remote_base_url: Option<String>,
    pub remote_headers: Option<BTreeMap<String, String>>,
}

impl Default for TaskRecord {
    fn default() -> Self {
        let now = now_millis();
        Self {
            task_id: String::new(),
            sub_agent_id: String::new(),
            parent_agent_id: String::new(),
            parent_session_id: String::new(),
            sub_session_id: None,
            status: TaskStatus::Pending,
            result: None,
            error_message: None,
            cancel_requested: false,
            created_at: now,
            last_checked_at: now,
            last_updated_at: now,
            delivered_at: None,
            transport_type: None,
            remote_base_url: None,
            remote_headers: None,
        }
    }
}

impl TaskRecord {
    pub fn new(
        task_id: impl Into<String>,
        sub_agent_id: impl Into<String>,
        parent_agent_id: impl Into<String>,
        parent_session_id: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            sub_agent_id: sub_agent_id.into(),
            parent_agent_id: parent_agent_id.into(),
            parent_session_id: parent_session_id.into(),
            ..Default::default()
        }
    }

    /// Convenience: update `last_updated_at` to now.
    pub fn touch(&mut self) {
        self.last_updated_at = now_millis();
    }

    /// Convenience: update `last_checked_at` to now.
    pub fn touch_checked(&mut self) {
        self.last_checked_at = now_millis();
    }

    /// Whether this task's completion has been pushed back to the parent agent.
    pub fn is_delivered(&self) -> bool {
        self.delivered_at.is_some()
    }

    /// Whether this task is executed via the HTTP task protocol.
    pub fn is_agent_protocol_transport(&self) -> bool {
        self.transport_type.as_deref() == Some("agent-protocol")
    }
}
