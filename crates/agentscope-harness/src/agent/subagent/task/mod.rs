//! Background subagent task repository, mirroring
//! `io.agentscope.harness.agent.subagent.task`.

pub mod background;
pub mod delivery;
pub mod record;
pub mod repository;
pub mod spec;
pub mod status;
pub mod workspace;

pub use background::BackgroundTask;
pub use delivery::TaskDelivery;
pub use record::TaskRecord;
pub use repository::{TaskCompletionCallback, TaskRepository};
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
