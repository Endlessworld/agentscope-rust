//! HTTP client for communicating with AgentScope Studio, mirroring `StudioClient`.
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use agentscope_core::message::{Msg, MsgRole};

use crate::error::{StudioError, StudioResult};
use crate::studio::config::StudioConfig;
use crate::studio::pojo::{
    PushMessageRequest, RegisterRunRequest, RequestUserInputRequest,
};

/// Timestamp format used by Studio (`yyyy-MM-dd HH:mm:ss.SSS`), UTC.
fn format_timestamp(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// HTTP client for Studio's REST API (`registerRun`, `pushMessage`,
/// `requestUserInput`), with automatic retry for transport failures.
#[derive(Clone)]
pub struct StudioClient {
    http: reqwest::Client,
    config: StudioConfig,
    base_url: String,
}

impl std::fmt::Debug for StudioClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `reqwest::Client` holds connection-pool state; print only the
        // meaningful, cheaply-formattable fields.
        f.debug_struct("StudioClient")
            .field("base_url", &self.base_url)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl StudioClient {
    /// Creates a new Studio HTTP client from the given configuration.
    pub fn new(config: StudioConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let base_url = config.studio_url().to_string();
        Self { http, config, base_url }
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
        // reqwest has no explicit shutdown; dropping the client closes the pool.
        tracing::debug!("studio http client shutdown requested");
    }

    /// Posts a JSON body, retrying transport errors with exponential backoff
    /// (base 100ms, capped at 5s, `max_retries` attempts).
    async fn post_json<T: serde::Serialize + ?Sized>(
        &self,
        url: &str,
        payload: &T,
    ) -> StudioResult<String> {
        let mut attempt = 0u32;
        let mut delay = Duration::from_millis(100);
        let max_delay = Duration::from_secs(5);
        loop {
            match self.try_post(url, payload).await {
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

    async fn try_post<T: serde::Serialize + ?Sized>(
        &self,
        url: &str,
        payload: &T,
    ) -> StudioResult<String> {
        let response = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
            .json(payload)
            .send()
            .await
            .map_err(|e| StudioError::Transport(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| String::new());
            return Err(StudioError::Http {
                status: status.as_u16(),
                message,
            });
        }
        Ok(response.text().await.unwrap_or_default())
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
