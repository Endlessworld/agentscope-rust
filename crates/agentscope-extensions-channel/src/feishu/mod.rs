//! Feishu (飞书 / Lark) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.feishu`:
//! - [`FeishuChannelProperties`] — provider configuration
//! - [`FeishuInboundMapper`] — decrypted Event Subscription v2 envelope → harness `InboundMessage`
//! - [`FeishuOutboundClient`] — `POST /open-apis/im/v1/messages` request builders
//! - [`FeishuAccessTokenProvider`] — cached `tenant_access_token` fetcher
//!
//! Not yet ported (see `docs/extensions/CHANNEL__GAP_TRACKER.md` §3): `FeishuCallbackController`
//! (axum shell over [`FeishuChannel::handle_callback`]).

pub mod channel;
pub mod controller;
pub mod crypto;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod token;

pub use channel::{FeishuCallbackOutcome, FeishuChannel, FeishuChannelRegistry};
pub use crypto::FeishuCrypto;
pub use inbound::FeishuInboundMapper;
pub use outbound::FeishuOutboundClient;
pub use properties::FeishuChannelProperties;
pub use token::FeishuAccessTokenProvider;
