//! Platform channel adapters for AgentScope (Rust port of
//! `agentscope-extensions-channel`).
//!
//! The harness owns the *routing skeleton* (`agentscope_harness::agent::gateway`):
//! [`agentscope_harness::agent::Channel`], `ChannelRouter`, `InboundMessage`,
//! `ChannelConfig`, `Peer`/`PeerKind` and `OutboundAddress`. This crate supplies the
//! *platform-specific adapters* that feed and consume that skeleton:
//!
//! - **common** — [`BotLoopGuard`] (per-peer sliding-window throttle), [`IdempotencyStore`]
//!   (bounded TTL de-duplication), the [`InboundEventDeduplicator`] / [`AccessTokenStore`]
//!   pluggable seams for multi-instance deployments, the inbound media metadata contract
//!   ([`common::media`]), outbound-address parsing, a pluggable [`common::HttpTransport`] (real
//!   [`common::ReqwestTransport`] + offline test doubles) and an [`common::AccessTokenProvider`]
//!   cache abstraction.
//! - **dingtalk** / **feishu** / **github** / **gitlab** / **wecom** / **weixin** — per-platform
//!   inbound webhook/event → `InboundMessage` mappers (text + media), outbound request builders,
//!   provider configuration parsing and webhook signature verification.
//! - **gateway** — [`build_gateway_registry`]: the single assembly entry point that registers the
//!   channels into a typed [`ChannelRegistry`] and builds the matching axum router over it
//!   (replacement for the Java `start()` self-registration, see CHANNEL-N09).
//!
//! Design notes:
//! - Provider DTOs are parsed straight out of [`serde_json::Value`] / raw XML strings so the
//!   adapters stay dependency-light and trivially testable offline; no network call happens in
//!   any test.
//! - Outbound clients take an [`common::HttpTransport`], which makes request construction and
//!   response handling fully unit-testable with a recording transport.
//! - All six platform adapters are ported, including their stream/long-poll lifecycles, callback
//!   controllers (axum) and the AES-CBC payload helpers. Remaining gaps are tracked in
//!   `docs/extensions/CHANNEL__GAP_TRACKER.md` §3.

pub mod common;
pub mod dingtalk;
pub mod feishu;
pub mod gateway;
pub mod github;
pub mod gitlab;
pub mod wecom;
pub mod weixin;

pub use common::{
    AccessTokenProvider, AccessTokenStore, BotLoopGuard, CachedAccessToken, ChannelCore,
    ChannelError, ChannelRegistry, ChannelSeams, HttpMethod, HttpTransport, IdempotencyStore,
    InMemoryAccessTokenStore, InboundEventDeduplicator, PeerTarget, PreparedRequest,
    PreparedResponse, ReqwestTransport, Result,
};
pub use gateway::build_gateway_registry;

/// Compile-time guard for the platform type names.
///
/// Java's `*Channel.TYPE` constants are the single source of truth for
/// `agentscope.json#channels[*].type` dispatch and for diagnostics. The Rust port keeps one
/// `CHANNEL_TYPE` per platform module (CHANNEL-N08); this block fails the build if any of them is
/// empty or if two platforms claim the same type.
const _: () = {
    const TYPES: [&str; 6] = [
        dingtalk::CHANNEL_TYPE,
        feishu::CHANNEL_TYPE,
        github::CHANNEL_TYPE,
        gitlab::CHANNEL_TYPE,
        wecom::CHANNEL_TYPE,
        weixin::CHANNEL_TYPE,
    ];
    let mut i = 0;
    while i < TYPES.len() {
        assert!(!TYPES[i].is_empty());
        let mut j = i + 1;
        while j < TYPES.len() {
            assert!(!const_str_eq(TYPES[i], TYPES[j]));
            j += 1;
        }
        i += 1;
    }
};

/// `str` equality in a `const` context (`str == str` is not const-callable).
const fn const_str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod channel_type_tests {
    #[test]
    fn channel_type_constants_match_the_java_type_fields() {
        assert_eq!(crate::dingtalk::CHANNEL_TYPE, "dingtalk");
        assert_eq!(crate::feishu::CHANNEL_TYPE, "feishu");
        assert_eq!(crate::github::CHANNEL_TYPE, "github");
        assert_eq!(crate::gitlab::CHANNEL_TYPE, "gitlab");
        assert_eq!(crate::wecom::CHANNEL_TYPE, "wecom");
        assert_eq!(crate::weixin::CHANNEL_TYPE, "weixin");
    }
}
