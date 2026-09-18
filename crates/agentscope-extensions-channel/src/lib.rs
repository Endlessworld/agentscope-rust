//! Platform channel adapters for AgentScope (Rust port of
//! `agentscope-extensions-channel`).
//!
//! The harness owns the *routing skeleton* (`agentscope_harness::agent::gateway`):
//! [`agentscope_harness::agent::Channel`], `ChannelRouter`, `InboundMessage`,
//! `ChannelConfig`, `Peer`/`PeerKind` and `OutboundAddress`. This crate supplies the
//! *platform-specific adapters* that feed and consume that skeleton:
//!
//! - **common** — [`BotLoopGuard`] (per-peer sliding-window throttle), [`IdempotencyStore`]
//!   (bounded TTL de-duplication), outbound-address parsing, a pluggable
//!   [`common::HttpTransport`] (real [`common::ReqwestTransport`] + offline test doubles) and
//!   an [`common::AccessTokenProvider`] cache abstra.
//! - **dingtalk** / **feishu** / **github** / **gitlab** / **wecom** — per-platform inbound
//!   webhook/event → `InboundMessage` mappers, outbound request builders, provider
//!   configuration parsing and webhook signature verification.
//!
//! Design notes:
//! - Provider DTOs are parsed straight out of [`serde_json::Value`] / raw XML strings so the
//!   adapters stay dependency-light and trivially testable offline; no network call happens in
//!   any test.
//! - Outbound clients take an [`common::HttpTransport`], which makes request construction and
//!   response handling fully unit-testable with a recording transport.
//! - Not yet ported (tracked in `docs/extensions/CHANNEL__GAP_TRACKER.md` §3): the DingTalk
//!   Stream (WebSocket) client, the Feishu/WeCom callback controllers (need a web framework) and
//!   the AES-CBC payload decryption helpers (need the `aes`/`cbc` crates).

pub mod common;
pub mod dingtalk;
pub mod feishu;
pub mod github;
pub mod gitlab;
pub mod wecom;

pub use common::{
    AccessTokenProvider, BotLoopGuard, ChannelError, HttpMethod, HttpTransport, IdempotencyStore,
    PeerTarget, PreparedRequest, PreparedResponse, ReqwestTransport, Result, TokenCache,
};

