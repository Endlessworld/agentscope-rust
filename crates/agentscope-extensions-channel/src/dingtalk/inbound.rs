//! Parses a DingTalk Stream bot-message payload into a harness `InboundMessage`.
//!
//! Rust port of `DingTalkInboundMapper`. Text (`msgtype=text`) maps to its content; media
//! (`picture` / `audio` / `video` / `file`) maps to a neutral marker plus the
//! [`crate::common::media`] contract, so the application can fetch the material itself. Other
//! payload kinds yield `None` so the caller can ack without dispatching.

use agentscope_core::message::{JsonMap, Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer};
use serde_json::Value;

use crate::common::media::{self, marker_text, put_if_present};
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

    /// Build the inbound message, or `None` when the payload is not dispatchable.
    ///
    /// Text messages map to their content; media messages map to a neutral marker plus the media
    /// metadata contract. Note the platform delivers audio/video/file to the robot in single chat
    /// only, not on group @-mention.
    pub fn map(&self, payload: &Value) -> Option<InboundMessage> {
        if payload.is_null() {
            return None;
        }
        let msg_type = get_str(payload, "msgtype")?;
        let (content, mut metadata) = if msg_type.eq_ignore_ascii_case("text") {
            let text = payload
                .get("text")
                .and_then(|t| t.get("content"))
                .and_then(|c| c.as_str())?
                .trim()
                .to_string();
            if text.is_empty() {
                return None;
            }
            (text, JsonMap::new())
        } else {
            Self::map_media(payload, &msg_type)?
        };

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
        metadata.insert(
            "channelMessageId".to_string(),
            Value::String(channel_message_id),
        );
        let msg = Msg::builder()
            .role(MsgRole::User)
            .name(sender_id.clone())
            .text_content(content)
            .metadata(metadata)
            .build();

        let mut builder =
            InboundMessage::builder(self.channel_id.clone(), peer, vec![msg]).sender_id(sender_id);
        if let Some(account_id) = &self.account_id {
            builder = builder.account_id(account_id.clone());
        }
        Some(builder.build())
    }

    /// Map a media message to a neutral marker plus the media metadata contract.
    ///
    /// Fields are read from the payload top level first and from the nested `content` object as a
    /// fallback — protocol revisions differ in field placement. `picture` carries a `downloadCode`
    /// (plus `pictureDownloadCode`); `audio` adds `recognition` (platform speech-to-text) and
    /// `duration`; `video` adds `videoType` and `duration` (both documented in milliseconds);
    /// `file` adds `fileName`.
    fn map_media(payload: &Value, msg_type: &str) -> Option<(String, JsonMap)> {
        let kind = match msg_type.to_ascii_lowercase().as_str() {
            "picture" => media::KIND_IMAGE,
            "audio" => media::KIND_AUDIO,
            "video" => media::KIND_VIDEO,
            "file" => media::KIND_FILE,
            _ => return None,
        };
        let download_code = Self::media_field(payload, "downloadCode")?;
        if download_code.trim().is_empty() {
            return None;
        }
        let mut metadata = JsonMap::new();
        metadata.insert(media::KIND.to_string(), Value::String(kind.to_string()));
        metadata.insert(
            media::PROVIDER_TYPE.to_string(),
            Value::String(msg_type.to_string()),
        );
        metadata.insert(media::ID.to_string(), Value::String(download_code));
        let mut file_name: Option<String> = None;
        if kind == media::KIND_IMAGE {
            put_if_present(
                &mut metadata,
                media::SECONDARY_ID,
                Self::media_field(payload, "pictureDownloadCode").as_deref(),
            );
        } else if kind == media::KIND_AUDIO {
            put_if_present(
                &mut metadata,
                media::RECOGNITION,
                Self::media_field(payload, "recognition").as_deref(),
            );
            put_if_present(
                &mut metadata,
                media::DURATION_MS,
                Self::media_field(payload, "duration").as_deref(),
            );
        } else if kind == media::KIND_VIDEO {
            put_if_present(
                &mut metadata,
                media::FORMAT,
                Self::media_field(payload, "videoType").as_deref(),
            );
            put_if_present(
                &mut metadata,
                media::DURATION_MS,
                Self::media_field(payload, "duration").as_deref(),
            );
        } else {
            file_name = Self::media_field(payload, "fileName");
            put_if_present(&mut metadata, media::FILE_NAME, file_name.as_deref());
        }
        Some((marker_text(kind, file_name.as_deref()), metadata))
    }

    /// Read a media field from the payload top level, falling back to the nested `content` object.
    fn media_field(payload: &Value, field: &str) -> Option<String> {
        get_str(payload, field).or_else(|| payload.get("content").and_then(|c| get_str(c, field)))
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

    #[test]
    fn maps_picture_to_the_media_contract() {
        let payload = serde_json::json!({
            "msgtype": "picture",
            "conversationType": "1",
            "senderStaffId": "staff-7",
            "msgId": "msg-9",
            "content": {
                "downloadCode": "dc-1",
                "pictureDownloadCode": "pic-dc"
            }
        });
        let inbound = mapper().map(&payload).expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[image]");
        let metadata = inbound.messages[0].metadata();
        assert_eq!(metadata.get("channelMediaKind"), Some(&serde_json::json!("image")));
        assert_eq!(
            metadata.get("channelMediaProviderType"),
            Some(&serde_json::json!("picture"))
        );
        assert_eq!(metadata.get("channelMediaId"), Some(&serde_json::json!("dc-1")));
        assert_eq!(
            metadata.get("channelMediaSecondaryId"),
            Some(&serde_json::json!("pic-dc"))
        );
        assert_eq!(metadata.get("channelMessageId"), Some(&serde_json::json!("msg-9")));
    }

    #[test]
    fn maps_audio_and_video_top_level_fields() {
        let audio = serde_json::json!({
            "msgtype": "audio",
            "conversationType": "1",
            "senderStaffId": "s",
            "downloadCode": "dc-a",
            "recognition": "hello there",
            "duration": 4200
        });
        let inbound = mapper().map(&audio).expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[audio]");
        let metadata = inbound.messages[0].metadata();
        assert_eq!(
            metadata.get("channelMediaRecognition"),
            Some(&serde_json::json!("hello there"))
        );
        assert_eq!(
            metadata.get("channelMediaDurationMs"),
            Some(&serde_json::json!("4200"))
        );

        let video = serde_json::json!({
            "msgtype": "video",
            "conversationType": "1",
            "senderStaffId": "s",
            "downloadCode": "dc-v",
            "videoType": "mp4",
            "duration": 9000
        });
        let inbound = mapper().map(&video).expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[video]");
        let metadata = inbound.messages[0].metadata();
        assert_eq!(metadata.get("channelMediaFormat"), Some(&serde_json::json!("mp4")));
        assert_eq!(
            metadata.get("channelMediaDurationMs"),
            Some(&serde_json::json!("9000"))
        );
    }

    #[test]
    fn maps_file_with_marker_and_raw_file_name() {
        let payload = serde_json::json!({
            "msgtype": "file",
            "conversationType": "1",
            "senderStaffId": "s",
            "downloadCode": "dc-f",
            "fileName": "report.pdf"
        });
        let inbound = mapper().map(&payload).expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[file: report.pdf]");
        assert_eq!(
            inbound.messages[0].metadata().get("channelMediaFileName"),
            Some(&serde_json::json!("report.pdf"))
        );
    }

    #[test]
    fn media_without_a_download_code_is_dropped() {
        let m = mapper();
        let payload = serde_json::json!({
            "msgtype": "picture",
            "conversationType": "1",
            "senderStaffId": "s"
        });
        assert!(m.map(&payload).is_none());
        let payload = serde_json::json!({
            "msgtype": "picture",
            "conversationType": "1",
            "senderStaffId": "s",
            "downloadCode": "   "
        });
        assert!(m.map(&payload).is_none());
    }

    #[test]
    fn unmapped_message_types_are_still_dropped() {
        let payload = serde_json::json!({
            "msgtype": "richText",
            "conversationType": "1",
            "senderStaffId": "s",
            "downloadCode": "dc"
        });
        assert!(mapper().map(&payload).is_none());
    }
}
