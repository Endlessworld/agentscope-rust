//! Parses a decrypted Feishu Event Subscription v2 envelope into a harness `InboundMessage`.

use agentscope_core::message::{Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer, PeerKind};
use serde_json::Value;

/// Maps a Feishu Schema 2.0 envelope into an [`InboundMessage`].
#[derive(Debug, Clone)]
pub struct FeishuInboundMapper {
    channel_id: String,
}

impl FeishuInboundMapper {
    pub fn new(channel_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// Returns the `header.event_id`, used by the idempotency store.
    pub fn extract_event_id(envelope: &Value) -> Option<String> {
        envelope
            .pointer("/header/event_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    }

    /// Returns the URL verification challenge when the envelope is a verification request.
    pub fn extract_url_challenge(envelope: &Value) -> Option<String> {
        if envelope.get("type").and_then(|v| v.as_str()) != Some("url_verification") {
            return None;
        }
        envelope
            .get("challenge")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    }

    /// Returns `event.sender.sender_id.open_id` for bot-loop self-detection.
    pub fn extract_sender_open_id(envelope: &Value) -> Option<String> {
        envelope
            .pointer("/event/sender/sender_id/open_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    }

    /// Returns the `header.tenant_key` (multi-tenant id).
    pub fn extract_tenant_key(envelope: &Value) -> Option<String> {
        envelope
            .pointer("/header/tenant_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    }

    /// Map a decrypted Schema 2.0 envelope; `None` for non-text / malformed events.
    pub fn map(&self, envelope: &Value) -> Option<InboundMessage> {
        let event = envelope.get("event")?;
        if !event.is_object() {
            return None;
        }
        let message = event.get("message")?;
        let message_type = message.get("message_type").and_then(|v| v.as_str());
        if !message_type
            .map(|s| s.eq_ignore_ascii_case("text"))
            .unwrap_or(false)
        {
            return None;
        }
        let chat_type = message.get("chat_type").and_then(|v| v.as_str());
        let chat_id = message
            .get("chat_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())?;

        let open_id = event
            .pointer("/sender/sender_id/open_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())?;
        let sender_type = event.pointer("/sender/sender_type").and_then(|v| v.as_str());
        if sender_type != Some("user") {
            return None;
        }
        let message_id = message
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if message_id.trim().is_empty() {
            return None;
        }

        let content_json = message
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())?;
        let content: Value = serde_json::from_str(&content_json).ok()?;
        let text = content.get("text").and_then(|v| v.as_str())?.to_string();

        let mut text = text.trim().to_string();
        if let Some(mentions) = message.get("mentions").and_then(|v| v.as_array()) {
            for mention in mentions {
                let key = mention.get("key").and_then(|v| v.as_str()).unwrap_or("");
                if !key.is_empty() && text.starts_with(key) {
                    text = text[key.len()..].trim_start().to_string();
                }
            }
        }

        let kind = if chat_type
            .map(|s| s.eq_ignore_ascii_case("group"))
            .unwrap_or(false)
        {
            PeerKind::Group
        } else {
            PeerKind::Direct
        };
        let sender_name = open_id.clone();
        let msg = Msg::builder()
            .role(MsgRole::User)
            .name(sender_name.clone())
            .text_content(text)
            .metadata_entry("channelMessageId", Value::String(message_id))
            .metadata_entry(
                "channelReplyToId",
                Value::String(
                    message
                        .get("parent_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
            )
            .metadata_entry(
                "channelThreadId",
                Value::String(
                    message
                        .get("root_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
            )
            .build();

        let mut builder = InboundMessage::builder(self.channel_id.clone(), Peer::new(kind, chat_id), vec![msg])
            .sender_id(sender_name);
        if let Some(tenant) = Self::extract_tenant_key(envelope) {
            builder = builder.account_id(tenant);
        }
        Some(builder.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> Value {
        serde_json::json!({
            "schema": "2.0",
            "header": {"event_id": "ev-1", "event_type": "im.message.receive_v1", "tenant_key": "tenant-9"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_abc"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_1",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"hello feishu\"}"
                }
            }
        })
    }

    #[test]
    fn maps_p2p_text_message() {
        let inbound = FeishuInboundMapper::new("feishu")
            .map(&envelope())
            .expect("mapped");
        assert_eq!(inbound.peer.kind, PeerKind::Direct);
        assert_eq!(inbound.peer.id, "oc_1");
        assert_eq!(inbound.account_id.as_deref(), Some("tenant-9"));
        assert_eq!(inbound.sender_id.as_deref(), Some("ou_abc"));
        assert_eq!(inbound.messages[0].text_content(), "hello feishu");
    }

    #[test]
    fn group_mentions_are_stripped() {
        let mut env = envelope();
        env["event"]["message"]["chat_type"] = serde_json::json!("group");
        env["event"]["message"]["content"] =
            serde_json::json!("{\"text\":\"@_user_1  run the tests\"}");
        env["event"]["message"]["mentions"] = serde_json::json!([
            {"key": "@_user_1", "id": {"open_id": "ou_bot"}}
        ]);
        let inbound = FeishuInboundMapper::new("feishu").map(&env).expect("mapped");
        assert_eq!(inbound.peer.kind, PeerKind::Group);
        assert_eq!(inbound.messages[0].text_content(), "run the tests");
    }

    #[test]
    fn drops_non_text_and_malformed() {
        let m = FeishuInboundMapper::new("feishu");
        let mut env = envelope();
        env["event"]["message"]["message_type"] = serde_json::json!("image");
        assert!(m.map(&env).is_none());

        let mut env = envelope();
        env["event"]["sender"]["sender_type"] = serde_json::json!("app");
        assert!(m.map(&env).is_none());

        let mut env = envelope();
        env["event"]["message"]["content"] = serde_json::json!("not-json");
        assert!(m.map(&env).is_none());

        assert!(m.map(&serde_json::json!({"schema": "2.0"})).is_none());
    }

    #[test]
    fn extracts_helpers() {
        let env = envelope();
        assert_eq!(
            FeishuInboundMapper::extract_event_id(&env),
            Some("ev-1".to_string())
        );
        assert_eq!(
            FeishuInboundMapper::extract_sender_open_id(&env),
            Some("ou_abc".to_string())
        );
        assert_eq!(
            FeishuInboundMapper::extract_tenant_key(&env),
            Some("tenant-9".to_string())
        );
        assert_eq!(FeishuInboundMapper::extract_url_challenge(&env), None);
    }

    #[test]
    fn detects_url_verification_challenge() {
        let env = serde_json::json!({"type": "url_verification", "challenge": "c-1"});
        assert_eq!(
            FeishuInboundMapper::extract_url_challenge(&env),
            Some("c-1".to_string())
        );
        assert_eq!(FeishuInboundMapper::extract_url_challenge(&serde_json::json!({})), None);
    }
}
