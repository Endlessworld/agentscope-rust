//! Receipt-aware delivery on the harness channel abstraction (CHANNEL-N05).
//!
//! Java `Channel.deliverWithReceipt(address, msg, deliveryId)` lets a durable outbound
//! worker de-duplicate a retry by provider message id. The harness traits therefore need
//! a *default* receipt method that fails closed, plus overrides from transports that can
//! actually report a receipt.
use std::sync::Mutex;

use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use agentscope_harness::agent::gateway::channel::Channel;
use agentscope_harness::{
    ChannelConfig, ChannelSink, DeliveryReceipt, InboundMessage, OutboundAddress,
};
use async_trait::async_trait;

fn address(channel_id: &str) -> OutboundAddress {
    OutboundAddress::direct(channel_id, format!("{channel_id}:DIRECT:u_1"))
}

/// A sink that only implements `deliver`: the inherited receipt path must fail closed
/// instead of fabricating a receipt.
struct PlainSink;

#[async_trait]
impl ChannelSink for PlainSink {
    fn channel_id(&self) -> &str {
        "plain"
    }

    async fn deliver(&self, _address: &OutboundAddress, _messages: Vec<Msg>) -> Result<()> {
        Ok(())
    }
}

/// A sink whose transport reports a provider message id (Feishu / Weixin shape).
#[derive(Default)]
struct ReceiptSink {
    seen: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl ChannelSink for ReceiptSink {
    fn channel_id(&self) -> &str {
        "receipts"
    }

    async fn deliver(&self, _address: &OutboundAddress, _messages: Vec<Msg>) -> Result<()> {
        Ok(())
    }

    async fn deliver_with_receipt(
        &self,
        _address: &OutboundAddress,
        _messages: Vec<Msg>,
        delivery_id: Option<&str>,
    ) -> Result<DeliveryReceipt> {
        self.seen
            .lock()
            .unwrap()
            .push(delivery_id.map(str::to_string));
        Ok(DeliveryReceipt {
            message_id: Some("om_1".to_string()),
            raw: serde_json::json!({ "code": 0 }),
        })
    }
}

/// A `Channel` adapter that only implements the required methods, so the receipt method
/// is exercised through the trait default.
struct PlainChannel;

#[async_trait]
impl Channel for PlainChannel {
    fn channel_id(&self) -> &str {
        "plain-channel"
    }

    fn config(&self) -> ChannelConfig {
        ChannelConfig::of("plain-channel")
    }

    async fn dispatch(&self, _message: InboundMessage) -> Result<Msg> {
        unreachable!("the receipt tests never dispatch")
    }
}

#[tokio::test]
async fn sink_receipt_defaults_to_unsupported_and_is_overridable() {
    let addr = address("plain");
    let err = PlainSink
        .deliver_with_receipt(&addr, vec![Msg::assistant("x")], Some("d1"))
        .await
        .expect_err("the default receipt path must fail closed");
    let text = format!("{err}");
    assert!(text.contains("unsupported"), "{text}");
    assert!(
        text.contains("plain"),
        "the error names the channel: {text}"
    );

    let sink = ReceiptSink::default();
    let receipt = sink
        .deliver_with_receipt(&addr, vec![Msg::assistant("x")], Some("d1"))
        .await
        .unwrap();
    assert_eq!(receipt.message_id.as_deref(), Some("om_1"));
    assert_eq!(receipt.raw["code"], serde_json::json!(0));
    assert_eq!(
        sink.seen.lock().unwrap().as_slice(),
        &[Some("d1".to_string())],
        "the delivery id must reach the transport"
    );
}

#[tokio::test]
async fn channel_receipt_default_is_unsupported() {
    let channel = PlainChannel;
    let err = channel
        .deliver_with_receipt(&address("plain-channel"), &Msg::assistant("x"), None)
        .await
        .expect_err("a channel without receipt support must not report success");
    let text = format!("{err}");
    assert!(text.contains("unsupported"), "{text}");
    assert!(text.contains("plain-channel"), "{text}");
    assert_eq!(
        DeliveryReceipt::with_message_id("om_2")
            .message_id
            .as_deref(),
        Some("om_2")
    );
}
