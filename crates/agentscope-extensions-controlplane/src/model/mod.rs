//! Level-2/3/4 reporting models, mirroring `io.agentscope.extensions.controlplane.model`.

pub mod agent_task;
pub mod context_snapshot;
pub mod context_tracker;
pub mod inventory;
pub mod message_page;
pub mod session_event;

pub use agent_task::AgentTaskAssignment;
pub use context_snapshot::{ContextMessage, ContextSnapshot, ContextSnapshotBuilder, ToolInfo, HASH_LEN};
pub use context_tracker::ContextTracker;
pub use inventory::{InstanceHealth, Inventory, SubagentInfo, WorkspaceInfo};
pub use message_page::{MessageItem, MessagePage};
pub use session_event::{
    SessionEvent, SessionEventBuilder, ROLE_ASSISTANT, ROLE_SYSTEM, ROLE_TOOL, ROLE_USER,
    TYPE_COMPACTION, TYPE_MESSAGE, TYPE_SESSION_END, TYPE_SESSION_START, TYPE_TOOL_CALL,
    TYPE_TOOL_RESULT,
};
