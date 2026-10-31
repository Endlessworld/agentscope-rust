//! axum HTTP entry point for WeCom callback deliveries.
//!
//! Rust port of `WeComCallbackController`. The controller is a thin shell: it extracts the query
//! parameters (`msg_signature`/`timestamp`/`nonce`, plus `echostr` on GET) and the raw XML body,
//! looks the channel up in the typed registry, and hands everything to
//! [`WeComChannel::verify_url`] / [`WeComChannel::handle_callback`]. All
//! signature/decrypt/dedupe/dispatch logic lives in the channel.
//!
//! Routes:
//! - `GET  /api/channels/wecom/:channel_id/callback` — URL-verification handshake, echoes the
//!   decrypted `echostr` as `text/plain`
//! - `POST /api/channels/wecom/:channel_id/callback` — encrypted XML message callback

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use super::channel::{WeComCallbackOutcome, WeComChannelRegistry};

/// The WeCom handshake query parameters (Java `@RequestParam`s).
#[derive(Debug, Default, Deserialize)]
pub struct WeComQuery {
    pub msg_signature: Option<String>,
    pub timestamp: Option<String>,
    pub nonce: Option<String>,
    pub echostr: Option<String>,
}

/// Build the WeCom callback router, sharing the typed channel registry as state.
pub fn router(registry: Arc<WeComChannelRegistry>) -> Router {
    Router::new()
        .route(
            "/api/channels/wecom/:channel_id/callback",
            get(verify).post(dispatch),
        )
        .with_state(registry)
}

async fn verify(
    State(registry): State<Arc<WeComChannelRegistry>>,
    Path(channel_id): Path<String>,
    Query(query): Query<WeComQuery>,
) -> Response {
    let Some(channel) = registry.get(&channel_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // `echostr` is mandatory on the handshake; without it there is nothing to echo.
    let Some(echostr) = query.echostr.as_deref().filter(|s| !s.is_empty()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let outcome = channel
        .verify_url(
            query.msg_signature.as_deref(),
            query.timestamp.as_deref(),
            query.nonce.as_deref(),
            echostr,
        )
        .await;
    into_response(outcome)
}

async fn dispatch(
    State(registry): State<Arc<WeComChannelRegistry>>,
    Path(channel_id): Path<String>,
    Query(query): Query<WeComQuery>,
    body: Bytes,
) -> Response {
    let Some(channel) = registry.get(&channel_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Strict decoding: the SHA-1 signature covers the exact callback bytes, so invalid UTF-8 is a
    // 400 instead of a lossily rewritten (and then unverifiable) body.
    let Ok(raw_body) = std::str::from_utf8(&body) else {
        return into_response(WeComCallbackOutcome::BadRequest);
    };
    let outcome = channel
        .handle_callback(
            query.msg_signature.as_deref(),
            query.timestamp.as_deref(),
            query.nonce.as_deref(),
            raw_body,
        )
        .await;
    into_response(outcome)
}

/// Map the channel's framework-agnostic outcome onto the response the Java controller produces
/// (`401` / `400` / plain-text `echostr` / empty `200`).
fn into_response(outcome: WeComCallbackOutcome) -> Response {
    match outcome {
        WeComCallbackOutcome::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        WeComCallbackOutcome::BadRequest => StatusCode::BAD_REQUEST.into_response(),
        WeComCallbackOutcome::Challenge(plain) => plain_text(StatusCode::OK, plain),
        WeComCallbackOutcome::Accepted => plain_text(StatusCode::OK, String::new()),
    }
}

fn plain_text(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::ChannelConfig;

    use crate::common::testing::{RecordingTransport, StaticTokenProvider};
    use crate::common::{AccessTokenProvider, BotLoopGuard, IdempotencyStore};
    use crate::wecom::channel::WeComChannel;
    use crate::wecom::crypto::WeComCrypto;
    use crate::wecom::inbound::WeComInboundMapper;
    use crate::wecom::outbound::WeComOutboundClient;
    use crate::wecom::properties::WeComChannelProperties;

    const CORP: &str = "corp-1";
    const ENCODING_AES_KEY: &str = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";

    fn channel(crypto: WeComCrypto) -> WeComChannel {
        let properties = WeComChannelProperties::from_json(
            "wecom",
            &serde_json::json!({
                "corpId": CORP,
                "agentId": 100001,
                "secret": "s",
                "token": "wecom-token",
                "encodingAesKey": ENCODING_AES_KEY
            }),
        )
        .unwrap();
        let token: Arc<dyn AccessTokenProvider> = Arc::new(StaticTokenProvider::new("tok"));
        WeComChannel::new(
            "wecom",
            ChannelConfig::of("wecom"),
            properties,
            crypto,
            WeComOutboundClient::new(
                "https://qyapi.weixin.qq.com",
                token,
                100001,
                Arc::new(RecordingTransport::new()),
            ),
            WeComInboundMapper::new("wecom", Some(CORP.to_string())),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::with_defaults(),
        )
    }

    async fn registry_with_channel() -> Arc<WeComChannelRegistry> {
        let crypto = WeComCrypto::new("wecom-token", ENCODING_AES_KEY, CORP).unwrap();
        let registry = Arc::new(WeComChannelRegistry::new());
        registry.register(Arc::new(channel(crypto))).await;
        registry
    }

    #[test]
    fn outcome_status_mapping_matches_the_java_controller() {
        assert_eq!(
            into_response(WeComCallbackOutcome::Unauthorized).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            into_response(WeComCallbackOutcome::BadRequest).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            into_response(WeComCallbackOutcome::Accepted).status(),
            StatusCode::OK
        );
        let challenge = into_response(WeComCallbackOutcome::Challenge("echo-1".to_string()));
        assert_eq!(challenge.status(), StatusCode::OK);
        assert_eq!(
            challenge.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn registry_lookup_is_by_channel_id() {
        let registry = registry_with_channel().await;
        assert!(registry.get("wecom").is_some());
        assert!(registry.get("feishu").is_none());
    }

    #[tokio::test]
    async fn verify_returns_not_found_for_an_unknown_channel() {
        let registry = Arc::new(WeComChannelRegistry::new());
        let response = verify(
            State(registry),
            Path("nope".to_string()),
            Query(WeComQuery {
                msg_signature: Some("s".to_string()),
                timestamp: Some("1".to_string()),
                nonce: Some("n".to_string()),
                echostr: Some("e".to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn verify_requires_an_echostr() {
        let registry = registry_with_channel().await;
        let missing = verify(
            State(registry),
            Path("wecom".to_string()),
            Query(WeComQuery::default()),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn verify_echoes_the_decrypted_echostr_as_plain_text() {
        let crypto = WeComCrypto::new("wecom-token", ENCODING_AES_KEY, CORP).unwrap();
        let echostr = crypto.encrypt("handshake-plain", CORP).unwrap();
        let signature = crypto.sign("1700000000", "nonce-1", &echostr);
        let registry = registry_with_channel().await;
        let response = verify(
            State(registry),
            Path("wecom".to_string()),
            Query(WeComQuery {
                msg_signature: Some(signature),
                timestamp: Some("1700000000".to_string()),
                nonce: Some("nonce-1".to_string()),
                echostr: Some(echostr),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn dispatch_rejects_a_body_without_an_encrypt_element() {
        let registry = registry_with_channel().await;
        let response = dispatch(
            State(registry),
            Path("wecom".to_string()),
            Query(WeComQuery {
                msg_signature: Some("s".to_string()),
                timestamp: Some("1".to_string()),
                nonce: Some("n".to_string()),
                echostr: None,
            }),
            Bytes::from_static(b"<xml/>"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dispatch_rejects_a_non_utf8_body() {
        let registry = registry_with_channel().await;
        let response = dispatch(
            State(registry),
            Path("wecom".to_string()),
            Query(WeComQuery {
                msg_signature: Some("s".to_string()),
                timestamp: Some("1".to_string()),
                nonce: Some("n".to_string()),
                echostr: None,
            }),
            Bytes::from_static(b"<xml><Encrypt>\xff\xfe</Encrypt></xml>"),
        )
        .await;
        // Before the strict decode the bytes were lossily rewritten and then failed the
        // signature check (401); invalid UTF-8 cannot be a valid WeCom payload at all.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dispatch_acks_a_non_text_payload_with_200() {
        let crypto = WeComCrypto::new("wecom-token", ENCODING_AES_KEY, CORP).unwrap();
        let encrypt = crypto
            .encrypt(
                "<xml><FromUserName>u</FromUserName><MsgType>event</MsgType><Event>subscribe</Event></xml>",
                CORP,
            )
            .unwrap();
        let signature = crypto.sign("1700000000", "nonce-1", &encrypt);
        let registry = registry_with_channel().await;
        let body = format!("<xml><Encrypt>{encrypt}</Encrypt></xml>");
        let response = dispatch(
            State(registry),
            Path("wecom".to_string()),
            Query(WeComQuery {
                msg_signature: Some(signature),
                timestamp: Some("1700000000".to_string()),
                nonce: Some("nonce-1".to_string()),
                echostr: None,
            }),
            Bytes::from(body),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
