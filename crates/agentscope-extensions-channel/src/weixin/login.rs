//! Stateless QR login for Tencent iLink personal Weixin accounts.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinLoginClient`. Each call returns a
//! portable [`WeixinLoginSession`], so an attempt can be resumed by another process without
//! carrying the QR image along.
//!
//! **The status URL carries a login secret.** The provider protocol takes `qrcode` — and, when it
//! asks for one, `verify_code` — as GET query parameters of `get_qrcode_status`, so the request URI
//! is short-lived credential material rather than something safe to record. Nothing here logs it or
//! copies it into an error message: failures are reported by status code and parse position only.

use super::dto::{required_field, WeixinLoginSession, WeixinLoginStatusResponse, WeixinLoginStep};
use super::endpoint::{normalize_base_url, validate_provider_endpoint, DEFAULT_BASE_URL};
use super::error::{WeixinError, WeixinResult};
use super::outbound::WeixinOutboundClient;
use super::properties::WeixinChannelProperties;
use super::protocol::PATH_GET_QRCODE;
use serde_json::Value;

/// Default QR-login timeout in milliseconds (Java: `Duration.ofSeconds(45)`).
pub const DEFAULT_LOGIN_TIMEOUT_MS: i64 = 45_000;

/// Stateless iLink QR-login client.
pub struct WeixinLoginClient {
    base_url: String,
    timeout_ms: i64,
    outbound: WeixinOutboundClient,
}

impl WeixinLoginClient {
    /// Build around an outbound client (which supplies the transport and the protocol headers).
    pub fn new(
        base_url: &str,
        timeout_ms: i64,
        outbound: WeixinOutboundClient,
    ) -> WeixinResult<Self> {
        Ok(Self {
            // The endpoint policy's own message is kept (it is built from constants only), so a
            // host can tell "Invalid iLink base URL" from "not an approved provider endpoint".
            base_url: normalize_base_url(base_url).map_err(endpoint_error)?,
            timeout_ms: if timeout_ms > 0 {
                timeout_ms
            } else {
                DEFAULT_LOGIN_TIMEOUT_MS
            },
            outbound,
        })
    }

    /// Build against the default iLink provider.
    pub fn with_default_base_url(outbound: WeixinOutboundClient) -> WeixinResult<Self> {
        Self::new(DEFAULT_BASE_URL, DEFAULT_LOGIN_TIMEOUT_MS, outbound)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn timeout_ms(&self) -> i64 {
        self.timeout_ms
    }

    /// Request a QR challenge. `bot_type` defaults to `"3"` and must be numeric.
    pub async fn start(&self, bot_type: Option<&str>) -> WeixinResult<WeixinLoginChallengeResult> {
        let bot_type = bot_type
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("3");
        if !bot_type.chars().all(|c| c.is_ascii_digit()) {
            return Err(WeixinError::operation("botType must be numeric"));
        }
        let (challenge, session) = self
            .outbound
            .qrcode_challenge_on(&self.base_url, bot_type)
            .await?;
        Ok(WeixinLoginChallengeResult {
            session,
            image_content: challenge.image_content,
        })
    }

    /// Poll the provider for the next login observation.
    pub async fn poll(&self, session: &WeixinLoginSession) -> WeixinResult<WeixinLoginStep> {
        self.observe(session, None).await
    }

    /// Submit the verification code and observe the result. A blank code is rejected before any
    /// request is built, mirroring the Java guard.
    pub async fn verify(
        &self,
        session: &WeixinLoginSession,
        verify_code: &str,
    ) -> WeixinResult<WeixinLoginStep> {
        let code = verify_code.trim();
        if code.is_empty() {
            return Err(WeixinError::operation("verifyCode must not be blank"));
        }
        self.observe(session, Some(code)).await
    }

    async fn observe(
        &self,
        session: &WeixinLoginSession,
        verify_code: Option<&str>,
    ) -> WeixinResult<WeixinLoginStep> {
        // The host switch is re-validated on every poll: a provider that answered with an
        // untrusted redirect host cannot move the next request off the allowlist.
        let polling_base_url = self.authorize(&self.base_url, &session.polling_base_url)?;
        let node = self
            .outbound
            .qrcode_status(&polling_base_url, session, verify_code)
            .await?;
        self.step(session, node)
    }

    /// Turn a raw status body into a [`WeixinLoginStep`], applying the `scaned_but_redirect` switch
    /// and validating every provider-supplied URL.
    fn step(&self, session: &WeixinLoginSession, node: Value) -> WeixinResult<WeixinLoginStep> {
        let body: WeixinLoginStatusResponse = serde_json::from_value(node)
            .map_err(|_| WeixinError::operation("iLink returned an unparseable login response"))?;
        let status = body
            .status
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();

        let mut updated = session.clone();
        if status.eq_ignore_ascii_case("scaned_but_redirect") {
            if let Some(redirect) = body
                .redirect_host
                .as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty())
            {
                let next = self.authorize(&session.polling_base_url, redirect)?;
                updated = session.with_polling_base_url(next);
            }
        }

        let credentials = match body.bot_token.as_deref().map(str::trim) {
            Some(token) if !token.is_empty() => Some(
                super::credentials::WeixinCredentials::new(token)
                    .map_err(|_| WeixinError::operation("botToken must not be blank"))?,
            ),
            _ => None,
        };

        let base_url = match body
            .baseurl
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            Some(candidate) => Some(self.authorize(&updated.polling_base_url, candidate)?),
            None => None,
        };

        Ok(WeixinLoginStep {
            session: updated,
            status,
            credentials,
            account_id: non_blank(body.ilink_bot_id),
            user_id: non_blank(body.ilink_user_id),
            base_url,
        })
    }

    /// Normalize and authorize a provider-supplied endpoint against `current`.
    fn authorize(&self, current: &str, candidate: &str) -> WeixinResult<String> {
        let normalized = normalize_base_url(candidate).map_err(endpoint_error)?;
        validate_provider_endpoint(current, &normalized).map_err(endpoint_error)
    }
}

/// Turn a shared endpoint-policy failure into a module error, keeping the policy's own message.
///
/// Both messages are built from constants only (never from the URL), so neither leaks the login
/// secret the status query carries.
fn endpoint_error(error: crate::common::ChannelError) -> WeixinError {
    WeixinError::operation(error.to_string())
}

fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The result of [`WeixinLoginClient::start`]: the portable session plus the QR image content.
#[derive(Clone, PartialEq, Eq)]
pub struct WeixinLoginChallengeResult {
    pub session: WeixinLoginSession,
    /// QR image content. Never rendered by `Display`.
    pub image_content: String,
}

impl std::fmt::Debug for WeixinLoginChallengeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WeixinLoginChallengeResult[session={:?}, imageContent=***]",
            self.session
        )
    }
}

impl std::fmt::Display for WeixinLoginChallengeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WeixinLoginChallengeResult[session={}, imageContent=***]",
            self.session
        )
    }
}

/// The QR-login paths, exposed so a host can report which endpoint it is about to call.
pub fn challenge_path() -> &'static str {
    PATH_GET_QRCODE
}

/// A required-field helper exposed for hosts that parse the raw login bodies themselves.
pub fn login_required(field: &str, value: Option<&str>) -> WeixinResult<String> {
    required_field(field, value)
        .map_err(|_| WeixinError::operation(format!("iLink response missing {field}")))
}

/// The provider properties a login client needs (its base URL and timeouts).
pub fn login_properties(
    base_url: &str,
    request_timeout_ms: i64,
) -> WeixinResult<WeixinChannelProperties> {
    WeixinChannelProperties::from_json(
        "weixin-login",
        &serde_json::json!({
            "accountId": "weixin-login",
            "baseUrl": base_url,
            "requestTimeoutMs": request_timeout_ms,
        }),
    )
    .map_err(|_| WeixinError::operation("Invalid iLink base URL"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;
    use crate::weixin::credentials::FixedCredentialProvider;
    use std::sync::Arc;

    fn outbound(transport: Arc<RecordingTransport>, base_url: &str) -> WeixinOutboundClient {
        let properties = WeixinChannelProperties::from_json(
            "wx",
            &serde_json::json!({ "accountId": "acct", "baseUrl": base_url }),
        )
        .unwrap();
        WeixinOutboundClient::with_wechat_uin(
            properties,
            FixedCredentialProvider::from_token("tok").unwrap().shared(),
            transport,
            7,
        )
    }

    fn challenge_body() -> PreparedResponse {
        PreparedResponse::new(
            200,
            "{\"qrcode\":\"qr-1\",\"qrcode_img_content\":\"https://qr\"}",
        )
    }

    #[test]
    fn rejects_a_base_url_off_the_allowlist() {
        let transport = Arc::new(RecordingTransport::new());
        assert!(WeixinLoginClient::new(
            "https://evil.example.com",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport.clone(), "https://ilinkai.weixin.qq.com")
        )
        .is_err());
        let client = WeixinLoginClient::new(
            "ilinkai.weixin.qq.com",
            0,
            outbound(transport, "https://ilinkai.weixin.qq.com"),
        )
        .unwrap();
        assert_eq!(client.base_url(), "https://ilinkai.weixin.qq.com");
        // A non-positive timeout falls back to the Java default.
        assert_eq!(client.timeout_ms(), DEFAULT_LOGIN_TIMEOUT_MS);
    }

    #[tokio::test]
    async fn start_returns_a_portable_session_and_the_qr_image() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![challenge_body()]));
        let client = WeixinLoginClient::new(
            "https://ilinkai.weixin.qq.com",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport.clone(), "https://ilinkai.weixin.qq.com"),
        )
        .unwrap();
        let challenge = client.start(None).await.unwrap();
        assert_eq!(challenge.session.qrcode, "qr-1");
        assert_eq!(challenge.image_content, "https://qr");
        assert!(!challenge.to_string().contains("https://qr"));
        assert_eq!(
            transport.last_request().unwrap().url,
            "https://ilinkai.weixin.qq.com/ilink/bot/get_bot_qrcode?bot_type=3"
        );

        // A non-numeric bot type is refused before any request is built.
        assert!(client.start(Some("bot")).await.is_err());
    }

    #[tokio::test]
    async fn poll_follows_a_scaned_but_redirect_host_switch() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                "{\"status\":\"scaned_but_redirect\",\"redirect_host\":\"https://weixin.qq.com\"}",
            ),
            PreparedResponse::new(
                200,
                "{\"status\":\"confirmed\",\"bot_token\":\"secret\",\"ilink_bot_id\":\"account-1\",\"ilink_user_id\":\"user-1\"}",
            ),
        ]));
        let client = WeixinLoginClient::new(
            "https://ilinkai.weixin.qq.com",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport.clone(), "https://ilinkai.weixin.qq.com"),
        )
        .unwrap();
        let session = WeixinLoginSession::new("qr-1", "https://ilinkai.weixin.qq.com").unwrap();

        let redirect = client.poll(&session).await.unwrap();
        assert_eq!(redirect.status, "scaned_but_redirect");
        assert_eq!(redirect.session.polling_base_url, "https://weixin.qq.com");

        let confirmed = client.poll(&redirect.session).await.unwrap();
        assert!(confirmed.connected());
        assert_eq!(confirmed.account_id.as_deref(), Some("account-1"));
        assert_eq!(confirmed.user_id.as_deref(), Some("user-1"));
        assert_eq!(
            confirmed.credentials.as_ref().map(|c| c.bot_token()),
            Some("secret")
        );
        assert_eq!(
            transport.last_request().unwrap().url,
            "https://weixin.qq.com/ilink/bot/get_qrcode_status?qrcode=qr-1"
        );
        // The session carried the switched host forward.
        assert_eq!(confirmed.session.polling_base_url, "https://weixin.qq.com");
    }

    #[tokio::test]
    async fn an_untrusted_redirect_host_is_refused() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                "{\"status\":\"scaned_but_redirect\",\"redirect_host\":\"https://evil.example.com\"}",
            ),
        ]));
        let client = WeixinLoginClient::new(
            "https://ilinkai.weixin.qq.com",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport.clone(), "https://ilinkai.weixin.qq.com"),
        )
        .unwrap();
        let session = WeixinLoginSession::new("qr-1", "https://ilinkai.weixin.qq.com").unwrap();
        let error = client.poll(&session).await.unwrap_err();
        // The endpoint allowlist rejects the host (Java `IllegalArgumentException` from
        // `normalizeBaseUrl`), and the refusal never quotes the URL or the login secret.
        assert!(error.to_string().contains("iLink base URL"), "{error}");
        assert!(!error.to_string().contains("qr-1"), "{error}");
        assert!(!error.to_string().contains("evil.example.com"), "{error}");
    }

    #[tokio::test]
    async fn an_off_allowlist_redirect_is_refused_as_untrusted() {
        // A redirect that resolves to an approved *host type* but is not the same provider and not
        // loopback takes the `validateProviderEndpoint` path ("not an approved provider endpoint").
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                "{\"status\":\"scaned_but_redirect\",\"redirect_host\":\"https://weixin.qq.com.evil.com\"}",
            ),
        ]));
        let client = WeixinLoginClient::new(
            "https://ilinkai.weixin.qq.com",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport, "https://ilinkai.weixin.qq.com"),
        )
        .unwrap();
        let session = WeixinLoginSession::new("qr-1", "https://ilinkai.weixin.qq.com").unwrap();
        let error = client.poll(&session).await.unwrap_err();
        assert!(error.to_string().contains("iLink base URL"), "{error}");
        assert!(!error.to_string().contains("evil.com"), "{error}");
    }

    #[tokio::test]
    async fn a_session_cannot_be_polled_against_a_foreign_host() {
        let transport = Arc::new(RecordingTransport::new());
        let client = WeixinLoginClient::new(
            "http://127.0.0.1:8089",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport.clone(), "http://127.0.0.1:8089"),
        )
        .unwrap();
        // A session pointing at the official provider cannot be reached from a loopback client.
        let session = WeixinLoginSession::new("qr-1", "https://ilinkai.weixin.qq.com").unwrap();
        assert!(client.poll(&session).await.is_err());
        assert_eq!(transport.request_count(), 0);
    }

    #[tokio::test]
    async fn verify_requires_a_code_and_sends_it_with_the_status_query() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                "{\"status\":\"confirmed\",\"bot_token\":\"secret\",\"ilink_bot_id\":\"account-1\",\"ilink_user_id\":\"user-1\"}",
            ),
        ]));
        let client = WeixinLoginClient::new(
            "https://ilinkai.weixin.qq.com",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport.clone(), "https://ilinkai.weixin.qq.com"),
        )
        .unwrap();
        let session = WeixinLoginSession::new("qr 1", "https://ilinkai.weixin.qq.com").unwrap();

        assert!(client.verify(&session, "   ").await.is_err());
        assert_eq!(transport.request_count(), 0);

        let step = client.verify(&session, " 123456 ").await.unwrap();
        assert!(step.connected());
        assert_eq!(
            transport.last_request().unwrap().url,
            "https://ilinkai.weixin.qq.com/ilink/bot/get_qrcode_status?qrcode=qr+1&verify_code=123456"
        );
    }

    #[tokio::test]
    async fn observes_the_verification_challenge_and_unknown_status() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"status\":\"need_verifycode\"}"),
            PreparedResponse::new(200, "{}"),
        ]));
        let client = WeixinLoginClient::new(
            "https://ilinkai.weixin.qq.com",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport, "https://ilinkai.weixin.qq.com"),
        )
        .unwrap();
        let session = WeixinLoginSession::new("qr-1", "https://ilinkai.weixin.qq.com").unwrap();

        let step = client.poll(&session).await.unwrap();
        assert!(step.requires_verification());
        assert!(!step.connected());
        assert_eq!(step.credentials, None);

        // A body with no status at all reads as `unknown`, not as a connection.
        let unknown = client.poll(&session).await.unwrap();
        assert_eq!(unknown.status, "unknown");
        assert!(!unknown.connected());
        assert!(!unknown.already_connected());
    }

    #[tokio::test]
    async fn a_provider_baseurl_is_authorized_before_it_is_reported() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                "{\"status\":\"confirmed\",\"bot_token\":\"secret\",\"baseurl\":\"https://weixin.qq.com\"}",
            ),
            PreparedResponse::new(
                200,
                "{\"status\":\"confirmed\",\"bot_token\":\"secret\",\"baseurl\":\"https://evil.example.com\"}",
            ),
        ]));
        let client = WeixinLoginClient::new(
            "https://ilinkai.weixin.qq.com",
            DEFAULT_LOGIN_TIMEOUT_MS,
            outbound(transport, "https://ilinkai.weixin.qq.com"),
        )
        .unwrap();
        let session = WeixinLoginSession::new("qr-1", "https://ilinkai.weixin.qq.com").unwrap();
        assert_eq!(
            client.poll(&session).await.unwrap().base_url.as_deref(),
            Some("https://weixin.qq.com")
        );
        assert!(client.poll(&session).await.is_err());
    }
}
