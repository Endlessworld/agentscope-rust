//! Receipt-aware delivery seam (CHANNEL-N05).
//!
//! Java `Channel#deliverWithReceipt(address, msg, deliveryId)` hands a host that persists outbound
//! notifications the **provider's** message id, so a retry can be de-duplicated without knowing the
//! platform's response shape. The harness traits carry that default (`Channel::deliver_with_receipt`
//! / `ChannelSink::deliver_with_receipt`, both fail-closed), but they also blanket-implement
//! `ChannelSink` for every `PlatformChannel`, which means a platform adapter (Feishu, Weixin) cannot
//! override the default directly.
//!
//! [`ReceiptChannel`] is the transport-side capability and [`ReceiptSink`] is the adapter that
//! forwards the harness method to it. [`ChannelRegistry`](super::ChannelRegistry) registers the
//! adapter instead of the raw channel when the platform can report receipts
//! (`ChannelRegistry::with_manager_and_receipts`), so a host delivery worker that only holds
//! `Arc<dyn ChannelSink>` still gets the real provider id.

use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use agentscope_harness::agent::gateway::channel::Channel;
use agentscope_harness::agent::{ChannelSink, DeliveryReceipt, OutboundAddress, PlatformChannel};
use async_trait::async_trait;

/// A platform channel whose transport reports the provider's delivery receipt.
///
/// Mirrors the Java `Channel#deliverWithReceipt` contract: one message, an optional caller-supplied
/// delivery id (forwarded to the provider as its idempotency key, e.g. the Feishu `uuid`), and the
/// provider message id on success. No provider acknowledgement, no receipt — a local `Ok(())` is
/// exactly what a durable worker must not mistake for a delivery.
#[async_trait]
pub trait ReceiptChannel: PlatformChannel {
    /// Deliver one message and return the provider's receipt.
    async fn deliver_with_receipt(
        &self,
        address: &OutboundAddress,
        message: &Msg,
        delivery_id: Option<&str>,
    ) -> Result<DeliveryReceipt>;
}

/// The harness [`ChannelSink`] view of a [`ReceiptChannel`].
///
/// `deliver` keeps delegating to the platform's own `deliver_reply`, so mirroring the adapter into
/// the harness `ChannelManager` loses nothing; only `deliver_with_receipt` changes from the
/// fail-closed default to the real transport call.
pub struct ReceiptSink<C: ?Sized> {
    channel: Arc<C>,
}

impl<C: ?Sized> ReceiptSink<C> {
    /// Wrap the platform channel.
    pub fn new(channel: Arc<C>) -> Self {
        Self { channel }
    }

    /// The wrapped platform channel, so a host can still reach its provider-specific internals.
    pub fn channel(&self) -> &Arc<C> {
        &self.channel
    }
}

impl<C: ?Sized> Clone for ReceiptSink<C> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
        }
    }
}

impl<C: ?Sized> std::fmt::Debug for ReceiptSink<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiptSink").finish_non_exhaustive()
    }
}

#[async_trait]
impl<C: ReceiptChannel + ?Sized> ChannelSink for ReceiptSink<C> {
    fn channel_id(&self) -> &str {
        Channel::channel_id(&*self.channel)
    }

    async fn deliver(&self, address: &OutboundAddress, messages: Vec<Msg>) -> Result<()> {
        PlatformChannel::deliver_reply(&*self.channel, address, messages).await
    }

    async fn deliver_with_receipt(
        &self,
        address: &OutboundAddress,
        messages: Vec<Msg>,
        delivery_id: Option<&str>,
    ) -> Result<DeliveryReceipt> {
        // Java's channel-level API is single-message: a batch is delivered in order and reports the
        // last provider receipt, and only a single-message batch carries the caller's delivery id
        // (a uuid shared by several messages would collapse them in the provider's dedupe index).
        let is_single = messages.len() == 1;
        let mut receipt = DeliveryReceipt::default();
        for message in &messages {
            receipt = ReceiptChannel::deliver_with_receipt(
                &*self.channel,
                address,
                message,
                if is_single { delivery_id } else { None },
            )
            .await?;
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use agentscope_harness::agent::{ChannelConfig, PlatformCtx};

    /// A receipt-capable double: the transport-side capability without any provider wire format.
    struct StubReceiptChannel {
        core: PlatformCtx,
        seen: Mutex<Vec<(OutboundAddress, Option<String>)>>,
    }

    impl StubReceiptChannel {
        fn new() -> Self {
            Self {
                core: PlatformCtx::new("stub", ChannelConfig::of("stub")),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl PlatformChannel for StubReceiptChannel {
        fn ctx(&self) -> &PlatformCtx {
            &self.core
        }

        async fn deliver_reply(
            &self,
            _address: &OutboundAddress,
            _messages: Vec<Msg>,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl ReceiptChannel for StubReceiptChannel {
        async fn deliver_with_receipt(
            &self,
            address: &OutboundAddress,
            message: &Msg,
            delivery_id: Option<&str>,
        ) -> Result<DeliveryReceipt> {
            self.seen
                .lock()
                .unwrap()
                .push((address.clone(), delivery_id.map(str::to_string)));
            Ok(DeliveryReceipt::with_message_id(format!(
                "id-{}",
                message.text_content()
            )))
        }
    }

    fn address() -> OutboundAddress {
        OutboundAddress::direct("stub", "stub:DIRECT:u_1")
    }

    #[tokio::test]
    async fn the_sink_reports_the_provider_receipt_and_forwards_the_delivery_id() {
        let sink = ReceiptSink::new(Arc::new(StubReceiptChannel::new()));
        let receipt = sink
            .deliver_with_receipt(&address(), vec![Msg::user("hi")], Some("d-1"))
            .await
            .unwrap();
        assert_eq!(receipt.message_id.as_deref(), Some("id-hi"));
        assert_eq!(sink.channel_id(), "stub");
        assert_eq!(
            sink.channel().seen.lock().unwrap().as_slice(),
            &[(address(), Some("d-1".to_string()))]
        );
    }

    #[tokio::test]
    async fn a_batch_reports_the_last_receipt_without_reusing_one_uuid() {
        let sink = ReceiptSink::new(Arc::new(StubReceiptChannel::new()));
        let receipt = sink
            .deliver_with_receipt(
                &address(),
                vec![Msg::user("a"), Msg::user("b")],
                Some("d-1"),
            )
            .await
            .unwrap();
        assert_eq!(receipt.message_id.as_deref(), Some("id-b"));
        assert_eq!(
            sink.channel().seen.lock().unwrap().as_slice(),
            &[(address(), None), (address(), None)],
            "a shared uuid would collapse the batch in the provider's dedupe index"
        );
    }

    #[tokio::test]
    async fn an_empty_batch_sends_nothing_and_reports_no_receipt() {
        let sink = ReceiptSink::new(Arc::new(StubReceiptChannel::new()));
        let receipt = sink
            .deliver_with_receipt(&address(), Vec::new(), Some("d-1"))
            .await
            .unwrap();
        assert_eq!(receipt, DeliveryReceipt::default());
        assert!(sink.channel().seen.lock().unwrap().is_empty());
    }
}
