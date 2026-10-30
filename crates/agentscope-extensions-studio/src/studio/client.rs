//! HTTP client for communicating with AgentScope Studio, mirroring `StudioClient`.
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use agentscope_core::message::{Msg, MsgRole};

use crate::error::{StudioError, StudioResult};
use crate::studio::config::StudioConfig;
use crate::studio::http::{ReqwestStudioTransport, StudioHttpRequest, StudioHttpTransport};
use crate::studio::pojo::{PushMessageRequest, RegisterRunRequest, RequestUserInputRequest};

/// Timestamp format used by Studio (`yyyy-MM-dd HH:mm:ss.SSS`), UTC.
fn format_timestamp(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// HTTP client for Studio's REST API (`registerRun`, `pushMessage`,
/// `requestUserInput`), with automatic retry for transport failures.
///
/// The wire call goes through an injectable [`StudioHttpTransport`] so the network path can be
/// exercised offline (Java instead builds an `OkHttpClient` inline, so its tests need a live
/// server).
#[derive(Clone)]
pub struct StudioClient {
    transport: Arc<dyn StudioHttpTransport>,
    config: StudioConfig,
    base_url: String,
}

impl std::fmt::Debug for StudioClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transport is a trait object without `Debug`; print only the meaningful,
        // cheaply-formattable fields.
        f.debug_struct("StudioClient")
            .field("base_url", &self.base_url)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl StudioClient {
    /// Creates a new Studio HTTP client from the given configuration, using the
    /// `reqwest`-backed transport (10s connect timeout, 30s request timeout).
    pub fn new(config: StudioConfig) -> Self {
        Self::with_transport(config, Arc::new(ReqwestStudioTransport::new()))
    }

    /// Creates a client over an explicit transport.
    ///
    /// Hosts can pass a shared [`ReqwestStudioTransport`] (one connection pool); tests pass an
    /// in-memory scripted transport.
    pub fn with_transport(config: StudioConfig, transport: Arc<dyn StudioHttpTransport>) -> Self {
        let base_url = config.studio_url().to_string();
        Self {
            transport,
            config,
            base_url,
        }
    }

    /// Returns the configuration backing this client.
    pub fn config(&self) -> &StudioConfig {
        &self.config
    }

    /// Registers a new run with Studio (`POST /trpc/registerRun`).
    ///
    /// Must be called before sending any messages.
    pub async fn register_run(&self) -> StudioResult<()> {
        let payload = RegisterRunRequest::builder()
            .id(self.config.run_id())
            .project(self.config.project())
            .name(self.config.run_name())
            .timestamp(format_timestamp(Utc::now()))
            .pid(std::process::id() as u64)
            .status("running")
            .run_dir("")
            .build();
        let url = format!("{}/trpc/registerRun", self.base_url);
        self.post_json(&url, &payload).await.map(|_| ())
    }

    /// Pushes a message to Studio for visualization (`POST /trpc/pushMessage`).
    pub async fn push_message(&self, msg: &Msg) -> StudioResult<()> {
        let message_id = Uuid::new_v4().to_string();
        let name = msg
            .name()
            .filter(|n| !n.is_empty())
            .map(|n| n.to_string())
            .unwrap_or_else(|| message_id.clone());
        let payload = PushMessageRequest::builder()
            .run_id(self.config.run_id())
            .reply_id(message_id)
            .name(name)
            .role(role_label(msg.role()))
            .msg(msg.clone())
            .build();
        let url = format!("{}/trpc/pushMessage", self.base_url);
        self.post_json(&url, &payload).await.map(|_| ())
    }

    /// Requests user input from Studio's web UI (`POST /trpc/requestUserInput`).
    ///
    /// Returns the generated request id, which is used to match the WebSocket
    /// response delivered to [`StudioWebSocketClient::wait_for_input`].
    pub async fn request_user_input(
        &self,
        agent_id: &str,
        agent_name: &str,
        structured_schema: Option<serde_json::Value>,
    ) -> StudioResult<String> {
        let request_id = Uuid::new_v4().to_string();
        let mut builder = RequestUserInputRequest::builder()
            .request_id(request_id.clone())
            .run_id(self.config.run_id())
            .agent_id(agent_id)
            .agent_name(agent_name);
        if let Some(schema) = structured_schema {
            builder = builder.structured_input(schema);
        }
        let payload = builder.build();
        let url = format!("{}/trpc/requestUserInput", self.base_url);
        self.post_json(&url, &payload).await?;
        Ok(request_id)
    }

    /// Shuts down the HTTP client, releasing pooled connections.
    pub fn shutdown(&self) {
        // reqwest has no explicit shutdown; dropping the transport closes the pool.
        tracing::debug!("studio http client shutdown requested");
    }

    /// Posts a JSON body, retrying transport errors with exponential backoff
    /// (base 100ms, capped at 5s, `max_retries` retries).
    ///
    /// Retry semantics are those of the previous inline implementation and are pinned by
    /// `http_status_errors_are_not_retried`: Java wraps a non-2xx response into an
    /// `IOException` inside the retried `Mono`, so it retries status errors too — a known
    /// divergence recorded in the tracker (§4.2 P2-1), not silently changed here.
    async fn post_json<T: serde::Serialize + ?Sized>(
        &self,
        url: &str,
        payload: &T,
    ) -> StudioResult<String> {
        let body = serde_json::to_string(payload)
            .map_err(|e| StudioError::Serialization(e.to_string()))?;
        let request = StudioHttpRequest::post_json(url, body);

        let mut attempt = 0u32;
        let mut delay = Duration::from_millis(100);
        let max_delay = Duration::from_secs(5);
        loop {
            match self.send_once(request.clone()).await {
                Ok(body) => return Ok(body),
                Err(err) if err.is_retryable() && attempt < self.config.max_retries() => {
                    attempt += 1;
                    tracing::warn!(
                        url = %url,
                        attempt,
                        error = %err,
                        "studio request failed, retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(max_delay);
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Performs one exchange and maps the response status onto [`StudioError`].
    async fn send_once(&self, request: StudioHttpRequest) -> StudioResult<String> {
        let url = request.url.clone();
        let response = self.transport.execute(request).await?;
        if !response.is_success() {
            return Err(StudioError::Http {
                status: response.status_code,
                message: response.body,
            });
        }
        tracing::trace!(url = %url, "studio request succeeded");
        Ok(response.body)
    }
}

/// Maps a [`MsgRole`] to the lowercased label Studio expects.
pub(crate) fn role_label(role: MsgRole) -> String {
    match role {
        MsgRole::System => "system",
        MsgRole::User => "user",
        MsgRole::Assistant => "assistant",
        MsgRole::Tool => "tool",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock};

    use super::{role_label, StudioClient};
    use crate::error::StudioError;
    use crate::studio::config::StudioConfig;
    use crate::studio::http::testing::{Outcome, ScriptedTransport};
    use crate::studio::http::StudioHttpRequest;

    const BASE: &str = "http://127.0.0.1:3000";

    fn test_config(max_retries: u32) -> StudioConfig {
        StudioConfig::builder()
            .studio_url(BASE)
            .project("proj")
            .run_name("run_test")
            .run_id("run-123")
            .max_retries(max_retries)
            .build()
    }

    fn client_with(transport: Arc<ScriptedTransport>, max_retries: u32) -> StudioClient {
        StudioClient::with_transport(test_config(max_retries), transport)
    }

    fn json_body(request: &StudioHttpRequest) -> serde_json::Value {
        serde_json::from_str(&request.body).expect("request body is JSON")
    }

    /// §3.2 / Java `StudioClient.registerRun`: `POST {base}/trpc/registerRun` with the
    /// `RegisterRunRequest` payload and the Java media type.
    #[tokio::test]
    async fn register_run_posts_java_shaped_request() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::ok(200, "")));
        let client = client_with(transport.clone(), 3);

        client.register_run().await.expect("register succeeds");

        let request = transport.last_request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, format!("{BASE}/trpc/registerRun"));
        // Java only sets the JSON media type — there is no auth header to reproduce.
        assert_eq!(
            request.headers,
            vec![(
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string()
            )]
        );

        let body = json_body(&request);
        assert_eq!(body["id"], "run-123");
        assert_eq!(body["project"], "proj");
        assert_eq!(body["name"], "run_test");
        assert_eq!(body["status"], "running");
        assert_eq!(body["run_dir"], "");
        assert_eq!(body["pid"].as_u64(), Some(std::process::id() as u64));
        let timestamp = body["timestamp"].as_str().expect("timestamp is a string");
        chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S%.3f")
            .expect("timestamp uses yyyy-MM-dd HH:mm:ss.SSS");
    }

    /// §3.2 / Java `StudioClient.pushMessage`: camelCase payload, lowercased role.
    #[tokio::test]
    async fn push_message_posts_java_shaped_request() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::ok(200, "")));
        let client = client_with(transport.clone(), 3);
        let msg = Msg::builder()
            .name("assistant")
            .role(MsgRole::Assistant)
            .block(ContentBlock::Text(TextBlock::new("hello")))
            .build();

        client.push_message(&msg).await.expect("push succeeds");

        let request = transport.last_request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, format!("{BASE}/trpc/pushMessage"));
        let body = json_body(&request);
        assert_eq!(body["runId"], "run-123");
        assert_eq!(body["name"], "assistant");
        // Java `msg.getRole().name().toLowerCase()`.
        assert_eq!(body["role"], "assistant");
        // `replyId` is a fresh UUID per push.
        let reply_id = body["replyId"].as_str().expect("replyId is a string");
        assert!(
            uuid::Uuid::parse_str(reply_id).is_ok(),
            "replyId: {reply_id}"
        );
        assert_eq!(body["msg"]["content"][0]["text"], "hello");
    }

    /// §3.2 / Java `StudioClient.pushMessage`: an unnamed message reuses the reply id as name.
    #[tokio::test]
    async fn push_message_falls_back_to_reply_id_for_unnamed_messages() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::ok(200, "")));
        let client = client_with(transport.clone(), 3);

        client
            .push_message(&Msg::user("hi"))
            .await
            .expect("push succeeds");

        let body = json_body(&transport.last_request());
        assert_eq!(body["name"], body["replyId"]);
        assert_eq!(body["role"], "user");
    }

    /// §3.2 / Java `StudioClient.requestUserInput`: the returned id is the payload's
    /// `requestId`; `structuredInput` is omitted when no schema is given.
    #[tokio::test]
    async fn request_user_input_posts_java_shaped_request() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::ok(200, "")));
        let client = client_with(transport.clone(), 3);

        let request_id = client
            .request_user_input("agent-1", "Agent One", None)
            .await
            .expect("request succeeds");

        let request = transport.last_request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, format!("{BASE}/trpc/requestUserInput"));
        let body = json_body(&request);
        assert_eq!(body["requestId"], request_id.as_str());
        assert_eq!(body["runId"], "run-123");
        assert_eq!(body["agentId"], "agent-1");
        assert_eq!(body["agentName"], "Agent One");
        assert!(body.get("structuredInput").is_none());
    }

    /// §3.2 / Java `requestUserInput(..., structuredSchema)`: the schema is forwarded as is.
    #[tokio::test]
    async fn request_user_input_forwards_structured_schema() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::ok(200, "")));
        let client = client_with(transport.clone(), 3);

        client
            .request_user_input(
                "agent-1",
                "Agent One",
                Some(serde_json::json!({"type": "object"})),
            )
            .await
            .expect("request succeeds");

        let body = json_body(&transport.last_request());
        assert_eq!(body["structuredInput"]["type"], "object");
    }

    /// §3.2: a 2xx response (with or without body) is a success.
    #[tokio::test]
    async fn success_statuses_are_decoded_as_ok() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            Outcome::ok(200, ""),
            Outcome::ok(204, ""),
        ]));
        let client = client_with(transport.clone(), 0);

        client.register_run().await.expect("200 succeeds");
        client
            .push_message(&Msg::user("hi"))
            .await
            .expect("204 succeeds");
        assert_eq!(transport.attempt_count(), 2);
    }

    /// §4.2 P2-1 (locked behaviour): Rust does **not** retry non-2xx statuses; Java wraps the
    /// non-2xx response in an `IOException` inside the retried `Mono` and therefore does.
    #[tokio::test]
    async fn http_status_errors_are_not_retried() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::ok(500, "boom")));
        let client = client_with(transport.clone(), 3);

        let err = client
            .push_message(&Msg::user("hi"))
            .await
            .expect_err("500 fails");

        match err {
            StudioError::Http { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected StudioError::Http, got {other:?}"),
        }
        assert_eq!(transport.attempt_count(), 1, "non-2xx must not be retried");
    }

    /// Java `executeWithRetry`: transport failures are retried with the same request.
    #[tokio::test]
    async fn transport_errors_are_retried_then_succeed() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            Outcome::transport("connection refused"),
            Outcome::ok(200, ""),
        ]));
        let client = client_with(transport.clone(), 3);

        client
            .push_message(&Msg::user("hi"))
            .await
            .expect("recovers");

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url, requests[1].url);
        assert_eq!(requests[0].body, requests[1].body);
    }

    /// Java `Retry.backoff(config.getMaxRetries(), ...)`: `max_retries` retries after the
    /// first attempt, then the last error is returned.
    #[tokio::test]
    async fn transport_errors_stop_after_max_retries() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::transport("studio down")));
        let client = client_with(transport.clone(), 2);

        let err = client.register_run().await.expect_err("still down");

        assert!(matches!(err, StudioError::Transport(_)), "got {err:?}");
        assert_eq!(transport.attempt_count(), 3, "1 attempt + max_retries (2)");
    }

    #[test]
    fn role_labels_match_java() {
        assert_eq!(role_label(MsgRole::System), "system");
        assert_eq!(role_label(MsgRole::User), "user");
        assert_eq!(role_label(MsgRole::Assistant), "assistant");
        assert_eq!(role_label(MsgRole::Tool), "tool");
    }
}
