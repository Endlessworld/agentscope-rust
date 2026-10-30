//! Background subagent task repository, mirroring
//! `io.agentscope.harness.agent.subagent.task`.

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::JsonMap;

pub mod background;
pub mod delivery;
pub mod protocol_client;
pub mod record;
pub mod repository;
pub mod spec;
pub mod status;
pub mod workspace;

pub use background::BackgroundTask;
pub use delivery::TaskDelivery;
pub use protocol_client::{
    AgentProtocolTaskClient, AgentProtocolTransport, EventStreamHandle, RemoteSubagentTransport,
    RemoteSubmitContext, RemoteSubmitContextBuilder, RemoteTarget, RemoteTaskStatus,
};
pub use record::TaskRecord;
pub use repository::{TaskCompletionCallback, TaskRepository, SUPPRESS_COMPLETION_CALLBACK};
pub use spec::TaskRunSpec;
pub use status::TaskStatus;
pub use workspace::WorkspaceTaskRepository;

/// Session key used for task records when the caller carries no session id.
///
/// Task submission (`agent_spawn` / `agent_send` / the task tools) and terminal-task
/// delivery (the subagent middlewares) must derive the key the same way, otherwise a task
/// submitted under the fallback is queried under another key and its result is never
/// delivered back.
pub const TASK_FALLBACK_SESSION: &str = "default";

/// Task session key of a runtime context, falling back to [`TASK_FALLBACK_SESSION`].
pub fn task_session_key(ctx: &RuntimeContext) -> String {
    ctx.session_id()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(TASK_FALLBACK_SESSION)
        .to_string()
}

/// Task session key from a tool call's runtime map (the `sessionId` key), falling back
/// to [`TASK_FALLBACK_SESSION`].
pub fn task_session_key_from_runtime(runtime: &JsonMap) -> String {
    runtime
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(TASK_FALLBACK_SESSION)
        .to_string()
}

/// Current wall-clock time in milliseconds since the Unix epoch.
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
