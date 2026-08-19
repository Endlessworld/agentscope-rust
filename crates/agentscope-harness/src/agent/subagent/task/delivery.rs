//! Snapshot of a terminal-state subagent task awaiting push delivery, mirroring
//! `TaskDelivery`.

use serde::{Deserialize, Serialize};

use super::status::TaskStatus;

/// Snapshot of a terminal-state subagent task awaiting (or undergoing) push delivery to the
/// parent agent's reasoning loop. Fields are denormalised from `TaskRecord`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDelivery {
    pub task_id: String,
    pub agent_id: Option<String>,
    pub status: TaskStatus,
    pub result: Option<String>,
    pub error_message: Option<String>,
    pub completed_at: u64,
}
