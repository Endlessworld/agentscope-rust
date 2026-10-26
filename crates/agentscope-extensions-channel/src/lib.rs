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

