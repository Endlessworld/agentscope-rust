//! Maps an iLink user text message onto a harness `InboundMessage`.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinInboundMapper`. The current official
//! plugin advertises direct chat only, so every mapped message becomes a `DIRECT` peer.

use agentscope_core::message::{Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer};
use serde_json::Value;

use super::dto::WeixinMessage;

/// Metadata key carrying the provider message id.
pub const META_MESSAGE_ID: &str = "channelMessageId";
/// Metadata key carrying the reply routing token (`context_token`).
pub const META_CONTEXT_TOKEN: &str = "weixinContextToken";

/// Maps iLink messages into [`InboundMessage`]s.
#[derive(Debug, Clone)]
pub struct WeixinInboundMapper {
    channel_id: String,
    account_id: String,
    /// Bot/account identity used for loop prevention.
    self_id: String,
}

impl WeixinInboundMapper {
    /// `self_id` is the *account* identity, not `ilink_user_id`: iLink's `ilink_user_id` identifies
    /// the person who scanned the QR code, so filtering peers against it would drop real users.
    pub fn new(
        channel_id: impl Into<String>,
        account_id: impl Into<String>,
        self_id: impl Into<String>,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            account_id: account_id.into(),
            self_id: self_id.into(),
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Map one provider message; `None` for non-text, malformed or self-sent payloads.
    pub fn map(&self, message: &WeixinMessage) -> Option<InboundMessage> {
        // Only `message_type = 1` (a user text message) is mapped.
        if message.message_type.unwrap_or(0) != 1 {
            return None;
        }
        let from = message
            .from_user_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())?;
        // Loop prevention: a message the account itself sent must not be dispatched again.
        if from == self.self_id {
            return None;
        }
        let body = message.text()?;
        let token = message
            .context_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty());

        let mut builder = Msg::builder()
            .role(MsgRole::User)
            .name(from)
            .text_content(body);
        if let Some(message_id) = message.message_id() {
            builder = builder.metadata_entry(META_MESSAGE_ID, Value::String(message_id));
        }
        if let Some(token) = token {
            builder = builder.metadata_entry(META_CONTEXT_TOKEN, Value::String(token.to_string()));
        }
        Some(
            InboundMessage::builder(
                self.channel_id.clone(),
                Peer::direct(from),
                vec![builder.build()],
            )
            .account_id(self.account_id.clone())
            .sender_id(from)
            .build(),
        )
    }
}

/// The reply routing token carried by a mapped inbound message, if any.
pub fn context_token_of(message: &Msg) -> Option<String> {
    message
        .metadata()
        .get(META_CONTEXT_TOKEN)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::PeerKind;

    fn message(json: Value) -> WeixinMessage {
        serde_json::from_value(json).expect("valid provider message")
    }

    fn mapper() -> WeixinInboundMapper {
        WeixinInboundMapper::new("wx", "account-1", "account-1")
    }

    #[test]
    fn maps_a_direct_text_message_and_preserves_the_context_token() {
        let inbound = mapper()
            .map(&message(serde_json::json!({
                "message_type": 1,
                "message_id": "m1",
                "from_user_id": "u1",
                "context_token": "ctx",
                "item_list": [{ "type": 1, "text_item": { "text": " hi " } }],
            })))
            .expect("mapped");
        assert_eq!(inbound.channel_id, "wx");
        assert_eq!(inbound.account_id.as_deref(), Some("account-1"));
        assert_eq!(inbound.peer.kind, PeerKind::Direct);
        assert_eq!(inbound.peer.id, "u1");
        assert_eq!(inbound.sender_id.as_deref(), Some("u1"));
        assert_eq!(inbound.messages[0].text_content(), "hi");
        assert_eq!(inbound.messages[0].name(), Some("u1"));
        assert_eq!(
            context_token_of(&inbound.messages[0]).as_deref(),
            Some("ctx")
        );
        assert_eq!(
            inbound.messages[0].metadata()[META_MESSAGE_ID],
            Value::String("m1".to_string())
        );
    }

    #[test]
    fn drops_non_text_self_sent_and_incomplete_payloads() {
        let mapper = mapper();
        // Not a user text message.
        assert!(mapper
            .map(&message(serde_json::json!({
                "message_type": 2,
                "from_user_id": "u1",
                "item_list": [{ "type": 1, "text_item": { "text": "hi" } }],
            })))
            .is_none());
        // Missing message_type is read as 0, like the Java `asInt(0)`.
        assert!(mapper
            .map(&message(serde_json::json!({
                "from_user_id": "u1",
                "item_list": [{ "type": 1, "text_item": { "text": "hi" } }],
            })))
            .is_none());
        // Missing / blank sender.
        assert!(mapper
            .map(&message(serde_json::json!({
                "message_type": 1,
                "item_list": [{ "type": 1, "text_item": { "text": "hi" } }],
            })))
            .is_none());
        assert!(mapper
            .map(&message(serde_json::json!({
                "message_type": 1,
                "from_user_id": "  ",
                "item_list": [{ "type": 1, "text_item": { "text": "hi" } }],
            })))
            .is_none());
        // Non-text item only.
        assert!(mapper
            .map(&message(serde_json::json!({
                "message_type": 1,
                "from_user_id": "u1",
                "item_list": [{ "type": 2, "text_item": { "text": "hi" } }],
            })))
            .is_none());
        // The bot's own account identity is filtered out (loop prevention).
        assert!(mapper
            .map(&message(serde_json::json!({
                "message_type": 1,
                "from_user_id": "account-1",
                "item_list": [{ "type": 1, "text_item": { "text": "hi" } }],
            })))
            .is_none());
    }

    #[test]
    fn omits_absent_metadata_instead_of_storing_blank_values() {
        let inbound = mapper()
            .map(&message(serde_json::json!({
                "message_type": 1,
                "from_user_id": "u1",
                "message_id": "  ",
                "context_token": "",
                "item_list": [{ "type": 1, "text_item": { "text": "hi" } }],
            })))
            .expect("mapped");
        assert!(inbound.messages[0]
            .metadata()
            .get(META_MESSAGE_ID)
            .is_none());
        assert!(inbound.messages[0]
            .metadata()
            .get(META_CONTEXT_TOKEN)
            .is_none());
        assert_eq!(context_token_of(&inbound.messages[0]), None);
    }
}
