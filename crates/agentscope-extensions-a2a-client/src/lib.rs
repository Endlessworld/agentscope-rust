//! A2A (Agent-to-Agent) client support, mirroring the Java
//! `agentscope-extensions-a2a-client` module.
//!
//! Sub-modules:
//! * [`model`] - wire data model (AgentCard, Message, Task, Part, JSON-RPC envelope).
//! * [`card`] - agent card resolvers (fixed / well-known URI).
//! * [`message`] - bidirectional conversion between [`agentscope_core::message::Msg`]
//!   and A2A messages/artifacts/parts.
//! * [`client`] - JSON-RPC client for calling remote A2A agents.
//! * [`agent`] - high-level remote agent facade ([`A2aAgent`]) mirroring Java `A2aAgent`.
//! * [`result`] - terminal-reply extraction rules shared by the facade and its callers.
//! * [`error`] - A2A-specific [`A2aError`] / [`A2aResult`].
//! * [`transport_properties`] - transport endpoint properties shared with the server crate.

pub mod agent;
pub mod card;
pub mod client;
pub mod error;
pub mod message;
pub mod model;
pub mod result;
pub mod transport_properties;

pub use agent::{A2aAgent, A2aAgentBuilder, A2aAgentConfig, A2aAgentConfigBuilder};
pub use card::{AgentCardResolver, FixedAgentCardResolver, WellKnownAgentCardResolver};
pub use error::{A2aError, A2aResult};
pub use message::{MessageConstants, MessageConvert};
pub use model::*;
pub use result::{status_update_result_message, task_result_message, TaskResultAccumulator};
pub use transport_properties::{
    format_host_for_url, TransportProperties, TransportPropertiesBuilder,
};
