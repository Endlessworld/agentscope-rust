//! WeCom (企业微信) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.wecom`:
//! - [`WeComChannel`] — the `Channel` lifecycle/routing adapter (inbound pipeline + outbound reply)
//! - [`WeComCallbackOutcome`] / [`controller::router`] — the GET URL handshake + POST callback
//!   axum entry point
//! - [`WeComChannelProperties`] — provider configuration
//! - [`WeComInboundMapper`] — decrypted callback XML → harness `InboundMessage`
//! - [`WeComOutboundClient`] — `/cgi-bin/message/send` + `/cgi-bin/appchat/send` request builders
//! - [`WeComCrypto`] — callback SHA-1 signature verification + AES-256-CBC decrypt/encrypt
//! - [`WeComAccessTokenProvider`] — cached `access_token` fetcher
//!
//! All of `WeComChannel`, `WeComCallbackController` and the AES-CBC half of `WeComCrypto` are now
//! ported; see `docs/extensions/CHANNEL__GAP_TRACKER.md` §3 for what remains.

pub mod channel;
pub mod controller;
pub mod crypto;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod signature;
pub mod token;

pub use channel::{WeComCallbackOutcome, WeComChannel, WeComChannelRegistry, CHANNEL_TYPE};
pub use crypto::WeComCrypto;
pub use inbound::WeComInboundMapper;
pub use outbound::WeComOutboundClient;
pub use properties::WeComChannelProperties;
pub use signature::WeComSignatureVerifier;
pub use token::WeComAccessTokenProvider;
