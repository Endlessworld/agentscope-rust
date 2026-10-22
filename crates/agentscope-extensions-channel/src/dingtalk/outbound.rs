//! DingTalk outbound HTTP client: request construction + execution.
//!
//! Rust port of `DingTalkOutboundClient`. Wraps
//! `POST /v1.0/robot/oToMessages/batchSend` (DM) and
//! `POST /v1.0/robot/groupMessages/send` (group).

use std::sync::Arc;

use agentscope_core::message::Msg;
use agentscope_harness::agent::{OutboundAddress, PeerKind};
use serde_json::{json, Value};

use crate::common::{
    looks_like_markdown, parse_peer_target, AccessTokenProvider, ChannelError, HttpMethod,
    HttpTransport, PeerTarget, PreparedRequest, Result,
};

/// DingTalk outbound client. The production `WebClient` is replaced by a pluggable
/// [`HttpTransport`], keeping request construction unit-testable without network access.
pub struct DingTalkOutboundClient {
    api_base: String,
    robot_code: String,
    token_provider: Arc<dyn AccessTokenProvider>,
    transport: Arc<dyn HttpTransport>,
}

impl DingTalkOutboundClient {
    pub fn new(
        api_base: impl Into<String>,
        token_provider: Arc<dyn AccessTokenProvider>,
        robot_code: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            api_base: api_base.into(),
            robot_code: robot_code.into(),
            token_provider,
            transport,
        }
    }

    /// Build the outbound requests for `messages`, without executing them.
    pub fn prepare(
        &self,
        token: &str,
        address: &OutboundAddress,
        messages: &[Msg],
    ) -> Result<Vec<PreparedRequest>> {
        let target = parse_peer_target(&address.to);
        let mut requests = Vec::new();
        for msg in messages {
            if let Some(req) = self.prepare_one(token, &target, msg)? {
                requests.push(req);
            }
        }
        Ok(requests)
    }

    fn prepare_one(
        &self,
        token: &str,
        target: &PeerTarget,
        msg: &Msg,
    ) -> Result<Option<PreparedRequest>> {
        let text = msg.text_content();
        if text.trim().is_empty() {
            return Ok(None);
        }
        let use_markdown = looks_like_markdown(&text);
        let msg_key = if use_markdown {
            "sampleMarkdown"
        } else {
            "sampleText"
        };
        let param: Value = if use_markdown {
            json!({ "title": "Agent", "text": text })
        } else {
            json!({ "content": text })
        };
        let msg_param = serde_json::to_string(&param)?;

        let mut body = serde_json::Map::new();
        body.insert("robotCode".to_string(), json!(self.robot_code));
        body.insert("msgKey".to_string(), json!(msg_key));
        body.insert("msgParam".to_string(), json!(msg_param));

        let path = match target.kind {
            PeerKind::Group => {
                body.insert("openConversationId".to_string(), json!(target.id));
                "/v1.0/robot/groupMessages/send"
            }
            PeerKind::Direct => {
                body.insert("userIds".to_string(), json!([target.id]));
                "/v1.0/robot/oToMessages/batchSend"
            }
            other => {
                return Err(ChannelError::Unsupported(format!(
                    "DingTalk outbound only supports DIRECT and GROUP peer kinds, got {other:?}"
                )))
            }
        };
        let url = format!("{}{}", self.api_base.trim_end_matches('/'), path);
        Ok(Some(
            PreparedRequest::post_json(url, Value::Object(body))
                .header("x-acs-dingtalk-access-token", token),
        ))
    }

    /// Fetch a token, build the requests and execute them in order.
    pub async fn send(&self, address: &OutboundAddress, messages: &[Msg]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let token = self.token_provider.token().await?;
        for request in self.prepare(&token, address, messages)? {
            let response = self.transport.execute(request).await?;
            if !response.is_success()
                || response.body.contains("AccessToken")
                || response.body.contains("token")
            {
                self.token_provider.invalidate();
            }
        }
        Ok(())
    }

    /// The HTTP verb used for outbound sends.
    pub fn method() -> HttpMethod {
        HttpMethod::Post
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::{RecordingTransport, StaticTokenProvider};

    fn client(transport: Arc<RecordingTransport>) -> DingTalkOutboundClient {
        DingTalkOutboundClient::new(
            "https://api.dingtalk.com",
            Arc::new(StaticTokenProvider::new("tok-1")),
            "robot-1",
            transport,
        )
    }

    #[test]
    fn direct_send_uses_otousers_endpoint_and_plain_text() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport);
        let address = OutboundAddress::direct("dingtalk", "dingtalk:DIRECT:staff-7");
        let reqs = c
            .prepare("tok", &address, &[Msg::user("plain reply")])
            .unwrap();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(
            req.url,
            "https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend"
        );
        assert_eq!(req.header_value("x-acs-dingtalk-access-token"), Some("tok"));
        let body = req.body.as_ref().unwrap();
        assert_eq!(body["robotCode"], json!("robot-1"));
        assert_eq!(body["msgKey"], json!("sampleText"));
        assert_eq!(body["userIds"], json!(["staff-7"]));
        let parsed: Value = serde_json::from_str(body["msgParam"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["content"], json!("plain reply"));
    }

    #[test]
    fn group_send_uses_group_endpoint_and_markdown_key() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport);
        let address = OutboundAddress::direct("dingtalk", "dingtalk:GROUP:conv-9");
        let reqs = c
            .prepare("tok", &address, &[Msg::user("**bold** reply")])
            .unwrap();
        let req = &reqs[0];
        assert_eq!(
            req.url,
            "https://api.dingtalk.com/v1.0/robot/groupMessages/send"
        );
        let body = req.body.as_ref().unwrap();
        assert_eq!(body["msgKey"], json!("sampleMarkdown"));
        assert_eq!(body["openConversationId"], json!("conv-9"));
        let parsed: Value = serde_json::from_str(body["msgParam"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["title"], json!("Agent"));
        assert_eq!(parsed["text"], json!("**bold** reply"));
    }

    #[test]
    fn multiple_messages_become_multiple_requests_and_blanks_are_skipped() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport);
        let address = OutboundAddress::direct("dingtalk", "dingtalk:DIRECT:u1");
        let reqs = c
            .prepare(
                "tok",
                &address,
                &[Msg::user("one"), Msg::user("   "), Msg::user("two")],
            )
            .unwrap();
        assert_eq!(reqs.len(), 2);
    }

    #[test]
    fn unsupported_peer_kind_is_rejected() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport);
        let address = OutboundAddress::direct("dingtalk", "dingtalk:THREAD:t1");
        let err = c.prepare("tok", &address, &[Msg::user("hi")]).unwrap_err();
        assert!(matches!(err, ChannelError::Unsupported(_)));
    }

    #[tokio::test]
    async fn send_executes_one_request_per_message() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport.clone());
        let address = OutboundAddress::direct("dingtalk", "dingtalk:DIRECT:u1");
        c.send(&address, &[Msg::user("a"), Msg::user("b")])
            .await
            .unwrap();
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn send_with_no_messages_is_a_noop() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport.clone());
        let address = OutboundAddress::direct("dingtalk", "dingtalk:DIRECT:u1");
        c.send(&address, &[]).await.unwrap();
        assert_eq!(transport.request_count(), 0);
    }
}
