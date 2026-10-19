//! iLink provider DTOs: the inbound message envelope, the long-poll response and the login step.
//!
//! Rust port of the Jackson `JsonNode` shapes the Java adapter reads directly. Every field is
//! optional except where the provider contract guarantees it, and unknown fields are ignored so a
//! provider-side protocol extension cannot break parsing (mirrors the Java `path(...)` reads).

use serde::{Deserialize, Serialize};

use super::error::WeixinError;
use crate::common::Result;

/// One entry of `item_list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WeixinItem {
    /// `1` = text item; every other type is ignored by the mapper.
    #[serde(default, rename = "type")]
    pub item_type: i64,
    /// Present for text items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_item: Option<WeixinTextItem>,
}

/// The text payload of a `type = 1` item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WeixinTextItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One message from a `getupdates` batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WeixinMessage {
    /// `1` = user text message. Absent is read as `0`, which the mapper drops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_type: Option<i64>,
    /// Provider message id; absent messages are keyed by a payload digest instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// The peer that sent the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_user_id: Option<String>,
    /// Reply routing token; echoed back on the outbound reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_list: Option<Vec<WeixinItem>>,
}

impl WeixinMessage {
    /// The first text of the first `type = 1` item, trimmed; `None` when absent/blank.
    pub fn text(&self) -> Option<String> {
        let items = self.item_list.as_ref()?;
        items
            .iter()
            .find(|item| item.item_type == 1)
            .and_then(|item| item.text_item.as_ref())
            .and_then(|text| text.text.as_deref())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }

    /// The provider message id, when it is present and non-blank.
    pub fn message_id(&self) -> Option<String> {
        self.message_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    }
}

/// `getupdates` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WeixinUpdatesResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ret: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errcode: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msgs: Option<Vec<WeixinMessage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub get_updates_buf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longpolling_timeout_ms: Option<i64>,
}

impl WeixinUpdatesResponse {
    /// The batch, or an empty slice when the response carries no `msgs`.
    pub fn messages(&self) -> Vec<WeixinMessage> {
        self.msgs.clone().unwrap_or_default()
    }

    /// `ret`, defaulted to `0` the way the Java `path("ret").asInt(0)` does.
    pub fn ret_or_zero(&self) -> i64 {
        self.ret.unwrap_or(0)
    }

    /// Whether the response carries the `-14` credential-rejection code in either field.
    pub fn is_credential_rejected(&self) -> bool {
        self.ret == Some(-14) || self.errcode == Some(-14)
    }

    /// Whether the response reports an unsuccessful outcome.
    pub fn is_failure(&self) -> bool {
        self.ret_or_zero() != 0 || self.errcode.is_some_and(|code| code != 0)
    }
}

/// Portable iLink QR-login state required to perform the next polling operation.
///
/// The `qrcode` is login-secret material: `Debug`/`Display` redact it, mirroring the Java record.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeixinLoginSession {
    pub qrcode: String,
    pub polling_base_url: String,
}

impl WeixinLoginSession {
    /// Build a session, rejecting blank fields (mirrors the Java compact constructor).
    pub fn new(qrcode: impl Into<String>, polling_base_url: impl Into<String>) -> Result<Self> {
        let qrcode = qrcode.into();
        let polling_base_url = polling_base_url.into();
        if qrcode.trim().is_empty() || polling_base_url.trim().is_empty() {
            return Err(WeixinError::operation(
                "Weixin login session fields must not be blank",
            )
            .into_channel());
        }
        Ok(Self {
            qrcode,
            polling_base_url,
        })
    }

    /// The same session pointed at a different polling host (the `scaned_but_redirect` switch).
    pub fn with_polling_base_url(&self, next_base_url: impl Into<String>) -> Self {
        Self {
            qrcode: self.qrcode.clone(),
            polling_base_url: next_base_url.into(),
        }
    }
}

impl std::fmt::Debug for WeixinLoginSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WeixinLoginSession[qrcode=***, pollingBaseUrl={}]",
            self.polling_base_url
        )
    }
}

impl std::fmt::Display for WeixinLoginSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WeixinLoginSession[qrcode=***, pollingBaseUrl={}]",
            self.polling_base_url
        )
    }
}

/// Initial QR challenge and portable session returned when an iLink login starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeixinLoginChallenge {
    pub session: WeixinLoginSession,
    /// QR image content. Secret-ish material, so it is not rendered by `Display`.
    pub image_content: String,
}

impl WeixinLoginChallenge {
    /// Build a challenge, rejecting blank fields.
    pub fn new(session: WeixinLoginSession, image_content: impl Into<String>) -> Result<Self> {
        let image_content = image_content.into();
        if image_content.trim().is_empty() {
            return Err(WeixinError::operation("imageContent must not be blank").into_channel());
        }
        Ok(Self {
            session,
            image_content,
        })
    }
}

impl std::fmt::Display for WeixinLoginChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WeixinLoginChallenge[session={}, imageContent=***]", self.session)
    }
}

/// One provider observation in an iLink QR-login sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeixinLoginStep {
    pub session: WeixinLoginSession,
    /// Provider status (`wait`, `scaned_but_redirect`, `need_verifycode`, `confirmed`, ...).
    pub status: String,
    /// Present once the provider issued a bot token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<super::credentials::WeixinCredentials>,
    /// `ilink_bot_id` — the bot/account identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// `ilink_user_id` — the person who scanned the QR code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Provider-supplied base URL for the following polls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl WeixinLoginStep {
    /// Mirrors the Java `connected()`: a confirmation status, or a token in hand.
    pub fn connected(&self) -> bool {
        self.status.eq_ignore_ascii_case("confirmed") || self.credentials.is_some()
    }

    /// Mirrors the Java `alreadyConnected()`.
    pub fn already_connected(&self) -> bool {
        self.status.eq_ignore_ascii_case("binded_redirect")
    }

    /// Mirrors the Java `requiresVerification()`.
    pub fn requires_verification(&self) -> bool {
        self.status.eq_ignore_ascii_case("need_verifycode")
    }
}

/// Raw `get_qrcode_status` body, before endpoint validation and DTO mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WeixinLoginStatusResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ilink_bot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ilink_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseurl: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ret: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errcode: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errmsg: Option<String>,
}

/// Raw `get_bot_qrcode` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WeixinQrCodeResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qrcode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qrcode_img_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ret: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errcode: Option<i64>,
}

/// A required provider field, mirroring the Java `required(node, field)` helper.
pub fn required_field(field: &str, value: Option<&str>) -> Result<String> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => Ok(value.to_string()),
        None => Err(WeixinError::operation(format!("iLink response missing {field}")).into_channel()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_message_deserializes_a_direct_text_event() {
        let raw = "{\n            \"message_type\":1,\n            \"message_id\":\"m1\",\n            \"from_user_id\":\"u1\",\n            \"context_token\":\"ctx\",\n            \"item_list\":[{\"type\":1,\"text_item\":{\"text\":\" hi \"}}]\n        }";
        let message: WeixinMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(message.message_type, Some(1));
        assert_eq!(message.message_id().as_deref(), Some("m1"));
        assert_eq!(message.text().as_deref(), Some("hi"));
        assert_eq!(message.context_token.as_deref(), Some("ctx"));
    }

    #[test]
    fn text_lookup_skips_non_text_items_and_reports_nothing_for_blank_text() {
        let message = WeixinMessage {
            item_list: Some(vec![
                WeixinItem {
                    item_type: 2,
                    text_item: Some(WeixinTextItem {
                        text: Some("ignored".to_string()),
                    }),
                },
                WeixinItem {
                    item_type: 1,
                    text_item: Some(WeixinTextItem {
                        text: Some("   ".to_string()),
                    }),
                },
            ]),
            ..Default::default()
        };
        assert_eq!(message.text(), None);
        assert_eq!(WeixinMessage::default().text(), None);
    }

    #[test]
    fn updates_response_ignores_additional_protocol_fields() {
        let raw = "{\"msgs\":[],\"get_updates_buf\":\"next\",\"longpolling_timeout_ms\":35000,\"server_extension\":{\"enabled\":true}}";
        let response: WeixinUpdatesResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(response.ret_or_zero(), 0);
        assert!(!response.is_failure());
        assert_eq!(response.get_updates_buf.as_deref(), Some("next"));
        assert_eq!(response.longpolling_timeout_ms, Some(35_000));
        assert!(response.messages().is_empty());
    }

    #[test]
    fn credential_rejection_is_detected_in_either_field() {
        let ret: WeixinUpdatesResponse = serde_json::from_str("{\"ret\":-14}").unwrap();
        assert!(ret.is_credential_rejected() && ret.is_failure());
        let errcode: WeixinUpdatesResponse =
            serde_json::from_str("{\"ret\":0,\"errcode\":-14}").unwrap();
        assert!(errcode.is_credential_rejected() && errcode.is_failure());
        let healthy: WeixinUpdatesResponse =
            serde_json::from_str("{\"ret\":0,\"errcode\":0}").unwrap();
        assert!(!healthy.is_credential_rejected() && !healthy.is_failure());
    }

    #[test]
    fn login_session_and_challenge_redact_secret_material() {
        let session = WeixinLoginSession::new("secret-qr", "https://weixin.qq.com").unwrap();
        assert!(!session.to_string().contains("secret-qr"));
        assert!(!format!("{session:?}").contains("secret-qr"));
        let challenge = WeixinLoginChallenge::new(session, "secret-image").unwrap();
        assert!(!challenge.to_string().contains("secret-image"));
        assert!(WeixinLoginChallenge::new(
            WeixinLoginSession::new("qr", "https://weixin.qq.com").unwrap(),
            "  "
        )
        .is_err());
    }

    #[test]
    fn login_step_status_predicates_match_java() {
        let session = WeixinLoginSession::new("qr", "https://weixin.qq.com").unwrap();
        let step = |status: &str| WeixinLoginStep {
            session: session.clone(),
            status: status.to_string(),
            credentials: None,
            account_id: None,
            user_id: None,
            base_url: None,
        };
        assert!(step("CONFIRMED").connected());
        assert!(!step("wait").connected());
        assert!(step("binded_redirect").already_connected());
        assert!(step("need_verifycode").requires_verification());
        let mut with_token = step("wait");
        with_token.credentials = Some(super::super::credentials::WeixinCredentials::new("t").unwrap());
        assert!(with_token.connected());
    }

    #[test]
    fn required_field_names_the_missing_field() {
        assert_eq!(required_field("qrcode", Some(" q ")).unwrap(), "q");
        let error = required_field("qrcode", None).unwrap_err().to_string();
        assert!(error.contains("qrcode"));
        assert!(required_field("qrcode", Some(" ")).is_err());
    }
}
