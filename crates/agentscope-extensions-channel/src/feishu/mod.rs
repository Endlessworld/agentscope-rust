//! Feishu (飞书 / Lark) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.feishu`:
//! - [`FeishuChannelProperties`] — provider configuration
//! - [`FeishuInboundMapper`] — decrypted Event Subscription v2 envelope → harness `InboundMessage`
//!   (text + image/file/folder/audio/media)
//! - [`FeishuOutboundClient`] — `POST /open-apis/im/v1/messages` request builders
//! - [`FeishuAccessTokenProvider`] — cached `tenant_access_token` fetcher
//! - [`FeishuCrypto`] — AES-256-CBC payload decryption + `X-Lark-Signature` verification
//! - [`FeishuChannel`] — lifecycle/routing adapter (`handle_callback` pipeline)
//! - [`controller::router`] — axum shell over `POST /api/channels/feishu/:channel_id/callback`

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
