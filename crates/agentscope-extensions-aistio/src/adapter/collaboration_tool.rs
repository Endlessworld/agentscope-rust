//! Runtime-bound proxy for one canonical control-plane collaboration MCP tool,
//! mirroring `io.agentscope.extensions.aistio.adapter.AgentTaskCollaborationTool`.
//!
//! One instance is built per dispatch, bound to that dispatch's
//! [`AgentTaskToolContext`]. Calls are forwarded to the control-plane MCP
//! (`CollaborationClient::call_tool`); the tool never touches task state directly.

use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use crate::adapter::agent_task::{AgentTaskOutcomeState, AgentTaskToolContext};
use crate::transport::CollaborationClient;

/// Read-only collaboration tool names (they perform no observable side effects).
pub const READ_ONLY_TOOLS: &[&str] = &[
    "issue.get",
    "issue.comment.list",
    "artifact.download",
    "task.get",
    "team.get",
    "run.get",
    "run.graph",
    "run.artifacts",
];

/// Tool names that commit the AgentTask lifecycle.
pub const TERMINAL_TOOLS: &[&str] = &["run.node.complete", "run.node.fail"];

/// Runtime-context-bound proxy for one control-plane collaboration tool.
#[derive(Clone)]
pub struct AgentTaskCollaborationTool {
    collaboration: CollaborationClient,
    context: AgentTaskToolContext,
    /// When present, terminal tool calls mark the lifecycle as committed.
    outcome_state: Option<Arc<AgentTaskOutcomeState>>,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

impl AgentTaskCollaborationTool {
    /// Builds a proxy for `name`/`description`/`parameters`, bound to `context`.
    pub fn new(
        collaboration: CollaborationClient,
        context: AgentTaskToolContext,
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            collaboration,
            context,
            outcome_state: None,
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// Installs the dispatch outcome state so terminal calls flip the committed flag.
    pub fn with_outcome_state(mut self, state: Arc<AgentTaskOutcomeState>) -> Self {
        self.outcome_state = Some(state);
        self
    }

    /// The AgentTask credentials this proxy is bound to.
    pub fn context(&self) -> &AgentTaskToolContext {
        &self.context
    }
}

#[async_trait::async_trait]
impl Tool for AgentTaskCollaborationTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn is_read_only(&self) -> bool {
        READ_ONLY_TOOLS.contains(&self.name.as_str())
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let arguments = Some(serde_json::Value::Object(args.into_iter().collect()));
        let tool_call_id = ctx.tool_call.as_ref().map(|t| t.id.as_str());
        match self
            .collaboration
            .call_tool(
                &self.context.task_id,
                &self.context.task_token,
                &self.name,
                arguments,
                tool_call_id,
            )
            .await
        {
            Ok(result) => {
                if TERMINAL_TOOLS.contains(&self.name.as_str()) {
                    if let Some(state) = &self.outcome_state {
                        state.mark_terminal_committed();
                    }
                }
                Ok(ToolResultBlock::text(result.to_string()))
            }
            Err(e) => Ok(ToolResultBlock::error(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::ControlPlaneHttpClient;

    fn tool(name: &str) -> AgentTaskCollaborationTool {
        let http = Arc::new(
            ControlPlaneHttpClient::new("http://127.0.0.1:1", "tok").unwrap(),
        );
        AgentTaskCollaborationTool::new(
            CollaborationClient::new(http),
            AgentTaskToolContext::new("task-1", "secret"),
            name,
            "desc",
            serde_json::json!({ "type": "object", "properties": {} }),
        )
    }

    #[test]
    fn read_only_allowlist_matches_java() {
        assert!(tool("issue.get").is_read_only());
        assert!(tool("run.artifacts").is_read_only());
        assert!(tool("team.get").is_read_only());
        assert!(!tool("issue.comment.add").is_read_only());
        assert!(!tool("run.node.complete").is_read_only());
    }

    #[test]
    fn schema_and_identity_are_forwarded() {
        let t = tool("issue.get");
        assert_eq!(t.name(), "issue.get");
        assert_eq!(t.description(), "desc");
        assert_eq!(t.parameters_schema()["type"], "object");
        assert_eq!(t.context().task_id, "task-1");
    }

    #[test]
    fn terminal_tools_listing_matches_java() {
        assert!(TERMINAL_TOOLS.contains(&"run.node.complete"));
        assert!(TERMINAL_TOOLS.contains(&"run.node.fail"));
        assert!(!TERMINAL_TOOLS.contains(&"run.get"));
    }

    #[tokio::test]
    async fn unreachable_control_plane_is_reported_not_panicked() {
        // Bypass principle: a failed report must not panic the agent path.
        let t = tool("issue.get");
        let block = t.call(JsonMap::new(), ToolExecutionContext::default()).await;
        assert!(block.is_ok());
    }
}
