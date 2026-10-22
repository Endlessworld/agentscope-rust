//! A2A (Agent-to-Agent) client support, mirroring the Java
//! `agentscope-extensions-a2a-client` module.
//!
//! Sub-modules:
//! * [`model`] - wire data model (AgentCard, Message, Task, Part, JSON-RPC envelope).
//! * [`card`] - agent card resolvers (fixed / well-known URI).
//! * [`message`] - bidirectional conversion between [`agentscope_core::message::Msg`]
//!   and A2A messages/artifacts/parts.
//! * [`client`] - JSON-RPC client for calling remote A2A agents.
//! * [`error`] - A2A-specific [`A2aError`] / [`A2aResult`].

pub mod card;
pub mod client;
pub mod error;
pub mod message;
pub mod model;

pub use card::{AgentCardResolver, FixedAgentCardResolver, WellKnownAgentCardResolver};
pub use error::{A2aError, A2aResult};
pub use message::{MessageConstants, MessageConvert};
pub use model::*;
