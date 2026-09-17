//! Remote subagent wire protocol, mirroring
//! `io.agentscope.harness.agent.subagent.protocol`.
//!
//! `RemoteAgentEvent` / `RemoteEventType` define a stable, forward-compatible wire
//! format for streaming a subagent's events across the Agent Protocol; `RemoteEventCodec`
//! maps between those DTOs and the internal `AgentEvent`s.

pub mod codec;
pub mod event;

pub use codec::{
    from_agent_event, matches_detail, matches_detail_event, to_agent_event, METADATA_TASK_ID,
};
pub use event::{
    RemoteAgentEvent, RemoteConfirmDecision, RemoteEventType, RemotePendingConfirm,
    RemoteStreamDetail,
};
