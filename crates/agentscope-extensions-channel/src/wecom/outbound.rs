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
            self.handle_send_response(&response.body)?;
        }
        Ok(())
    }

    /// Inspect one WeCom send response, mirroring the Java `checkSendResponse`.
    ///
    /// An auth error (`42001` / `40014`) invalidates the cached token, and **every** rejected send
    /// is an error: an unparsable body, a missing `errcode` and any non-zero `errcode` (e.g.
    /// `45009` api frequency limit, `81013` unknown user) would otherwise be reported as
    /// delivered and silently lose the message.
    pub fn handle_send_response(&self, body: &str) -> Result<()> {
        let node: Value = serde_json::from_str(body).map_err(|_| {
            ChannelError::Http(format!(
                "wecom send response is not JSON: {}",
                error_snippet(body)
            ))
        })?;
        let Some(errcode) = node.get("errcode").and_then(Value::as_i64) else {
            return Err(ChannelError::Http(format!(
                "wecom send response has no errcode: {}",
                error_snippet(body)
            )));
        };
        if errcode == 42001 || errcode == 40014 {
            self.token_provider.invalidate();
        }
        if errcode != 0 {
            return Err(ChannelError::Http(format!(
                "wecom rejected the send: errcode={errcode} body={}",
                error_snippet(body)
            )));
        }
        Ok(())
    }
}

/// Java `MAX_SNIPPET_LENGTH`: error bodies are truncated before being surfaced (log/exception).
pub const MAX_ERROR_SNIPPET_CHARS: usize = 200;

/// Truncate a provider body to [`MAX_ERROR_SNIPPET_CHARS`] characters (never bytes, so a
/// multi-byte body cannot be cut mid-character).
fn error_snippet(body: &str) -> String {
    if body.chars().count() <= MAX_ERROR_SNIPPET_CHARS {
        return body.to_string();
    }
    let mut snippet: String = body.chars().take(MAX_ERROR_SNIPPET_CHARS).collect();
    snippet.push('…');
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::{RecordingTransport, StaticTokenProvider};
    use crate::common::PreparedResponse;

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
    fn auth_errors_invalidate_the_token_and_still_report_a_failure() {
        let transport = Arc::new(RecordingTransport::new());
        let provider = Arc::new(StaticTokenProvider::new("tok"));
        let c = client(transport, provider.clone());
        let error = c
            .handle_send_response(r#"{"errcode":42001,"errmsg":"expired"}"#)
            .unwrap_err();
        assert!(provider.was_invalidated());
        assert!(matches!(error, ChannelError::Http(_)));
    }

    #[test]
    fn a_rejected_send_is_an_error() {
        // 45009 = api frequency limit: accepted by WeCom's HTTP layer but rejected by the API.
        let c = client(
            Arc::new(RecordingTransport::new()),
            Arc::new(StaticTokenProvider::new("tok")),
        );
        let error = c
            .handle_send_response(r#"{"errcode":45009,"errmsg":"api freq out of limit"}"#)
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("45009"), "{message}");
        assert!(message.contains("api freq out of limit"), "{message}");
    }

    #[test]
    fn an_empty_unparsable_or_errcode_less_body_is_an_error() {
        let c = client(
            Arc::new(RecordingTransport::new()),
            Arc::new(StaticTokenProvider::new("tok")),
        );
        for body in ["", "not-json", "{}", r#"{"errmsg":"no errcode here"}"#] {
            assert!(
                c.handle_send_response(body).is_err(),
                "body {body:?} must not be treated as delivered"
            );
        }
        // A successful response is still fine.
        c.handle_send_response(r#"{"errcode":0,"errmsg":"ok"}"#)
            .unwrap();
    }

    #[test]
    fn an_error_body_is_truncated_to_a_bounded_snippet() {
        let c = client(
            Arc::new(RecordingTransport::new()),
            Arc::new(StaticTokenProvider::new("tok")),
        );
        let body = format!(r#"{{"errcode":45009,"errmsg":"{}"}}"#, "x".repeat(1_000));
        let message = c.handle_send_response(&body).unwrap_err().to_string();
        // The snippet is capped at MAX_ERROR_SNIPPET_CHARS characters (plus the ellipsis);
        // the surrounding error prefix is fixed text and must not be counted against it.
        let prefix = "http transport error: wecom rejected the send: errcode=45009 body=";
        assert!(
            message.chars().count() <= prefix.chars().count() + MAX_ERROR_SNIPPET_CHARS + 1,
            "snippet not truncated: {} chars",
            message.chars().count()
        );
        assert!(!message.contains(&"x".repeat(MAX_ERROR_SNIPPET_CHARS + 1)));
    }

    #[tokio::test]
    async fn send_executes_one_request_per_message() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"errcode":0,"errmsg":"ok"}"#),
            PreparedResponse::new(200, r#"{"errcode":0,"errmsg":"ok"}"#),
        ]));
        let c = client(transport.clone(), Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("wecom", "wecom:DIRECT:user-1");
        c.send(&address, &[Msg::user("a"), Msg::user("b")])
            .await
            .unwrap();
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn send_propagates_a_rejection() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                r#"{"errcode":81013,"errmsg":"user & party & tag all invalid"}"#,
            ),
        ]));
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("wecom", "wecom:DIRECT:user-1");
        let error = c.send(&address, &[Msg::user("a")]).await.unwrap_err();
        assert!(matches!(error, ChannelError::Http(_)));
    }
}
