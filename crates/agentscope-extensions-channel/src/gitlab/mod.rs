//! GitLab channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.gitlab`:
//! - [`GitLabChannelProperties`] — provider configuration
//! - [`GitLabInboundMapper`] — Note Hook payload → harness `InboundMessage`
//! - [`GitLabOutboundClient`] — issue/MR note POST request builders
//! - [`verify_webhook_token`] — constant-time `X-Gitlab-Token` check
//!
//! Not yet ported (see `docs/extensions/CHANNEL__GAP_TRACKER.md` §3): `GitLabWebhookController`,
//! `GitLabBotIdentityResolver`, `GitLabChannelRegistry` and `GitLabChannel`.

pub mod bot_identity;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod signature;

pub use bot_identity::GitLabBotIdentityResolver;
pub use inbound::GitLabInboundMapper;
pub use outbound::GitLabOutboundClient;
pub use properties::GitLabChannelProperties;
pub use signature::verify_webhook_token;
