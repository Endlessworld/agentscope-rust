//! A2A (Agent-to-Agent) protocol support, mirroring
//! `io.agentscope.core.a2a` in the Java SDK.
//!
//! Sub-modules:
//! * [`model`] - wire data model (AgentCard, Message, Task, Part, JSON-RPC envelope).
//! * [`card`] - agent card resolvers (fixed / well-known URI).
//! * [`message`] - bidirectional conversion between [`agentscope_core::message::Msg`]
//!   and A2A messages/artifacts/parts.
//! * [`client`] - JSON-RPC client for calling remote A2A agents.
//! * [`server`] - in-process/axum A2A request handler + task registry.

pub mod card;
pub mod client;
pub mod message;
pub mod model;
pub mod server;

pub use card::{AgentCardResolver, FixedAgentCardResolver, WellKnownAgentCardResolver};
pub use message::{MessageConstants, MessageConvert};
pub use model::*;
