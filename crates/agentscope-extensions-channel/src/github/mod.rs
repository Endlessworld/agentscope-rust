//! GitHub channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.github`:
//! - [`GitHubChannelProperties`] — provider configuration
//! - [`GitHubInboundMapper`] — webhook delivery → harness `InboundMessage`
//! - [`GitHubOutboundClient`] — issue/PR comment POST request builders
//! - [`GitHubSignatureVerifier`] — `X-Hub-Signature-256` HMAC-SHA256 verification
//! - [`GitHubBotIdentityResolver`] — resolves the bot's own user id for bot-loop self-detection
//! - [`GitHubChannel`] — `Channel` adapter + webhook pipeline
//! - [`GitHubWebhookController`](controller) — axum shell over [`GitHubChannel::handle_webhook`]
//!
//! The Java `GitHubChannelRegistry` singleton is replaced by the generic
//! [`ChannelRegistry`](crate::common::ChannelRegistry)`<GitHubChannel>`
//! ([`GitHubChannelRegistry`]).

pub mod bot_identity;
pub mod channel;
pub mod controller;
pub mod inbound;
pub mod outbound;
pub mod properties;
pub mod signature;

pub use bot_identity::{BotIdentity, GitHubBotIdentityResolver, UNRESOLVED_USER_ID};
pub use channel::{GitHubCallbackOutcome, GitHubChannel, GitHubChannelRegistry};
pub use inbound::GitHubInboundMapper;
pub use outbound::GitHubOutboundClient;
pub use properties::GitHubChannelProperties;
pub use signature::GitHubSignatureVerifier;
