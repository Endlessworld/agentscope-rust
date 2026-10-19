//! iLink protocol headers and request-body envelopes.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinProtocolHeaders` plus the request
//! builders of `WeixinOutboundClient` / `WeixinLoginClient`. Kept as pure functions so every
//! header set and body shape is unit-testable offline.

use serde_json::{json, Map, Value};

use crate::common::PreparedRequest;

/// `iLink-App-Id` value.
pub const APP_ID: &str = "bot";
/// `iLink-App-ClientVersion`, encoded as `major << 16 | minor << 8 | patch`.
pub const APP_CLIENT_VERSION: &str = "132105";
/// `AuthorizationType` value for the bot-token scheme.
pub const AUTHORIZATION_TYPE: &str = "ilink_bot_token";

/// Path of the long-poll endpoint (also the only call with an extended timeout).
pub const PATH_UPDATES: &str = "/ilink/bot/getupdates";
/// Path of the outbound text-send endpoint.
pub const PATH_SEND_MESSAGE: &str = "/ilink/bot/sendmessage";
/// Path of the provider `notifystart` control call.
pub const PATH_NOTIFY_START: &str = "/ilink/bot/msg/notifystart";
/// Path of the provider `notifystop` control call.
pub const PATH_NOTIFY_STOP: &str = "/ilink/bot/msg/notifystop";
/// Path of the QR-login challenge request.
pub const PATH_GET_QRCODE: &str = "/ilink/bot/get_bot_qrcode";
/// Path of the QR-login status poll.
pub const PATH_QRCODE_STATUS: &str = "/ilink/bot/get_qrcode_status";

/// Grace on top of the long-poll window before the request is abandoned.
pub const LONG_POLL_GRACE_MS: i64 = 5_000;

/// `X-WECHAT-UIN` derived from an unsigned 32-bit value: the decimal text, then base64-encoded,
/// exactly as the Java `randomWechatUin` does with `Integer.toUnsignedLong(nextInt())`.
pub fn encode_wechat_uin(value: u32) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(value.to_string())
}

/// The application-level headers required by every iLink call.
pub fn application_headers(request: PreparedRequest) -> PreparedRequest {
    request
        .header("iLink-App-Id", APP_ID)
        .header("iLink-App-ClientVersion", APP_CLIENT_VERSION)
}

/// An application-level JSON POST (the QR-login `start` call).
///
/// Mirrors the Java `jsonPost`, which layers on top of `application(...)`: the iLink app headers
/// come first, then the auth scheme and the per-request `X-WECHAT-UIN`.
pub fn json_post(request: PreparedRequest, wechat_uin: &str) -> PreparedRequest {
    let mut request = application_headers(request);
    // `PreparedRequest::post_json` already sets `Content-Type: application/json`; only add it when
    // the caller built the request some other way, so the header is never listed twice.
    if request.header_value("Content-Type").is_none() {
        request = request.header("Content-Type", "application/json");
    }
    request
        .header("AuthorizationType", AUTHORIZATION_TYPE)
        .header("X-WECHAT-UIN", wechat_uin)
}

/// An authenticated JSON POST (every bot call). `bot_token` is stripped first, mirroring Java.
pub fn authenticated_json_post(
    request: PreparedRequest,
    bot_token: &str,
    wechat_uin: &str,
) -> PreparedRequest {
    json_post(request, wechat_uin).header("Authorization", format!("Bearer {}", bot_token.trim()))
}

/// `{"bot_type": "<n>"}` body for the QR-login challenge request, plus the `bot_type` query.
pub fn qrcode_challenge_request(base_url: &str, bot_type: &str) -> PreparedRequest {
    let url = format!(
        "{}{PATH_GET_QRCODE}?bot_type={bot_type}",
        base_url.trim_end_matches('/')
    );
    PreparedRequest::post_json(url, json!({ "local_token_list": [] }))
}

/// Query path of the QR-login status poll, mirroring Java `URLEncoder.encode(..., UTF-8)`.
///
/// Java encodes `qrcode` — and, when present, `verify_code` — as GET query parameters, so the
/// resulting URI is credential material. Nothing here logs or embeds it in an error.
pub fn qrcode_status_path(qrcode: &str, verify_code: Option<&str>) -> String {
    let mut path = format!("{PATH_QRCODE_STATUS}?qrcode={}", form_urlencode(qrcode));
    if let Some(code) = verify_code.map(str::trim).filter(|c| !c.is_empty()) {
        path.push_str(&format!("&verify_code={}", form_urlencode(code)));
    }
    path
}

/// `{"channel_version": .., "bot_agent": ..}` — the `base_info` object every bot call carries.
pub fn base_info(channel_version: &str, bot_agent: &str) -> Value {
    json!({ "channel_version": channel_version, "bot_agent": bot_agent })
}

/// Body of the `getupdates` long poll.
pub fn updates_request_body(cursor: &str, channel_version: &str, bot_agent: &str) -> Value {
    json!({
        "get_updates_buf": cursor,
        "base_info": base_info(channel_version, bot_agent),
    })
}

/// Body of the `notifystart` / `notifystop` control calls.
pub fn notify_request_body(channel_version: &str, bot_agent: &str) -> Value {
    json!({ "base_info": base_info(channel_version, bot_agent) })
}

/// Message envelope of one outbound text reply, mirroring the Java `sendOne`.
///
/// The provider reports an accepted send by returning the message id alone; there is no
/// `ret`/`errcode` in a success body, so the receipt is the only proof iLink accepted the reply.
pub fn send_message_body(
    to_user_id: &str,
    text: &str,
    context_token: Option<&str>,
    client_id: &str,
) -> Value {
    let mut message = Map::new();
    message.insert("from_user_id".to_string(), json!(""));
    message.insert("to_user_id".to_string(), json!(to_user_id));
    message.insert("client_id".to_string(), json!(client_id));
    message.insert("message_type".to_string(), json!(2));
    message.insert("message_state".to_string(), json!(2));
    message.insert(
        "item_list".to_string(),
        json!([{ "type": 1, "text_item": { "text": text } }]),
    );
    if let Some(token) = context_token.filter(|t| !t.is_empty()) {
        message.insert("context_token".to_string(), json!(token));
    }
    json!({ "msg": Value::Object(message) })
}

/// Body of one outbound text reply including `base_info`.
pub fn send_message_request_body(
    to_user_id: &str,
    text: &str,
    context_token: Option<&str>,
    client_id: &str,
    channel_version: &str,
    bot_agent: &str,
) -> Value {
    let mut body = send_message_body(to_user_id, text, context_token, client_id);
    if let Value::Object(map) = &mut body {
        map.insert(
            "base_info".to_string(),
            base_info(channel_version, bot_agent),
        );
    }
    body
}

/// `application/x-www-form-urlencoded` encoding as Java's `URLEncoder.encode(..., UTF-8)` does it:
/// space becomes `+`, everything outside `A-Za-z0-9.-_*` is percent-escaped.
pub fn form_urlencode(value: &str) -> String {
    const UNRESERVED: &[u8] = b"-._*";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let b = *byte;
        if b.is_ascii_alphanumeric() || UNRESERVED.contains(&b) {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_and_authenticated_headers_match_the_protocol() {
        let request = application_headers(PreparedRequest::post_json(
            "https://ilinkai.weixin.qq.com/ilink/bot/getupdates",
            json!({}),
        ));
        assert_eq!(request.header_value("iLink-App-Id"), Some("bot"));
        assert_eq!(request.header_value("iLink-App-ClientVersion"), Some("132105"));

        // The Java `jsonPost` layers on `application(...)`, so an authenticated POST carries the
        // app headers *and* the bot-token scheme.
        let authenticated = authenticated_json_post(
            PreparedRequest::post_json("https://example.test/x", json!({})),
            "  secret ",
            "uin",
        );
        assert_eq!(authenticated.header_value("Authorization"), Some("Bearer secret"));
        assert_eq!(
            authenticated.header_value("AuthorizationType"),
            Some("ilink_bot_token")
        );
        assert_eq!(authenticated.header_value("X-WECHAT-UIN"), Some("uin"));
        assert_eq!(authenticated.header_value("iLink-App-Id"), Some("bot"));
        assert_eq!(
            authenticated.header_value("iLink-App-ClientVersion"),
            Some("132105")
        );
        assert_eq!(
            authenticated.header_value("Content-Type"),
            Some("application/json")
        );
        // Content-Type must not be duplicated by the two layers.
        assert_eq!(
            authenticated
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .count(),
            1
        );
    }

    #[test]
    fn json_post_adds_the_content_type_when_the_builder_did_not() {
        let request = json_post(PreparedRequest::get("https://example.test/x"), "uin");
        assert_eq!(
            request.header_value("Content-Type"),
            Some("application/json")
        );
        assert_eq!(request.header_value("X-WECHAT-UIN"), Some("uin"));
    }

    #[test]
    fn wechat_uin_is_base64_of_the_unsigned_decimal() {
        // Java: Base64(Integer.toUnsignedLong(nextInt()).toString())
        assert_eq!(encode_wechat_uin(0), "MA==");
        assert_eq!(encode_wechat_uin(4_294_967_295), "NDI5NDk2NzI5NQ==");
    }

    #[test]
    fn qrcode_status_path_encodes_both_query_parameters() {
        assert_eq!(
            qrcode_status_path("qr 1", None),
            "/ilink/bot/get_qrcode_status?qrcode=qr+1"
        );
        assert_eq!(
            qrcode_status_path("qr-1", Some(" 123456 ")),
            "/ilink/bot/get_qrcode_status?qrcode=qr-1&verify_code=123456"
        );
        // A blank verification code is dropped, mirroring the Java `isBlank()` guard.
        assert_eq!(
            qrcode_status_path("qr-1", Some("   ")),
            "/ilink/bot/get_qrcode_status?qrcode=qr-1"
        );
    }

    #[test]
    fn form_urlencode_matches_java_urlencoder() {
        assert_eq!(form_urlencode("a b"), "a+b");
        assert_eq!(form_urlencode("a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
        assert_eq!(form_urlencode("A-Z.a_z*0"), "A-Z.a_z*0");
        assert_eq!(form_urlencode("\u{4e2d}"), "%E4%B8%AD");
    }

    #[test]
    fn updates_body_carries_the_cursor_and_base_info() {
        let body = updates_request_body("cursor-1", "v/1", "AgentScope");
        assert_eq!(body["get_updates_buf"], json!("cursor-1"));
        assert_eq!(body["base_info"]["channel_version"], json!("v/1"));
        assert_eq!(body["base_info"]["bot_agent"], json!("AgentScope"));
        assert_eq!(notify_request_body("v/1", "AgentScope")["base_info"], body["base_info"]);
    }

    #[test]
    fn send_body_shapes_the_text_item_and_omits_an_absent_context_token() {
        let body = send_message_body("peer-a", "hello", Some("ctx-9"), "client-1");
        assert_eq!(body["msg"]["from_user_id"], json!(""));
        assert_eq!(body["msg"]["to_user_id"], json!("peer-a"));
        assert_eq!(body["msg"]["client_id"], json!("client-1"));
        assert_eq!(body["msg"]["message_type"], json!(2));
        assert_eq!(body["msg"]["message_state"], json!(2));
        assert_eq!(body["msg"]["context_token"], json!("ctx-9"));
        assert_eq!(
            body["msg"]["item_list"][0]["text_item"]["text"],
            json!("hello")
        );
        assert_eq!(body["msg"]["item_list"][0]["type"], json!(1));

        let without = send_message_body("peer-a", "hi", None, "client-2");
        assert!(without["msg"].get("context_token").is_none());
        // An empty context token is treated as absent, mirroring the Java `token != null` check
        // after the mapper already dropped blank values.
        let blank = send_message_body("peer-a", "hi", Some(""), "client-3");
        assert!(blank["msg"].get("context_token").is_none());
    }

    #[test]
    fn challenge_request_uses_the_bot_type_query_and_an_empty_token_list() {
        let request = qrcode_challenge_request("https://ilinkai.weixin.qq.com/", "3");
        assert_eq!(
            request.url,
            "https://ilinkai.weixin.qq.com/ilink/bot/get_bot_qrcode?bot_type=3"
        );
        assert_eq!(request.body, Some(json!({ "local_token_list": [] })));
    }
}
