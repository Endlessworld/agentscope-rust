//! axum HTTP entry point for GitLab webhook deliveries.
//!
//! Rust port of `GitLabWebhookController`. The controller is a thin shell: it extracts the raw
//! body plus the `X-Gitlab-Event` / `X-Gitlab-Token` headers, looks the channel up in the typed
//! registry, and hands everything to [`GitLabChannel::handle_webhook`]. All token / parse / dedupe
//! / dispatch logic lives in the channel, so this module only maps [`GitLabCallbackOutcome`] onto
//! HTTP responses.
//!
//! Route: `POST /api/channels/gitlab/:channel_id/webhook`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;

use super::channel::{GitLabCallbackOutcome, GitLabChannelRegistry};

/// Header carrying the GitLab hook name (e.g. `Note Hook`).
pub const EVENT_HEADER: &str = "x-gitlab-event";
/// Header carrying the shared webhook token.
pub const TOKEN_HEADER: &str = "x-gitlab-token";

/// Build the GitLab webhook router, sharing the typed channel registry as state.
pub fn router(registry: Arc<GitLabChannelRegistry>) -> Router {
    Router::new()
        .route("/api/channels/gitlab/:channel_id/webhook", post(webhook))
        .with_state(registry)
}

async fn webhook(
    State(registry): State<Arc<GitLabChannelRegistry>>,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(channel) = registry.get(&channel_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let raw_body = String::from_utf8_lossy(&body);
    let outcome = channel
        .handle_webhook(
            header(&headers, EVENT_HEADER).unwrap_or_default(),
            header(&headers, TOKEN_HEADER),
            &raw_body,
        )
        .await;
    into_response(outcome)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Map the channel's framework-agnostic outcome onto the response the Java controller produces.
fn into_response(outcome: GitLabCallbackOutcome) -> Response {
    match outcome {
        GitLabCallbackOutcome::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        GitLabCallbackOutcome::BadRequest => StatusCode::BAD_REQUEST.into_response(),
        GitLabCallbackOutcome::Accepted => axum::Json(serde_json::json!({})).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::ChannelConfig;

    use crate::common::testing::RecordingTransport;
    use crate::gitlab::channel::GitLabChannel;

    const TOKEN: &str = "hook-secret";

    fn channel() -> GitLabChannel {
        GitLabChannel::from_json(
            "gitlab",
            ChannelConfig::of("gitlab"),
            &serde_json::json!({"token": "t", "webhookToken": TOKEN}),
            Arc::new(RecordingTransport::new()),
        )
        .unwrap()
    }

    async fn registry_with_channel() -> Arc<GitLabChannelRegistry> {
        let registry = Arc::new(GitLabChannelRegistry::new());
        registry.register(Arc::new(channel())).await;
        registry
    }

    fn headers(event: Option<&str>, token: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(event) = event {
            headers.insert(EVENT_HEADER, event.parse().unwrap());
        }
        if let Some(token) = token {
            headers.insert(TOKEN_HEADER, token.parse().unwrap());
        }
        headers
    }

    fn body() -> Bytes {
        Bytes::from_static(
            br#"{"user":{"id":1,"username":"alice"},"project":{"path_with_namespace":"grp/proj"},"object_attributes":{"id":1,"note":"hi","noteable_type":"Issue","system":false},"issue":{"iid":9}}"#,
        )
    }

    #[test]
    fn outcome_status_mapping_matches_the_java_controller() {
        assert_eq!(
            into_response(GitLabCallbackOutcome::Unauthorized).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            into_response(GitLabCallbackOutcome::BadRequest).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            into_response(GitLabCallbackOutcome::Accepted).status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn registry_lookup_is_by_channel_id() {
        let registry = registry_with_channel().await;
        assert!(registry.get("gitlab").is_some());
        assert!(registry.get("github").is_none());
    }

    #[tokio::test]
    async fn unknown_channel_id_is_not_found() {
        let registry = registry_with_channel().await;
        let response = webhook(
            State(registry),
            Path("missing".to_string()),
            headers(Some("Note Hook"), Some(TOKEN)),
            body(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn token_mismatch_is_unauthorized() {
        let registry = registry_with_channel().await;
        let response = webhook(
            State(registry),
            Path("gitlab".to_string()),
            headers(Some("Note Hook"), Some("nope")),
            body(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_json_is_a_bad_request() {
        let registry = registry_with_channel().await;
        let response = webhook(
            State(registry),
            Path("gitlab".to_string()),
            headers(Some("Note Hook"), Some(TOKEN)),
            Bytes::from_static(b"not-json"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_valid_note_hook_is_accepted() {
        let registry = registry_with_channel().await;
        let response = webhook(
            State(registry),
            Path("gitlab".to_string()),
            headers(Some("Note Hook"), Some(TOKEN)),
            body(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn other_hook_types_are_acknowledged() {
        let registry = registry_with_channel().await;
        let response = webhook(
            State(registry),
            Path("gitlab".to_string()),
            headers(Some("Push Hook"), Some(TOKEN)),
            body(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
