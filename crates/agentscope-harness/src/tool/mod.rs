//! Harness agent tools, mirroring `io.agentscope.harness.agent.tool`.
//! Registers filesystem and shell-execution tools into the core `Toolkit`.

use agentscope_core::error::Result;
use agentscope_core::message::JsonMap;

pub mod agent_generate;
pub mod agent_spawn;
pub mod artifact_delivery;
pub mod filesystem_tools;
pub mod fuzzy_text_matcher;
pub mod grep_options;
pub mod memory_get;
pub mod memory_save;
pub mod memory_search;
pub mod plan_mode;
pub mod session_search;
pub mod shell_execute_tool;
pub mod task_tool;
pub mod team;
pub mod wait_async;
pub mod web_tools;

pub use agent_generate::AgentGenerateTool;
pub use agent_spawn::{AgentListTool, AgentSendTool, AgentSpawnTool};
pub use artifact_delivery::ArtifactDeliveryTool;
pub use filesystem_tools::FilesystemTool;
pub use fuzzy_text_matcher::{search, Level, MatchRange, SearchResult};
pub use memory_get::MemoryGetTool;
pub use memory_save::MemorySaveTool;
pub use memory_search::MemorySearchTool;
pub use plan_mode::{PlanEnterTool, PlanExitTool, PlanModeHandle, PlanWriteTool};
pub use session_search::{SessionHistoryTool, SessionListTool, SessionSearchTool};
pub use shell_execute_tool::{command_with_working_directory, ShellExecuteTool};
pub use task_tool::{TaskCancelTool, TaskListTool, TaskOutputTool, TaskTool};
pub use team::TeamTool;
pub use wait_async::WaitAsyncResultsTool;
pub use web_tools::{register_web_tools, WebFetchTool, WebSearchTool};

/// Extracts a required string argument.
pub(crate) fn str_arg(args: &JsonMap, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| agentscope_core::Error::Tool(format!("Missing string argument: {key}")))
}

/// Extracts an optional string argument.
pub(crate) fn opt_str_arg(args: &JsonMap, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extracts an optional unsigned integer argument.
pub(crate) fn int_arg(args: &JsonMap, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|u| u as usize)
}
