//! User proxy agent representing human users, mirroring `StudioUserAgent`.
//!
//! The agent obtains input either from the terminal or from the Studio web UI.
//! Rust core has no `AgentBase`; this type is a lightweight, standalone input
//! provider rather than an agent subclass. Callers can wire it into an agent
//! loop as the "user" turn.
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

use agentscope_core::message::{ContentBlock, JsonMap, Msg, MsgRole, TextBlock};

use crate::error::{StudioError, StudioResult};
use crate::studio::client::StudioClient;
use crate::studio::pojo::UserInputMetadata;
use crate::studio::websocket::{StudioWebSocketClient, UserInputData};

/// Default timeout for waiting on Studio user input (30 minutes).
pub const DEFAULT_INPUT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A user proxy that produces user-role messages from either the terminal or
/// the Studio web interface.
#[derive(Clone)]
pub struct StudioUserAgent {
    name: String,
    description: Option<String>,
    studio_client: Option<Arc<StudioClient>>,
    ws_client: Option<Arc<StudioWebSocketClient>>,
    input_timeout: Duration,
}

impl StudioUserAgent {
    /// Creates a new builder.
    pub fn builder() -> StudioUserAgentBuilder {
        StudioUserAgentBuilder::default()
    }

    /// The agent name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The agent description, if any.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Whether Studio integration is enabled (both clients present).
    pub fn studio_enabled(&self) -> bool {
        self.studio_client.is_some() && self.ws_client.is_some()
    }

    /// Obtains the next user message, using Studio when enabled and falling back
    /// to the terminal on error.
    pub async fn call(&self) -> Msg {
        if self.studio_enabled() {
            match self.input_from_studio().await {
                Ok(msg) => msg,
                Err(err) => {
                    tracing::warn!(error = %err, "failed to get input from Studio, falling back to terminal");
                    self.input_from_terminal().await
                }
            }
        } else {
            self.input_from_terminal().await
        }
    }

    /// Reads a line from stdin and wraps it in a user message.
    pub async fn input_from_terminal(&self) -> Msg {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut line = String::new();
        let _ = reader.read_line(&mut line).await;
        let input = line.trim_end_matches(['\n', '\r']).to_string();

        let mut metadata = JsonMap::new();
        metadata.insert(
            "source".to_string(),
            serde_json::Value::String("terminal".to_string()),
        );
        Msg::builder()
            .id(Uuid::new_v4().to_string())
            .name(&self.name)
            .role(MsgRole::User)
            .block(ContentBlock::Text(TextBlock::new(input)))
            .metadata(metadata)
            .build()
    }

    /// Requests input via the Studio web UI and waits for the user's response.
    pub async fn input_from_studio(&self) -> StudioResult<Msg> {
        let (Some(client), Some(ws)) = (self.studio_client.as_ref(), self.ws_client.as_ref())
        else {
            return Err(StudioError::NotInitialized);
        };

        let request_id = client
            .request_user_input(&self.name, &self.name, None)
            .await?;
        let input = ws.wait_for_input(&request_id, self.input_timeout).await?;
        Ok(self.build_msg(request_id, input))
    }

    fn build_msg(&self, request_id: String, input: UserInputData) -> Msg {
        let metadata = UserInputMetadata::builder()
            .request_id(request_id)
            .structured_input(input.structured_input.clone().unwrap_or_default())
            .build()
            .to_map();

        let blocks = if input.blocks_input.is_empty() {
            vec![ContentBlock::Text(TextBlock::new(""))]
        } else {
            input.blocks_input
        };

        Msg::builder()
            .id(Uuid::new_v4().to_string())
            .name(&self.name)
            .role(MsgRole::User)
            .content(blocks)
            .metadata(metadata)
            .build()
    }
}

/// Builder for [`StudioUserAgent`].
#[derive(Debug, Clone)]
pub struct StudioUserAgentBuilder {
    name: String,
    description: Option<String>,
    studio_client: Option<Arc<StudioClient>>,
    ws_client: Option<Arc<StudioWebSocketClient>>,
    input_timeout: Duration,
}

impl Default for StudioUserAgentBuilder {
    fn default() -> Self {
        Self {
            name: "User".to_string(),
            description: None,
            studio_client: None,
            ws_client: None,
            input_timeout: DEFAULT_INPUT_TIMEOUT,
        }
    }
}

impl StudioUserAgentBuilder {
    /// Sets the agent name (default `User`).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Sets the agent description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets the Studio HTTP client.
    pub fn studio_client(mut self, client: Arc<StudioClient>) -> Self {
        self.studio_client = Some(client);
        self
    }

    /// Sets the Studio WebSocket client.
    pub fn web_socket_client(mut self, client: Arc<StudioWebSocketClient>) -> Self {
        self.ws_client = Some(client);
        self
    }

    /// Sets the input timeout (default 30 minutes).
    pub fn input_timeout(mut self, input_timeout: Duration) -> Self {
        self.input_timeout = input_timeout;
        self
    }

    /// Builds the user agent.
    pub fn build(self) -> StudioUserAgent {
        StudioUserAgent {
            name: self.name,
            description: self.description,
            studio_client: self.studio_client,
            ws_client: self.ws_client,
            input_timeout: self.input_timeout,
        }
    }
}
