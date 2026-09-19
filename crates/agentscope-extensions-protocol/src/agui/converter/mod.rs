//! Conversion between AG-UI messages/tools/state and AgentScope types, mirroring
//! `io.agentscope.core.agui.converter`.
//!
//! * [`AguiMessageConverter`] - `Msg` <-> `AguiMessage`, including resume-entry handling.
//! * [`AguiStateConverter`] - state snapshots and RFC 6902 deltas.
//! * [`AguiToolConverter`] - `AguiTool` <-> core `ToolSchema`.

pub mod message;
pub mod state;
pub mod tool;

pub use message::{AguiMessageConverter, METADATA_INTERRUPT_ID, METADATA_RESUME_STATUS};
pub use state::AguiStateConverter;
pub use tool::AguiToolConverter;
