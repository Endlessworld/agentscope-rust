//! Framework adapter SPI + platform adapters, mirroring
//! `io.agentscope.extensions.controlplane.adapter`.

mod agent_scope_adapter;
mod agent_task;
mod collaboration_tool;
mod framework_adapter;
mod observer_middleware;
mod support;
mod task_starter;
mod workspace_factory;

pub use agent_scope_adapter::{AgentScopeAdapter, FRAMEWORK};
pub use agent_task::{
    AgentTaskOutcome, AgentTaskOutcomeState, AgentTaskOutcomeTool, AgentTaskToolContext,
    OUTCOME_BLOCKED, OUTCOME_FAILED, OUTCOME_SUCCEEDED, OUTCOME_WAITING, TOOL_SUBMIT_RESULT,
};
pub use collaboration_tool::{AgentTaskCollaborationTool, READ_ONLY_TOOLS, TERMINAL_TOOLS};
pub use framework_adapter::{
    CommandOutcome, FrameworkAdapter, CAP_AGENT_TASK, CAP_CONTEXT_QUERY, CAP_CONVERSATION_INBOUND,
    CAP_EXPORT_TRANSCRIPT, CAP_MESSAGE_QUERY, CAP_PLAN_MODE, CAP_SESSION_ABORT,
    CAP_SESSION_COMMAND, CAP_SUBAGENT_INVENTORY, CAP_SUBAGENT_TASK_COMMAND,
    CAP_SUBAGENT_TASK_QUERY, CAP_TASK_QUERY, CAP_WORKSPACE_INVENTORY, COMMAND_ABORT,
    COMMAND_COMPRESS, COMMAND_TERMINATE,
};
pub(crate) use observer_middleware::AdapterShared;
pub use observer_middleware::ControlPlaneObserverMiddleware;
pub use support::{AgentRuntimeSource, AgentTaskStarter, SessionCompactor, SessionHistorySource};
pub use task_starter::{
    available_actions, cancel_unfinished, input_ids, kickoff_prompt, model_context,
    role_instructions, run_agent_turn, run_to_outcome, safe_payload, strip_credentials,
    HarnessAgentTaskStarter, HarnessTurnRunner, PendingTask, StarterLimits, TurnRunner,
};
pub use workspace_factory::{
    apply_tool_selection, materialize, permissions, skill_filter, tool_name, tools,
    HarnessWorkspaceAgentFactory, McpSelection, ModelResolver, ToolsSelection,
};
