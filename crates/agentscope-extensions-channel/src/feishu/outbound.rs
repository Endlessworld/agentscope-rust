//! Feishu outbound HTTP client: `POST /open-apis/im/v1/messages` request builders.

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::message::Msg;
use agentscope_harness::agent::OutboundAddress;
use serde_json::{json, Value};

use crate::common::http::{apply_response_timeout, ensure_positive_timeout};
use crate::common::{
    parse_peer_target, AccessTokenProvider, ChannelError, HttpTransport, PreparedRequest, Result,
};

/// Default send timeout, mirroring the Java
/// `FeishuOutboundClient.DEFAULT_RESPONSE_TIMEOUT = Duration.ofSeconds(10)`.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Feishu outbound client built on the pluggable [`HttpTransport`] seam.
pub struct FeishuOutboundClient {
    api_base: String,
    token_provider: Arc<dyn AccessTokenProvider>,
    transport: Arc<dyn HttpTransport>,
    response_timeout: Duration,
}

impl FeishuOutboundClient {
    pub fn new(
        api_base: impl Into<String>,
        token_provider: Arc<dyn AccessTokenProvider>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            api_base: api_base.into(),
            token_provider,
            transport,
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
        }
    }

    /// Override the send timeout (Java's `responseTimeout` constructor parameter).
    ///
    /// Returns [`ChannelError::Config`] for a zero duration, mirroring the Java
    /// `"responseTimeout must be positive"` guard. When the injected transport supports
    /// reconfiguration (e.g. [`crate::common::ReqwestTransport`]) the timeout is applied to it;
    /// otherwise it is recorded and left to the caller's transport.
    pub fn with_response_timeout(mut self, timeout: Duration) -> Result<Self> {
        ensure_positive_timeout(timeout)?;
        self.transport = apply_response_timeout(self.transport.clone(), timeout);
        self.response_timeout = timeout;
        Ok(self)
    }

    /// The configured send timeout.
    pub fn response_timeout(&self) -> Duration {
        self.response_timeout
    }

    /// The `receive_id_type` used for both DIRECT and GROUP (we address by `chat_id`).
    pub fn receive_id_type() -> &'static str {
        "chat_id"
    }

    /// Build the outbound request for a single message (`delivery_id` becomes the `uuid`).
    pub fn prepare_with_receipt(
        &self,
        token: &str,
        address: &OutboundAddress,
        msg: &Msg,
        delivery_id: Option<&str>,
    ) -> Result<PreparedRequest> {
        let target = parse_peer_target(&address.to);
        let text = msg.text_content();
        if text.trim().is_empty() {
            return Err(ChannelError::Parse("Message text required".to_string()));
        }
        let mut body = serde_json::Map::new();
        body.insert("msg_type".to_string(), json!("text"));
        if let Some(delivery_id) = delivery_id {
            body.insert("uuid".to_string(), json!(delivery_id));
        }
        let thread_id = address
            .thread_id
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let threaded = thread_id.is_some();
        if threaded {
            body.insert("reply_in_thread".to_string(), json!(true));
        } else {
            body.insert("receive_id".to_string(), json!(target.id));
        }
        body.insert(
            "content".to_string(),
            json!(serde_json::to_string(&json!({ "text": text }))?),
        );
        let base = self.api_base.trim_end_matches('/');
        let url = match thread_id {
            Some(thread_id) => format!("{base}/open-apis/im/v1/messages/{thread_id}/reply"),
            None => format!(
                "{base}/open-apis/im/v1/messages?receive_id_type={}",
                Self::receive_id_type()
            ),
        };
        Ok(PreparedRequest::post_json(url, Value::Object(body))
            .header("Authorization", format!("Bearer {token}")))
    }

    /// Build one request per message (no delivery id).
    pub fn prepare(
        &self,
        token: &str,
        address: &OutboundAddress,
        messages: &[Msg],
    ) -> Result<Vec<PreparedRequest>> {
        messages
            .iter()
            .map(|msg| self.prepare_with_receipt(token, address, msg, None))
            .collect()
    }

    /// Fetch a token and execute the requests in order.
    pub async fn send(&self, address: &OutboundAddress, messages: &[Msg]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let token = self.token_provider.token().await?;
        for request in self.prepare(&token, address, messages)? {
            let response = self.transport.execute(request).await?;
            self.parse_send_response(&response.body)?;
        }
        Ok(())
    }

    /// Send a single message and return the Feishu `message_id`.
    ///
    /// `delivery_id` becomes the request `uuid`, Feishu's idempotency key, so a retried durable
    /// notification returns the first message id instead of posting twice (Java
    /// `FeishuOutboundClient#sendWithReceipt`).
    pub async fn send_with_receipt(
        &self,
        address: &OutboundAddress,
        msg: &Msg,
        delivery_id: Option<&str>,
    ) -> Result<String> {
        let token = self.token_provider.token().await?;
        let request = self.prepare_with_receipt(token.as_str(), address, msg, delivery_id)?;
        let response = self.transport.execute(request).await?;
        let message_id = self.parse_send_response(&response.body)?;
        if message_id.trim().is_empty() {
            return Err(ChannelError::Parse("Feishu message ID missing".to_string()));
        }
        Ok(message_id)
    }

    /// Validate the send response, invalidating the token on auth-related codes.
    pub fn parse_send_response(&self, body: &str) -> Result<String> {
        let node: Value = serde_json::from_str(body)
            .map_err(|e| ChannelError::Parse(format!("Invalid Feishu send response: {e}")))?;
        let code = node
            .get("code")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ChannelError::Parse("Feishu response code missing".to_string()))?;
        if code == 99991663 || code == 99991664 {
            self.token_provider.invalidate();
        }
        if code != 0 {
            return Err(ChannelError::Parse(format!(
                "Feishu rejected message with code {code}"
            )));
        }
        Ok(node
            .pointer("/data/message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::{RecordingTransport, StaticTokenProvider, TimeoutAwareTransport};
    use crate::common::PreparedResponse;

    fn client(
        transport: Arc<RecordingTransport>,
        provider: Arc<StaticTokenProvider>,
    ) -> FeishuOutboundClient {
        FeishuOutboundClient::new("https://open.feishu.cn", provider, transport)
    }

    #[test]
    fn p2p_send_uses_chat_id_addressing() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("feishu", "feishu:DIRECT:oc_1");
        let req = c
            .prepare_with_receipt("tok", &address, &Msg::user("hi"), None)
            .unwrap();
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id"
        );
        assert_eq!(req.header_value("Authorization"), Some("Bearer tok"));
        let body = req.body.unwrap();
        assert_eq!(body["msg_type"], json!("text"));
        assert_eq!(body["receive_id"], json!("oc_1"));
        let content: Value = serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["text"], json!("hi"));
    }

    #[test]
    fn threaded_send_targets_reply_endpoint_and_sets_uuid() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address =
            OutboundAddress::direct("feishu", "feishu:THREAD:om_root").with_thread("om_root");
        let req = c
            .prepare_with_receipt("tok", &address, &Msg::user("reply"), Some("d-1"))
            .unwrap();
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_root/reply"
        );
        let body = req.body.unwrap();
        assert_eq!(body["uuid"], json!("d-1"));
        assert_eq!(body["reply_in_thread"], json!(true));
        assert!(body.get("receive_id").is_none());
    }

    #[test]
    fn blank_text_is_rejected() {
        let transport = Arc::new(RecordingTransport::new());
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("feishu", "feishu:DIRECT:oc_1");
        assert!(c
            .prepare_with_receipt("tok", &address, &Msg::user("   "), None)
            .is_err());
    }

    #[test]
    fn parse_send_response_extracts_message_id_and_invalidates_on_auth_error() {
        let transport = Arc::new(RecordingTransport::new());
        let provider = Arc::new(StaticTokenProvider::new("tok"));
        let c = client(transport, provider.clone());
        assert_eq!(
            c.parse_send_response(r#"{"code":0,"data":{"message_id":"om_9"}}"#)
                .unwrap(),
            "om_9"
        );
        assert!(c.parse_send_response(r#"{"code":99991663}"#).is_err());
        assert!(provider.was_invalidated());
        assert!(c.parse_send_response(r#"{"msg":"no code"}"#).is_err());
    }

    #[tokio::test]
    async fn send_executes_one_request_per_message() {
        // `send` validates each response (`checkSendResponse`), so every request needs a `code:0`.
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"code":0,"data":{"message_id":"om_1"}}"#),
            PreparedResponse::new(200, r#"{"code":0,"data":{"message_id":"om_2"}}"#),
        ]));
        let c = client(transport.clone(), Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("feishu", "feishu:DIRECT:oc_1");
        c.send(&address, &[Msg::user("a"), Msg::user("b")])
            .await
            .unwrap();
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn send_with_receipt_returns_message_id() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"code":0,"data":{"message_id":"om_42"}}"#),
        ]));
        let c = client(transport, Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("feishu", "feishu:DIRECT:oc_1");
        let id = c
            .send_with_receipt(&address, &Msg::user("hi"), Some("d-1"))
            .await
            .unwrap();
        assert_eq!(id, "om_42");
    }

    #[tokio::test]
    async fn a_receipt_without_a_delivery_id_sends_no_uuid() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"code":0,"data":{"message_id":"om_43"}}"#),
        ]));
        let c = client(transport.clone(), Arc::new(StaticTokenProvider::new("tok")));
        let address = OutboundAddress::direct("feishu", "feishu:DIRECT:oc_1");
        let id = c
            .send_with_receipt(&address, &Msg::user("hi"), None)
            .await
            .unwrap();
        assert_eq!(id, "om_43");
        let body = transport.last_request().unwrap().body.unwrap();
        assert!(
            body.get("uuid").is_none(),
            "a host without a delivery id must not fabricate one: {body}"
        );
    }

    #[test]
    fn default_response_timeout_is_ten_seconds() {
        let c = client(
            Arc::new(RecordingTransport::new()),
            Arc::new(StaticTokenProvider::new("tok")),
        );
        assert_eq!(c.response_timeout(), DEFAULT_RESPONSE_TIMEOUT);
        assert_eq!(DEFAULT_RESPONSE_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn explicit_response_timeout_is_applied_to_a_reconfigurable_transport() {
        let transport = Arc::new(TimeoutAwareTransport::new());
        let c = FeishuOutboundClient::new(
            "https://open.feishu.cn",
            Arc::new(StaticTokenProvider::new("tok")),
            transport.clone(),
        )
        .with_response_timeout(Duration::from_millis(750))
        .unwrap();
        assert_eq!(c.response_timeout(), Duration::from_millis(750));
        assert_eq!(transport.timeout(), Some(Duration::from_millis(750)));
    }

    #[test]
    fn explicit_response_timeout_is_recorded_for_a_non_reconfigurable_transport() {
        let c = client(
            Arc::new(RecordingTransport::new()),
            Arc::new(StaticTokenProvider::new("tok")),
        )
        .with_response_timeout(Duration::from_secs(5))
        .unwrap();
        assert_eq!(c.response_timeout(), Duration::from_secs(5));
    }

    #[test]
    fn zero_response_timeout_is_rejected() {
        let c = client(
            Arc::new(RecordingTransport::new()),
            Arc::new(StaticTokenProvider::new("tok")),
        );
        let err = c
            .with_response_timeout(Duration::ZERO)
            .err()
            .expect("zero must be rejected");
        assert!(
            err.to_string().contains("responseTimeout must be positive"),
            "unexpected message: {err}"
        );
    }
}
