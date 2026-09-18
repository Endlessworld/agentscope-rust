//! Shared channel-adapter building blocks.
//!
//! Mirrors `io.agentscope.extensions.channel.common` plus the small pieces that every platform
//! adapter would otherwise duplicate (outbound-address parsing, HTTP plumbing, token caching).

pub mod bot_loop_guard;
pub mod crypto;
pub mod error;
pub mod http;
pub mod idempotency_store;
pub mod outbound_address;
pub mod props;
pub mod text;
pub mod token;
pub mod xml;

#[cfg(test)]
pub mod testing;

pub use bot_loop_guard::{BotLoopGuard, BotLoopGuardConfig};
pub use crypto::{aes_256_cbc_decrypt, pkcs7_unpad, AES_BLOCK_SIZE, AES_KEY_SIZE};
pub use error::{ChannelError, Result};
pub use http::{HttpMethod, HttpTransport, PreparedRequest, PreparedResponse, ReqwestTransport};
pub use idempotency_store::IdempotencyStore;
pub use outbound_address::{parse_peer_target, PeerTarget};
pub use props::{get_int, get_str, get_str_or, object_from_btree};
pub use text::{constant_time_eq, looks_like_markdown, to_hex};
pub use token::{now_ms, refresh_at_ms, AccessTokenProvider, TokenCache};
pub use xml::xml_element_text;
