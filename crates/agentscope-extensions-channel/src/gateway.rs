//! Gateway bootstrap: the single assembly entry point for the webhook/channel adapters.
//!
//! ## Why this exists (vs. Java)
//!
//! Java's `*Channel#start()` registers the instance into its `*ChannelRegistry` singleton, so a
//! controller always finds the channel it serves. The Rust port keeps the registry
//! instance-scoped (see [`ChannelRegistry`]) and therefore needs one explicit registration step —
//! which is easy to forget: the webhook then answers 404 and every inbound message is lost, with
//! no compile-time hint (CHANNEL-N09).
//!
//! [`build_gateway_registry`] is that step. It creates the registry, registers every channel and
//! hands the **same** registry to the router builder, so the router the host mounts and the
//! registry the controllers read can never diverge.
//!
//! ## Startup hooks
//!
//! Two things have to happen before the first webhook arrives and both are best-effort:
//! 1. registration (this module), and
//! 2. provider bot-identity resolution (`GitHubChannel::start` / `GitLabChannel::start`), which
//!    enables self-authored event detection. Hosts should call `start()` on those channels next to
//!    [`build_gateway_registry`]; a failure only logs a warning and never fails startup.
//!
//! `deliver_with_receipt` (CHANNEL-N05) is deliberately not part of this module: it needs the
//! harness `Channel`/`ChannelSink` default method from the shared abstract layer.

use std::sync::Arc;

use agentscope_harness::agent::gateway::channel::Channel;
use agentscope_harness::agent::{ChannelManager, ChannelSink};
use axum::Router;

use crate::common::ChannelRegistry;

/// Build a typed channel registry **and** its HTTP router from the same set of channels.
///
/// This is the Rust equivalent of the Java `start()` self-registration: every channel in
/// `channels` is registered into the returned registry, and `build_router` receives that very
/// `Arc` so the router state and the returned handle are the same object (proactive delivery and
/// inbound callback lookups therefore agree by construction).
///
/// Passing `manager` also mirrors every registration into the harness [`ChannelManager`], which is
/// what makes gateway-initiated outbound delivery reuse the same `deliver_reply` path.
///
/// ```ignore
/// let (router, registry) = build_gateway_registry(
///     vec![Arc::new(feishu_channel)],
///     Some(channel_manager),
///     crate::feishu::controller::router,
/// )
/// .await;
/// host.mount(router);
/// ```
pub async fn build_gateway_registry<C, F>(
    channels: impl IntoIterator<Item = Arc<C>>,
    manager: Option<Arc<ChannelManager>>,
    build_router: F,
) -> (Router, Arc<ChannelRegistry<C>>)
where
    C: Channel + ChannelSink + 'static,
    F: FnOnce(Arc<ChannelRegistry<C>>) -> Router,
{
    let registry = Arc::new(match manager {
        Some(manager) => ChannelRegistry::with_manager(manager),
        None => ChannelRegistry::new(),
    });
    for channel in channels {
        registry.register(channel).await;
    }
    let router = build_router(registry.clone());
    (router, registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::ChannelConfig;

    use crate::common::testing::RecordingTransport;
    use crate::github::channel::{GitHubCallbackOutcome, GitHubChannel, GitHubChannelRegistry};

    /// The real assembly entry, exercised end to end: no `register` call anywhere in this test.
    #[tokio::test]
    async fn build_gateway_registry_registers_the_channels_the_router_reads() {
        let channel = Arc::new(
            GitHubChannel::from_json(
                "github",
                ChannelConfig::of("github"),
                &serde_json::json!({"token": "t", "webhookSecret": "s"}),
                Arc::new(RecordingTransport::new()),
            )
            .unwrap(),
        );
        let handle = channel.clone();

        let mut router_registry: Option<Arc<GitHubChannelRegistry>> = None;
        let (_router, registry) = build_gateway_registry(vec![channel], None, |registry| {
            router_registry = Some(registry.clone());
            crate::github::controller::router(registry)
        })
        .await;

        // The router was built over the same registry the host got back...
        let router_registry = router_registry.expect("router builder received a registry");
        assert!(Arc::ptr_eq(&router_registry, &registry));
        // ...which already contains the channel the entry point registered.
        assert_eq!(registry.channel_ids(), vec!["github".to_string()]);
        let registered = registry.get("github").expect("registered by the entry point");
        assert!(Arc::ptr_eq(&registered, &handle));

        // And the registered handle really serves the webhook pipeline (a bad signature is 401).
        assert_eq!(
            registered.handle_webhook("issue_comment", None, "{}").await,
            GitHubCallbackOutcome::Unauthorized
        );
    }

    /// Registrations are mirrored into the harness `ChannelManager` when one is supplied.
    #[tokio::test]
    async fn build_gateway_registry_can_mirror_into_the_channel_manager() {
        let manager = Arc::new(agentscope_harness::agent::ChannelManager::new());
        let channel = Arc::new(
            GitHubChannel::from_json(
                "github",
                ChannelConfig::of("github"),
                &serde_json::json!({"token": "t", "webhookSecret": "s"}),
                Arc::new(RecordingTransport::new()),
            )
            .unwrap(),
        );
        let (_router, registry) = build_gateway_registry(
            vec![channel],
            Some(manager.clone()),
            crate::github::controller::router,
        )
        .await;

        assert_eq!(registry.channel_ids(), vec!["github".to_string()]);
        assert_eq!(manager.channel_ids().await, vec!["github".to_string()]);
    }
}
