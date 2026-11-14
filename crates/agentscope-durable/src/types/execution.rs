//! Tool calls, execution identities and the three-state recovery surface.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{RunStepRef, ThreadKey, ThreadSpec};
use crate::json;
use crate::{DurableError, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub call_id: String,
    pub executor: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    /// Digest of the bound tool catalog entry; required by catalog-backed workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RunStepRef>,
}

impl ToolCall {
    pub fn new(
        call_id: impl Into<String>,
        executor: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Result<Self> {
        let call = Self {
            call_id: call_id.into(),
            executor: executor.into(),
            name: name.into(),
            arguments: json::copy(&arguments),
            binding: None,
            origin: None,
        };
        call.validate()?;
        Ok(call)
    }

    pub fn with_binding(mut self, binding: impl Into<String>) -> Result<Self> {
        self.binding = Some(binding.into());
        self.validate()?;
        Ok(self)
    }

    pub fn with_origin(mut self, origin: RunStepRef) -> Self {
        self.origin = Some(origin);
        self
    }

    pub fn validate(&self) -> Result<()> {
        json::text(&self.call_id, "callId")?;
        json::text(&self.executor, "executor")?;
        json::text(&self.name, "tool name")?;
        if let Some(binding) = &self.binding {
            let valid = binding.len() == 64 && binding.chars().all(|c| c.is_ascii_hexdigit());
            if !valid {
                return Err(DurableError::invalid("Invalid tool binding digest"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRequest {
    pub execution_id: String,
    pub thread: ThreadKey,
    pub turn_id: String,
    pub call: ToolCall,
}

impl ExecutionRequest {
    pub fn new(
        execution_id: impl Into<String>,
        thread: ThreadKey,
        turn_id: impl Into<String>,
        call: ToolCall,
    ) -> Result<Self> {
        let execution_id = execution_id.into();
        let turn_id = turn_id.into();
        json::text(&execution_id, "executionId")?;
        json::text(&turn_id, "turnId")?;
        Ok(Self {
            execution_id,
            thread,
            turn_id,
            call,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    NotFound,
    Unknown,
}

impl ExecutionState {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionResult {
    pub state: ExecutionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ExecutionResult {
    pub fn succeeded(result: Value) -> Self {
        Self {
            state: ExecutionState::Succeeded,
            handle: None,
            result,
            error: None,
        }
    }

    /// The effect may or may not have happened; never retried automatically.
    pub fn unknown(error: impl Into<String>) -> Self {
        Self {
            state: ExecutionState::Unknown,
            handle: None,
            result: Value::Null,
            error: Some(error.into()),
        }
    }

    pub fn missing() -> Self {
        Self {
            state: ExecutionState::NotFound,
            handle: None,
            result: Value::Null,
            error: None,
        }
    }
}

/// Durable record of one tool effect: three states plus the run/step that applied it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecution {
    pub request: ExecutionRequest,
    pub dispatch_attempted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ExecutionResult>,
    /// The result is merged into native input/state; `applied_at` prevents double application.
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<RunStepRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationLink {
    pub thread: ThreadKey,
    pub turn_id: String,
}

/// Parent identity of a delegated (child) turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationParent {
    pub thread: ThreadKey,
    pub turn_id: String,
    pub execution_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RunStepRef>,
}

/// Frozen child binding and input; naming an existing thread reuses its context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationTarget {
    pub child: ThreadSpec,
    #[serde(default)]
    pub input: Value,
}

/// A cancellation before admission has no child; it never creates a runnable task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationView {
    pub parent: ExecutionRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child: Option<DelegationLink>,
    pub cancel_requested: bool,
}

/// Result application and its checkpoint commit together, possibly in a later run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedResult {
    pub execution_id: String,
    pub applied_at: RunStepRef,
}

/// Host-verified terminal fact for an UNKNOWN effect; evidence is an audit reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionReconciliation {
    pub request_id: String,
    pub result: ExecutionResult,
    pub evidence_ref: String,
}

impl ExecutionReconciliation {
    pub fn validate(&self) -> Result<()> {
        json::text(&self.request_id, "reconciliation requestId")?;
        if !self.result.state.terminal() {
            return Err(DurableError::invalid(
                "Reconciliation requires a verified terminal result",
            ));
        }
        if self.evidence_ref.trim().is_empty() || self.evidence_ref.chars().count() > 2048 {
            return Err(DurableError::invalid(
                "Reconciliation requires an evidence reference",
            ));
        }
        Ok(())
    }
}

/// Base64 output bytes; byte offsets never share the public event cursor namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputPage {
    pub offset: u64,
    pub next_offset: u64,
    pub data: String,
    pub size: u64,
    pub terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<String>,
}
