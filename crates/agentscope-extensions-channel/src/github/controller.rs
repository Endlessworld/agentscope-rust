//! axum HTTP entry point for GitHub webhook deliveries.
//!
//! Rust port of `GitHubWebhookController`. The controller is a thin shell: it extracts the raw body
//! plus the `X-GitHub-Event` / `X-Hub-Signature-256` headers, looks the channel up in the typed
//! registry, and hands everything to [`GitHubChannel::handle_webhook`]. All signature/mapping/
//! dedupe/dispatch logic lives in the channel, so this module only maps
//! [`GitHubCallbackOutcome`] onto HTTP responses.
//!
//! Route: `POST /api/channels/github/:channel_id/webhook`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde_json::json;

use super::channel::{GitHubCallbackOutcome, GitHubChannelRegistry};

/// Header carrying the GitHub event type (e.g. `issue_comment`).
pub const EVENT_HEADER: &str = "x-github-event";
/// Header carrying the HMAC-SHA256 webhook signature.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";

/// Build the GitHub webhook router, sharing the typed channel registry as state.
pub fn router(registry: Arc<GitHubChannelRegistry>) -> Router {
    Router::new()
        .route("/api/channels/github/:channel_id/webhook", post(webhook))
        .with_state(registry)
}

async fn webhook(
    State(registry): State<Arc<GitHubChannelRegistry>>,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(channel) = registry.get(&channel_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Strict decoding: signature verification and JSON parsing both need the exact bytes the
    // provider sent. `from_utf8_lossy` would rewrite invalid bytes to U+FFFD, so a valid
    // signature would be computed over different bytes and the delivery rejected as 401.
    let Ok(raw_body) = std::str::from_utf8(&body) else {
        return into_response(GitHubCallbackOutcome::BadRequest);
    };
    let outcome = channel
        .handle_webhook(
            header(&headers, EVENT_HEADER).unwrap_or_default(),
            header(&headers, SIGNATURE_HEADER),
            raw_body,
        )
        .await;
    into_response(outcome)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Map the channel's framework-agnostic outcome onto the response the Java controller produces.
fn into_response(outcome: GitHubCallbackOutcome) -> Response {
    match outcome {
        GitHubCallbackOutcome::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        GitHubCallbackOutcome::BadRequest => StatusCode::BAD_REQUEST.into_response(),
        GitHubCallbackOutcome::Accepted => axum::Json(json!({})).into_response(),
        // Unhandled event type: Java answers `204 No Content` so monitoring can tell it apart
        // from a handled (200) delivery.
        GitHubCallbackOutcome::Ignored => StatusCode::NO_CONTENT.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::ChannelConfig;

    use crate::common::testing::RecordingTransport;
    use crate::common::{BotLoopGuard, IdempotencyStore};
    use crate::github::bot_identity::GitHubBotIdentityResolver;
    use crate::github::channel::GitHubChannel;
    use crate::github::inbound::GitHubInboundMapper;
    use crate::github::outbound::GitHubOutboundClient;
    use crate::github::properties::GitHubChannelProperties;
    use crate::github::signature::GitHubSignatureVerifier;

    async fn registry_with_channel() -> Arc<GitHubChannelRegistry> {
        let props = GitHubChannelProperties::from_json(
            "github",
            &json!({"token": "t", "webhookSecret": "s"}),
        )
        .unwrap();
        let transport = Arc::new(RecordingTransport::new());
        let channel = GitHubChannel::new(
            "github",
            ChannelConfig::of("github"),
            props,
            GitHubOutboundClient::new("https://api.github.com", "t", transport.clone()),
            GitHubInboundMapper::new("github"),
            Arc::new(GitHubBotIdentityResolver::new(
                "https://api.github.com",
                "t",
                None,
                transport,
            )),
            GitHubSignatureVerifier::new("s").unwrap(),
            Arc::new(IdempotencyStore::with_defaults()),
            BotLoopGuard::with_defaults(),
        );
        let registry = Arc::new(GitHubChannelRegistry::new());
        registry.register(Arc::new(channel)).await;
        registry
    }

    #[test]
    fn outcome_status_mapping_matches_the_java_controller() {
        assert_eq!(
            into_response(GitHubCallbackOutcome::Unauthorized).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            into_response(GitHubCallbackOutcome::BadRequest).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            into_response(GitHubCallbackOutcome::Accepted).status(),
            StatusCode::OK
        );
        assert_eq!(
            into_response(GitHubCallbackOutcome::Ignored).status(),
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn an_unhandled_event_type_is_204() {
        let registry = registry_with_channel().await;
        let body = r#"{"action":"created"}"#;
        let signature = GitHubSignatureVerifier::new("s")
            .unwrap()
            .sign(body.as_bytes());
        let mut headers = HeaderMap::new();
        headers.insert(EVENT_HEADER, "push".parse().unwrap());
        headers.insert(SIGNATURE_HEADER, signature.parse().unwrap());
        let response = webhook(
            State(registry),
            Path("github".to_string()),
            headers,
            Bytes::from(body.to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_non_utf8_body_is_a_bad_request_not_a_signature_failure() {
        let registry = registry_with_channel().await;
        let mut headers = HeaderMap::new();
        headers.insert(EVENT_HEADER, "issue_comment".parse().unwrap());
        headers.insert(SIGNATURE_HEADER, "sha256=deadbeef".parse().unwrap());
        let response = webhook(
            State(registry),
            Path("github".to_string()),
            headers,
            Bytes::from_static(b"{\"comment\":\"\xff\xfe\"}"),
        )
        .await;
        // Before the strict decode this was lossily rewritten and reported as 401.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn registry_lookup_is_by_channel_id() {
        let registry = registry_with_channel().await;
        assert!(registry.get("github").is_some());
        assert!(registry.get("feishu").is_none());
    }
}
