//! Background subagent task repository, mirroring
//! `io.agentscope.harness.agent.subagent.task`.

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

/// Current wall-clock time in milliseconds since the Unix epoch.
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
