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

/// The identity a resource is addressed by: its namespace path and key, never ambiguous when a
/// namespace part or the key contains the separator the path is rendered with.
fn resource_identity(namespace: &[String], key: &str) -> String {
    let mut identity = namespace.join("\u{0}");
    identity.push('\u{0}');
    identity.push_str(key);
    identity
}

/// One private resource value, stored on its own.
///
/// Resources are not embedded in the session root: a resource write would then rewrite the whole
/// conversation, and a session document already grows with the conversation. One part per value
/// keeps a resource mutation proportional to the value.
pub(crate) fn resource_part(namespace: &[String], key: &str) -> String {
    format!("resource:{}", hash(&resource_identity(namespace, key)))
}

pub(crate) fn resource_prefix() -> String {
    "resource:".to_string()
}

/// The answer one mutation produced, so a replayed request costs a read and no second write.
pub(crate) fn resource_receipt_part(request_id: &str) -> String {
    format!("resource-receipt:{}", hash(request_id))
}

/// The result of one installed restoration, keyed by its request id.
pub(crate) fn restore_part(request_id: &str) -> String {
    format!("restore:{}", hash(request_id))
}

/// The lineage one registered delegation produced, keyed by its request id.
pub(crate) fn delegation_part(request_id: &str) -> String {
    format!("delegation:{}", hash(request_id))
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

/// A model call as stored inside its turn.
///
/// The stored shape leaves out everything the turn already knows (`thread`, `turnId`): repeating
/// the session key on every call of every turn is pure write amplification on a store that
/// rewrites the scope document per commit. Absent optional fields are skipped as well, so a call
/// that never ended (no model, no usage, no request) costs a few dozen bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelCallData {
    pub model_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_millis: Option<i64>,
    pub status: ModelStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default)]
    pub usage_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<ModelRequestView>,
}

impl ModelCallData {
    pub fn new(model_call_id: String, started_at: i64) -> Self {
        Self {
            model_call_id,
            model: None,
            purpose: None,
            iteration: None,
            step_id: None,
            started_at,
            ended_at: None,
            duration_millis: None,
            status: ModelStatus::Running,
            usage: None,
            usage_complete: false,
            error_type: None,
            requests: Vec::new(),
        }
    }

    pub fn view(&self, thread: &ThreadKey, turn_id: &str) -> ModelCallView {
        ModelCallView {
            thread: thread.clone(),
            turn_id: turn_id.to_string(),
            run_id: None,
            step_id: self.step_id.clone(),
            model_call_id: self.model_call_id.clone(),
            model: self.model.clone(),
            purpose: self.purpose.clone(),
            iteration: self.iteration,
            started_at: self.started_at,
            ended_at: self.ended_at,
            duration_millis: self.duration_millis,
            status: self.status,
            observation: ModelObservation::ModelCall,
            usage: self.usage.clone(),
            usage_complete: self.usage_complete,
            requests: self.requests.clone(),
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
    /// Model calls this turn observed, in dispatch order.
    ///
    /// `serde(default)` keeps every scope written before model facts existed readable: such a
    /// turn has no calls, which is what it truthfully has.
    #[serde(default)]
    pub model_calls: BTreeMap<String, ModelCallData>,
    #[serde(default)]
    pub model_call_order: Vec<String>,
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
            model_calls: BTreeMap::new(),
            model_call_order: Vec::new(),
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

/// A checkpoint record: metadata plus, for older roots, its inline content.
///
/// Newer roots do not inline the state: the `state/checkpoint` entry in the journal is the only
/// copy, and `journal_seq` names the entry. Inlining it here as well stored the same state three
/// times (the journal entry, this record and `checkpoint-history`), which is the largest
/// duplication a session had after the request bodies stopped repeating the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CheckpointRecord {
    pub metadata: CheckpointMetadata,
    /// The state itself; `null` when [`Self::journal_seq`] names the entry that carries it.
    #[serde(default)]
    pub checkpoint: Value,
    pub branch: String,
    pub journal_position: u64,
    pub resource_position: u64,
    pub settled: bool,
    /// The `seq` of the `state/checkpoint` entry this record describes; `0` for an older root.
    #[serde(default)]
    pub journal_seq: u64,
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

    /// The stored model call is the only byte cost of wiring model facts, so it is pinned here:
    /// one call with its single provider attempt must stay small, because the turn document that
    /// carries it is rewritten by every later commit of the same turn.
    #[test]
    fn a_stored_model_call_keeps_its_record_small() {
        let mut call = ModelCallData::new("0123456789abcdef0123456789abcdef".to_string(), 0);
        call.model = Some("scripted".to_string());
        call.purpose = Some("REASONING".to_string());
        call.iteration = Some(0);
        call.step_id = Some("7c2fd8c0-6ad3-4b2c-9f1e-5a1e6d1f0a11".to_string());
        call.status = ModelStatus::Completed;
        call.ended_at = Some(1_700_000_000_123);
        call.duration_millis = Some(123);
        call.usage = Some(TokenUsage {
            input_tokens: Some(1_204),
            output_tokens: Some(87),
            details: Default::default(),
        });
        call.usage_complete = true;
        call.requests.push(ModelRequestView {
            request_id: "0123456789abcdef0123456789abcdef:1".to_string(),
            attempt: 1,
            provider: "scripted".to_string(),
            status: ModelStatus::Completed,
            started_at: 0,
            ended_at: Some(1_700_000_000_123),
            duration_millis: Some(123),
            usage: call.usage.clone(),
            usage_complete: true,
            error_type: None,
        });
        let bytes = serde_json::to_vec(&call).expect("serialize").len();
        eprintln!("one stored model call = {bytes} bytes");
        assert!(
            bytes <= 600,
            "one model call costs {bytes} stored bytes, which is more than the 600-byte budget"
        );
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
