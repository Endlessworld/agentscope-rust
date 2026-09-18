//! GitLab channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.gitlab`:
//! - [`GitLabChannelProperties`] — provider configuration
//! - [`GitLabInboundMapper`] — Note Hook payload → harness `InboundMessage`
//! - [`GitLabOutboundClient`] — issue/MR note POST request builders
//! - [`verify_webhook_token`] — constant-time `X-Gitlab-Token` check
//! - [`GitLabBotIdentityResolver`] — best-effort `GET /user` bot identity for loop detection
//! - [`GitLabChannel`] — lifecycle/routing adapter (webhook pipeline + reply delivery)
//! - [`controller::router`] — axum shell over `POST /api/channels/gitlab/:channel_id/webhook`
//!
//! Complete: all five GitLab classes from `io.agentscope.extensions.channel.gitlab` are ported
//! (see `docs/extensions/CHANNEL__GAP_TRACKER.md` §3).

pub mod bot_identity;
pub mod channel;
pub mod controller;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod signature;

pub use bot_identity::GitLabBotIdentityResolver;
pub use channel::{GitLabCallbackOutcome, GitLabChannel, GitLabChannelRegistry};
pub use inbound::GitLabInboundMapper;
pub use outbound::GitLabOutboundClient;
pub use properties::GitLabChannelProperties;
pub use signature::verify_webhook_token;
