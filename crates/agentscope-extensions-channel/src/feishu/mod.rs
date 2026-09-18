//! Feishu (飞书 / Lark) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.feishu`:
//! - [`FeishuChannelProperties`] — provider configuration
//! - [`FeishuInboundMapper`] — decrypted Event Subscription v2 envelope → harness `InboundMessage`
//! - [`FeishuOutboundClient`] — `POST /open-apis/im/v1/messages` request builders
//! - [`FeishuAccessTokenProvider`] — cached `tenant_access_token` fetcher
//!
//! Not yet ported (see `docs/extensions/CHANNEL__GAP_TRACKER.md` §3): `FeishuCrypto` (AES-256-CBC
//! payload decrypt), `FeishuCallbackController` (needs a web framework) and `FeishuChannel`.

pub mod crypto;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod token;

pub use crypto::FeishuCrypto;
pub use inbound::FeishuInboundMapper;
pub use outbound::FeishuOutboundClient;
pub use properties::FeishuChannelProperties;
pub use token::FeishuAccessTokenProvider;
