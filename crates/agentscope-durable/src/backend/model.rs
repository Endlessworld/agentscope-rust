//! Stored shapes and part-key layout for the transactional backend.
//!
//! Parts that paginate (turns, events, journal, checkpoints) stay separate so `keys(prefix,
//! after, limit)` can scan them; small collections (tools, receipts, runs, steps) are
//! embedded in their turn to keep one read per turn.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json;
use crate::types::*;

pub(crate) const CURRENT_FORMAT: u32 = 1;

pub(crate) fn hash(value: &str) -> String {
    json::hash_text(value)
}

/// Zero-padded order key so that lexicographic order is numeric order.
pub(crate) fn pos(n: u64) -> String {
    format!("{n:020}")
}

/// Session-scope part holding the root state (also the commit marker).
pub(crate) const ROOT: &str = "";

pub(crate) fn thread_part(thread_id: &str, suffix: &str) -> String {
    format!("thread:{}:{suffix}", hash(thread_id))
}

pub(crate) fn thread_data_part(thread_id: &str, suffix: &str) -> String {
    format!("thread-data:{}:{suffix}", hash(thread_id))
}

pub(crate) fn turn_part(turn_id: &str) -> String {
    format!("turn:{}", hash(turn_id))
}

pub(crate) fn admission_part(request_id: &str) -> String {
    format!("admission:{}", hash(request_id))
}

pub(crate) fn command_part(identity: &str) -> String {
    format!("command:{}", hash(identity))
}

pub(crate) fn control_receipt_part(request_id: &str) -> String {
    format!("control-receipt:{}", hash(request_id))
}

pub(crate) fn event_id_part(event_id: &str) -> String {
    format!("event-id:{}", hash(event_id))
}

pub(crate) fn event_part(sequence: u64) -> String {
    format!("event:{}", pos(sequence))
}

pub(crate) fn turn_order_part(admitted: u64) -> String {
    format!("turn-order:{}", pos(admitted))
}

pub(crate) fn execution_part(execution_id: &str) -> String {
    format!("execution:{}", hash(execution_id))
}

pub(crate) fn queue_part(thread_id: &str, index: u64) -> String {
    thread_data_part(thread_id, &format!("queue:{}", pos(index)))
}

pub(crate) fn checkpoint_part(thread_id: &str) -> String {
    thread_data_part(thread_id, "checkpoint")
}

pub(crate) fn checkpoint_history_part(thread_id: &str, position: u64) -> String {
    thread_data_part(thread_id, &format!("checkpoint-history:{}", pos(position)))
}

pub(crate) fn journal_part(thread_id: &str, branch: &str, position: u64) -> String {
    thread_data_part(thread_id, &format!("journal:{branch}:{}", pos(position)))
}

pub(crate) fn journal_prefix(thread_id: &str, branch: &str) -> String {
    thread_data_part(thread_id, &format!("journal:{branch}:"))
}

pub(crate) fn checkpoint_history_prefix(thread_id: &str) -> String {
    thread_data_part(thread_id, "checkpoint-history:")
}

pub(crate) fn event_prefix() -> String {
    "event:".to_string()
}

pub(crate) fn queue_prefix(thread_id: &str) -> String {
    thread_data_part(thread_id, "queue:")
}

pub(crate) fn turn_order_prefix() -> String {
    "turn-order:".to_string()
}

pub(crate) fn thread_prefix() -> String {
    "thread:".to_string()
}

/// Root record of one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionState {
    pub format: u32,
    pub session: SessionView,
    pub creation_digest: String,
    /// Session-wide admission counter; also the `turn-order` high-watermark.
    pub admitted: u64,
    /// Session-wide public event sequence.
    pub sequence: u64,
    pub retained_after: u64,
    /// Bytes the scope held after the last compaction, as measured by the commit that followed
    /// it (`0` when no compaction has been measured yet).
    ///
    /// A compaction runs inside the transaction that rewrites the document, so it cannot see how
    /// big the result will be; the next commit reads the size it wrote and stores it here. The
    /// value is what lets [`super::compact::worth_compacting`] refuse to re-walk a scope that
    /// hardly grew since its last trim. `serde(default)` so that a scope written before this
    /// field existed still loads.
    #[serde(default)]
    pub compacted_bytes: u64,
    /// Set by a compaction whose resulting size has not been recorded yet; cleared by the commit
    /// that records it.
    #[serde(default)]
    pub compact_pending: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ThreadData {
    pub definition: ThreadSpec,
    pub status: Lifecycle,
    /// Number of turns admitted to this thread.
    pub admitted: u64,
    /// Materialized prefix of this thread's queue: how many leading queue slots hold a turn that
    /// has already reached a terminal status.
    ///
    /// This is the turn axis, and it is what lets a reader skip the finished front of a
    /// conversation instead of reading it turn by turn to find the live one (`current_turn`). It
    /// is deliberately *not* the event retention watermark (`SessionState::retained_after`), which
    /// drops public events by sequence and says nothing about which turns are finished.
    ///
    /// `serde(default)` so that a thread record written before the prefix was materialized loads
    /// as `0`: readers then scan from the start, which is exactly the old behaviour.
    #[serde(default)]
    pub settled: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_metadata: Option<CheckpointMetadata>,
    pub checkpoint_position: u64,
    pub journal_position: u64,
    pub journal_branch: String,
    /// Monotonic fencing counter for the whole thread.
    pub epoch: u64,
    pub resource_position: u64,
}

impl ThreadData {
    pub fn new(definition: ThreadSpec) -> Self {
        Self {
            definition,
            status: Lifecycle::Open,
            admitted: 0,
            settled: 0,
            checkpoint_metadata: None,
            checkpoint_position: 0,
            journal_position: 0,
            journal_branch: "initial".to_string(),
            epoch: 0,
            resource_position: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ToolData {
    pub request: ExecutionRequest,
    /// Set once, before the effect leaves the runtime.
    pub attempted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ExecutionResult>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<RunStepRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationLink>,
}

impl ToolData {
    pub fn view(&self) -> ToolExecution {
        ToolExecution {
            request: self.request.clone(),
            dispatch_attempted: self.attempted,
            result: self.result.clone(),
            applied: self.applied,
            applied_at: self.applied_at.clone(),
            delegation: self.delegation.clone(),
        }
    }

    pub fn result_state(&self) -> ExecutionState {
        self.result
            .as_ref()
            .map(|result| result.state)
            .unwrap_or(ExecutionState::Pending)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TurnData {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub epoch: u64,
    pub expires_at: i64,
    pub cancel: bool,
    pub pause_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<WaitCondition>,
    #[serde(default)]
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_epoch: Option<u64>,
    pub next_attempt_at: i64,
    #[serde(default)]
    pub input: Value,
    pub admitted_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<DelegationParent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
    pub invocation_count: u64,
    #[serde(default)]
    pub consumed: Vec<String>,
    #[serde(default)]
    pub withdrawn: Vec<String>,
    #[serde(default)]
    pub signals: BTreeMap<String, Signal>,
    #[serde(default)]
    pub runs: BTreeMap<String, RunView>,
    #[serde(default)]
    pub run_order: Vec<String>,
    #[serde(default)]
    pub steps: BTreeMap<String, StepView>,
    #[serde(default)]
    pub step_order: Vec<String>,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolData>,
    #[serde(default)]
    pub tool_order: Vec<String>,
    #[serde(default)]
    pub receipts: BTreeMap<String, CommitReceipt>,
}

impl TurnData {
    pub fn new(turn_id: String, thread_id: String, input: Value, now: i64) -> Self {
        Self {
            turn_id,
            thread_id,
            status: TurnStatus::Queued,
            revision: 0,
            owner: None,
            epoch: 0,
            expires_at: 0,
            cancel: false,
            pause_requested: false,
            waiting: None,
            output: Value::Null,
            error: None,
            recovery: None,
            failed_epoch: None,
            next_attempt_at: now,
            input,
            admitted_at: now,
            parent: None,
            last_run_id: None,
            invocation_count: 0,
            consumed: Vec::new(),
            withdrawn: Vec::new(),
            signals: BTreeMap::new(),
            runs: BTreeMap::new(),
            run_order: Vec::new(),
            steps: BTreeMap::new(),
            step_order: Vec::new(),
            tools: BTreeMap::new(),
            tool_order: Vec::new(),
            receipts: BTreeMap::new(),
        }
    }

    pub fn view(&self, thread: &ThreadKey) -> TurnView {
        TurnView {
            thread: thread.clone(),
            turn_id: self.turn_id.clone(),
            status: self.status,
            revision: self.revision,
            cancellation_requested: self.cancel,
            output: self.output.clone(),
            error: self.error.clone(),
            waiting: self.waiting.clone(),
            recovery: self.recovery.clone(),
            parent: self.parent.clone(),
        }
    }

    /// Tools whose effect has not been settled yet.
    pub fn unsettled_tool(&self) -> bool {
        self.tools
            .values()
            .any(|tool| !tool.result_state().concluded() && tool.attempted)
    }

    pub fn any_unsettled_tool(&self) -> bool {
        self.tools
            .values()
            .any(|tool| !tool.result_state().concluded())
    }

    /// Tools whose effect may or may not have happened.
    ///
    /// Such an effect can never be applied, so a turn holding one must not report success; it
    /// can only be accepted as unknown and ended as failed or cancelled.
    pub fn unknown_tool(&self) -> bool {
        self.tools
            .values()
            .any(|tool| tool.result_state() == ExecutionState::Unknown)
    }

    pub fn unapplied_tool(&self) -> bool {
        self.tools
            .values()
            .any(|tool| tool.result_state() == ExecutionState::Succeeded && !tool.applied)
    }

    pub fn current_run(&self) -> Option<&RunView> {
        self.last_run_id.as_ref().and_then(|id| self.runs.get(id))
    }
}

/// Admission record: the frozen input digest and its receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AdmissionRecord {
    pub digest: String,
    pub receipt: TurnReceipt,
}

/// Control idempotency record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CommandRecord {
    pub digest: String,
}

/// A checkpoint record: metadata plus its (inline) content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CheckpointRecord {
    pub metadata: CheckpointMetadata,
    pub checkpoint: Value,
    pub branch: String,
    pub journal_position: u64,
    pub resource_position: u64,
    pub settled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scope document outlives releases, so a root record written before the compaction guard
    /// existed must still load: the two new fields default, and nothing else moves.
    #[test]
    fn a_root_record_from_before_the_compaction_guard_still_loads() {
        let record = serde_json::json!({
            "format": CURRENT_FORMAT,
            "session": {
                "key": { "namespace": "ns", "sessionId": "s" },
                "primaryThreadId": "main",
                "status": "OPEN",
                "metadata": null,
            },
            "creation_digest": "digest",
            "admitted": 3,
            "sequence": 7,
            "retained_after": 2,
        });
        let state: SessionState = serde_json::from_value(record).expect("an older record loads");
        assert_eq!(state.admitted, 3);
        assert_eq!(state.sequence, 7);
        assert_eq!(state.retained_after, 2);
        assert_eq!(state.compacted_bytes, 0, "no trim has been measured");
        assert!(!state.compact_pending);
    }

    /// The settled prefix is a new field on an old record: a thread written before it existed
    /// must load as "nothing materialized yet", which is what makes readers scan from the start
    /// exactly as they used to.
    #[test]
    fn a_thread_record_from_before_the_settled_prefix_still_loads() {
        let record = serde_json::json!({
            "definition": {
                "key": {
                    "session": { "namespace": "ns", "sessionId": "s" },
                    "threadId": "main",
                },
                "binding": {
                    "harness": "harness-a",
                    "definitionVersion": "def-1",
                    "stateVersion": "state-1",
                },
            },
            "status": "OPEN",
            "admitted": 12,
            "checkpoint_position": 0,
            "journal_position": 0,
            "journal_branch": "initial",
            "epoch": 0,
            "resource_position": 0,
        });
        let thread: ThreadData =
            serde_json::from_value(record).expect("a thread record without the prefix loads");
        assert_eq!(thread.admitted, 12);
        assert_eq!(thread.settled, 0, "an old record materializes nothing");
    }
}
