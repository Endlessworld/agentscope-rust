//! DingTalk (钉钉) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk`:
//! - [`DingTalkChannelProperties`] — provider configuration parsed from the channel properties map
//! - [`DingTalkInboundMapper`] — Stream bot-message JSON → harness `InboundMessage`
//! - [`DingTalkOutboundClient`] — outbound request builders for DM / group sends
//! - [`DingTalkAccessTokenProvider`] — cached OpenAPI `accessToken` fetcher
//!
//! Not yet ported (see `docs/extensions/CHANNEL__GAP_TRACKER.md` §3): `DingTalkStreamClient`
//! (persistent WebSocket gateway) and `DingTalkChannel` (the `Channel` lifecycle/routing adapter).

pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod token;

pub use inbound::DingTalkInboundMapper;
pub use outbound::DingTalkOutboundClient;
pub use properties::DingTalkChannelProperties;
pub use token::DingTalkAccessTokenProvider;
