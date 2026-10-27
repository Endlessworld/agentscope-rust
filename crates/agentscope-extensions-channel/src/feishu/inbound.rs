//! Parses a decrypted Feishu Event Subscription v2 envelope into a harness `InboundMessage`.
//!
//! Text messages (`message_type=text`) map to their content; media messages (`image` / `file` /
//! `folder` / `audio` / `media`) map to a neutral marker plus the [`crate::common::media`]
//! contract; other message types (post, sticker, interactive, share cards, ...) yield `None` so
//! the caller can ack without dispatching.

use agentscope_core::message::{JsonMap, Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer, PeerKind};
use serde_json::Value;

use crate::common::media::{self, marker_text, put_if_present};

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

    /// Map a decrypted Schema 2.0 envelope; `None` for unmapped / malformed events.
    pub fn map(&self, envelope: &Value) -> Option<InboundMessage> {
        let event = envelope.get("event")?;
        if !event.is_object() {
            return None;
        }
        let message = event.get("message")?;
        let message_type = message.get("message_type").and_then(|v| v.as_str())?;
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
        let sender_type = event
            .pointer("/sender/sender_type")
            .and_then(|v| v.as_str());
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

        let mut metadata = JsonMap::new();
        metadata.insert("channelMessageId".to_string(), Value::String(message_id));
        metadata.insert(
            "channelReplyToId".to_string(),
            Value::String(
                message
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
        );
        metadata.insert(
            "channelThreadId".to_string(),
            Value::String(
                message
                    .get("root_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
        );

        let text = if message_type.eq_ignore_ascii_case("text") {
            let content_json = message
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())?;
            let content: Value = serde_json::from_str(content_json).ok()?;
            let mut text = content
                .get("text")
                .and_then(|v| v.as_str())?
                .trim()
                .to_string();
            // Providers encode mentions as structured keys. Strip leading mention keys so an
            // addressed bot can receive explicit work commands in group chats.
            if let Some(mentions) = message.get("mentions").and_then(|v| v.as_array()) {
                for mention in mentions {
                    let key = mention.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    if !key.is_empty() && text.starts_with(key) {
                        text = text[key.len()..].trim_start().to_string();
                    }
                }
            }
            text
        } else {
            Self::map_media_content(message, message_type, &mut metadata)?
        };

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
            .metadata(metadata)
            .build();

        let mut builder =
            InboundMessage::builder(self.channel_id.clone(), Peer::new(kind, chat_id), vec![msg])
                .sender_id(sender_name);
        if let Some(tenant) = Self::extract_tenant_key(envelope) {
            builder = builder.account_id(tenant);
        }
        Some(builder.build())
    }

    /// Map the content of a media message into the media metadata contract and return the neutral
    /// marker text, or `None` when the type is unmapped or the material identifier is missing.
    ///
    /// `image` keys on `image_key`; `file`, `folder`, `audio` and `media` key on `file_key`;
    /// `media` is a video-bearing composite whose `image_key` is the cover. A folder's `file_key`
    /// identifies the folder node itself (the message-resource download API serves image / file /
    /// audio / video resources only), so it is a reference id rather than fetchable bytes.
    fn map_media_content(
        message: &Value,
        message_type: &str,
        metadata: &mut JsonMap,
    ) -> Option<String> {
        let kind = match message_type.to_ascii_lowercase().as_str() {
            "image" => media::KIND_IMAGE,
            "file" | "folder" => media::KIND_FILE,
            "audio" => media::KIND_AUDIO,
            "media" => media::KIND_VIDEO,
            _ => return None,
        };
        let content_json = message
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())?;
        let content: Value = serde_json::from_str(content_json).ok()?;
        let id_key = if kind == media::KIND_IMAGE {
            "image_key"
        } else {
            "file_key"
        };
        let id = content.get(id_key).and_then(|v| v.as_str())?;
        if id.trim().is_empty() {
            return None;
        }
        metadata.insert(media::KIND.to_string(), Value::String(kind.to_string()));
        metadata.insert(
            media::PROVIDER_TYPE.to_string(),
            Value::String(message_type.to_string()),
        );
        metadata.insert(media::ID.to_string(), Value::String(id.to_string()));
        let cover = content.get("image_key").and_then(|v| v.as_str());
        if kind == media::KIND_VIDEO {
            put_if_present(metadata, media::SECONDARY_ID, cover);
        }
        if kind == media::KIND_AUDIO || kind == media::KIND_VIDEO {
            let duration = content.get("duration").and_then(as_text);
            put_if_present(metadata, media::DURATION_MS, duration.as_deref());
        }
        let file_name = content.get("file_name").and_then(|v| v.as_str());
        put_if_present(metadata, media::FILE_NAME, file_name);
        Some(marker_text(kind, file_name))
    }
}

/// Java `JsonNode.asText()` equivalent for the scalar shapes a media content object carries.
fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
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
        let inbound = FeishuInboundMapper::new("feishu")
            .map(&env)
            .expect("mapped");
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
        assert_eq!(
            FeishuInboundMapper::extract_url_challenge(&serde_json::json!({})),
            None
        );
    }

    fn media_envelope(message_type: &str, content: &str) -> Value {
        serde_json::json!({
            "schema": "2.0",
            "header": {"event_id": "ev-2", "tenant_key": "tenant-9"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_abc"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_2",
                    "chat_id": "oc_2",
                    "chat_type": "p2p",
                    "message_type": message_type,
                    "content": content
                }
            }
        })
    }

    #[test]
    fn maps_image_media() {
        let inbound = FeishuInboundMapper::new("feishu")
            .map(&media_envelope("image", r#"{"image_key":"img-1"}"#))
            .expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[image]");
        let metadata = inbound.messages[0].metadata();
        assert_eq!(
            metadata.get("channelMediaKind"),
            Some(&serde_json::json!("image"))
        );
        assert_eq!(
            metadata.get("channelMediaId"),
            Some(&serde_json::json!("img-1"))
        );
        assert_eq!(
            metadata.get("channelMediaProviderType"),
            Some(&serde_json::json!("image"))
        );
        assert_eq!(
            metadata.get("channelMessageId"),
            Some(&serde_json::json!("om_2"))
        );
    }

    #[test]
    fn maps_video_composite_with_cover_duration_and_name() {
        let inbound = FeishuInboundMapper::new("feishu")
            .map(&media_envelope(
                "media",
                r#"{"file_key":"v-1","image_key":"cover","duration":12000,"file_name":"clip.mp4"}"#,
            ))
            .expect("mapped");
        assert_eq!(inbound.messages[0].text_content(), "[video: clip.mp4]");
        let metadata = inbound.messages[0].metadata();
        assert_eq!(
            metadata.get("channelMediaKind"),
            Some(&serde_json::json!("video"))
        );
        assert_eq!(
            metadata.get("channelMediaId"),
            Some(&serde_json::json!("v-1"))
        );
        assert_eq!(
            metadata.get("channelMediaSecondaryId"),
            Some(&serde_json::json!("cover"))
        );
        assert_eq!(
            metadata.get("channelMediaDurationMs"),
            Some(&serde_json::json!("12000"))
        );
        assert_eq!(
            metadata.get("channelMediaFileName"),
            Some(&serde_json::json!("clip.mp4"))
        );
    }

    #[test]
    fn maps_audio_and_folder_to_the_media_contract() {
        let audio = FeishuInboundMapper::new("feishu")
            .map(&media_envelope(
                "audio",
                r#"{"file_key":"a-1","duration":3000}"#,
            ))
            .expect("mapped");
        assert_eq!(audio.messages[0].text_content(), "[audio]");
        assert_eq!(
            audio.messages[0].metadata().get("channelMediaDurationMs"),
            Some(&serde_json::json!("3000"))
        );

        let folder = FeishuInboundMapper::new("feishu")
            .map(&media_envelope("folder", r#"{"file_key":"f-1"}"#))
            .expect("mapped");
        assert_eq!(folder.messages[0].text_content(), "[file]");
        assert_eq!(
            folder.messages[0].metadata().get("channelMediaKind"),
            Some(&serde_json::json!("file"))
        );
    }

    #[test]
    fn media_without_a_material_id_is_dropped() {
        let m = FeishuInboundMapper::new("feishu");
        assert!(m
            .map(&media_envelope("image", r#"{"file_key":"x"}"#))
            .is_none());
        assert!(m
            .map(&media_envelope("file", r#"{"file_key":" "}"#))
            .is_none());
        assert!(m
            .map(&media_envelope("sticker", r#"{"file_key":"x"}"#))
            .is_none());
        assert!(m.map(&media_envelope("image", "not-json")).is_none());
    }
}
