//! AgentTask outcome capture, mirroring
//! `io.agentscope.extensions.controlplane.adapter.{AgentTaskOutcome, AgentTaskToolContext,
//! AgentTaskOutcomeTool}`.
//!
//! A business outcome is submitted by the model independently of a physical model turn
//! ending. Only the adapter is allowed to commit the physical AgentTask lifecycle, so the
//! tool merely records the model's intent into [`AgentTaskOutcomeState`], which the
//! adapter owns per dispatch.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

/// Allowed values for [`AgentTaskOutcome::outcome`].
pub const OUTCOME_SUCCEEDED: &str = "succeeded";
/// `waiting` outcome.
pub const OUTCOME_WAITING: &str = "waiting";
/// `blocked` outcome.
pub const OUTCOME_BLOCKED: &str = "blocked";
/// `failed` outcome.
pub const OUTCOME_FAILED: &str = "failed";

/// Tool name of [`AgentTaskOutcomeTool`].
pub const TOOL_SUBMIT_RESULT: &str = "task.submit_result";

/// A business outcome submitted by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskOutcome {
    /// One of the `OUTCOME_*` constants.
    pub outcome: String,
    /// Full deliverable or partial work.
    pub result: String,
    /// Reason for completion, waiting or inability.
    pub reason: String,
    /// Background task ids to collect, or delegated AgentTask ids for a team leader.
    pub pending_task_ids: Vec<String>,
}

impl AgentTaskOutcome {
    /// Validates and builds an outcome, mirroring the Java canonical constructor.
    pub fn new(
        outcome: impl Into<String>,
        result: impl Into<String>,
        reason: impl Into<String>,
        pending_task_ids: Vec<String>,
    ) -> std::result::Result<Self, String> {
        let outcome = outcome.into();
        if ![
            OUTCOME_SUCCEEDED,
            OUTCOME_WAITING,
            OUTCOME_BLOCKED,
            OUTCOME_FAILED,
        ]
        .contains(&outcome.as_str())
        {
            return Err("outcome must be succeeded, waiting, blocked or failed".into());
        }
        let result = result.into();
        let reason = reason.into();
        if outcome == OUTCOME_SUCCEEDED && result.trim().is_empty() {
            return Err("Successful work requires the actual deliverable in result".into());
        }
        if outcome != OUTCOME_SUCCEEDED && reason.trim().is_empty() {
            return Err("An unfinished outcome requires a concrete reason".into());
        }
        if outcome == OUTCOME_WAITING && pending_task_ids.is_empty() {
            return Err("Waiting requires actual pending task IDs".into());
        }
        Ok(Self {
            outcome,
            result,
            reason,
            pending_task_ids,
        })
    }
}

/// Per-dispatch storage for the submitted outcome, owned by the adapter.
///
/// Never stored on a shared tool instance: one dispatch gets one state.
#[derive(Debug, Default)]
pub struct AgentTaskOutcomeState {
    value: Mutex<Option<AgentTaskOutcome>>,
    terminal_committed: AtomicBool,
}

impl AgentTaskOutcomeState {
    /// Creates an empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a submitted outcome, replacing any previous one.
    pub fn submit(&self, outcome: AgentTaskOutcome) {
        *self.value.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome);
    }

    /// Takes the pending outcome, leaving the slot empty.
    pub fn take(&self) -> Option<AgentTaskOutcome> {
        self.value.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    /// Marks that a terminal run-node tool has committed the lifecycle.
    pub fn mark_terminal_committed(&self) {
        self.terminal_committed.store(true, Ordering::SeqCst);
    }

    /// Whether a terminal run-node tool has already committed the lifecycle.
    pub fn is_terminal_committed(&self) -> bool {
        self.terminal_committed.load(Ordering::SeqCst)
    }
}

/// Per-call credentials used by task-scoped collaboration tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskToolContext {
    /// Durable AgentTask id.
    pub task_id: String,
    /// Task-scoped token.
    pub task_token: String,
}

impl AgentTaskToolContext {
    /// Creates a tool context.
    pub fn new(task_id: impl Into<String>, task_token: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            task_token: task_token.into(),
        }
    }
}

/// Tool that captures the model's intended outcome for the current AgentTask.
///
/// Only the adapter may commit the physical lifecycle; submitting this tool records
/// intent into the dispatch-scoped [`AgentTaskOutcomeState`].
pub struct AgentTaskOutcomeTool {
    state: Arc<AgentTaskOutcomeState>,
}

impl AgentTaskOutcomeTool {
    /// Creates the tool bound to `state`.
    pub fn new(state: Arc<AgentTaskOutcomeState>) -> Self {
        Self { state }
    }

    /// The dispatch-scoped state this tool writes into.
    pub fn state(&self) -> &Arc<AgentTaskOutcomeState> {
        &self.state
    }
}

#[async_trait::async_trait]
impl Tool for AgentTaskOutcomeTool {
    fn name(&self) -> &str {
        TOOL_SUBMIT_RESULT
    }

    fn description(&self) -> &str {
        "Submit the actual outcome of this AgentTask, then end your turn. succeeded requires the \
         full deliverable in result, not a plan or promise. waiting requires real background task \
         IDs (or control-plane AgentTask IDs for a Team leader). If required evidence or tools are \
         missing, submit blocked with the missing capability and partial result. Submitting this \
         tool does not by itself mark the task successful."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "outcome": {
                    "type": "string",
                    "description": "succeeded, waiting, blocked or failed"
                },
                "result": {
                    "type": "string",
                    "description": "Full deliverable or partial work"
                },
                "reason": {
                    "type": "string",
                    "description": "Reason for completion, waiting or inability"
                },
                "pending_task_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Background task IDs to collect, or delegated AgentTask IDs for a Team leader"
                }
            },
            "required": ["outcome"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let outcome = args
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let result = args
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let pending: Vec<String> = args
            .get("pending_task_ids")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        match AgentTaskOutcome::new(outcome, result, reason, pending) {
            Ok(outcome) => {
                self.state.submit(outcome);
                Ok(ToolResultBlock::text(
                    "Outcome submitted for adapter validation. End this turn now.",
                ))
            }
            Err(message) => Ok(ToolResultBlock::error(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_rejects_unknown_value() {
        let err = AgentTaskOutcome::new("done", "x", "", vec![]).unwrap_err();
        assert!(err.contains("succeeded, waiting, blocked or failed"));
    }

    #[test]
    fn succeeded_requires_deliverable() {
        let err = AgentTaskOutcome::new(OUTCOME_SUCCEEDED, "  ", "", vec![]).unwrap_err();
        assert!(err.contains("deliverable"));
        assert!(AgentTaskOutcome::new(OUTCOME_SUCCEEDED, "artifacts/out.md", "", vec![]).is_ok());
    }

    #[test]
    fn unfinished_requires_reason() {
        let err = AgentTaskOutcome::new(OUTCOME_BLOCKED, "partial", "", vec![]).unwrap_err();
        assert!(err.contains("concrete reason"));
        assert!(AgentTaskOutcome::new(OUTCOME_BLOCKED, "partial", "tool missing", vec![]).is_ok());
        assert!(AgentTaskOutcome::new(OUTCOME_FAILED, "", "crashed", vec![]).is_ok());
    }

    #[test]
    fn waiting_requires_pending_ids() {
        let err = AgentTaskOutcome::new(OUTCOME_WAITING, "", "awaiting", vec![]).unwrap_err();
        assert!(err.contains("pending task IDs"));
        assert!(AgentTaskOutcome::new(OUTCOME_WAITING, "", "awaiting", vec!["t1".into()]).is_ok());
    }

    #[test]
    fn state_submit_and_take_drains() {
        let state = AgentTaskOutcomeState::new();
        assert!(state.take().is_none());
        state.submit(AgentTaskOutcome::new(OUTCOME_FAILED, "", "boom", vec![]).unwrap());
        let taken = state.take().unwrap();
        assert_eq!(taken.outcome, OUTCOME_FAILED);
        assert!(state.take().is_none());
    }

    #[test]
    fn terminal_committed_flag_round_trips() {
        let state = AgentTaskOutcomeState::new();
        assert!(!state.is_terminal_committed());
        state.mark_terminal_committed();
        assert!(state.is_terminal_committed());
    }

    #[tokio::test]
    async fn tool_records_valid_outcome() {
        let state = Arc::new(AgentTaskOutcomeState::new());
        let tool = AgentTaskOutcomeTool::new(Arc::clone(&state));
        assert_eq!(tool.name(), TOOL_SUBMIT_RESULT);
        let args = serde_json::json!({
            "outcome": "succeeded",
            "result": "done"
        })
        .as_object()
        .cloned()
        .unwrap();
        let block = tool
            .call(args, ToolExecutionContext::default())
            .await
            .unwrap();
        assert!(!block.output.is_empty());
        assert_eq!(state.take().unwrap().result, "done");
    }

    #[tokio::test]
    async fn tool_reports_invalid_outcome_without_recording() {
        let state = Arc::new(AgentTaskOutcomeState::new());
        let tool = AgentTaskOutcomeTool::new(Arc::clone(&state));
        let args = serde_json::json!({ "outcome": "succeeded", "result": "" })
            .as_object()
            .cloned()
            .unwrap();
        let _ = tool
            .call(args, ToolExecutionContext::default())
            .await
            .unwrap();
        assert!(state.take().is_none());
    }

    #[test]
    fn parameters_schema_requires_outcome() {
        let tool = AgentTaskOutcomeTool::new(Arc::new(AgentTaskOutcomeState::new()));
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"][0], "outcome");
        assert!(schema["properties"]["pending_task_ids"].is_object());
    }

    #[test]
    fn tool_context_holds_credentials() {
        let ctx = AgentTaskToolContext::new("t1", "tok");
        assert_eq!(ctx.task_id, "t1");
        assert_eq!(ctx.task_token, "tok");
    }
}
