//! Parses a DingTalk Stream bot-message payload into a harness `InboundMessage`.
//!
//! Rust port of `DingTalkInboundMapper`: only `msgtype=text` is mapped; everything else yields
//! `None` so the caller can ack without dispatching.

use agentscope_core::message::{Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer};
use serde_json::Value;

use crate::common::props::get_str;

/// Maps a DingTalk bot-message JSON payload into an [`InboundMessage`].
#[derive(Debug, Clone)]
pub struct DingTalkInboundMapper {
    channel_id: String,
    account_id: Option<String>,
}

impl DingTalkInboundMapper {
    pub fn new(channel_id: impl Into<String>, account_id: Option<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            account_id,
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// Returns the `msgId` field if present, used by the idempotency store.
    pub fn extract_msg_id(payload: &Value) -> Option<String> {
        get_str(payload, "msgId").filter(|s| !s.trim().is_empty())
    }

    /// Build the inbound message, or `None` when the payload is not a dispatchable text message.
    pub fn map(&self, payload: &Value) -> Option<InboundMessage> {
        if payload.is_null() {
            return None;
        }
        let msg_type = get_str(payload, "msgtype")?;
        if !msg_type.eq_ignore_ascii_case("text") {
            return None;
        }
        let content = payload
            .get("text")
            .and_then(|t| t.get("content"))
            .and_then(|c| c.as_str())?
            .trim()
            .to_string();
        if content.is_empty() {
            return None;
        }

        let conversation_type = get_str(payload, "conversationType");
        let sender_staff_id = get_str(payload, "senderStaffId");
        let conversation_id = get_str(payload, "conversationId");

        let (peer, sender_id) =
            if conversation_type.as_deref() == Some("2") && conversation_id.is_some() {
                let conversation_id = conversation_id.expect("checked above");
                let sender = sender_staff_id
                    .clone()
                    .unwrap_or_else(|| conversation_id.clone());
                (Peer::group(conversation_id), sender)
            } else {
                let peer_id = sender_staff_id.clone().or_else(|| conversation_id.clone());
                match peer_id {
                    Some(id) if !id.trim().is_empty() => {
                        let sender = id.clone();
                        (Peer::direct(id), sender)
                    }
                    _ => return None,
                }
            };

        let channel_message_id = Self::extract_msg_id(payload).unwrap_or_default();
        let msg = Msg::builder()
            .role(MsgRole::User)
            .name(sender_id.clone())
            .text_content(content)
            .metadata_entry("channelMessageId", Value::String(channel_message_id))
            .build();

        let mut builder =
            InboundMessage::builder(self.channel_id.clone(), peer, vec![msg]).sender_id(sender_id);
        if let Some(account_id) = &self.account_id {
            builder = builder.account_id(account_id.clone());
        }
        Some(builder.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::PeerKind;

    fn mapper() -> DingTalkInboundMapper {
        DingTalkInboundMapper::new("dingtalk", Some("acct-1".to_string()))
    }

    #[test]
    fn maps_direct_text_message() {
        let payload = serde_json::json!({
            "msgtype": "text",
            "text": {"content": "  hello bot  "},
            "conversationType": "1",
            "senderStaffId": "staff-7",
            "conversationId": "cid-1",
            "msgId": "msg-42"
        });
        let inbound = mapper().map(&payload).expect("mapped");
        assert_eq!(inbound.channel_id, "dingtalk");
        assert_eq!(inbound.account_id.as_deref(), Some("acct-1"));
        assert_eq!(inbound.peer.kind, PeerKind::Direct);
        assert_eq!(inbound.peer.id, "staff-7");
        assert_eq!(inbound.sender_id.as_deref(), Some("staff-7"));
        assert_eq!(inbound.messages.len(), 1);
        assert_eq!(inbound.messages[0].text_content(), "hello bot");
        assert_eq!(
            inbound.messages[0].metadata().get("channelMessageId"),
            Some(&serde_json::json!("msg-42"))
        );
    }

    #[test]
    fn maps_group_message_using_conversation_id() {
        let payload = serde_json::json!({
            "msgtype": "text",
            "text": {"content": "@bot do it"},
            "conversationType": "2",
            "senderStaffId": "staff-9",
            "conversationId": "conv-88"
        });
        let inbound = mapper().map(&payload).expect("mapped");
        assert_eq!(inbound.peer.kind, PeerKind::Group);
        assert_eq!(inbound.peer.id, "conv-88");
        assert_eq!(inbound.sender_id.as_deref(), Some("staff-9"));
    }

    #[test]
    fn group_without_staff_id_falls_back_to_conversation_id_as_sender() {
        let payload = serde_json::json!({
            "msgtype": "text",
            "text": {"content": "hi"},
            "conversationType": "2",
            "conversationId": "conv-88"
        });
        let inbound = mapper().map(&payload).expect("mapped");
        assert_eq!(inbound.sender_id.as_deref(), Some("conv-88"));
    }

    #[test]
    fn drops_non_text_and_empty_payloads() {
        let m = mapper();
        assert!(m
            .map(&serde_json::json!({"msgtype": "image", "text": {"content": "x"}}))
            .is_none());
        assert!(m
            .map(&serde_json::json!({
                "msgtype": "text",
                "conversationType": "1",
                "senderStaffId": "s"
            }))
            .is_none());
        assert!(m
            .map(&serde_json::json!({
                "msgtype": "text",
                "text": {"content": "   "},
                "conversationType": "1",
                "senderStaffId": "s"
            }))
            .is_none());
        assert!(m.map(&Value::Null).is_none());
    }

    #[test]
    fn direct_requires_a_peer_id() {
        let payload = serde_json::json!({
            "msgtype": "text",
            "text": {"content": "hi"},
            "conversationType": "1"
        });
        assert!(mapper().map(&payload).is_none());
    }

    #[test]
    fn extract_msg_id_ignores_blanks() {
        assert_eq!(
            DingTalkInboundMapper::extract_msg_id(&serde_json::json!({"msgId": "m1"})),
            Some("m1".to_string())
        );
        assert_eq!(
            DingTalkInboundMapper::extract_msg_id(&serde_json::json!({"msgId": "  "})),
            None
        );
        assert_eq!(DingTalkInboundMapper::extract_msg_id(&Value::Null), None);
    }
}
