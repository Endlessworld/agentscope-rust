//! WeCom (企业微信) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.wecom`:
//! - [`WeComChannelProperties`] — provider configuration
//! - [`WeComInboundMapper`] — decrypted callback XML → harness `InboundMessage`
//! - [`WeComOutboundClient`] — `/cgi-bin/message/send` + `/cgi-bin/appchat/send` request builders
//! - [`WeComSignatureVerifier`] — callback SHA-1 signature verification
//! - [`WeComAccessTokenProvider`] — cached `access_token` fetcher
//!
//! Not yet ported (see `docs/extensions/CHANNEL__GAP_TRACKER.md` §3): the AES-256-CBC payload
//! decrypt half of `WeComCrypto` (needs `aes`/`cbc`), `WeComCallbackController` and `WeComChannel`.

pub mod crypto;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod signature;
pub mod token;

pub use crypto::WeComCrypto;
pub use inbound::WeComInboundMapper;
pub use outbound::WeComOutboundClient;
pub use properties::WeComChannelProperties;
pub use signature::WeComSignatureVerifier;
pub use token::WeComAccessTokenProvider;
