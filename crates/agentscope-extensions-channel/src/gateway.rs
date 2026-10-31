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
//! `deliver_with_receipt` (CHANNEL-N05) has its own entry point,
//! [`build_gateway_registry_with_receipts`]: a platform whose transport reports provider message
//! ids (Feishu, Weixin) must mirror a receipt-aware sink into the `ChannelManager`, otherwise a
//! durable outbound worker only ever sees the fail-closed default.

use std::sync::Arc;

use agentscope_harness::agent::gateway::channel::Channel;
use agentscope_harness::agent::{ChannelManager, ChannelSink};
use axum::Router;

use crate::common::{ChannelRegistry, ReceiptChannel};

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
    let registry = match manager {
        Some(manager) => ChannelRegistry::with_manager(manager),
        None => ChannelRegistry::new(),
    };
    assemble(registry, channels, build_router).await
}

/// [`build_gateway_registry`] for a platform whose transport returns a provider delivery receipt
/// (Feishu, Weixin — CHANNEL-N05).
///
/// The channels are registered exactly as in [`build_gateway_registry`]; the difference is the
/// harness `ChannelManager` mirror, which is a [`ReceiptSink`](crate::common::ReceiptSink). A host
/// delivery worker calling `ChannelSink::deliver_with_receipt` on the mirrored channel therefore
/// gets the provider message id instead of the fail-closed unsupported error.
pub async fn build_gateway_registry_with_receipts<C, F>(
    channels: impl IntoIterator<Item = Arc<C>>,
    manager: Arc<ChannelManager>,
    build_router: F,
) -> (Router, Arc<ChannelRegistry<C>>)
where
    C: ReceiptChannel + Channel + ChannelSink + 'static,
    F: FnOnce(Arc<ChannelRegistry<C>>) -> Router,
{
    let registry = ChannelRegistry::with_manager_and_receipts(manager);
    assemble(registry, channels, build_router).await
}

/// Register every channel into `registry` and hand the same `Arc` to the router builder, so the
/// handle the host keeps and the one the controllers read can never diverge (CHANNEL-N09).
async fn assemble<C, F>(
    registry: ChannelRegistry<C>,
    channels: impl IntoIterator<Item = Arc<C>>,
    build_router: F,
) -> (Router, Arc<ChannelRegistry<C>>)
where
    C: Channel + ChannelSink + 'static,
    F: FnOnce(Arc<ChannelRegistry<C>>) -> Router,
{
    let registry = Arc::new(registry);
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

    use crate::common::testing::{RecordingTransport, StaticTokenProvider};
    use crate::common::PreparedResponse;
    use crate::github::channel::{GitHubCallbackOutcome, GitHubChannel, GitHubChannelRegistry};
    use agentscope_core::message::Msg;

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
        let registered = registry
            .get("github")
            .expect("registered by the entry point");
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

    /// A Feishu channel with a seeded send response, addressed by chat id.
    fn feishu_channel(transport: Arc<RecordingTransport>) -> crate::feishu::FeishuChannel {
        use crate::common::{AccessTokenProvider, BotLoopGuard, IdempotencyStore};
        use crate::feishu::{FeishuChannelProperties, FeishuInboundMapper, FeishuOutboundClient};

        let props = FeishuChannelProperties::from_json(
            "feishu",
            &serde_json::json!({"appId": "cli_x", "appSecret": "s", "verificationToken": "vt"}),
        )
        .unwrap();
        let token: Arc<dyn AccessTokenProvider> = Arc::new(StaticTokenProvider::new("tok"));
        crate::feishu::FeishuChannel::new(
            "feishu",
            ChannelConfig::of("feishu"),
            props,
            None,
            FeishuOutboundClient::new("https://open.feishu.cn", token, transport),
            FeishuInboundMapper::new("feishu"),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::with_defaults(),
        )
    }

    fn feishu_address() -> agentscope_harness::agent::OutboundAddress {
        agentscope_harness::agent::OutboundAddress::direct("feishu", "feishu:DIRECT:oc_1")
    }

    fn one_message() -> Vec<Msg> {
        vec![Msg::user("hi")]
    }

    /// CHANNEL-N05 through the real assembly entry: a durable worker only holds
    /// `Arc<dyn ChannelSink>` from the `ChannelManager`, yet must still get the provider id.
    #[tokio::test]
    async fn a_receipt_aware_registry_exposes_provider_receipts_through_the_channel_manager() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"code":0,"data":{"message_id":"om_9"}}"#),
            PreparedResponse::new(200, r#"{"code":0,"data":{"message_id":"om_10"}}"#),
        ]));
        let manager = Arc::new(ChannelManager::new());
        let (_router, registry) = build_gateway_registry_with_receipts(
            vec![Arc::new(feishu_channel(transport.clone()))],
            manager.clone(),
            crate::feishu::controller::router,
        )
        .await;
        assert_eq!(registry.channel_ids(), vec!["feishu".to_string()]);

        let sink = manager
            .get_channel("feishu")
            .await
            .expect("mirrored into the manager");
        let receipt = sink
            .deliver_with_receipt(&feishu_address(), one_message(), Some("d-1"))
            .await
            .unwrap();
        assert_eq!(receipt.message_id.as_deref(), Some("om_9"));
        assert_eq!(
            transport.last_request().unwrap().body.unwrap()["uuid"],
            serde_json::json!("d-1")
        );

        // The mirror is a strict superset: plain proactive delivery still works.
        sink.deliver(&feishu_address(), one_message())
            .await
            .unwrap();
        assert_eq!(transport.request_count(), 2);
    }

    /// Without the receipt-aware entry point the harness default stays fail-closed, so a durable
    /// worker can never mistake "unsupported" for "delivered".
    #[tokio::test]
    async fn a_plain_registry_keeps_the_fail_closed_receipt_default() {
        let transport = Arc::new(RecordingTransport::new());
        let manager = Arc::new(ChannelManager::new());
        let (_router, _registry) = build_gateway_registry(
            vec![Arc::new(feishu_channel(transport.clone()))],
            Some(manager.clone()),
            crate::feishu::controller::router,
        )
        .await;

        let sink = manager.get_channel("feishu").await.unwrap();
        let error = sink
            .deliver_with_receipt(&feishu_address(), one_message(), Some("d-1"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unsupported"), "{error}");
        assert_eq!(transport.request_count(), 0);
    }
}
