//! iLink outbound client: text send and update polling on the shared [`HttpTransport`] seam.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinOutboundClient`. Request construction
//! is separated from execution so every body shape is unit-testable with a recording transport.
//!
//! Two rules the Java client states and this port keeps:
//!
//! 1. **A provider response has to state its outcome.** Defaulting a missing `ret` to zero would
//!    accept any 2xx body — an empty object, a proxy error page rendered as JSON — as a delivered
//!    message, and the channel would then complete the inbox claim and drop the reply.
//!    `getupdates` is the one call that reports success without `ret`/`errcode`, so it is allowed to
//!    carry its batch fields instead — and *only* those.
//! 2. **The provider's own `errmsg` is never copied into an error**, because that text reaches host
//!    logs and may embed anything the provider chose to echo. Only numeric codes are reported.

use agentscope_core::message::Msg;
use agentscope_harness::agent::OutboundAddress;
use serde_json::Value;

use super::credentials::{WeixinCredentialProvider, WeixinCredentials};
use super::dto::{WeixinUpdatesResponse, WeixinQrCodeResponse, required_field};
use super::error::{WeixinError, WeixinResult};
use super::properties::WeixinChannelProperties;
use super::protocol::{
    application_headers, authenticated_json_post, encode_wechat_uin, notify_request_body,
    qrcode_challenge_request, qrcode_status_path, send_message_request_body, updates_request_body,
    LONG_POLL_GRACE_MS, PATH_NOTIFY_START, PATH_NOTIFY_STOP, PATH_SEND_MESSAGE, PATH_UPDATES,
};
use crate::common::{Result, HttpTransport, PreparedRequest};

/// Client id sent with every outbound message. Only provider-side de-duplication uses it, so a
/// monotonic counter derived from the clock is sufficient and keeps this module dependency-free.
pub fn next_client_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{}",
        crate::common::now_ms(),
        sequence
    )
}

/// The peer id from a routed outbound address, mirroring the Java `peer(...)` helper: everything
/// after the last `:` — `"weixin:DIRECT:peer-a"` yields `"peer-a"`.
pub fn peer_of(address: &str) -> &str {
    match address.rfind(':') {
        Some(index) => &address[index + 1..],
        None => address,
    }
}

/// Read timeout for `path`: `getupdates` holds the connection open for the provider's long-poll
/// window, so its read timeout has to outlast `long_poll_timeout_ms`; every control call keeps
/// `request_timeout_ms`.
pub fn timeout_for(path: &str, properties: &WeixinChannelProperties) -> i64 {
    if path.ends_with("getupdates") {
        (properties.long_poll_timeout_ms + LONG_POLL_GRACE_MS).max(properties.request_timeout_ms)
    } else {
        properties.request_timeout_ms
    }
}

/// iLink outbound client.
pub struct WeixinOutboundClient {
    properties: WeixinChannelProperties,
    credentials: std::sync::Arc<dyn WeixinCredentialProvider>,
    transport: std::sync::Arc<dyn HttpTransport>,
    /// Injected `X-WECHAT-UIN` value; production callers pass a random one per request.
    wechat_uin_seed: u32,
}

impl WeixinOutboundClient {
    pub fn new(
        properties: WeixinChannelProperties,
        credentials: std::sync::Arc<dyn WeixinCredentialProvider>,
        transport: std::sync::Arc<dyn HttpTransport>,
    ) -> Self {
        Self::with_wechat_uin(properties, credentials, transport, 1)
    }

    /// Build with an explicit `X-WECHAT-UIN` seed so tests are deterministic.
    pub fn with_wechat_uin(
        properties: WeixinChannelProperties,
        credentials: std::sync::Arc<dyn WeixinCredentialProvider>,
        transport: std::sync::Arc<dyn HttpTransport>,
        wechat_uin_seed: u32,
    ) -> Self {
        Self {
            properties,
            credentials,
            transport,
            wechat_uin_seed,
        }
    }

    pub fn properties(&self) -> &WeixinChannelProperties {
        &self.properties
    }

    fn token(&self) -> WeixinResult<WeixinCredentials> {
        self.credentials.current().map_err(WeixinError::from)
    }

    fn post<'a>(&self, path: &str, body: Value) -> WeixinResult<PreparedRequest> {
        let credentials = self.token()?;
        let url = format!("{}{path}", self.properties.base_url.trim_end_matches('/'));
        let request = authenticated_json_post(
            PreparedRequest::post_json(url, body),
            credentials.bot_token(),
            &encode_wechat_uin(self.wechat_uin_seed),
        );
        Ok(request)
    }

    /// Build the `sendmessage` request for one non-blank text message.
    pub fn prepare_send(
        &self,
        address: &OutboundAddress,
        message: &Msg,
        context_token: Option<&str>,
        client_id: &str,
    ) -> WeixinResult<Option<PreparedRequest>> {
        let text = message.text_content();
        if text.trim().is_empty() {
            return Ok(None);
        }
        let body = send_message_request_body(
            peer_of(&address.to),
            &text,
            context_token,
            client_id,
            &self.properties.channel_version,
            &self.properties.bot_agent,
        );
        Ok(Some(self.post(PATH_SEND_MESSAGE, body)?))
    }

    /// Build the `getupdates` long-poll request for `cursor`.
    pub fn prepare_updates(&self, cursor: &str) -> WeixinResult<PreparedRequest> {
        self.post(
            PATH_UPDATES,
            updates_request_body(
                cursor,
                &self.properties.channel_version,
                &self.properties.bot_agent,
            ),
        )
    }

    /// Build the `notifystart` control request.
    pub fn prepare_notify_start(&self) -> WeixinResult<PreparedRequest> {
        self.post(
            PATH_NOTIFY_START,
            notify_request_body(&self.properties.channel_version, &self.properties.bot_agent),
        )
    }

    /// Build the `notifystop` control request.
    pub fn prepare_notify_stop(&self) -> WeixinResult<PreparedRequest> {
        self.post(
            PATH_NOTIFY_STOP,
            notify_request_body(&self.properties.channel_version, &self.properties.bot_agent),
        )
    }

    /// Build the QR-login challenge request (application headers only — no bot token yet).
    pub fn prepare_qrcode_challenge(
        &self,
        bot_type: &str,
    ) -> WeixinResult<PreparedRequest> {
        if bot_type.is_empty() || !bot_type.chars().all(|c| c.is_ascii_digit()) {
            return Err(WeixinError::operation("botType must be numeric"));
        }
        Ok(application_headers(qrcode_challenge_request(
            &self.properties.base_url,
            bot_type,
        )))
    }

    /// Build the QR-login status poll request. The URI carries the login secret, so nothing here
    /// logs it and no error embeds it.
    pub fn prepare_qrcode_status(
        &self,
        polling_base_url: &str,
        qrcode: &str,
        verify_code: Option<&str>,
    ) -> WeixinResult<PreparedRequest> {
        let url = format!(
            "{}{}",
            polling_base_url.trim_end_matches('/'),
            qrcode_status_path(qrcode, verify_code)
        );
        Ok(application_headers(PreparedRequest::get(url)))
    }

    /// Send one message and return the provider's receipt.
    ///
    /// A host that persists replies needs the receipt, not a local acknowledgement: only the
    /// provider's message id proves iLink accepted the reply.
    pub async fn send_with_receipt(
        &self,
        address: &OutboundAddress,
        message: &Msg,
        context_token: Option<&str>,
    ) -> WeixinResult<Option<String>> {
        let Some(request) = self.prepare_send(address, message, context_token, &next_client_id())?
        else {
            return Ok(None);
        };
        let response = self.transport.execute(request).await?;
        let node = self.assert_success("sendmessage", &response, &["message_id"])?;
        let receipt = node
            .get("message_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|receipt| !receipt.is_empty())
            .ok_or_else(|| WeixinError::operation("iLink sendmessage returned no receipt"))?;
        Ok(Some(receipt.to_string()))
    }

    /// Send every non-blank message in order, returning the receipts in the same order.
    pub async fn send(
        &self,
        address: &OutboundAddress,
        messages: &[Msg],
        context_token: Option<&str>,
    ) -> WeixinResult<Vec<String>> {
        let mut receipts = Vec::new();
        for message in messages {
            if let Some(receipt) = self.send_with_receipt(address, message, context_token).await? {
                receipts.push(receipt);
            }
        }
        Ok(receipts)
    }

    /// Execute the `getupdates` long poll and parse its batch.
    pub async fn updates(&self, cursor: &str) -> WeixinResult<WeixinUpdatesResponse> {
        let request = self.prepare_updates(cursor)?;
        let response = self.transport.execute(request).await?;
        let node = self.assert_success("getupdates", &response, &["msgs", "get_updates_buf"])?;
        Ok(serde_json::from_value(node).map_err(|_| {
            WeixinError::operation("iLink returned an unparseable response")
        })?)
    }

    /// Announce the consumer to the provider.
    pub async fn notify_start(&self) -> WeixinResult<()> {
        let request = self.prepare_notify_start()?;
        let response = self.transport.execute(request).await?;
        self.assert_success("notifystart", &response, &[])?;
        Ok(())
    }

    /// Tell the provider the consumer is leaving.
    pub async fn notify_stop(&self) -> WeixinResult<()> {
        let request = self.prepare_notify_stop()?;
        let response = self.transport.execute(request).await?;
        self.assert_success("notifystop", &response, &[])?;
        Ok(())
    }

    /// Start a QR login and return the challenge plus its portable session.
    pub async fn qrcode_challenge(
        &self,
        bot_type: &str,
    ) -> WeixinResult<(WeixinChallenge, super::dto::WeixinLoginSession)> {
        self.qrcode_challenge_on(&self.properties.base_url, bot_type)
            .await
    }

    /// Start a QR login against an explicit base URL: the login client keeps its own allowlisted
    /// base URL, which may legitimately differ from the channel's polling base.
    pub async fn qrcode_challenge_on(
        &self,
        base_url: &str,
        bot_type: &str,
    ) -> WeixinResult<(WeixinChallenge, super::dto::WeixinLoginSession)> {
        if bot_type.is_empty() || !bot_type.chars().all(|c| c.is_ascii_digit()) {
            return Err(WeixinError::operation("botType must be numeric"));
        }
        let request = application_headers(qrcode_challenge_request(base_url, bot_type));
        let response = self.transport.execute(request).await?;
        let node = self.response_node(&response)?;
        let start = node
            .get("base_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .unwrap_or(base_url)
            .to_string();
        let body: WeixinQrCodeResponse = serde_json::from_value(node).map_err(|_| {
            WeixinError::operation("iLink returned an unparseable response")
        })?;
        let qrcode = required_field("qrcode", body.qrcode.as_deref())?;
        let image = required_field("qrcode_img_content", body.qrcode_img_content.as_deref())?;
        let session = super::dto::WeixinLoginSession::new(qrcode, start)
            .map_err(|_| WeixinError::operation("Invalid iLink base URL"))?;
        Ok((
            WeixinChallenge {
                image_content: image,
            },
            session,
        ))
    }

    /// Poll the QR-login status. Endpoint validation happens in [`super::login`].
    pub async fn qrcode_status(
        &self,
        polling_base_url: &str,
        session: &super::dto::WeixinLoginSession,
        verify_code: Option<&str>,
    ) -> WeixinResult<Value> {
        let request = self.prepare_qrcode_status(polling_base_url, &session.qrcode, verify_code)?;
        let response = self.transport.execute(request).await?;
        self.response_node(&response)
    }

    /// Parse a 2xx response body without letting `serde_json` quote the input: inbox payloads and
    /// login tokens travel in these bodies, and the default parse error embeds an input fragment.
    fn response_node(&self, response: &crate::common::PreparedResponse) -> WeixinResult<Value> {
        if !response.is_success() {
            return Err(WeixinError::operation(format!(
                "iLink HTTP {}",
                response.status
            )));
        }
        serde_json::from_str(&response.body)
            .map_err(|_| WeixinError::operation("iLink returned an unparseable response"))
    }

    /// Validate a bot-call outcome and return the parsed body.
    fn assert_success(
        &self,
        operation: &str,
        response: &crate::common::PreparedResponse,
        outcome_fields: &[&str],
    ) -> WeixinResult<Value> {
        let node = self.response_node(response)?;
        require_provider_result(operation, &node, outcome_fields)?;
        let ret = node.get("ret").and_then(Value::as_i64).unwrap_or(0);
        let errcode = node.get("errcode").and_then(Value::as_i64).unwrap_or(0);
        if ret != 0 || errcode != 0 {
            if ret == -14 || errcode == -14 {
                return Err(WeixinError::credential_rejected(operation, -14));
            }
            return Err(WeixinError::operation(format!(
                "iLink {operation} failed ret={ret} errcode={errcode}"
            )));
        }
        Ok(node)
    }
}

/// The QR image handed back to the host at the start of a login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeixinChallenge {
    /// QR image content. Rendered redacted, matching the Java challenge record.
    pub image_content: String,
}

impl std::fmt::Display for WeixinChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeixinChallenge[imageContent=***]")
    }
}

/// Every provider response has to state its outcome.
///
/// A body with `ret`/`errcode` states it explicitly. `getupdates` is the one call that reports
/// success with neither, so it must instead carry one of its batch fields — the relaxation must
/// not turn an unrelated 2xx body into an empty batch.
fn require_provider_result(operation: &str, node: &Value, outcome_fields: &[&str]) -> WeixinResult<()> {
    if node.get("ret").is_some() || node.get("errcode").is_some() {
        return Ok(());
    }
    if outcome_fields.iter().any(|field| node.get(*field).is_some()) {
        return Ok(());
    }
    Err(WeixinError::operation(format!(
        "iLink {operation} response carries no ret/errcode"
    )))
}

/// Map a Weixin failure onto the shared channel error type.
pub fn to_channel_error(error: WeixinError) -> crate::common::ChannelError {
    error.into_channel()
}

/// Convenience alias used by callers that only need the shared error type.
pub type ChannelResult<T> = Result<T>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;
    use crate::weixin::credentials::FixedCredentialProvider;

    fn properties() -> WeixinChannelProperties {
        WeixinChannelProperties::from_json(
            "wx",
            &serde_json::json!({ "accountId": "acct", "longPollTimeoutMs": 35_000, "requestTimeoutMs": 45_000 }),
        )
        .unwrap()
    }

    fn properties_at(base_url: &str) -> WeixinChannelProperties {
        WeixinChannelProperties::from_json(
            "wx",
            &serde_json::json!({ "accountId": "acct", "baseUrl": base_url }),
        )
        .unwrap()
    }

    fn client(transport: std::sync::Arc<RecordingTransport>) -> WeixinOutboundClient {
        WeixinOutboundClient::with_wechat_uin(
            properties(),
            FixedCredentialProvider::from_token("tok").unwrap().shared(),
            transport,
            42,
        )
    }

    fn assistant(text: &str) -> Msg {
        Msg::builder()
            .role(agentscope_core::message::MsgRole::Assistant)
            .text_content(text)
            .build()
    }

    fn address() -> OutboundAddress {
        OutboundAddress::direct("weixin", "weixin:DIRECT:peer-a")
    }

    #[test]
    fn peer_of_keeps_the_last_segment() {
        assert_eq!(peer_of("weixin:DIRECT:peer-a"), "peer-a");
        assert_eq!(peer_of("weixin:GROUP:g:1"), "1");
        assert_eq!(peer_of("peer"), "peer");
    }

    #[test]
    fn long_poll_timeout_outlasts_the_poll_window() {
        // Java: max(longPollTimeoutMs + LONG_POLL_GRACE_MS, requestTimeoutMs). With the shipped
        // defaults (35s poll / 45s control) the two coincide, so a longer poll window is used to
        // show the derived deadline really does grow for `getupdates` only.
        let properties = WeixinChannelProperties::from_json(
            "wx",
            &serde_json::json!({
                "accountId": "acct",
                "longPollTimeoutMs": 60_000,
                "requestTimeoutMs": 45_000,
            }),
        )
        .unwrap();
        assert_eq!(timeout_for(PATH_UPDATES, &properties), 65_000);
        assert_eq!(timeout_for(PATH_SEND_MESSAGE, &properties), 45_000);
        assert_eq!(timeout_for(PATH_NOTIFY_START, &properties), 45_000);
        assert_eq!(timeout_for(PATH_NOTIFY_STOP, &properties), 45_000);

        // A short poll window never shortens a control call below `requestTimeoutMs`.
        let short = WeixinChannelProperties::from_json(
            "wx",
            &serde_json::json!({
                "accountId": "acct",
                "longPollTimeoutMs": 5_000,
                "requestTimeoutMs": 45_000,
            }),
        )
        .unwrap();
        assert_eq!(timeout_for(PATH_UPDATES, &short), 45_000);
    }

    #[tokio::test]
    async fn send_posts_an_authenticated_text_envelope_and_returns_the_receipt() {
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"message_id\":\"receipt-1\"}"),
        ]));
        let outbound = client(transport.clone());
        let receipt = outbound
            .send_with_receipt(&address(), &assistant("hello"), Some("ctx-9"))
            .await
            .unwrap();
        assert_eq!(receipt.as_deref(), Some("receipt-1"));

        let request = transport.last_request().unwrap();
        assert_eq!(
            request.url,
            "https://ilinkai.weixin.qq.com/ilink/bot/sendmessage"
        );
        assert_eq!(request.method, crate::common::HttpMethod::Post);
        assert_eq!(request.header_value("Authorization"), Some("Bearer tok"));
        assert_eq!(request.header_value("iLink-App-Id"), Some("bot"));
        assert_eq!(request.header_value("AuthorizationType"), Some("ilink_bot_token"));
        assert_eq!(request.header_value("X-WECHAT-UIN"), Some("NDI="));
        let body = request.body.unwrap();
        assert_eq!(body["msg"]["to_user_id"], serde_json::json!("peer-a"));
        assert_eq!(body["msg"]["context_token"], serde_json::json!("ctx-9"));
        assert_eq!(
            body["msg"]["item_list"][0]["text_item"]["text"],
            serde_json::json!("hello")
        );
        assert_eq!(body["base_info"]["channel_version"], serde_json::json!("agentscope-weixin/1.0"));
    }

    #[tokio::test]
    async fn blank_text_is_skipped_without_sending_anything() {
        let transport = std::sync::Arc::new(RecordingTransport::new());
        let outbound = client(transport.clone());
        assert_eq!(
            outbound
                .send_with_receipt(&address(), &assistant("   "), None)
                .await
                .unwrap(),
            None
        );
        assert_eq!(transport.request_count(), 0);
    }

    #[tokio::test]
    async fn business_failure_is_reported_and_never_treated_as_delivery() {
        for body in ["{\"ret\":-14}", "{\"ret\":0,\"errcode\":-14}"] {
            let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
                PreparedResponse::new(200, body),
            ]));
            let error = client(transport)
                .send_with_receipt(&address(), &assistant("hello"), None)
                .await
                .unwrap_err();
            assert!(error.is_credential_rejected());
            assert!(error.to_string().contains("-14"));
        }
        // A non-credential business failure keeps the numeric codes but not the provider errmsg.
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                "{\"ret\":1,\"errcode\":2,\"errmsg\":\"secret provider text\"}",
            ),
        ]));
        let error = client(transport)
            .send_with_receipt(&address(), &assistant("hello"), None)
            .await
            .unwrap_err();
        assert!(!error.is_credential_rejected());
        assert!(error.to_string().contains("ret=1 errcode=2"));
        assert!(!error.to_string().contains("secret provider text"));
    }

    #[tokio::test]
    async fn a_2xx_body_without_an_outcome_is_rejected() {
        // An empty JSON object must not count as a delivered message.
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{}"),
        ]));
        let error = client(transport)
            .send_with_receipt(&address(), &assistant("hello"), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("carries no ret/errcode"));

        // An unparseable 2xx body is reported without quoting it back.
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "<html>proxy error</html>"),
        ]));
        let error = client(transport).updates("").await.unwrap_err();
        assert!(error.to_string().contains("unparseable"));
        assert!(!error.to_string().contains("proxy error"));

        // A non-2xx status is reported by code only.
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(503, "secret"),
        ]));
        let error = client(transport).notify_start().await.unwrap_err();
        assert!(error.to_string().contains("iLink HTTP 503"));
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn sendmessage_without_a_receipt_is_an_operation_failure() {
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"ret\":0}"),
        ]));
        let error = client(transport)
            .send_with_receipt(&address(), &assistant("hello"), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no receipt"));
    }

    #[tokio::test]
    async fn updates_carries_the_cursor_and_tolerates_extra_protocol_fields() {
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                "{\"msgs\":[{\"message_type\":1,\"from_user_id\":\"u1\",\"item_list\":[{\"type\":1,\"text_item\":{\"text\":\"hi\"}}]}],\"get_updates_buf\":\"next\",\"longpolling_timeout_ms\":35000,\"server_extension\":{\"enabled\":true}}",
            ),
        ]));
        let response = client(transport.clone()).updates("cursor-1").await.unwrap();
        assert_eq!(response.ret_or_zero(), 0);
        assert_eq!(response.get_updates_buf.as_deref(), Some("next"));
        assert_eq!(response.longpolling_timeout_ms, Some(35_000));
        assert_eq!(response.messages()[0].text().as_deref(), Some("hi"));
        assert_eq!(
            transport.last_request().unwrap().body.unwrap()["get_updates_buf"],
            serde_json::json!("cursor-1")
        );
    }

    #[tokio::test]
    async fn an_empty_getupdates_body_is_rejected_rather_than_read_as_an_empty_batch() {
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{}"),
        ]));
        let error = client(transport).updates("").await.unwrap_err();
        assert!(error.to_string().contains("carries no ret/errcode"));
    }

    #[tokio::test]
    async fn notify_calls_post_the_base_info_and_surface_credential_rejection() {
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"ret\":0}"),
            PreparedResponse::new(200, "{\"ret\":-14}"),
            PreparedResponse::new(200, "{\"ret\":0}"),
        ]));
        let outbound = client(transport.clone());
        outbound.notify_start().await.unwrap();
        let request = transport.last_request().unwrap();
        assert_eq!(
            request.url,
            "https://ilinkai.weixin.qq.com/ilink/bot/msg/notifystart"
        );
        assert_eq!(request.header_value("Authorization"), Some("Bearer tok"));
        assert_eq!(
            request.body.unwrap()["base_info"]["bot_agent"],
            serde_json::json!("AgentScope")
        );
        assert!(outbound.notify_start().await.unwrap_err().is_credential_rejected());
        outbound.notify_stop().await.unwrap();
        assert_eq!(
            transport.last_request().unwrap().url,
            "https://ilinkai.weixin.qq.com/ilink/bot/msg/notifystop"
        );
    }

    #[tokio::test]
    async fn qrcode_challenge_and_status_use_the_application_headers_only() {
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                "{\"qrcode\":\"qr-1\",\"qrcode_img_content\":\"https://qr\"}",
            ),
            PreparedResponse::new(200, "{\"status\":\"wait\"}"),
        ]));
        let outbound = client(transport.clone());
        let (challenge, session) = outbound.qrcode_challenge("3").await.unwrap();
        assert_eq!(challenge.image_content, "https://qr");
        assert_eq!(session.qrcode, "qr-1");
        let start = transport.last_request().unwrap();
        assert_eq!(
            start.url,
            "https://ilinkai.weixin.qq.com/ilink/bot/get_bot_qrcode?bot_type=3"
        );
        assert_eq!(start.header_value("iLink-App-Id"), Some("bot"));
        assert_eq!(start.header_value("Authorization"), None);

        outbound
            .qrcode_status("https://weixin.qq.com", &session, Some("123456"))
            .await
            .unwrap();
        let poll = transport.last_request().unwrap();
        assert_eq!(
            poll.url,
            "https://weixin.qq.com/ilink/bot/get_qrcode_status?qrcode=qr-1&verify_code=123456"
        );
        assert_eq!(poll.method, crate::common::HttpMethod::Get);
        assert_eq!(poll.header_value("Authorization"), None);
    }

    #[tokio::test]
    async fn qrcode_challenge_requires_a_numeric_bot_type_and_both_fields() {
        let outbound = client(std::sync::Arc::new(RecordingTransport::new()));
        assert!(outbound.prepare_qrcode_challenge("").is_err());
        assert!(outbound.prepare_qrcode_challenge("a3").is_err());
        assert!(outbound.prepare_qrcode_challenge("3").is_ok());

        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"qrcode\":\"qr-1\"}"),
        ]));
        let error = client(transport).qrcode_challenge("3").await.unwrap_err();
        assert!(error.to_string().contains("qrcode_img_content"));
    }

    #[tokio::test]
    async fn base_url_from_properties_is_used_for_every_bot_call() {
        let transport = std::sync::Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"ret\":0}"),
        ]));
        let outbound = WeixinOutboundClient::with_wechat_uin(
            properties_at("http://127.0.0.1:8089"),
            FixedCredentialProvider::from_token("tok").unwrap().shared(),
            transport.clone(),
            1,
        );
        outbound.notify_start().await.unwrap();
        assert_eq!(
            transport.last_request().unwrap().url,
            "http://127.0.0.1:8089/ilink/bot/msg/notifystart"
        );
    }
}
