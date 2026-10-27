//! WeCom outbound HTTP client: `/cgi-bin/message/send` and `/cgi-bin/appchat/send`.

use std::sync::Arc;

use agentscope_core::message::Msg;
use agentscope_harness::agent::{OutboundAddress, PeerKind};
use serde_json::{json, Value};

use crate::common::{
    looks_like_markdown, parse_peer_target, AccessTokenProvider, ChannelError, HttpTransport,
    PreparedRequest, Result,
};

/// WeCom outbound client built on the pluggable [`HttpTransport`] seam.
pub struct WeComOutboundClient {
    api_base: String,
    agent_id: i32,
    token_provider: Arc<dyn AccessTokenProvider>,
    transport: Arc<dyn HttpTransport>,
}

impl WeComOutboundClient {
    pub fn new(
        api_base: impl Into<String>,
        token_provider: Arc<dyn AccessTokenProvider>,
        agent_id: i32,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            api_base: api_base.into(),
            agent_id,
            token_provider,
            transport,
        }
    }

    /// Build the outbound requests for `messages` (blank texts skipped), without executing them.
    pub fn prepare(
        &self,
        token: &str,
        address: &OutboundAddress,
        messages: &[Msg],
    ) -> Result<Vec<PreparedRequest>> {
        let target = parse_peer_target(&address.to);
        let base = self.api_base.trim_end_matches('/');
        let mut requests = Vec::new();
        for msg in messages {
            let text = msg.text_content();
            if text.trim().is_empty() {
                continue;
            }
            let use_markdown = looks_like_markdown(&text);
            let msg_type = if use_markdown { "markdown" } else { "text" };
            let mut body = serde_json::Map::new();
            body.insert("msgtype".to_string(), json!(msg_type));
            body.insert(msg_type.to_string(), json!({ "content": text }));
            let path = match target.kind {
                PeerKind::Direct => {
                    body.insert("touser".to_string(), json!(target.id));
                    body.insert("agentid".to_string(), json!(self.agent_id));
                    "/cgi-bin/message/send"
                }
                PeerKind::Group => {
                    body.insert("chatid".to_string(), json!(target.id));
                    "/cgi-bin/appchat/send"
                }
                other => {
                    return Err(ChannelError::Unsupported(format!(
                        "WeCom outbound only supports DIRECT and GROUP peer kinds, got {other:?}"
                    )))
                }
            };
            let url = format!("{base}{path}?access_token={token}");
            requests.push(PreparedRequest::post_json(url, Value::Object(body)));
        }
        Ok(requests)
    }

    /// Fetch a token and execute the requests in order.
    pub async fn send(&self, address: &OutboundAddress, messages: &[Msg]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let token = self.token_provider.token().await?;
        for request in self.prepare(&token, address, messages)? {
            let response = self.transport.execute(request).await?;
            self.handle_send_response(&response.body);
        }
        Ok(())
    }

    /// Inspect the WeCom send response: refresh the token on auth errors (non-zero codes are only
    /// logged, mirroring the Java `checkSendResponse`).
    pub fn handle_send_response(&self, body: &str) {
        let Ok(node) = serde_json::from_str::<Value>(body) else {
            return;
        };
        let errcode = node.get("errcode").and_then(|v| v.as_i64()).unwrap_or(0);
        if errcode == 42001 || errcode == 40014 {
            self.token_provider.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::{RecordingTransport, StaticTokenProvider};

    fn client(
        transport: Arc<RecordingTransport>,
        provider: Arc<StaticTokenProvider>,
    ) -> WeComOutboundClient {
        WeComOutboundClient::new("https://qyapi.weixin.qq.com", provider, 100001, transport)
    }

    #[test]
    fn direct_send_uses_message_send_with_touser_and_agentid() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("wecom", "wecom:DIRECT:user-1");
        let reqs = c.prepare("tok", &address, &[Msg::user("plain")]).unwrap();
        let req = &reqs[0];
        assert_eq!(
            req.url,
            "https://qyapi.weixin.qq.com/cgi-bin/message/send?access_token=tok"
        );
        let body = req.body.as_ref().unwrap();
        assert_eq!(body["touser"], json!("user-1"));
        assert_eq!(body["agentid"], json!(100001));
        assert_eq!(body["msgtype"], json!("text"));
        assert_eq!(body["text"], json!({"content": "plain"}));
    }

    #[test]
    fn markdown_text_switches_message_type() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("wecom", "wecom:DIRECT:user-1");
        let reqs = c
            .prepare("tok", &address, &[Msg::user("**bold** reply")])
            .unwrap();
        let body = reqs[0].body.as_ref().unwrap();
        assert_eq!(body["msgtype"], json!("markdown"));
        assert_eq!(body["markdown"], json!({"content": "**bold** reply"}));
    }

    #[test]
    fn group_send_uses_appchat_with_chatid() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("wecom", "wecom:GROUP:chat-9");
        let reqs = c.prepare("tok", &address, &[Msg::user("hi")]).unwrap();
        let req = &reqs[0];
        assert_eq!(
            req.url,
            "https://qyapi.weixin.qq.com/cgi-bin/appchat/send?access_token=tok"
        );
        let body = req.body.as_ref().unwrap();
        assert_eq!(body["chatid"], json!("chat-9"));
    }

    #[test]
    fn unsupported_peer_kind_is_rejected() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("wecom", "wecom:THREAD:t1");
        assert!(matches!(
            c.prepare("tok", &address, &[Msg::user("hi")]).unwrap_err(),
            ChannelError::Unsupported(_)
        ));
    }

    #[test]
    fn auth_errors_invalidate_token() {
        let transport = Arc::new(RecordingTransport::new());
        let provider = Arc::new(StaticTokenProvider::new("tok"));
        let c = client(transport, provider.clone());
        c.handle_send_response(r#"{"errcode":42001,"errmsg":"expired"}"#);
        assert!(provider.was_invalidated());
    }

    #[tokio::test]
    async fn send_executes_one_request_per_message() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport.clone(), Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("wecom", "wecom:DIRECT:user-1");
        c.send(&address, &[Msg::user("a"), Msg::user("b")])
            .await
            .unwrap();
        assert_eq!(transport.request_count(), 2);
    }
}
