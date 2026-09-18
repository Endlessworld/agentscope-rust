//! agentscope-extensions-protocol - see docs/extensions/PROTOCOL__GAP_TRACKER.md
//!
//! Protocol extension for AgentScope Rust, mirroring the Java
//! `agentscope-extensions-protocol` module:
//!
//! * [`a2a`] - A2A (Agent-to-Agent) wire model, agent card resolution,
//!   `Msg` <-> A2A conversion, JSON-RPC client and axum server.
//! * [`agent_protocol`] - the "agent protocol" task lifecycle HTTP+SSE API
//!   (`/api/tasks/...`), task store, event bus and runtime context customizers.
//! * [`agui`] - the AG-UI event taxonomy and message/content model.
//! * [`error`] - shared [`ProtocolError`] / [`ProtocolResult`].

pub mod a2a;
pub mod agent_protocol;
pub mod agui;
pub mod chat_completions;
pub mod error;

pub use error::{ProtocolError, ProtocolResult};
