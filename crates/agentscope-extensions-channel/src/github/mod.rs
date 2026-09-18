//! GitHub channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.github`:
//! - [`GitHubChannelProperties`] — provider configuration
//! - [`GitHubInboundMapper`] — webhook delivery → harness `InboundMessage`
//! - [`GitHubOutboundClient`] — issue/PR comment POST request builders
//! - [`GitHubSignatureVerifier`] — `X-Hub-Signature-256` HMAC-SHA256 verification
//!
//! Not yet ported (see `docs/extensions/CHANNEL__GAP_TRACKER.md` §3): `GitHubWebhookController`,
//! `GitHubBotIdentityResolver`, `GitHubChannelRegistry` and `GitHubChannel`.

pub mod bot_identity;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod signature;

pub use bot_identity::{BotIdentity, GitHubBotIdentityResolver, UNRESOLVED_USER_ID};
pub use inbound::GitHubInboundMapper;
pub use outbound::GitHubOutboundClient;
pub use properties::GitHubChannelProperties;
pub use signature::GitHubSignatureVerifier;
