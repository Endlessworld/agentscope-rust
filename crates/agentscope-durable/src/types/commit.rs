//! Commits, checkpoints and the public event contract.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    AppliedResult, ModelCallChange, ResourceValue, RunChange, SessionKey, Signal, StepChange,
    ThreadSpec, ToolCall, ToolExecution, ToolResultRecord, TurnStatus, TurnView, WaitCondition,
};
use crate::json;
use crate::{DurableError, Result};

/// Wire contract version shared by local components and transport adapters.
pub const PROTOCOL_VERSION: &str = "durable.v1";

/// Native state binding version; bumped whenever the checkpoint layout changes.
pub const CHECKPOINT_SCHEMA_VERSION: &str = "1";

/// An internal fact to append inside the same transaction as the checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventInput {
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

impl EventInput {
    pub fn new(event_id: impl Into<String>, event_type: impl Into<String>, data: Value) -> Self {
        Self {
            event_id: event_id.into(),
            event_type: event_type.into(),
            data: json::copy(&data),
            run_id: None,
            step_id: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        json::text(&self.event_id, "eventId")?;
        json::text(&self.event_type, "event type")?;
        if let Some(run_id) = &self.run_id {
            json::text(run_id, "runId")?;
        }
        if let Some(step_id) = &self.step_id {
            json::text(step_id, "stepId")?;
            if self.run_id.is_none() {
                return Err(DurableError::invalid("Step requires a Run"));
            }
        }
        Ok(())
    }
}

/// A committed public fact with a stable cursor.
#[allow(clippy::too_many_arguments)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicEvent {
    pub schema_version: String,
    pub event_id: String,
    pub session: SessionKey,
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub sequence: String,
    pub cursor: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Value,
}

/// Best-effort live signal: no durable sequence or cursor, never part of history.
#[allow(clippy::too_many_arguments)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveEvent {
    pub schema_version: String,
    pub event_id: String,
    pub session: SessionKey,
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub occurred_at: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Value,
}

/// Invocation-local input used to publish a live event after its native commit succeeds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveEventInput {
    pub event_id: String,
    pub occurred_at: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPage {
    pub events: Vec<PublicEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high_watermark: Option<String>,
}

/// A committed snapshot of one session, used for resync after a lost cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub session: SessionKey,
    pub turns: Vec<TurnView>,
    #[serde(default)]
    pub projection: Value,
    pub sequence: String,
    pub cursor: String,
}

/// Opaque native entries appended atomically with checkpoint and effects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalAppend {
    pub expected_position: u64,
    #[serde(default)]
    pub entries: Vec<Value>,
}

/// One fenced state transition. Every field is optional except the identity and target status.
#[allow(clippy::too_many_arguments)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Commit {
    pub commit_id: String,
    pub expected_revision: u64,
    #[serde(default)]
    pub checkpoint: Value,
    #[serde(default)]
    pub tools: Vec<ToolCall>,
    /// Execution ids dispatched by this commit, applied after the intents above.
    #[serde(default)]
    pub dispatches: Vec<String>,
    /// Results observed by this commit, applied after the dispatches.
    #[serde(default)]
    pub results: Vec<ToolResultRecord>,
    #[serde(default)]
    pub applied_results: Vec<AppliedResult>,
    #[serde(default)]
    pub consumed_signals: Vec<String>,
    #[serde(default)]
    pub events: Vec<EventInput>,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<WaitCondition>,
    #[serde(default)]
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<JournalAppend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunChange>,
    #[serde(default)]
    pub steps: Vec<StepChange>,
    /// Model calls this commit observed (a dispatch, its ending, or both).
    ///
    /// Skipped when empty so that a commit which observes no model call serializes to exactly the
    /// bytes it did before this fact existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_calls: Vec<ModelCallChange>,
}

impl Commit {
    pub fn new(commit_id: impl Into<String>, expected_revision: u64, status: TurnStatus) -> Self {
        Self {
            commit_id: commit_id.into(),
            expected_revision,
            checkpoint: Value::Null,
            tools: Vec::new(),
            dispatches: Vec::new(),
            results: Vec::new(),
            applied_results: Vec::new(),
            consumed_signals: Vec::new(),
            events: Vec::new(),
            status,
            waiting: None,
            output: Value::Null,
            error: None,
            journal: None,
            run: None,
            steps: Vec::new(),
            model_calls: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        json::text(&self.commit_id, "commitId")?;
        if matches!(
            self.status,
            TurnStatus::Queued | TurnStatus::Pausing | TurnStatus::Cancelling
        ) {
            return Err(DurableError::invalid("Invalid commit revision/status"));
        }
        for tool in &self.tools {
            tool.validate()?;
        }
        for execution_id in &self.dispatches {
            super::execution::check_execution_id(execution_id)?;
        }
        for record in &self.results {
            record.validate()?;
        }
        for event in &self.events {
            event.validate()?;
        }
        if let Some(run) = &self.run {
            run.validate()?;
        }
        for step in &self.steps {
            step.validate()?;
        }
        for call in &self.model_calls {
            call.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitReceipt {
    pub commit_id: String,
    pub digest: String,
    pub revision: u64,
    pub cursor: String,
}

/// Frozen identity of a checkpoint: only restorable under the same binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointMetadata {
    pub schema_version: String,
    pub harness: String,
    pub definition_version: String,
    pub state_version: String,
    pub position: String,
    pub digest: String,
}

impl CheckpointMetadata {
    pub fn verify(&self, definition: &ThreadSpec, checkpoint: &Value) -> Result<()> {
        if self.schema_version != CHECKPOINT_SCHEMA_VERSION
            || definition.harness() != self.harness
            || definition.definition_version() != self.definition_version
            || definition.state_version() != self.state_version
        {
            return Err(DurableError::incompatible(
                "Checkpoint binding/version mismatch",
            ));
        }
        let position_valid = !self.position.is_empty()
            && !self.position.starts_with('0')
            && self.position.chars().all(|c| c.is_ascii_digit());
        if !position_valid || !json::matches(&self.digest, checkpoint) {
            return Err(DurableError::invalid("Checkpoint integrity check failed"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointView {
    pub definition: ThreadSpec,
    pub metadata: CheckpointMetadata,
    pub checkpoint: Value,
    pub journal_position: u64,
    pub settled: bool,
}

/// A settled baseline prepared by a native adapter; installing it requires an idle target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Restoration {
    pub request_id: String,
    pub expected_position: u64,
    pub target: ThreadSpec,
    pub checkpoint: Value,
    #[serde(default)]
    pub journal: Vec<Value>,
    #[serde(default)]
    pub resources: Vec<ResourceValue>,
}

/// The material state a worker needs to make progress on one thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    pub definition: ThreadSpec,
    pub turn: TurnView,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub checkpoint: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_metadata: Option<CheckpointMetadata>,
    #[serde(default)]
    pub tools: Vec<ToolExecution>,
    #[serde(default)]
    pub signals: Vec<Signal>,
    pub now: i64,
}

/// A selection within one session's committed stream, retaining its original cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl EventFilter {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn validate(&self) -> Result<()> {
        for (value, name) in [
            (self.thread_id.as_deref(), "threadId"),
            (self.turn_id.as_deref(), "turnId"),
            (self.run_id.as_deref(), "runId"),
        ] {
            if let Some(value) = value {
                json::text(value, name)?;
            }
        }
        if (self.turn_id.is_some() && self.thread_id.is_none())
            || (self.run_id.is_some() && self.turn_id.is_none())
        {
            return Err(DurableError::invalid(
                "Event filter requires parent identities",
            ));
        }
        Ok(())
    }

    pub fn matches(&self, event: &PublicEvent) -> bool {
        self.matches_ids(
            Some(&event.thread_id),
            Some(&event.turn_id),
            event.run_id.as_deref(),
        )
    }

    pub fn matches_live(&self, event: &LiveEvent) -> bool {
        self.matches_ids(
            Some(&event.thread_id),
            Some(&event.turn_id),
            event.run_id.as_deref(),
        )
    }

    /// `turn_id`/`run_id` are always present on committed events, absent on some live ones.
    fn matches_ids(&self, thread: Option<&str>, turn: Option<&str>, run: Option<&str>) -> bool {
        let field_matches = |filter: &Option<String>, value: Option<&str>| match filter {
            None => true,
            Some(expected) => value == Some(expected.as_str()),
        };
        field_matches(&self.thread_id, thread)
            && field_matches(&self.turn_id, turn)
            && field_matches(&self.run_id, run)
    }
}
