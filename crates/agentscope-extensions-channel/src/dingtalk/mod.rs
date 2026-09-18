//! DingTalk (钉钉) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk`:
//! - [`DingTalkChannelProperties`] — provider configuration parsed from the channel properties map
//! - [`DingTalkInboundMapper`] — Stream bot-message JSON → harness `InboundMessage`
//! - [`DingTalkOutboundClient`] — outbound request builders for DM / group sends
//! - [`DingTalkAccessTokenProvider`] — cached OpenAPI `accessToken` fetcher
//! - [`DingTalkChannel`] — the `Channel` lifecycle/routing adapter
//! - [`DingTalkStreamClient`] — persistent WebSocket gateway client (register → connect → ACK loop)

pub mod channel;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod stream;
pub mod token;

pub use channel::{DingTalkChannel, DingTalkChannelRegistry};
pub use stream::{
    ChannelPayloadSink, DingTalkStreamClient, DingTalkStreamProtocol, StreamEndpoint,
    StreamPayloadSink, StreamSocket, TungsteniteConnector, WebSocketConnector,
};
pub use inbound::DingTalkInboundMapper;
pub use outbound::DingTalkOutboundClient;
pub use properties::DingTalkChannelProperties;
pub use token::DingTalkAccessTokenProvider;
