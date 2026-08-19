//! Live message transport for coordinating work across sessions and processes,
//! mirroring `io.agentscope.harness.agent.bus`.
//!
//! - `MessageBus` / `WorkspaceMessageBus` — drain queues, replay logs, degraded
//!   pub/sub, plus domain helpers (inbox, wakeups, session events).
//! - `AsyncToolRecord` / `AsyncToolRegistry` / `WorkspaceAsyncToolRegistry` —
//!   lifecycle tracking for async tool executions.

pub mod async_tool;
pub mod message_bus;
pub mod workspace_message_bus;

pub use async_tool::{AsyncToolRecord, AsyncToolRegistry, WorkspaceAsyncToolRegistry};
pub use message_bus::{
    BusEntry, MessageBus, INBOX_KEY_PREFIX, SESSION_EVENTS_KEY_PREFIX, SESSION_REPLAY_MAX_LEN,
    WAKEUPS_KEY, WAKEUP_SIGNAL_KEY,
};
pub use workspace_message_bus::WorkspaceMessageBus;
