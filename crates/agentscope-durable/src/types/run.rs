//! Invocation facts: runs, steps, model calls and the changes a commit may declare.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ThreadKey;
use crate::json;
use crate::{DurableError, Result};

/// A position inside the enclosing thread/turn; auxiliary operations may have no step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStepRef {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

impl RunStepRef {
    pub fn new(run_id: impl Into<String>, step_id: Option<String>) -> Result<Self> {
        let run_id = run_id.into();
        json::text(&run_id, "runId")?;
        if let Some(step_id) = &step_id {
            json::text(step_id, "stepId")?;
        }
        Ok(Self { run_id, step_id })
    }
}

/// One actual native invocation; it never owns the logical task queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunStatus {
    Prepared,
    Running,
    Completed,
    Stopped,
    Failed,
    Lost,
}

impl RunStatus {
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Prepared | Self::Running)
    }
}

#[allow(clippy::too_many_arguments)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunView {
    pub thread: ThreadKey,
    pub turn_id: String,
    pub run_id: String,
    pub request_id: String,
    pub owner: String,
    pub epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_run_id: Option<String>,
    pub status: RunStatus,
    pub prepared_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunPage {
    pub items: Vec<RunView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepStatus {
    Running,
    Completed,
    Stopped,
    Failed,
    Lost,
}

impl StepStatus {
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepPhase {
    Reasoning,
    Acting,
    Finalization,
}

#[allow(clippy::too_many_arguments)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepView {
    pub thread: ThreadKey,
    pub turn_id: String,
    pub run_id: String,
    pub step_id: String,
    pub index: i32,
    pub status: StepStatus,
    pub phase: StepPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from_step_id: Option<String>,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepPage {
    pub items: Vec<StepView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ModelStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

/// A model call cannot reveal retries hidden inside an uninstrumented implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ModelObservation {
    ModelCall,
    ProviderAttempts,
}

/// Known token counts. Missing values stay absent; details are subsets, not extra tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, i64>,
}

impl TokenUsage {
    pub fn plus(&self, other: &Self) -> Result<Self> {
        let mut details = self.details.clone();
        for (key, value) in &other.details {
            let merged = details.get(key).copied().unwrap_or(0);
            details.insert(
                key.clone(),
                merged.checked_add(*value).ok_or_else(overflow)?,
            );
        }
        Ok(Self {
            input_tokens: sum(self.input_tokens, other.input_tokens)?,
            output_tokens: sum(self.output_tokens, other.output_tokens)?,
            details,
        })
    }

    pub fn validate(&self) -> Result<()> {
        for count in [self.input_tokens, self.output_tokens]
            .into_iter()
            .flatten()
        {
            if count < 0 {
                return Err(DurableError::invalid("Token counts must be nonnegative"));
            }
        }
        if self.details.values().any(|value| *value < 0) {
            return Err(DurableError::invalid("Token details must be nonnegative"));
        }
        Ok(())
    }
}

fn sum(left: Option<i64>, right: Option<i64>) -> Result<Option<i64>> {
    match (left, right) {
        (None, None) => Ok(None),
        (Some(value), None) | (None, Some(value)) => Ok(Some(value)),
        (Some(left), Some(right)) => left.checked_add(right).map(Some).ok_or_else(overflow),
    }
}

fn overflow() -> DurableError {
    DurableError::invalid("Token counts overflow")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRequestView {
    pub request_id: String,
    pub attempt: i32,
    pub provider: String,
    pub status: ModelStatus,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_millis: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    pub usage_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

/// A logical call with its observed requests. A replay refers to the original call.
#[allow(clippy::too_many_arguments)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCallView {
    pub thread: ThreadKey,
    pub turn_id: String,
    pub run_id: String,
    pub step_id: String,
    pub model_call_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub iteration: i32,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub status: ModelStatus,
    pub observation: ModelObservation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    pub usage_complete: bool,
    #[serde(default)]
    pub requests: Vec<ModelRequestView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCallPage {
    pub items: Vec<ModelCallView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Invocation facts commit with the native checkpoint; only takeover may declare `Lost`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunChange {
    pub run_id: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RunChange {
    pub fn validate(&self) -> Result<()> {
        json::text(&self.run_id, "runId")?;
        if matches!(self.status, RunStatus::Prepared | RunStatus::Lost) {
            return Err(DurableError::invalid(
                "Run status is owned by the execution host",
            ));
        }
        if self.status.terminal() {
            let outcome = self.outcome.as_deref().unwrap_or_default();
            json::text(outcome, "run outcome")?;
        } else if self.outcome.is_some() || self.reason.is_some() {
            return Err(DurableError::invalid(
                "A running invocation has no final outcome",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepChange {
    pub run_id: String,
    pub step_id: String,
    pub status: StepStatus,
    pub phase: StepPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from_step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl StepChange {
    pub fn validate(&self) -> Result<()> {
        json::text(&self.run_id, "runId")?;
        json::text(&self.step_id, "stepId")?;
        if self.status == StepStatus::Lost {
            return Err(DurableError::invalid(
                "Only takeover may declare a lost Step",
            ));
        }
        if let Some(step) = &self.resumed_from_step_id {
            json::text(step, "resumedFromStepId")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub items: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// A pending interaction surfaced for inspection (permission request, user question, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interaction {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub id: String,
    pub request_id: String,
    pub turn_id: String,
    pub key: String,
    pub kind: String,
    pub status: String,
    pub automatic: bool,
    #[serde(default)]
    pub call: serde_json::Value,
}
