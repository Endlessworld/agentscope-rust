//! Channel routing and Chat UI adapter, mirroring
//! `io.agentscope.harness.agent.gateway.channel`.
//!
//! - `Peer` / `PeerKind` — conversation peer identity
//! - `DmScope` — how DM sessions are keyed
//! - `InboundMessage` — normalized inbound message before routing
//! - `ChannelBinding` / `ChannelConfig` — routing rules
//! - `ChannelRouter` / `RouteResult` — 7-tier binding evaluation
//! - `ChatUiChannel` — programmatic chat channel (no external transport)

pub mod binding;
#[allow(clippy::module_inception)]
pub mod channel;
pub mod chatui;
pub mod config;
pub mod dm_scope;
pub mod inbound;
pub mod peer;
pub mod router;

pub use binding::ChannelBinding;
pub use channel::Channel;
pub use chatui::{ChatUiChannel, ChatUiRequest, OutboundEnvelope, SendOptions};
pub use config::ChannelConfig;
pub use dm_scope::DmScope;
pub use inbound::InboundMessage;
pub use peer::{Peer, PeerKind};
pub use router::{ChannelRouter, RouteResult};
