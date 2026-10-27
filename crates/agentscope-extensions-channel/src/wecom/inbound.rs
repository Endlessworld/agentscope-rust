//! Parses a decrypted WeCom callback XML body into a harness `InboundMessage`.
//!
//! Rust port of `WeComInboundMapper`. Text (`MsgType=text`) maps to its content; media
//! (`image` / `voice` / `video`) maps to a neutral marker plus the [`crate::common::media`]
//! contract; other inbound types (event, location, link, ...) yield `None` so the caller can ack
//! the webhook without dispatching.

use agentscope_core::message::{JsonMap, Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer};
use serde_json::Value;

use crate::common::media::{self, marker_text, put_if_present};
use crate::common::xml_element_text;

/// Maps WeCom callback XML into [`InboundMessage`]s.
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

    /// Map the decrypted XML; `None` for unmapped types or unparseable payloads.
    ///
    /// Note: the Java mapper throws on malformed XML; this adapter is non-panicking and returns
    /// `None` instead (see tracker §4 design deviations).
    pub fn map(&self, xml: &str) -> Option<InboundMessage> {
        let msg_type = xml_element_text(xml, "MsgType")?;
        let (from_user, content, mut metadata) = if msg_type.eq_ignore_ascii_case("text") {
            let from_user = xml_element_text(xml, "FromUserName")?;
            if from_user.trim().is_empty() {
                return None;
            }
            let content = xml_element_text(xml, "Content")?;
            (from_user, content, JsonMap::new())
        } else {
            Self::map_media(xml, &msg_type)?
        };
        metadata.insert(
            "channelMessageId".to_string(),
            Value::String(Self::extract_msg_id(xml).unwrap_or_default()),
        );
        let msg = Msg::builder()
            .role(MsgRole::User)
            .name(from_user.clone())
            .text_content(content)
            .metadata(metadata)
            .build();
        let mut builder = InboundMessage::builder(
            self.channel_id.clone(),
            Peer::direct(from_user.clone()),
            vec![msg],
        )
        .sender_id(from_user);
        if let Some(account_id) = &self.account_id {
            builder = builder.account_id(account_id.clone());
        }
        Some(builder.build())
    }

    /// Map a media message to a neutral marker plus the media metadata contract. `image` carries
    /// `PicUrl` + `MediaId`; `voice` carries `MediaId` + `Format`; `video` carries `MediaId` +
    /// `ThumbMediaId`. Returns `(from_user, marker_text, metadata)`.
    fn map_media(xml: &str, msg_type: &str) -> Option<(String, String, JsonMap)> {
        let kind = match msg_type.to_ascii_lowercase().as_str() {
            "image" => media::KIND_IMAGE,
            "voice" => media::KIND_AUDIO,
            "video" => media::KIND_VIDEO,
            _ => return None,
        };
        let from_user = xml_element_text(xml, "FromUserName")?;
        if from_user.trim().is_empty() {
            return None;
        }
        let media_id = xml_element_text(xml, "MediaId")?;
        if media_id.trim().is_empty() {
            return None;
        }
        let mut metadata = JsonMap::new();
        metadata.insert(media::KIND.to_string(), Value::String(kind.to_string()));
        metadata.insert(
            media::PROVIDER_TYPE.to_string(),
            Value::String(msg_type.to_string()),
        );
        metadata.insert(media::ID.to_string(), Value::String(media_id));
        if kind == media::KIND_IMAGE {
            put_if_present(
                &mut metadata,
                media::URL,
                xml_element_text(xml, "PicUrl").as_deref(),
            );
        } else if kind == media::KIND_AUDIO {
            put_if_present(
                &mut metadata,
                media::FORMAT,
                xml_element_text(xml, "Format").as_deref(),
            );
        } else {
            put_if_present(
                &mut metadata,
                media::SECONDARY_ID,
                xml_element_text(xml, "ThumbMediaId").as_deref(),
            );
        }
        Some((from_user, marker_text(kind, None), metadata))
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

    #[test]
    fn maps_image_with_url() {
        let xml = "<xml><FromUserName>user-1</FromUserName><MsgType>image</MsgType>\
<PicUrl>https://example.test/p.jpg</PicUrl><MediaId>media-1</MediaId>\
<MsgId>9001</MsgId></xml>";
        let inbound = mapper().map(xml).expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[image]");
        let metadata = inbound.messages[0].metadata();
        assert_eq!(
            metadata.get("channelMediaKind"),
            Some(&serde_json::json!("image"))
        );
        assert_eq!(
            metadata.get("channelMediaProviderType"),
            Some(&serde_json::json!("image"))
        );
        assert_eq!(
            metadata.get("channelMediaId"),
            Some(&serde_json::json!("media-1"))
        );
        assert_eq!(
            metadata.get("channelMediaUrl"),
            Some(&serde_json::json!("https://example.test/p.jpg"))
        );
        assert_eq!(
            metadata.get("channelMessageId"),
            Some(&serde_json::json!("9001"))
        );
    }

    #[test]
    fn maps_voice_and_video() {
        let voice = "<xml><FromUserName>u</FromUserName><MsgType>voice</MsgType>\
<MediaId>m-v</MediaId><Format>amr</Format><MsgId>1</MsgId></xml>";
        let inbound = mapper().map(voice).expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[audio]");
        assert_eq!(
            inbound.messages[0].metadata().get("channelMediaFormat"),
            Some(&serde_json::json!("amr"))
        );

        let video = "<xml><FromUserName>u</FromUserName><MsgType>video</MsgType>\
<MediaId>m-vid</MediaId><ThumbMediaId>thumb-1</ThumbMediaId><MsgId>2</MsgId></xml>";
        let inbound = mapper().map(video).expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[video]");
        assert_eq!(
            inbound.messages[0]
                .metadata()
                .get("channelMediaSecondaryId"),
            Some(&serde_json::json!("thumb-1"))
        );
    }

    #[test]
    fn media_requires_a_from_user_and_a_media_id() {
        let m = mapper();
        assert!(m
            .map("<xml><MsgType>image</MsgType><MediaId>m</MediaId></xml>")
            .is_none());
        assert!(m
            .map("<xml><FromUserName>u</FromUserName><MsgType>image</MsgType></xml>")
            .is_none());
        assert!(m
            .map("<xml><FromUserName>u</FromUserName><MsgType>image</MsgType><MediaId> </MediaId></xml>")
            .is_none());
        // Unmapped inbound types stay dropped.
        assert!(m
            .map("<xml><FromUserName>u</FromUserName><MsgType>location</MsgType></xml>")
            .is_none());
    }
}
