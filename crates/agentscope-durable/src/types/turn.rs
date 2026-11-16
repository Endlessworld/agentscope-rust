//! Turn state machine, recovery views, leases and control commands.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DelegationParent, SessionKey, ThreadKey};
use crate::json;
use crate::{DurableError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TurnStatus {
    Queued,
    Running,
    Waiting,
    Pausing,
    Paused,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl TurnStatus {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnReceipt {
    pub thread: ThreadKey,
    pub request_id: String,
    pub turn_id: String,
}

/// Additional invocation retries; the first recorded failure freezes this policy for its turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryPolicy {
    pub max_retries: u32,
    pub initial_delay_millis: u64,
    pub max_delay_millis: u64,
}

impl RecoveryPolicy {
    pub fn new(max_retries: u32, initial_delay_millis: u64, max_delay_millis: u64) -> Result<Self> {
        if max_retries > 1000
            || initial_delay_millis < 1
            || max_delay_millis < initial_delay_millis
            || max_delay_millis > 86_400_000
        {
            return Err(DurableError::invalid("Invalid bounded recovery policy"));
        }
        Ok(Self {
            max_retries,
            initial_delay_millis,
            max_delay_millis,
        })
    }

    pub fn defaults() -> Self {
        Self {
            // A transient store/lock outage commonly lasts tens of seconds; three retries at
            // 1/2/4s gave up in seven seconds and ended the turn. Six retries with a 60s cap
            // cover roughly a minute before the failure becomes terminal.
            max_retries: 6,
            initial_delay_millis: 1_000,
            max_delay_millis: 60_000,
        }
    }

    /// Exponential backoff for the `failures`-th consecutive failure.
    pub fn delay_millis(&self, failures: u32) -> u64 {
        let mut delay = self.initial_delay_millis;
        for _ in 1..failures {
            if delay >= self.max_delay_millis {
                break;
            }
            delay = (delay * 2).min(self.max_delay_millis);
        }
        delay
    }
}

/// Host classification of a failure; never permission to repeat an unknown external effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub retryable: bool,
    pub message: String,
    pub policy: RecoveryPolicy,
}

impl Failure {
    pub fn new(retryable: bool, message: impl Into<String>, policy: RecoveryPolicy) -> Self {
        Self {
            retryable,
            message: message.into(),
            policy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryView {
    pub failures: u32,
    pub policy: RecoveryPolicy,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    pub message: String,
}

impl RecoveryView {
    pub fn retry_allowed(&self) -> bool {
        self.retryable && self.failures <= self.policy.max_retries
    }

    pub fn validate(&self) -> Result<()> {
        let allowed = self.retry_allowed();
        if self.failures < 1
            || self.failures > self.policy.max_retries + 1
            || allowed != self.retry_at.is_some()
            || self.retry_at.is_some_and(|retry_at| retry_at < 0)
        {
            return Err(DurableError::invalid("Invalid recovery state"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnView {
    pub thread: ThreadKey,
    pub turn_id: String,
    pub status: TurnStatus,
    pub revision: u64,
    pub cancellation_requested: bool,
    #[serde(default)]
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<WaitCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<DelegationParent>,
}

/// The control facts a running invocation polls while it holds the lease.
///
/// Deliberately smaller than a [`Frame`](crate::types::Frame): a poll happens while a model call
/// is streaming — several times a second — and what it asks is "was I stopped, and am I still the
/// writer". The frame carries the conversation (tools, checkpoint, signals), so polling it costs
/// a read of the whole session; this costs one record, which is why the poll can stay frequent
/// while the rest of the tick got cheaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnControl {
    pub status: TurnStatus,
    pub cancellation_requested: bool,
}

/// Queue-facing projection of one turn.
///
/// Every field is read from committed facts: the status and the ending come from the turn itself,
/// the run id from the invocation that last worked on it. A host renders this as its task list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTask {
    pub thread: ThreadKey,
    pub turn_id: String,
    pub status: TurnStatus,
    pub settled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub output: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnPage {
    pub items: Vec<TurnView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// A claim on a thread's single writer slot. `epoch` is the fencing token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub thread: ThreadKey,
    pub turn_id: String,
    pub owner: String,
    pub epoch: u64,
    pub expires_at: i64,
}

impl Lease {
    pub fn session(&self) -> &SessionKey {
        &self.thread.session
    }
}

/// What a turn is waiting for: a signal key plus an optional deadline in epoch millis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitCondition {
    pub key: String,
    pub deadline: i64,
}

impl WaitCondition {
    pub fn new(key: impl Into<String>, deadline: i64) -> Result<Self> {
        let key = key.into();
        json::text(&key, "wait key")?;
        if deadline < 0 {
            return Err(DurableError::invalid("Negative deadline"));
        }
        Ok(Self { key, deadline })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Signal {
    pub id: String,
    pub key: String,
    #[serde(default)]
    pub value: Value,
}

impl Signal {
    pub fn new(id: impl Into<String>, key: impl Into<String>, value: Value) -> Result<Self> {
        let id = id.into();
        let key = key.into();
        json::text(&id, "signal id")?;
        json::text(&key, "signal key")?;
        Ok(Self {
            id,
            key,
            value: json::copy(&value),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ControlKind {
    Steer,
    Inject,
    Withdraw,
    Cancel,
    Respond,
    Pause,
    Resume,
    Signal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlCommand {
    pub turn_id: String,
    pub request_id: String,
    pub kind: ControlKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub value: Value,
    pub expected_revision: u64,
}

/// A stable admission receipt; later application is observed through events and state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandReceipt {
    pub request_id: String,
    pub turn_id: String,
    pub operation: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Moves a committed checkpoint into another thread; the target must be idle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointRestore {
    pub source: ThreadKey,
    pub position: u64,
    pub target: super::ThreadSpec,
    pub expected_position: u64,
    pub request_id: String,
}
