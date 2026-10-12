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
pub mod event;
pub mod permission_store;
pub mod session;

pub use coding_agent::{AcpCodingAgent, AcpCodingAgentBuilder, ACP_CONTEXT_WINDOW_TOKENS};
pub use session::AcpAgentSession;
