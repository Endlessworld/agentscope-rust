//! ACP agent session: holds per-session state and drives the core [`ReActAgent`]
//! on each prompt, streaming ACP `SessionUpdate` notifications back to the client.
//!
//! Mirrors `AgiAgentSession` from the Kotlin `AgiHarnessAgent`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, SessionConfigOption, SessionId, SessionMode, SessionModeState, StopReason,
};
use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{ChatUsage, Msg};

use crate::agent::HarnessAgent;

use super::config::{ModeChoice, ModelChoice};

/// Current configuration of an ACP session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub model: String,
    pub mode: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: "default".to_string(),
            mode: "yolo".to_string(),
        }
    }
}

/// A single ACP session backed by a [`HarnessAgent`].
#[derive(Clone)]
pub struct AcpAgentSession {
    session_id: SessionId,
    cwd: PathBuf,
    agent: Arc<HarnessAgent>,
    config: SessionConfig,
    models: Vec<ModelChoice>,
    modes: Vec<ModeChoice>,
    /// Cancel flag for the currently active prompt request.
    cancel: Arc<AtomicBool>,
    usage: ChatUsage,
}

impl AcpAgentSession {
    /// Creates a new session for the given working directory.
    pub fn new(
        session_id: SessionId,
        cwd: PathBuf,
        agent: HarnessAgent,
        models: Vec<ModelChoice>,
        modes: Vec<ModeChoice>,
    ) -> Self {
        Self {
            session_id,
            cwd,
            agent: Arc::new(agent),
            config: SessionConfig::default(),
            models,
            modes,
            cancel: Arc::new(AtomicBool::new(false)),
            usage: ChatUsage::default(),
        }
    }

    /// The session id.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The working directory.
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// The underlying agent.
    pub fn agent(&self) -> &HarnessAgent {
        &self.agent
    }

    /// Current model id.
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Current mode id.
    pub fn mode(&self) -> &str {
        &self.config.mode
    }

    /// Sets the current model.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.config.model = model.into();
    }

    /// Sets the current mode.
    pub fn set_mode(&mut self, mode: impl Into<String>) {
        self.config.mode = mode.into();
    }

    /// Returns the session mode state advertised to the client.
    pub fn session_mode_state(&self) -> SessionModeState {
        let modes: Vec<SessionMode> = self
            .modes
            .iter()
            .map(|m| SessionMode::new(m.id.clone(), m.name.clone()))
            .collect();
        SessionModeState::new(self.config.mode.clone(), modes)
    }

    /// Returns the session config options advertised to the client.
    pub fn config_options(&self) -> Vec<SessionConfigOption> {
        super::config::default_config_options(
            &self.config.model,
            &self.models,
            &self.config.mode,
            &self.modes,
        )
    }

    /// Cancels any active prompt request.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Runs a prompt against the core agent and streams updates.
    ///
    /// The `emit` closure is invoked for each ACP `SessionNotification` to send
    /// back to the client. Returns the `StopReason` for the turn.
    pub async fn run_prompt<F>(
        &mut self,
        content: Vec<ContentBlock>,
        emit: F,
    ) -> StopReason
    where
        F: Fn(agent_client_protocol::schema::v1::SessionNotification) + Send + Sync,
    {
        self.cancel.store(false, Ordering::SeqCst);
        let cancel_flag = self.cancel.clone();

        // Convert ACP content blocks into core messages.
        let mut msgs = Vec::new();
        let mut text = String::new();
        for block in content {
            if let ContentBlock::Text(t) = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
        }
        let user_msg = if text.is_empty() {
            Msg::user("")
        } else {
            Msg::user(text)
        };
        msgs.push(user_msg);

        let stream = self.agent.stream_events(msgs, StreamOptions::defaults());
        let mut stream = std::pin::pin!(stream);

        use futures::StreamExt;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                    if cancel_flag.load(Ordering::SeqCst) {
                        return StopReason::Cancelled;
                    }
                }
                item = stream.next() => {
                    match item {
                        Some(Ok(ev)) => {
                            if let Some(notif) = super::event::map_delta_event(self.session_id.clone(), &ev) {
                                emit(notif);
                            }
                            if let Some(notif) = super::event::map_tool_event(self.session_id.clone(), &ev) {
                                emit(notif);
                            }
                        }
                        Some(Err(_)) => {
                            return StopReason::Refusal;
                        }
                        None => break,
                    }
                }
            }
        }

        StopReason::EndTurn
    }

    /// Cumulative token usage for this session.
    pub fn usage(&self) -> &ChatUsage {
        &self.usage
    }

    /// Accumulates usage from a turn.
    pub fn accumulate_usage(&mut self, usage: ChatUsage) {
        self.usage.input_tokens += usage.input_tokens;
        self.usage.output_tokens += usage.output_tokens;
    }
}
