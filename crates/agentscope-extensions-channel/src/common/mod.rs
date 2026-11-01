//! Shared channel-adapter building blocks.
//!
//! Mirrors `io.agentscope.extensions.channel.common` plus the small pieces that every platform
//! adapter would otherwise duplicate (outbound-address parsing, HTTP plumbing, token caching).

pub mod bot_loop_guard;
pub mod channel_core;
pub mod channel_registry;
pub mod crypto;
pub mod error;
pub mod http;
pub mod idempotency_store;
pub mod media;
pub mod outbound_address;
pub mod props;
pub mod text;
pub mod token;
pub mod xml;

#[cfg(test)]
pub mod testing;

pub use bot_loop_guard::{BotLoopGuard, BotLoopGuardConfig};
pub use channel_core::{to_core_error, ChannelCore, ChannelSeams};
pub use channel_registry::ChannelRegistry;
pub use crypto::{aes_256_cbc_decrypt, pkcs7_unpad, AES_BLOCK_SIZE, AES_KEY_SIZE};
pub use error::{ChannelError, Result};
pub use http::{
    ensure_success, HttpMethod, HttpTransport, PreparedRequest, PreparedResponse, ReqwestTransport,
    MAX_ERROR_SNIPPET_CHARS,
};
pub use idempotency_store::{IdempotencyStore, InboundEventDeduplicator};
pub use media::{marker_text, put_if_present};
pub use outbound_address::{parse_peer_target, PeerTarget};
pub use props::{get_int, get_str, get_str_or, object_from_btree};
pub use text::{constant_time_eq, looks_like_markdown, to_hex};
pub use token::{
    now_ms, refresh_at_ms, AccessTokenProvider, AccessTokenStore, CachedAccessToken,
    InMemoryAccessTokenStore,
};
pub use xml::xml_element_text;
