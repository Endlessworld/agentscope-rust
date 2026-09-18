//! Framework-agnostic channel lookup used by the (not-yet-ported) webhook/callback controllers.
//!
//! ## Why this is not a duplicate of the harness
//!
//! The harness already provides both halves of "route to a channel":
//! - `agentscope_harness::agent::ChannelRouter` — evaluates the 7 binding tiers for an
//!   `InboundMessage`
//! - `agentscope_harness::agent::ChannelManager` — the `channel_id -> Arc<dyn ChannelSink>` lookup
//!   used for **proactive outbound** delivery
//!
//! What the Java `*ChannelRegistry` classes add on top is a **typed** `channelId -> *Channel`
//! lookup, which the Spring controllers use to find the platform adapter (and then its crypto /
//! mapper / idempotency internals) for an inbound HTTP request. `ChannelManager` cannot serve that
//! because it stores `dyn ChannelSink` (no access to e.g. `FeishuCrypto`).
//!
//! So [`ChannelRegistry`] is deliberately thin: it keeps the typed handle **and** mirrors every
//! registration into the harness `ChannelManager` when one is supplied, so there is exactly one
//! outbound-sink registry at runtime.
//!
//! Unlike the Java singletons (`FeishuChannelRegistry.instance()`), the Rust registry is
//! instance-scoped: no process-wide mutable state, so tests can build isolated registries.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use agentscope_harness::agent::{Channel, ChannelManager, ChannelSink};

/// Typed registry of platform channels, optionally mirrored into a harness [`ChannelManager`].
pub struct ChannelRegistry<C>
where
    C: Channel + ChannelSink + 'static,
{
    channels: RwLock<BTreeMap<String, Arc<C>>>,
    manager: Option<Arc<ChannelManager>>,
}

impl<C> std::fmt::Debug for ChannelRegistry<C>
where
    C: Channel + ChannelSink + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelRegistry")
            .field("channel_ids", &self.channel_ids())
            .field("mirrors_into_channel_manager", &self.manager.is_some())
            .finish()
    }
}

impl<C> Default for ChannelRegistry<C>
where
    C: Channel + ChannelSink + 'static,
{
    fn default() -> Self {
        Self {
            channels: RwLock::new(BTreeMap::new()),
            manager: None,
        }
    }
}

impl<C> ChannelRegistry<C>
where
    C: Channel + ChannelSink + 'static,
{
    /// An isolated registry (no harness mirror).
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry that also registers every channel as a harness [`ChannelSink`] so proactive
    /// outbound delivery goes through the harness `ChannelManager`.
    pub fn with_manager(manager: Arc<ChannelManager>) -> Self {
        Self {
            channels: RwLock::new(BTreeMap::new()),
            manager: Some(manager),
        }
    }

    /// Register (or replace) a channel by its `channel_id`. Mirrors the Java `register(channel)`,
    /// additionally mirroring into the harness `ChannelManager` when configured.
    pub async fn register(&self, channel: Arc<C>) {
        let channel_id = channel.channel_id().to_string();
        self.channels
            .write()
            .expect("channel registry lock poisoned")
            .insert(channel_id, channel.clone());
        if let Some(manager) = &self.manager {
            let sink: Arc<dyn ChannelSink> = channel;
            manager.register(sink).await;
        }
    }

    /// Remove a channel by id. Returns `true` when something was removed.
    ///
    /// The harness `ChannelManager` has no `unregister` (mirroring the Java side, where `stop()`
    /// only touches the platform registry), so the mirror is intentionally left in place.
    pub fn unregister(&self, channel_id: &str) -> bool {
        self.channels
            .write()
            .expect("channel registry lock poisoned")
            .remove(channel_id)
            .is_some()
    }

    /// Typed lookup. Mirrors the Java `get(channelId)`.
    pub fn get(&self, channel_id: &str) -> Option<Arc<C>> {
        self.channels
            .read()
            .expect("channel registry lock poisoned")
            .get(channel_id)
            .cloned()
    }

    /// Registered channel ids, sorted (deterministic for tests / diagnostics).
    pub fn channel_ids(&self) -> Vec<String> {
        self.channels
            .read()
            .expect("channel registry lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Number of registered channels.
    pub fn len(&self) -> usize {
        self.channels
            .read()
            .expect("channel registry lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use agentscope_core::error::Result as CoreResult;
    use agentscope_core::message::Msg;
    use agentscope_harness::agent::{ChannelConfig, InboundMessage, OutboundAddress};

    /// Minimal `Channel + ChannelSink` double so the registry can be tested without a platform.
    #[derive(Debug)]
    struct StubChannel {
        id: String,
        config: ChannelConfig,
    }

    impl StubChannel {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                config: ChannelConfig::of(id),
            }
        }
    }

    #[async_trait]
    impl Channel for StubChannel {
        fn channel_id(&self) -> &str {
            &self.id
        }

        fn config(&self) -> ChannelConfig {
            self.config.clone()
        }

        async fn dispatch(&self, _message: InboundMessage) -> CoreResult<Msg> {
            Ok(Msg::builder().text_content("stub").build())
        }
    }

    #[async_trait]
    impl ChannelSink for StubChannel {
        fn channel_id(&self) -> &str {
            &self.id
        }

        async fn deliver(&self, _address: &OutboundAddress, _messages: Vec<Msg>) -> CoreResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn registers_looks_up_and_unregisters() {
        let registry: ChannelRegistry<StubChannel> = ChannelRegistry::new();
        assert!(registry.is_empty());
        assert!(registry.get("feishu").is_none());

        registry.register(Arc::new(StubChannel::new("feishu"))).await;
        registry.register(Arc::new(StubChannel::new("github"))).await;
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.channel_ids(), vec!["feishu", "github"]);
        assert!(registry.get("feishu").is_some());

        assert!(registry.unregister("feishu"));
        assert!(!registry.unregister("feishu"));
        assert_eq!(registry.channel_ids(), vec!["github"]);
    }

    #[tokio::test]
    async fn registering_again_replaces_the_handle() {
        let registry: ChannelRegistry<StubChannel> = ChannelRegistry::new();
        registry.register(Arc::new(StubChannel::new("wecom"))).await;
        let first = registry.get("wecom").unwrap();
        registry.register(Arc::new(StubChannel::new("wecom"))).await;
        let second = registry.get("wecom").unwrap();
        assert_eq!(registry.len(), 1);
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn mirrors_registration_into_the_harness_channel_manager() {
        let manager = Arc::new(ChannelManager::new());
        let registry: ChannelRegistry<StubChannel> =
            ChannelRegistry::with_manager(manager.clone());
        registry.register(Arc::new(StubChannel::new("gitlab"))).await;

        // The harness manager now owns the outbound sink for this channel id.
        assert_eq!(manager.channel_ids().await, vec!["gitlab".to_string()]);
        assert!(manager.get_channel("gitlab").await.is_some());
        // And proactive delivery through the harness path succeeds (no-op stub).
        manager
            .deliver(
                &OutboundAddress::direct("gitlab", "grp/proj#1:Issue"),
                vec![Msg::builder().text_content("hello").build()],
            )
            .await;
    }
}
