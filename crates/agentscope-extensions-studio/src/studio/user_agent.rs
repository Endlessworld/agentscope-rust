//! User proxy agent representing human users, mirroring `StudioUserAgent`.
//!
//! The agent obtains input either from the terminal or from the Studio web UI.
//! Rust core has no `AgentBase`; this type is a lightweight, standalone input
//! provider rather than an agent subclass. [`StudioUserAgent::drive_round`] drives one
//! conversation round against any `CallableAgent` (core `ReActAgent`, harness
//! `HarnessAgent`, …), which is how the Java `AgentBase` loop is reproduced without
//! subclassing a core agent.
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

use agentscope_core::agent::CallableAgent;
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

    /// Drives one conversation round against a core agent.
    ///
    /// Obtains the user's next message (Studio web UI when enabled, otherwise the terminal) and
    /// hands it to any [`CallableAgent`] — `ReActAgent`, `HarnessAgent`, or any other
    /// implementation — returning the agent's reply. Java reaches the same loop by making
    /// `StudioUserAgent` an `AgentBase` subclass; Rust core has no agent subclassing, so the
    /// round is an explicit helper instead.
    ///
    /// The user message is pushed to Studio first (when a Studio client is configured) so the
    /// web UI shows both sides; the agent's reply is pushed by
    /// [`StudioMessageHook`](crate::StudioMessageHook) on the agent side.
    ///
    /// Unlike Java's `onErrorResume`, an agent failure is surfaced as [`StudioError::Agent`]
    /// rather than swallowed; a failed push of the user message is logged and ignored.
    pub async fn drive_round<A>(&self, agent: &A) -> StudioResult<Msg>
    where
        A: CallableAgent + ?Sized,
    {
        let user_message = self.call().await;
        self.drive_round_with(agent, user_message).await
    }

    /// [`Self::drive_round`] with a caller-supplied user message.
    ///
    /// This is the deterministic half of the round: no terminal/Studio input acquisition, so
    /// hosts that already hold the user's text (and all tests) can use it.
    pub async fn drive_round_with<A>(&self, agent: &A, user_message: Msg) -> StudioResult<Msg>
    where
        A: CallableAgent + ?Sized,
    {
        if let Some(client) = &self.studio_client {
            if let Err(err) = client.push_message(&user_message).await {
                tracing::warn!(error = %err, "failed to push the user message to Studio");
            }
        }
        agent
            .call(vec![user_message])
            .await
            .map_err(|err| StudioError::Agent(err.to_string()))
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
    ///
    /// Mirrors Java's `StudioUserAgent.Builder.build()` (`StudioUserAgent.java:327-331`),
    /// which rejects a null/blank name with `IllegalArgumentException("Agent name cannot be
    /// null or empty")`; here that becomes [`StudioError::InvalidArgument`] (tracker §6.4 P2-4).
    pub fn build(self) -> StudioResult<StudioUserAgent> {
        if self.name.trim().is_empty() {
            return Err(StudioError::InvalidArgument(
                "Agent name cannot be null or empty".to_string(),
            ));
        }
        Ok(StudioUserAgent {
            name: self.name,
            description: self.description,
            studio_client: self.studio_client,
            ws_client: self.ws_client,
            input_timeout: self.input_timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agentscope_core::agent::ReActAgent;
    use agentscope_core::message::Msg;
    use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};

    use super::StudioUserAgent;
    use crate::error::StudioError;
    use crate::studio::client::StudioClient;
    use crate::studio::config::StudioConfig;
    use crate::studio::http::testing::{Outcome, ScriptedTransport};

    /// A model that answers with one text block, so a real `ReActAgent` run completes.
    struct AnsweringModel;

    #[async_trait::async_trait]
    impl Model for AnsweringModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
            let response = ChatResponse::builder()
                .id("resp-round")
                .content(vec![agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("hello back"),
                )])
                .finish_reason("stop")
                .build();
            Box::pin(futures::stream::iter(vec![Ok(response)]))
        }

        fn model_name(&self) -> &str {
            "gpt-4o-mini"
        }
    }

    fn test_config() -> StudioConfig {
        StudioConfig::builder()
            .studio_url("http://127.0.0.1:3000")
            .run_id("run-123")
            .max_retries(0)
            .build()
    }

    fn agent() -> ReActAgent {
        ReActAgent::builder()
            .name("assistant")
            .model(Arc::new(AnsweringModel))
            .build()
            .expect("agent builds")
    }

    /// §3.2: one full round — user message pushed to Studio, core agent loop driven, reply
    /// returned. Nothing is stubbed below the seam: the `ReActAgent` really runs.
    #[tokio::test]
    async fn drive_round_with_feeds_a_core_agent_loop() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::ok(200, "")));
        let user = StudioUserAgent::builder()
            .name("User")
            .studio_client(Arc::new(StudioClient::with_transport(
                test_config(),
                transport.clone(),
            )))
            .build()
            .expect("valid name");

        let reply = user
            .drive_round_with(&agent(), Msg::user("hello"))
            .await
            .expect("round completes");

        assert_eq!(reply.text_content(), "hello back");
        assert_eq!(reply.role(), agentscope_core::message::MsgRole::Assistant);

        // The user side of the conversation reaches Studio before the agent runs.
        let request = transport.last_request();
        assert_eq!(request.url, "http://127.0.0.1:3000/trpc/pushMessage");
        let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
        assert_eq!(body["role"], "user");
        assert_eq!(body["runId"], "run-123");
        assert_eq!(body["msg"]["content"][0]["text"], "hello");
    }

    /// A Studio push failure must not abort the round (the UI is a viewer, not the driver).
    #[tokio::test]
    async fn drive_round_with_tolerates_a_failed_push() {
        let transport = Arc::new(ScriptedTransport::always(Outcome::ok(500, "nope")));
        let user = StudioUserAgent::builder()
            .studio_client(Arc::new(StudioClient::with_transport(
                test_config(),
                transport.clone(),
            )))
            .build()
            .expect("valid name");

        let reply = user
            .drive_round_with(&agent(), Msg::user("hello"))
            .await
            .expect("agent still runs");

        assert_eq!(reply.text_content(), "hello back");
        assert_eq!(transport.attempt_count(), 1);
    }

    /// An agent failure is surfaced (not swallowed like Java's `onErrorResume`).
    #[tokio::test]
    async fn drive_round_with_surfaces_agent_errors() {
        struct FailingAgent;

        #[async_trait::async_trait]
        impl agentscope_core::agent::Agent for FailingAgent {
            fn agent_id(&self) -> &str {
                "failing"
            }
            fn name(&self) -> &str {
                "failing"
            }
            fn description(&self) -> &str {
                ""
            }
            fn interrupt(&self) {}
            fn agent_state(&self) -> agentscope_core::state::AgentState {
                agentscope_core::state::AgentState::default()
            }
            fn toolkit(&self) -> &agentscope_core::tool::Toolkit {
                static TOOLKIT: std::sync::OnceLock<agentscope_core::tool::Toolkit> =
                    std::sync::OnceLock::new();
                TOOLKIT.get_or_init(agentscope_core::tool::Toolkit::new)
            }
        }

        #[async_trait::async_trait]
        impl agentscope_core::agent::CallableAgent for FailingAgent {
            async fn call(&self, _messages: Vec<Msg>) -> agentscope_core::error::Result<Msg> {
                Err(agentscope_core::error::Error::Agent("boom".into()))
            }

            async fn call_with_schema(
                &self,
                _messages: Vec<Msg>,
                _schema: agentscope_core::formatter::JsonSchema,
            ) -> agentscope_core::error::Result<Msg> {
                Err(agentscope_core::error::Error::Agent("boom".into()))
            }
        }

        let user = StudioUserAgent::builder()
            .name("User")
            .build()
            .expect("valid name");
        let err = user
            .drive_round_with(&FailingAgent, Msg::user("hello"))
            .await
            .expect_err("agent error surfaces");
        assert!(matches!(err, StudioError::Agent(_)), "got {err:?}");
    }

    /// Tracker §6.4 P2-4: Java's `StudioUserAgent.Builder.build()` rejects a blank name.
    #[test]
    fn blank_names_are_rejected_like_java() {
        for name in ["", "   ", "\t"] {
            match StudioUserAgent::builder().name(name).build() {
                Err(StudioError::InvalidArgument(message)) => {
                    assert_eq!(message, "Agent name cannot be null or empty");
                }
                Err(other) => panic!("unexpected error for {name:?}: {other}"),
                Ok(_) => panic!("blank name {name:?} was accepted"),
            }
        }
        assert_eq!(
            StudioUserAgent::builder()
                .name("User")
                .build()
                .expect("valid name")
                .name(),
            "User"
        );
    }
}
