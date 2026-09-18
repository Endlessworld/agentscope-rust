//! Maps a GitLab Note Hook payload into a harness `InboundMessage`.
//!
//! Only Issue and MergeRequest notes are mapped; system notes are dropped.

use agentscope_core::message::{Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer, PeerKind};
use serde_json::Value;

/// Maps GitLab Note Hook payloads into [`InboundMessage`]s.
#[derive(Debug, Clone)]
pub struct GitLabInboundMapper {
    channel_id: String,
}

impl GitLabInboundMapper {
    pub fn new(channel_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// Returns `object_attributes.id`, used as the idempotency key.
    pub fn extract_note_id(payload: &Value) -> Option<i64> {
        payload
            .pointer("/object_attributes/id")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
    }

    /// Returns `user.id`, used for bot-loop self-detection.
    pub fn extract_author_id(payload: &Value) -> Option<i64> {
        payload
            .pointer("/user/id")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
    }

    /// Map a Note Hook; `None` for unsupported noteable types, system notes, or malformed payloads.
    pub fn map(&self, payload: &Value) -> Option<InboundMessage> {
        if payload.is_null() {
            return None;
        }
        let attrs = payload.get("object_attributes")?;
        if !attrs.is_object() {
            return None;
        }
        if attrs.get("system").and_then(|v| v.as_bool()).unwrap_or(false) {
            return None;
        }
        let noteable_type = attrs.get("noteable_type").and_then(|v| v.as_str())?;
        if noteable_type != "Issue" && noteable_type != "MergeRequest" {
            return None;
        }

        let iid = if noteable_type == "Issue" {
            payload.pointer("/issue/iid").and_then(|v| v.as_i64())
        } else {
            payload
                .pointer("/merge_request/iid")
                .and_then(|v| v.as_i64())
        }
        .filter(|n| *n > 0)?;

        let path_with_namespace = payload
            .pointer("/project/path_with_namespace")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())?;
        let namespace = payload
            .pointer("/project/namespace")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let note = attrs.get("note").and_then(|v| v.as_str())?.to_string();
        let username = payload
            .pointer("/user/username")
            .and_then(|v| v.as_str())?
            .to_string();

        let peer_id = format!("{path_with_namespace}#{iid}:{noteable_type}");
        let msg = Msg::builder()
            .role(MsgRole::User)
            .name(username.clone())
            .text_content(note)
            .build();
        let mut builder =
            InboundMessage::builder(self.channel_id.clone(), Peer::new(PeerKind::Thread, peer_id), vec![msg])
                .sender_id(username);
        if let Some(namespace) = namespace {
            builder = builder.account_id(namespace);
        }
        Some(builder.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_hook() -> Value {
        serde_json::json!({
            "object_attributes": {"id": 900, "note": "looks good", "noteable_type": "Issue", "system": false},
            "project": {"path_with_namespace": "grp/proj", "namespace": "grp"},
            "issue": {"iid": 9},
            "user": {"username": "alice", "id": 3}
        })
    }

    #[test]
    fn maps_issue_note_to_thread_peer() {
        let inbound = GitLabInboundMapper::new("gitlab")
            .map(&note_hook())
            .expect("mapped");
        assert_eq!(inbound.peer.kind, PeerKind::Thread);
        assert_eq!(inbound.peer.id, "grp/proj#9:Issue");
        assert_eq!(inbound.account_id.as_deref(), Some("grp"));
        assert_eq!(inbound.sender_id.as_deref(), Some("alice"));
        assert_eq!(inbound.messages[0].text_content(), "looks good");
    }

    #[test]
    fn maps_merge_request_note() {
        let payload = serde_json::json!({
            "object_attributes": {"id": 1, "note": "nit", "noteable_type": "MergeRequest", "system": false},
            "project": {"path_with_namespace": "grp/proj", "namespace": "grp"},
            "merge_request": {"iid": 42},
            "user": {"username": "bob"}
        });
        let inbound = GitLabInboundMapper::new("gitlab").map(&payload).unwrap();
        assert_eq!(inbound.peer.id, "grp/proj#42:MergeRequest");
    }

    #[test]
    fn drops_system_notes_and_unsupported_types() {
        let m = GitLabInboundMapper::new("gitlab");
        let mut payload = note_hook();
        payload["object_attributes"]["system"] = serde_json::json!(true);
        assert!(m.map(&payload).is_none());

        let mut payload = note_hook();
        payload["object_attributes"]["noteable_type"] = serde_json::json!("Commit");
        assert!(m.map(&payload).is_none());
    }

    #[test]
    fn drops_payloads_missing_required_fields() {
        let m = GitLabInboundMapper::new("gitlab");
        let mut payload = note_hook();
        payload["project"] = serde_json::json!({});
        assert!(m.map(&payload).is_none());

        let mut payload = note_hook();
        payload["issue"] = serde_json::json!({});
        assert!(m.map(&payload).is_none());

        let mut payload = note_hook();
        payload["user"] = serde_json::json!({});
        assert!(m.map(&payload).is_none());

        assert!(m.map(&Value::Null).is_none());
    }

    #[test]
    fn extracts_idempotency_and_author_ids() {
        let payload = note_hook();
        assert_eq!(GitLabInboundMapper::extract_note_id(&payload), Some(900));
        assert_eq!(GitLabInboundMapper::extract_author_id(&payload), Some(3));
    }
}
