//! Built-in `task_requirement_propose` tool, mirroring
//! `io.agentscope.core.tool.builtin.RequirementTools#proposeRequirement`.
//!
//! Opt-in: the host registers the tool itself, so a session that never asked for requirement
//! tracking does not grow a model-callable surface. A proposal records a candidate only; it
//! cannot confirm authority, rewrite confirmed text, or mark criteria satisfied.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::Result;
use crate::message::{JsonMap, ToolResultBlock};
use crate::state::{AgentState, RequirementKind, RequirementStatus};

use super::tool::{Tool, ToolExecutionContext};

const DESCRIPTION: &str = "Propose a candidate CONSTRAINT or ACCEPTANCE_CRITERION for this \
session. Include a source reference (such as a user message ID or PLAN.md section). References \
are unverified claims. This tool cannot confirm requirements, change confirmed text, or mark \
criteria satisfied. Do not treat candidates as binding.";

const KIND_REQUIRED: &str = "Requirement kind is required";
const KIND_INVALID: &str = "Requirement kind must be CONSTRAINT or ACCEPTANCE_CRITERION";

/// Records one candidate requirement on the session task context.
///
/// Mirrors the Java `stateInjected = true` tool: the live [`AgentState`] is injected by the host,
/// and a host with no state to inject leaves it `None`, which mirrors Java's `state == null`
/// error result.
pub struct RequirementProposeTool {
    state: Option<Arc<Mutex<AgentState>>>,
}

impl RequirementProposeTool {
    /// Builds the tool over the agent state it proposes into.
    ///
    /// `None` means the host has no state to inject; the tool then refuses every call instead
    /// of inventing a detached task context the agent loop would never see.
    pub fn new(state: Option<Arc<Mutex<AgentState>>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for RequirementProposeTool {
    fn name(&self) -> &str {
        "task_requirement_propose"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["CONSTRAINT", "ACCEPTANCE_CRITERION"],
                    "description": "CONSTRAINT or ACCEPTANCE_CRITERION"
                },
                "text": {
                    "type": "string",
                    "description": "Exact proposed requirement, at most 2000 characters"
                },
                "source_ref": {
                    "type": "string",
                    "description": "Where this proposal came from"
                }
            },
            "required": ["kind", "text", "source_ref"]
        })
    }

    /// Proposals mutate the task context, so they must not run concurrently with each other.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(state) = self.state.as_ref() else {
            return Ok(ToolResultBlock::error("Agent state unavailable"));
        };
        let kind = match args.get("kind").and_then(|value| value.as_str()) {
            None | Some("") => return Ok(ToolResultBlock::error(KIND_REQUIRED)),
            Some(raw) => match serde_json::from_value::<RequirementKind>(serde_json::json!(raw)) {
                Ok(kind) => kind,
                Err(_) => return Ok(ToolResultBlock::error(KIND_INVALID)),
            },
        };
        let text = args.get("text").and_then(|value| value.as_str()).unwrap_or("");
        let source_ref = args
            .get("source_ref")
            .and_then(|value| value.as_str())
            .unwrap_or("");

        let mut guard = state.lock().unwrap();
        match guard.tasks_context.propose(kind, text, source_ref) {
            Ok(id) => Ok(ToolResultBlock::text(format!(
                "Requirement {id} recorded; status={}. Use current TASK_STATE for requirements. \
                 This receipt does not confirm authority or verification.",
                RequirementStatus::Candidate
            ))),
            Err(error) => Ok(ToolResultBlock::error(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolResultState;
    use crate::state::TaskRequirement;

    fn tool_with_state() -> (RequirementProposeTool, Arc<Mutex<AgentState>>) {
        let state = Arc::new(Mutex::new(AgentState::default()));
        (RequirementProposeTool::new(Some(Arc::clone(&state))), state)
    }

    fn call(tool: &RequirementProposeTool, kind: &str) -> ToolResultBlock {
        let args: JsonMap = serde_json::from_value(serde_json::json!({
            "kind": kind,
            "text": "The report must stay under 2000 words",
            "source_ref": "user-message-7"
        }))
        .unwrap();
        futures::executor::block_on(tool.call(args, ToolExecutionContext::default())).unwrap()
    }

    #[test]
    fn a_proposal_becomes_a_candidate_requirement() {
        let (tool, state) = tool_with_state();

        let result = call(&tool, "CONSTRAINT");

        assert_eq!(result.final_state(), ToolResultState::Success);
        let receipt = result.text_content();
        assert!(receipt.contains("status=CANDIDATE"), "receipt was {receipt}");
        let state = state.lock().unwrap();
        let requirements: &[TaskRequirement] = state.tasks_context.requirements();
        assert_eq!(requirements.len(), 1);
        assert_eq!(requirements[0].kind, RequirementKind::Constraint);
        assert_eq!(requirements[0].status, RequirementStatus::Candidate);
        assert_eq!(requirements[0].proposed_source_ref, "user-message-7");
    }

    #[test]
    fn an_invalid_kind_is_an_error_result() {
        let (tool, state) = tool_with_state();

        let result = call(&tool, "MUST_HAVE");

        assert_eq!(result.final_state(), ToolResultState::Error);
        assert!(result.text_content().contains(KIND_INVALID));
        assert!(state.lock().unwrap().tasks_context.requirements().is_empty());
    }

    #[test]
    fn a_missing_kind_is_an_error_result() {
        let (tool, _) = tool_with_state();

        let result = call(&tool, "");

        assert_eq!(result.final_state(), ToolResultState::Error);
        assert!(result.text_content().contains(KIND_REQUIRED));
    }

    #[test]
    fn a_missing_state_is_an_error_result() {
        let tool = RequirementProposeTool::new(None);

        let result = call(&tool, "ACCEPTANCE_CRITERION");

        assert_eq!(result.final_state(), ToolResultState::Error);
        assert!(result.text_content().contains("Agent state unavailable"));
    }

    #[test]
    fn a_blank_requirement_text_is_rejected_without_touching_the_state() {
        let (tool, state) = tool_with_state();
        let args: JsonMap = serde_json::from_value(serde_json::json!({
            "kind": "CONSTRAINT",
            "text": "   ",
            "source_ref": "user-message-7"
        }))
        .unwrap();

        let result =
            futures::executor::block_on(tool.call(args, ToolExecutionContext::default())).unwrap();

        assert_eq!(result.final_state(), ToolResultState::Error);
        assert!(state.lock().unwrap().tasks_context.requirements().is_empty());
    }
}
