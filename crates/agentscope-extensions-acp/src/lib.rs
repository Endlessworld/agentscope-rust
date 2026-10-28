//! ACP (Agent Client Protocol) CodingAgent integration.
//!
//! This module provides an ACP **agent** (server role) that wraps the core
//! [`ReActAgent`](agentscope_core::agent::ReActAgent) and communicates with an
//! ACP client (IDE/CLI) over stdio. It mirrors the Kotlin `AgiHarnessAgent`
//! and Java `HarnessCodingAgent` from the ai-agents project.
//!
//! Reference SDK: `agent-client-protocol` (path dependency, v2.0.0).

pub mod coding_agent;
pub mod config;
pub mod elicitation;
pub mod event;
mod event_queue;
pub mod ext;
pub mod mcp_over_acp;
pub mod permission_store;
pub mod provider;
pub mod session;
pub mod support;

pub use coding_agent::{
    AcpCodingAgent, AcpCodingAgentBuilder, SessionBuild, ACP_CONTEXT_WINDOW_TOKENS,
};
pub use elicitation::{AskUserTool, ElicitationClient, ElicitationSlot};
pub use mcp_over_acp::{McpAcpConnection, McpAcpTool};
pub use provider::{ProviderDescriptor, ProviderRegistry};
pub use session::AcpAgentSession;
pub use support::ClientSupport;
