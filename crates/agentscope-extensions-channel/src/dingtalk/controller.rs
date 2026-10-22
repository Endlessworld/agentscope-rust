//! HTTP callback entry point for DingTalk channels running in `http` mode.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk.DingTalkCallbackController`.
//!
//! Responsibilities, in the Java order:
//! 1. look the channel up by `channelId` (unknown -> `401`, indistinguishable from a bad
//!    signature so the endpoint is not a channel oracle);
//! 2. verify the `timestamp`/`sign` headers (Java HMAC-SHA256 scheme) or, when a legacy `token` is
//!    configured, the `msg_signature` query parameter (classic SHA-1 scheme);
//! 3. parse the body, require an `{"encrypt": ...}` envelope when the channel has an `aesKey`
//!    (fail closed), and decrypt it otherwise;
//! 4. answer DingTalk's first `check_url` verification event with an encrypted `success` reply;
//! 5. hand every other payload to [`DingTalkChannel::handle_stream_payload`] so both reception
//!    modes share one intake pipeline (msgId dedupe, mapping, bot-loop guard, dispatch).
//!
//! Any accepted payload answers `200` with an empty body (the actual reply is delivered through
//! the outbound OpenAPI, not this response), so DingTalk does not retry.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde_json::Value;

use super::channel::{DingTalkChannel, DingTalkChannelRegistry};

/// Upper bound on a callback body accepted for parsing (Java `MAX_BODY_LENGTH`, measured in
/// characters).
pub const MAX_BODY_LENGTH: usize = 64 * 1024;

/// Body returned for DingTalk's first `check_url` verification event.
pub const CHECK_URL_SUCCESS: &str = r#"{"msgtype":"text","text":{"content":"success"}}"#;

/// Framework-agnostic outcome of one callback, mirroring the Java controller's HTTP answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DingTalkCallbackOutcome {
    /// Unknown channel, missing headers, or signature mismatch (401).
    Unauthorized,
    /// Empty / oversized / malformed body, or a decryption failure (400).
    BadRequest,
    /// Payload accepted (dispatched, duplicate, non-text, or loop-guarded) -> empty 200.
    Accepted,
    /// URL-verification event: 200 with this plain-text body (encrypted when `aesKey` is set).
    Challenge(String),
}

/// Callback dispatcher bound to a typed registry.
#[derive(Debug)]
pub struct DingTalkCallbackController {
    registry: Arc<DingTalkChannelRegistry>,
}

impl DingTalkCallbackController {
    pub fn new(registry: Arc<DingTalkChannelRegistry>) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &Arc<DingTalkChannelRegistry> {
        &self.registry
    }

    /// Dispatch one callback.
    ///
    /// `sign` is the Java-scheme HMAC-SHA256 header; `msg_signature`/`nonce` are the optional
    /// classic-scheme headers (only consulted when `sign` does not verify and the channel has a
    /// legacy `token`).
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch(
        &self,
        channel_id: &str,
        timestamp: Option<&str>,
        sign: Option<&str>,
        msg_signature: Option<&str>,
        nonce: Option<&str>,
        body: &str,
    ) -> DingTalkCallbackOutcome {
        let Some(channel) = self.registry.get(channel_id) else {
            // Java: 401, not 404 — an unauthenticated probe must not learn whether the id exists.
            return DingTalkCallbackOutcome::Unauthorized;
        };
        let Some(crypto) = channel.crypto() else {
            // Only http-mode channels install crypto; a stream-mode handle cannot answer callbacks.
            return DingTalkCallbackOutcome::Unauthorized;
        };

        let mut authenticated = crypto.verify(timestamp, sign);
        if !authenticated {
            // Classic scheme: msg_signature covers the `encrypt` value, so the body must be
            // inspected before the signature can be checked.
            if let (Some(msg_signature), Some(timestamp), Some(nonce)) =
                (msg_signature, timestamp, nonce)
            {
                if let Some(encrypt) = extract_encrypt_raw(body) {
                    authenticated =
                        crypto.verify_legacy(Some(msg_signature), timestamp, nonce, &encrypt);
                }
            }
        }
        if !authenticated {
            return DingTalkCallbackOutcome::Unauthorized;
        }

        let payload = match extract_payload(&channel, body) {
            Ok(payload) => payload,
            Err(_) => return DingTalkCallbackOutcome::BadRequest,
        };

        // DingTalk's first URL verification replaces the deprecated GET handshake with a signed
        // event; answer it instead of trying to map it as a bot message.
        if is_check_url(&payload) {
            if !crypto.has_aes_key() {
                // Unencrypted channels answer the verification event in plain text.
                return DingTalkCallbackOutcome::Challenge(CHECK_URL_SUCCESS.to_string());
            }
            return match crypto.encrypt(CHECK_URL_SUCCESS, None) {
                Ok(encrypted) => DingTalkCallbackOutcome::Challenge(encrypted),
                Err(_) => DingTalkCallbackOutcome::BadRequest,
            };
        }

        channel.handle_stream_payload(&payload).await;
        DingTalkCallbackOutcome::Accepted
    }
}

/// Build the axum router: `POST /api/channels/dingtalk/:channel_id/callback`.
pub fn router(registry: Arc<DingTalkChannelRegistry>) -> Router {
    Router::new()
        .route("/api/channels/dingtalk/:channel_id/callback", post(handle))
        .with_state(Arc::new(DingTalkCallbackController::new(registry)))
}

async fn handle(
    State(controller): State<Arc<DingTalkCallbackController>>,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw = String::from_utf8_lossy(&body);
    let outcome = controller
        .dispatch(
            &channel_id,
            header_value(&headers, "timestamp"),
            header_value(&headers, "sign"),
            header_value(&headers, "msg_signature"),
            header_value(&headers, "nonce"),
            &raw,
        )
        .await;
    into_response(outcome)
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Map the outcome onto the same statuses as the Java controller (`401` / `400` / empty `200`).
fn into_response(outcome: DingTalkCallbackOutcome) -> Response {
    match outcome {
        DingTalkCallbackOutcome::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        DingTalkCallbackOutcome::BadRequest => (StatusCode::BAD_REQUEST, "").into_response(),
        DingTalkCallbackOutcome::Accepted => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            String::new(),
        )
            .into_response(),
        DingTalkCallbackOutcome::Challenge(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            body,
        )
            .into_response(),
    }
}

/// Java `extractPayload`: validate size, parse JSON, require/ignore the `encrypt` envelope.
fn extract_payload(channel: &DingTalkChannel, body: &str) -> Result<Value, String> {
    if body.trim().is_empty() {
        return Err("Request body is empty".to_string());
    }
    // Java measures `String.length()` in UTF-16 code units.
    if body.encode_utf16().count() > MAX_BODY_LENGTH {
        return Err(format!("Request body exceeds {MAX_BODY_LENGTH} characters"));
    }
    let root: Value =
        serde_json::from_str(body).map_err(|e| format!("Request body is not valid JSON: {e}"))?;

    let encrypt = root
        .get("encrypt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let crypto = channel.crypto().expect("caller checked crypto");

    // Fail closed: an encrypted channel only ever receives envelopes. The reverse mismatch
    // (envelope on a plain channel) is a 400 because there is no key to decrypt it with.
    if crypto.has_aes_key() && encrypt.is_none() {
        return Err("Channel expects an encrypted callback body but none was present".to_string());
    }
    let Some(encrypt) = encrypt else {
        return Ok(root);
    };

    let plain = crypto
        .decrypt(encrypt)
        .map_err(|e| format!("callback decrypt failed: {e}"))?;
    serde_json::from_str(&plain).map_err(|e| format!("Decrypted payload is not valid JSON: {e}"))
}

/// The untrimmed `encrypt` string, needed to verify the classic `msg_signature`.
fn extract_encrypt_raw(body: &str) -> Option<String> {
    let root: Value = serde_json::from_str(body).ok()?;
    root.get("encrypt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn is_check_url(payload: &Value) -> bool {
    payload
        .get("eventType")
        .and_then(Value::as_str)
        .map(|value| value.eq_ignore_ascii_case("check_url"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use agentscope_harness::agent::ChannelConfig;

    use crate::common::testing::RecordingTransport;
    use crate::dingtalk::crypto::DingTalkCallbackCrypto;

    const SECRET: &str = "test-app-secret";
    const AES_KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY";

    fn props(extra: Value) -> Value {
        let mut base = serde_json::json!({
            "appKey": "app-key",
            "appSecret": SECRET,
            "robotCode": "robot",
            "mode": "http"
        });
        let object = base.as_object_mut().unwrap();
        for (key, value) in extra.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }
        base
    }

    fn channel(id: &str, extra: Value) -> Arc<DingTalkChannel> {
        Arc::new(
            DingTalkChannel::from_json(
                id,
                ChannelConfig::of(id),
                &props(extra),
                Arc::new(RecordingTransport::new()),
            )
            .unwrap(),
        )
    }

    async fn setup(extra: Value) -> (Arc<DingTalkCallbackController>, Arc<DingTalkChannel>) {
        let channel = channel("dt-test", extra);
        let registry = Arc::new(DingTalkChannelRegistry::new());
        registry.register(channel.clone()).await;
        (Arc::new(DingTalkCallbackController::new(registry)), channel)
    }

    fn now() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string()
    }

    fn sign(channel: &DingTalkChannel, timestamp: &str) -> String {
        channel.crypto().unwrap().sign(timestamp).unwrap()
    }

    fn bot_message(msg_id: &str) -> String {
        format!(
            r#"{{"msgtype":"text","text":{{"content":"hello"}},"conversationId":"cid-1","conversationType":"1","senderStaffId":"staff-1","msgId":"{msg_id}"}}"#
        )
    }

    fn encrypted_body(channel: &DingTalkChannel, json: &str) -> String {
        let encrypted = channel.crypto().unwrap().encrypt(json, None).unwrap();
        format!(r#"{{"encrypt":"{encrypted}"}}"#)
    }

    #[tokio::test]
    async fn returns_401_for_unknown_channel_and_missing_headers() {
        let (controller, _) = setup(serde_json::json!({})).await;
        let ts = now();
        let outcome = controller
            .dispatch(
                "unknown",
                Some(&ts),
                Some("any"),
                None,
                None,
                &bot_message("m-1"),
            )
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Unauthorized);

        let outcome = controller
            .dispatch("dt-test", None, None, None, None, &bot_message("m-2"))
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Unauthorized);
    }

    #[tokio::test]
    async fn returns_401_for_a_bad_or_stale_signature() {
        let (controller, channel) = setup(serde_json::json!({})).await;
        let ts = now();
        let body = bot_message("m-bad");
        let outcome = controller
            .dispatch("dt-test", Some(&ts), Some("wrong-sign"), None, None, &body)
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Unauthorized);

        let stale = (ts.parse::<i64>().unwrap() - 2 * 3_600_000).to_string();
        let stale_sign = sign(&channel, &stale);
        let outcome = controller
            .dispatch(
                "dt-test",
                Some(&stale),
                Some(&stale_sign),
                None,
                None,
                &body,
            )
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Unauthorized);
    }

    #[tokio::test]
    async fn dispatches_a_plain_message_and_drops_duplicates() {
        let (controller, channel) = setup(serde_json::json!({})).await;
        let ts = now();
        let signature = sign(&channel, &ts);
        let body = bot_message("m-1");

        let outcome = controller
            .dispatch("dt-test", Some(&ts), Some(&signature), None, None, &body)
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Accepted);
        assert_eq!(channel.core().admitted(), 1);

        let outcome = controller
            .dispatch("dt-test", Some(&ts), Some(&signature), None, None, &body)
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Accepted);
        assert_eq!(
            channel.core().admitted(),
            1,
            "duplicate msgId must not dispatch"
        );
    }

    #[tokio::test]
    async fn dispatches_an_encrypted_message() {
        let (controller, channel) = setup(serde_json::json!({ "aesKey": AES_KEY })).await;
        let ts = now();
        let signature = sign(&channel, &ts);
        let body = encrypted_body(&channel, &bot_message("m-enc"));

        let outcome = controller
            .dispatch("dt-test", Some(&ts), Some(&signature), None, None, &body)
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Accepted);
        assert_eq!(channel.core().admitted(), 1);
    }

    #[tokio::test]
    async fn returns_400_when_the_envelope_and_the_aes_key_disagree() {
        // Encrypted body on a channel without aesKey.
        let (plain_controller, plain_channel) = setup(serde_json::json!({})).await;
        let ts = now();
        let signature = sign(&plain_channel, &ts);
        let encrypted = DingTalkCallbackCrypto::new(SECRET, Some(AES_KEY))
            .unwrap()
            .encrypt(&bot_message("m-nokey"), None)
            .unwrap();
        let body = format!(r#"{{"encrypt":"{encrypted}"}}"#);
        let outcome = plain_controller
            .dispatch("dt-test", Some(&ts), Some(&signature), None, None, &body)
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::BadRequest);
        assert_eq!(plain_channel.core().admitted(), 0);

        // Plain body on a channel with aesKey (fail closed).
        let (enc_controller, enc_channel) = setup(serde_json::json!({ "aesKey": AES_KEY })).await;
        let ts = now();
        let signature = sign(&enc_channel, &ts);
        let outcome = enc_controller
            .dispatch(
                "dt-test",
                Some(&ts),
                Some(&signature),
                None,
                None,
                &bot_message("m-plain"),
            )
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::BadRequest);
        assert_eq!(enc_channel.core().admitted(), 0);
    }

    #[tokio::test]
    async fn returns_400_for_malformed_and_oversized_bodies() {
        let (controller, channel) = setup(serde_json::json!({})).await;
        let ts = now();
        let signature = sign(&channel, &ts);

        let outcome = controller
            .dispatch(
                "dt-test",
                Some(&ts),
                Some(&signature),
                None,
                None,
                "not-json",
            )
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::BadRequest);

        let huge = format!(
            r#"{{"msgtype":"text","text":{{"content":"{}"}}}}"#,
            "x".repeat(70_000)
        );
        let outcome = controller
            .dispatch("dt-test", Some(&ts), Some(&signature), None, None, &huge)
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::BadRequest);

        let outcome = controller
            .dispatch("dt-test", Some(&ts), Some(&signature), None, None, "")
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::BadRequest);
    }

    #[tokio::test]
    async fn answers_the_first_check_url_verification_with_an_encrypted_success() {
        let (controller, channel) = setup(serde_json::json!({ "aesKey": AES_KEY })).await;
        let ts = now();
        let signature = sign(&channel, &ts);
        let body = encrypted_body(&channel, r#"{"eventType":"check_url"}"#);

        let outcome = controller
            .dispatch("dt-test", Some(&ts), Some(&signature), None, None, &body)
            .await;
        let DingTalkCallbackOutcome::Challenge(reply) = outcome else {
            panic!("expected a challenge reply, got {outcome:?}");
        };
        assert_eq!(
            channel.crypto().unwrap().decrypt(&reply).unwrap(),
            CHECK_URL_SUCCESS
        );
        assert_eq!(
            channel.core().admitted(),
            0,
            "check_url is not a bot message"
        );
    }

    #[tokio::test]
    async fn answers_check_url_in_plain_text_when_encryption_is_disabled() {
        let (controller, channel) = setup(serde_json::json!({})).await;
        let ts = now();
        let signature = sign(&channel, &ts);
        let outcome = controller
            .dispatch(
                "dt-test",
                Some(&ts),
                Some(&signature),
                None,
                None,
                r#"{"eventType":"check_url"}"#,
            )
            .await;
        assert_eq!(
            outcome,
            DingTalkCallbackOutcome::Challenge(CHECK_URL_SUCCESS.to_string())
        );
        assert_eq!(channel.core().admitted(), 0);
    }

    #[tokio::test]
    async fn accepts_the_classic_msg_signature_when_a_token_is_configured() {
        let (controller, channel) = setup(serde_json::json!({
            "aesKey": AES_KEY,
            "token": "legacy-token"
        }))
        .await;
        let encrypt = channel
            .crypto()
            .unwrap()
            .encrypt(&bot_message("m-legacy"), None)
            .unwrap();
        let body = format!(r#"{{"encrypt":"{encrypt}"}}"#);
        let msg_signature = channel
            .crypto()
            .unwrap()
            .sign_legacy("1700000000", "nonce-1", &encrypt)
            .unwrap();

        let outcome = controller
            .dispatch(
                "dt-test",
                Some("1700000000"),
                None,
                Some(&msg_signature),
                Some("nonce-1"),
                &body,
            )
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Accepted);
        assert_eq!(channel.core().admitted(), 1);

        let outcome = controller
            .dispatch(
                "dt-test",
                Some("1700000000"),
                None,
                Some("bad-signature"),
                Some("nonce-1"),
                &body,
            )
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Unauthorized);
    }

    #[tokio::test]
    async fn a_stream_mode_channel_cannot_answer_callbacks() {
        let stream_channel = Arc::new(
            DingTalkChannel::from_json(
                "dt-stream",
                ChannelConfig::of("dt-stream"),
                &serde_json::json!({
                    "appKey": "app-key",
                    "appSecret": SECRET,
                    "robotCode": "robot"
                }),
                Arc::new(RecordingTransport::new()),
            )
            .unwrap(),
        );
        let registry = Arc::new(DingTalkChannelRegistry::new());
        registry.register(stream_channel).await;
        let controller = DingTalkCallbackController::new(registry);
        let outcome = controller
            .dispatch("dt-stream", Some("1"), Some("s"), None, None, "{}")
            .await;
        assert_eq!(outcome, DingTalkCallbackOutcome::Unauthorized);
    }

    #[test]
    fn outcome_status_mapping_matches_the_java_controller() {
        assert_eq!(
            into_response(DingTalkCallbackOutcome::Unauthorized).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            into_response(DingTalkCallbackOutcome::BadRequest).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            into_response(DingTalkCallbackOutcome::Accepted).status(),
            StatusCode::OK
        );
        let challenge = into_response(DingTalkCallbackOutcome::Challenge("x".into()));
        assert_eq!(challenge.status(), StatusCode::OK);
        assert_eq!(
            challenge.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
    }
}
