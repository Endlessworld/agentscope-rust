//! axum HTTP entry point for Feishu event-subscription callbacks.
//!
//! Rust port of `FeishuCallbackController`. The controller is a thin shell: it extracts the raw
//! body + signature headers, looks the channel up in the typed registry, and hands everything to
//! [`FeishuChannel::handle_callback`]. All signature/decrypt/dedupe/dispatch logic lives in the
//! channel, so this module only maps [`FeishuCallbackOutcome`] onto HTTP responses.
//!
//! Route: `POST /api/channels/feishu/:channel_id/callback`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde_json::json;

use super::channel::{FeishuCallbackOutcome, FeishuChannelRegistry};

/// Header carrying the Feishu callback signature.
pub const SIGNATURE_HEADER: &str = "x-lark-signature";
/// Header carrying the Feishu callback timestamp.
pub const TIMESTAMP_HEADER: &str = "x-lark-request-timestamp";
/// Header carrying the Feishu callback nonce.
pub const NONCE_HEADER: &str = "x-lark-request-nonce";

/// Build the Feishu callback router, sharing the typed channel registry as state.
pub fn router(registry: Arc<FeishuChannelRegistry>) -> Router {
    Router::new()
        .route("/api/channels/feishu/:channel_id/callback", post(callback))
        .with_state(registry)
}

async fn callback(
    State(registry): State<Arc<FeishuChannelRegistry>>,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(channel) = registry.get(&channel_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Strict decoding: the signature is verified over the exact bytes Feishu signed, so invalid
    // UTF-8 must be a 400 instead of being rewritten to U+FFFD.
    let Ok(raw_body) = std::str::from_utf8(&body) else {
        return into_response(FeishuCallbackOutcome::BadRequest);
    };
    let outcome = channel
        .handle_callback(
            header(&headers, SIGNATURE_HEADER),
            header(&headers, TIMESTAMP_HEADER),
            header(&headers, NONCE_HEADER),
            raw_body,
        )
        .await;
    into_response(outcome)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Map the channel's framework-agnostic outcome onto the response the Java controller produces.
fn into_response(outcome: FeishuCallbackOutcome) -> Response {
    match outcome {
        FeishuCallbackOutcome::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        FeishuCallbackOutcome::BadRequest => StatusCode::BAD_REQUEST.into_response(),
        FeishuCallbackOutcome::Challenge(challenge) => {
            axum::Json(json!({ "challenge": challenge })).into_response()
        }
        FeishuCallbackOutcome::Accepted => axum::Json(json!({})).into_response(),
        // Dispatch failed: tell Feishu to redeliver instead of acknowledging a lost message
        // (Java `onErrorResume(... HttpStatus.SERVICE_UNAVAILABLE)`).
        FeishuCallbackOutcome::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::ChannelConfig;

    use crate::common::testing::{RecordingTransport, StaticTokenProvider};
    use crate::common::{AccessTokenProvider, BotLoopGuard, IdempotencyStore};
    use crate::feishu::channel::FeishuChannel;
    use crate::feishu::inbound::FeishuInboundMapper;
    use crate::feishu::outbound::FeishuOutboundClient;
    use crate::feishu::properties::FeishuChannelProperties;

    async fn registry_with_channel() -> Arc<FeishuChannelRegistry> {
        let props = FeishuChannelProperties::from_json(
            "feishu",
            &json!({"appId": "cli_x", "appSecret": "s", "verificationToken": "vt"}),
        )
        .unwrap();
        let token: Arc<dyn AccessTokenProvider> = Arc::new(StaticTokenProvider::new("tok"));
        let channel = FeishuChannel::new(
            "feishu",
            ChannelConfig::of("feishu"),
            props,
            None,
            FeishuOutboundClient::new(
                "https://open.feishu.cn",
                token,
                Arc::new(RecordingTransport::new()),
            ),
            FeishuInboundMapper::new("feishu"),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::with_defaults(),
        );
        let registry = Arc::new(FeishuChannelRegistry::new());
        registry.register(Arc::new(channel)).await;
        registry
    }

    #[test]
    fn outcome_status_mapping_matches_the_java_controller() {
        assert_eq!(
            into_response(FeishuCallbackOutcome::Unauthorized).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            into_response(FeishuCallbackOutcome::BadRequest).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            into_response(FeishuCallbackOutcome::Accepted).status(),
            StatusCode::OK
        );
        assert_eq!(
            into_response(FeishuCallbackOutcome::Unavailable).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn a_non_utf8_body_is_a_bad_request() {
        let registry = registry_with_channel().await;
        let response = callback(
            State(registry),
            Path("feishu".to_string()),
            HeaderMap::new(),
            Bytes::from_static(b"{\"challenge\":\"\xff\xfe\"}"),
        )
        .await;
        // Before the strict decode the invalid bytes were lossily replaced and then handled as a
        // normal (lossy) payload.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_dispatch_failure_answers_503_so_feishu_redelivers() {
        // The registry holds a channel without a gateway, so dispatch cannot run.
        let registry = registry_with_channel().await;
        let body = Bytes::from_static(
            br#"{"header":{"event_id":"e1","token":"vt"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou_1"}},"message":{"message_id":"om_1","message_type":"text","content":"{\"text\":\"hi\"}","chat_id":"oc_1","chat_type":"p2p"}}}"#,
        );
        let first = callback(
            State(registry.clone()),
            Path("feishu".to_string()),
            HeaderMap::new(),
            body.clone(),
        )
        .await;
        assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);
        // Same `event_id` again: the failed delivery did not consume the idempotency key.
        let second = callback(
            State(registry),
            Path("feishu".to_string()),
            HeaderMap::new(),
            body,
        )
        .await;
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn registry_lookup_is_by_channel_id() {
        let registry = registry_with_channel().await;
        assert!(registry.get("feishu").is_some());
        assert!(registry.get("github").is_none());
    }
}
