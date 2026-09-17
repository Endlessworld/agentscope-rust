//! Persistent metadata for a background subagent task, mirroring `TaskRecord`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::now_millis;
use super::status::TaskStatus;
use crate::agent::subagent::protocol::RemotePendingConfirm;

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
    /// When `true`, the remote (or local) agent is paused waiting for user tool confirmation.
    /// Status remains `Running` so barriers keep waiting.
    pub awaiting_confirm: bool,
    /// Tool calls awaiting user confirmation while `awaiting_confirm` is set.
    pub pending_confirms: Option<Vec<RemotePendingConfirm>>,
    /// Last remote event sequence observed by the parent (for SSE resume).
    pub last_event_seq: i64,
    /// When `"agent-protocol"`, this task is driven by a remote agent-protocol client.
    pub transport_type: Option<String>,
    pub remote_base_url: Option<String>,
    pub remote_headers: Option<BTreeMap<String, String>>,
    /// Monotonic record version, incremented on every guarded write. Used for optimistic
    /// concurrency (CAS) when several nodes/threads update the same workspace task file.
    pub version: u64,
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
            awaiting_confirm: false,
            pending_confirms: None,
            last_event_seq: 0,
            transport_type: None,
            remote_base_url: None,
            remote_headers: None,
            version: 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serializes_confirm_fields_in_camel_case() {
        let mut rec = TaskRecord::new("t1", "sub1", "parent", "sess");
        rec.awaiting_confirm = true;
        rec.pending_confirms = Some(vec![RemotePendingConfirm::new("c1", "read_file", None)]);
        rec.last_event_seq = 7;

        let json = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["awaitingConfirm"], true);
        assert_eq!(json["lastEventSeq"], 7);
        assert_eq!(json["pendingConfirms"][0]["toolCallId"], "c1");

        let back: TaskRecord = serde_json::from_value(json).unwrap();
        assert!(back.awaiting_confirm);
        assert_eq!(back.last_event_seq, 7);
        assert_eq!(back.pending_confirms.unwrap().len(), 1);
    }

    #[test]
    fn record_defaults_missing_confirm_fields() {
        let rec: TaskRecord = serde_json::from_str("{\"taskId\":\"t1\"}").unwrap();
        assert!(!rec.awaiting_confirm);
        assert!(rec.pending_confirms.is_none());
        assert_eq!(rec.last_event_seq, 0);
    }
}
