//! Maps a GitHub webhook delivery into a harness `InboundMessage`.
//!
//! MVP handles `issue_comment` (issues AND PRs) and `pull_request_review_comment`.

use agentscope_core::message::{Msg, MsgRole};
use agentscope_harness::agent::{InboundMessage, Peer, PeerKind};
use serde_json::Value;

/// Maps GitHub webhook payloads into [`InboundMessage`]s.
#[derive(Debug, Clone)]
pub struct GitHubInboundMapper {
    channel_id: String,
}

impl GitHubInboundMapper {
    pub fn new(channel_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// Returns `comment.id`, used as the idempotency key.
    pub fn extract_comment_id(payload: &Value) -> Option<i64> {
        payload
            .pointer("/comment/id")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
    }

    /// Returns `comment.user.id`, used for bot-loop self-detection.
    pub fn extract_commenter_id(payload: &Value) -> Option<i64> {
        payload
            .pointer("/comment/user/id")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
    }

    /// Map a webhook delivery; `None` for unsupported actions or malformed payloads.
    pub fn map(&self, event_type: &str, payload: &Value) -> Option<InboundMessage> {
        if payload.is_null() {
            return None;
        }
        if payload.get("action").and_then(|v| v.as_str()) != Some("created") {
            return None;
        }
        let repo = payload.get("repository")?;
        let full_name = repo
            .get("full_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())?;
        let owner_login = repo
            .pointer("/owner/login")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let number = match event_type {
            "issue_comment" => payload.pointer("/issue/number").and_then(|v| v.as_i64()),
            "pull_request_review_comment" => {
                payload.pointer("/pull_request/number").and_then(|v| v.as_i64())
            }
            _ => return None,
        }
        .filter(|n| *n > 0)?;

        let body = payload
            .pointer("/comment/body")
            .and_then(|v| v.as_str())?
            .to_string();
        let author_login = payload
            .pointer("/comment/user/login")
            .and_then(|v| v.as_str())?
            .to_string();

        let peer_id = format!("{full_name}#{number}");
        let msg = Msg::builder()
            .role(MsgRole::User)
            .name(author_login.clone())
            .text_content(body)
            .build();
        let mut builder =
            InboundMessage::builder(self.channel_id.clone(), Peer::new(PeerKind::Thread, peer_id), vec![msg])
                .sender_id(author_login);
        if let Some(owner) = owner_login {
            builder = builder.account_id(owner);
        }
        Some(builder.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_comment() -> Value {
        serde_json::json!({
            "action": "created",
            "repository": {"full_name": "octo/repo", "owner": {"login": "octo"}},
            "issue": {"number": 7},
            "comment": {"id": 555, "body": "please fix", "user": {"login": "alice", "id": 42}}
        })
    }

    #[test]
    fn maps_issue_comment_to_thread_peer() {
        let inbound = GitHubInboundMapper::new("github")
            .map("issue_comment", &issue_comment())
            .expect("mapped");
        assert_eq!(inbound.peer.kind, PeerKind::Thread);
        assert_eq!(inbound.peer.id, "octo/repo#7");
        assert_eq!(inbound.account_id.as_deref(), Some("octo"));
        assert_eq!(inbound.sender_id.as_deref(), Some("alice"));
        assert_eq!(inbound.messages[0].text_content(), "please fix");
        assert_eq!(GitHubInboundMapper::extract_comment_id(&issue_comment()), Some(555));
        assert_eq!(
            GitHubInboundMapper::extract_commenter_id(&issue_comment()),
            Some(42)
        );
    }

    #[test]
    fn maps_review_comment_using_pull_request_number() {
        let payload = serde_json::json!({
            "action": "created",
            "repository": {"full_name": "octo/repo", "owner": {"login": "octo"}},
            "pull_request": {"number": 12},
            "comment": {"id": 1, "body": "nit", "user": {"login": "bob"}}
        });
        let inbound = GitHubInboundMapper::new("github")
            .map("pull_request_review_comment", &payload)
            .unwrap();
        assert_eq!(inbound.peer.id, "octo/repo#12");
    }

    #[test]
    fn drops_non_created_actions_and_unknown_events() {
        let m = GitHubInboundMapper::new("github");
        let mut payload = issue_comment();
        payload["action"] = serde_json::json!("edited");
        assert!(m.map("issue_comment", &payload).is_none());
        assert!(m.map("push", &issue_comment()).is_none());
    }

    #[test]
    fn drops_payloads_missing_required_fields() {
        let m = GitHubInboundMapper::new("github");
        let mut payload = issue_comment();
        payload["repository"] = serde_json::json!({});
        assert!(m.map("issue_comment", &payload).is_none());

        let mut payload = issue_comment();
        payload["issue"] = serde_json::json!({});
        assert!(m.map("issue_comment", &payload).is_none());

        let mut payload = issue_comment();
        payload["comment"] = serde_json::json!({"id": 1});
        assert!(m.map("issue_comment", &payload).is_none());

        assert!(m.map("issue_comment", &Value::Null).is_none());
    }
}
