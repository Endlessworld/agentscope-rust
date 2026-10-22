//! DingTalk (钉钉) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk`:
//! - [`DingTalkChannelProperties`] — provider configuration parsed from the channel properties map
//! - [`DingTalkInboundMapper`] — Stream/callback bot-message JSON → harness `InboundMessage`
//! - [`DingTalkOutboundClient`] — outbound request builders for DM / group sends
//! - [`DingTalkAccessTokenProvider`] — cached OpenAPI `accessToken` fetcher
//! - [`DingTalkChannel`] — the `Channel` lifecycle/routing adapter (`start`/`stop` selects the
//!   Stream client or the HTTP callback path)
//! - [`DingTalkStreamClient`] — persistent WebSocket gateway client (register → connect → ACK loop)
//! - [`DingTalkCallbackCrypto`] — HTTP callback signature verification + AES-CBC envelope
//! - [`DingTalkCallbackController`] — axum callback entry point
//!
//! ```text
//! stream mode:  DingTalkStreamClient ─┐
//! http mode:    DingTalkCallbackController ─┴→ DingTalkChannel::handle_stream_payload
//! ```

pub mod channel;
pub mod controller;
pub mod crypto;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod stream;
pub mod token;

pub use channel::{DingTalkChannel, DingTalkChannelRegistry};
pub use controller::{DingTalkCallbackController, DingTalkCallbackOutcome};
pub use crypto::DingTalkCallbackCrypto;
pub use inbound::DingTalkInboundMapper;
pub use outbound::DingTalkOutboundClient;
pub use properties::{DingTalkChannelProperties, MODE_HTTP, MODE_STREAM};
pub use stream::{
    ChannelPayloadSink, DingTalkStreamClient, DingTalkStreamProtocol, StreamEndpoint,
    StreamPayloadSink, StreamSocket, TungsteniteConnector, WebSocketConnector,
};
pub use token::DingTalkAccessTokenProvider;
