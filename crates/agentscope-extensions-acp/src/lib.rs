//! ACP (Agent Client Protocol) CodingAgent integration.
//!
//! This module provides an ACP **agent** (server role) that wraps the core
//! [`ReActAgent`](agentscope_core::agent::ReActAgent) and communicates with an
//! ACP client (IDE/CLI) over stdio. It mirrors the Kotlin `AgiHarnessAgent`
//! and Java `HarnessCodingAgent` from the ai-agents project.
//!
//! Reference SDK: `agent-client-protocol` 2.2 (crates.io; schema 1.9, protocol v1).

pub mod coding_agent;
pub mod config;
pub mod elicitation;
pub mod event;
mod event_queue;
pub mod ext;
pub mod mcp_over_acp;
pub mod mem;
pub mod permission_store;
pub mod provider;
pub mod recording;
pub mod retention;
pub mod session;
pub mod setup;
pub mod support;

pub use coding_agent::{
    compaction_trigger_for, compaction_window_tokens, release_catalog_if_unused, AcpCodingAgent,
    AcpCodingAgentBuilder, ImageSupport, SessionBuild, ACP_CONTEXT_WINDOW_TOKENS,
};
pub use elicitation::{AskUserTool, ElicitationClient, ElicitationSlot};
pub use mcp_over_acp::{McpAcpConnection, McpAcpTool};
pub use provider::{ProviderDescriptor, ProviderRegistry};
pub use session::AcpAgentSession;
pub use support::ClientSupport;
