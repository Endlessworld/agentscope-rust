//! Parses a decrypted WeCom callback XML body into a harness `InboundMessage`.

use agentscope_core::message::{Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer};
use serde_json::Value;

use crate::common::xml_element_text;

/// Maps WeCom callback XML into [`InboundMessage`]s (only `MsgType=text` is mapped).
#[derive(Debug, Clone)]
pub struct WeComInboundMapper {
    channel_id: String,
    account_id: Option<String>,
}

impl WeComInboundMapper {
    pub fn new(channel_id: impl Into<String>, account_id: Option<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            account_id,
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// Returns the `MsgId` field if present, used by the idempotency store.
    pub fn extract_msg_id(xml: &str) -> Option<String> {
        xml_element_text(xml, "MsgId").filter(|s| !s.trim().is_empty())
    }

    /// Map the decrypted XML; `None` for non-text or unparseable payloads.
    ///
    /// Note: the Java mapper throws on malformed XML; this adapter is non-panicking and returns
    /// `None` instead (see tracker §4 design deviations).
    pub fn map(&self, xml: &str) -> Option<InboundMessage> {
        let msg_type = xml_element_text(xml, "MsgType")?;
        if !msg_type.eq_ignore_ascii_case("text") {
            return None;
        }
        let from_user = xml_element_text(xml, "FromUserName")?;
        if from_user.trim().is_empty() {
            return None;
        }
        let content = xml_element_text(xml, "Content")?;
        let msg = Msg::builder()
            .role(MsgRole::User)
            .name(from_user.clone())
            .text_content(content)
            .metadata_entry(
                "channelMessageId",
                Value::String(Self::extract_msg_id(xml).unwrap_or_default()),
            )
            .build();
        let mut builder = InboundMessage::builder(self.channel_id.clone(), Peer::direct(from_user.clone()), vec![msg])
            .sender_id(from_user);
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

    const TEXT_XML: &str = "<xml><ToUserName>corp</ToUserName><FromUserName>user-1</FromUserName>\
<CreateTime>1348831860</CreateTime><MsgType>text</MsgType>\
<Content>hello &amp; goodbye</Content><MsgId>1234567890123456</MsgId><AgentID>1</AgentID></xml>";

    fn mapper() -> WeComInboundMapper {
        WeComInboundMapper::new("wecom", Some("corp-1".to_string()))
    }

    #[test]
    fn maps_text_message_to_direct_peer() {
        let inbound = mapper().map(TEXT_XML).expect("mapped");
        assert_eq!(inbound.channel_id, "wecom");
        assert_eq!(inbound.account_id.as_deref(), Some("corp-1"));
        assert_eq!(inbound.peer.kind, PeerKind::Direct);
        assert_eq!(inbound.peer.id, "user-1");
        assert_eq!(inbound.sender_id.as_deref(), Some("user-1"));
        assert_eq!(inbound.messages[0].text_content(), "hello & goodbye");
        assert_eq!(
            WeComInboundMapper::extract_msg_id(TEXT_XML).as_deref(),
            Some("1234567890123456")
        );
    }

    #[test]
    fn drops_non_text_and_malformed_payloads() {
        let m = mapper();
        let event_xml = "<xml><FromUserName>u</FromUserName><MsgType>event</MsgType><Event>subscribe</Event></xml>";
        assert!(m.map(event_xml).is_none());
        assert!(m.map("<xml><MsgType>text</MsgType></xml>").is_none());
        assert!(m.map("not xml at all").is_none());
        assert!(m
            .map("<xml><FromUserName>u</FromUserName><MsgType>text</MsgType><Content>x</Content></xml>")
            .is_some());
    }
}
