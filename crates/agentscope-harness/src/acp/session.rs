//! ACP agent session: holds per-session state and drives the core [`ReActAgent`]
//! on each prompt, streaming ACP `SessionUpdate` notifications back to the client.
//!
//! Mirrors `AgiAgentSession` from the Kotlin `AgiHarnessAgent`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    ContentBlock, MessageId, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigOption, SessionId, SessionMode, SessionModeState, StopReason, ToolCallUpdate,
};
use agent_client_protocol::{Client, ConnectionTo};
use agentscope_core::agent::StreamOptions;
use agentscope_core::event::ConfirmResult;
use agentscope_core::message::{ChatUsage, Msg, ToolUseBlock};
use agentscope_core::permission::{PermissionBehavior, PermissionRule};
use agentscope_core::ConfirmHandler;
use agentscope_core::model::GenerateOptions;

use crate::agent::HarnessAgent;

use super::config::{self, ModeChoice, ModelChoice};
use super::event;
use super::permission_store::PermissionRuleStore;

/// Current configuration of an ACP session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub model: String,
    pub mode: String,
    /// Reasoning-effort / thought level (`disabled` / `low` / `medium` /
    /// `high` / `max`), mirroring Java `ThoughtLevel`. Applied as per-call
    /// generation defaults on the *next* prompt.
    pub thought_level: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: "default".to_string(),
            mode: config::default_mode().to_string(),
            thought_level: config::DEFAULT_THOUGHT_LEVEL.to_string(),
        }
    }
}

/// Permission option ids advertised to ACP clients, mirroring the Java agent's
/// `PermissionOptionKind` naming.
const OPTION_ALLOW_ONCE: &str = "ALLOW_ONCE";
const OPTION_ALLOW_ALWAYS: &str = "ALLOW_ALWAYS";
const OPTION_REJECT_ONCE: &str = "REJECT_ONCE";
const OPTION_REJECT_ALWAYS: &str = "REJECT_ALWAYS";

/// Client handle used to push `session/request_permission` (human review)
/// requests back to the ACP client and await the user's decision.
///
/// The wrapped connection is the same `ConnectionTo<Client>` the coding agent's
/// prompt handler receives; it is cheaply cloneable and safe to share into the
/// spawned task that runs the agent.
#[derive(Clone)]
pub struct AcpClientHandle {
    connection: ConnectionTo<Client>,
}

impl AcpClientHandle {
    /// Wraps an agent-side connection whose counterpart is the ACP client.
    pub(crate) fn new(connection: ConnectionTo<Client>) -> Self {
        Self { connection }
    }

    /// Asks the client to request the user's permission for `tool_call`,
    /// returning the selected/rejected outcome.
    ///
    /// Only safe to await from a task outside the dispatch loop (the agent run
    /// already lives in a `ConnectionTo::spawn`ed task), which is exactly how
    /// the confirm handler invokes it.
    pub(crate) async fn request_permission(
        &self,
        session_id: &SessionId,
        tool_call: ToolCallUpdate,
    ) -> Result<RequestPermissionResponse, agent_client_protocol::Error> {
        let request =
            RequestPermissionRequest::new(session_id.clone(), tool_call, permission_options());
        self.connection.send_request(request).block_task().await
    }
}

/// The four choices surfaced to the reviewer, mirroring the Java agent.
fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            OPTION_ALLOW_ONCE,
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            OPTION_ALLOW_ALWAYS,
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            OPTION_REJECT_ONCE,
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            OPTION_REJECT_ALWAYS,
            "Always reject",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Builds the rule recorded when the user picks an "always" option: it matches
/// the exact tool call pattern (tool name + serialized arguments), so the same
/// invocation is decided automatically on later turns.
fn always_rule_for(call: &ToolUseBlock, allow: bool) -> PermissionRule {
    let behavior = if allow {
        PermissionBehavior::Allow
    } else {
        PermissionBehavior::Deny
    };
    let content = if call.input.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&call.input).unwrap_or_default())
    };
    PermissionRule::new(call.name.clone(), content, behavior, "user-permission")
}

/// Builds the core `ConfirmHandler` that turns an `Ask` permission decision
/// into an ACP `session/request_permission` round-trip and maps the outcome
/// back to a [`ConfirmResult`].
fn acp_confirm_handler(
    session_id: SessionId,
    client: Arc<AcpClientHandle>,
    agent: Arc<HarnessAgent>,
    store: Arc<PermissionRuleStore>,
) -> ConfirmHandler {
    Arc::new(move |call: ToolUseBlock| {
        let client = Arc::clone(&client);
        let agent = Arc::clone(&agent);
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        Box::pin(async move {
            let raw_input = if call.input.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(call.input.clone()))
            };
            let tool_call = event::tool_call_update_for_permission(
                call.id.clone(),
                &call.name,
                raw_input,
            );
            match client.request_permission(&session_id, tool_call).await {
                Ok(response) => match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => match option_id.to_string().as_str() {
                        OPTION_ALLOW_ONCE => ConfirmResult::allowed(),
                        OPTION_ALLOW_ALWAYS => {
                            let rule = always_rule_for(&call, true);
                            agent.add_permission_rule(rule.clone());
                            if let Err(error) = store.add(rule) {
                                tracing::warn!(
                                    tool = %call.name,
                                    %error,
                                    "failed to persist 'always allow' permission rule"
                                );
                            }
                            ConfirmResult::allowed()
                        }
                        OPTION_REJECT_ONCE => {
                            ConfirmResult::denied("Permission rejected by user")
                        }
                        OPTION_REJECT_ALWAYS => {
                            let rule = always_rule_for(&call, false);
                            agent.add_permission_rule(rule.clone());
                            if let Err(error) = store.add(rule) {
                                tracing::warn!(
                                    tool = %call.name,
                                    %error,
                                    "failed to persist 'always reject' permission rule"
                                );
                            }
                            ConfirmResult::denied("Permission rejected by user")
                        }
                        other => {
                            // Unknown option id: fail open to avoid stalling the
                            // agent, and log so misconfiguration is visible.
                            tracing::warn!(
                                tool = %call.name,
                                option = other,
                                "unexpected permission option id; allowing"
                            );
                            ConfirmResult::allowed()
                        }
                    },
                    RequestPermissionOutcome::Cancelled => {
                        ConfirmResult::denied("Permission request cancelled")
                    }
                    // The schema enum is non-exhaustive; treat any future
                    // outcome as a rejection to avoid executing on ambiguity.
                    _ => ConfirmResult::denied("Unexpected permission outcome"),
                },
                Err(error) => {
                    // No working client channel: mirror the Java behaviour of
                    // auto-approving when ClientSessionOperations is absent.
                    tracing::warn!(
                        tool = %call.name,
                        %error,
                        "request_permission failed; auto-approving tool"
                    );
                    ConfirmResult::allowed()
                }
            }
        })
    })
}


/// Maps a thought level to per-call generation defaults.
///
/// - `auto` (default): sends nothing — the provider default applies.
/// - Any explicit level: `reasoning_effort` (the official field), `max`
///   clamped to `high` (no `xhigh`).
///
/// Vendor-specific body params (`enable_thinking` / `thinking_budget`) are
/// never sent: strict OpenAI-compatible gateways (e.g. the Go/Zen gateways)
/// validate the body and reject unknown fields with
/// `json: unknown field "thinking_budget"`.
fn thought_level_params(level: &str) -> GenerateOptions {
    if level == "auto" || level.is_empty() {
        return GenerateOptions::default();
    }
    let effort = match level {
        "disabled" | "none" => "minimal",
        "max" => "high",
        other => other,
    };
    GenerateOptions::builder().reasoning_effort(effort).build()
}

/// A single ACP session backed by a [`HarnessAgent`].
#[derive(Clone)]
pub struct AcpAgentSession {
    session_id: SessionId,
    cwd: PathBuf,
    agent: Arc<HarnessAgent>,
    /// Shared across clones: handlers mutate the live session while the
    /// registry holds stale clones, so config changes (model / thought level /
    /// mode) must not be lost on `register_session` write-back.
    config: Arc<Mutex<SessionConfig>>,
    models: Vec<ModelChoice>,
    modes: Vec<ModeChoice>,
    /// Optional resolver building a model from a model id (dynamic model
    /// switching). When set, `setModel` swaps the live agent's model on the next
    /// prompt.
    model_builder: Option<
        Arc<dyn Fn(&str, &str) -> std::result::Result<Arc<dyn agentscope_core::model::Model>, String> + Send + Sync>,
    >,
    /// The model id currently applied to the live agent (for change detection).
    applied_model: String,
    /// Cancel flag for the currently active prompt request.
    cancel: Arc<AtomicBool>,
    usage: ChatUsage,
    /// Client connection used to push `session/request_permission` requests
    /// when a tool call needs human confirmation. Set per prompt by the
    /// connection layer (the coding agent has the live `ConnectionTo`).
    client: Option<Arc<AcpClientHandle>>,
    /// Persistent "always allow / always reject" rule store for this workspace.
    permission_store: Arc<PermissionRuleStore>,
    /// Manages MCP client connections for this session (stdio servers declared
    /// by the ACP client via `mcp_servers`). Shared across clones so tools keep
    /// working after `register_session` write-back.
    mcp_manager: Arc<crate::agent::McpClientManager>,
    /// Server names already bridged into the toolkit this session (dedup).
    bridged_servers: Arc<Mutex<Vec<String>>>,
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
        let permission_store = Arc::new(PermissionRuleStore::at_dir(&cwd));
        let mut session = Self {
            session_id,
            cwd,
            agent: Arc::new(agent),
            config: Arc::new(Mutex::new(SessionConfig::default())),
            models,
            modes,
            model_builder: None,
            applied_model: String::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            usage: ChatUsage::default(),
            client: None,
            permission_store,
            mcp_manager: Arc::new(crate::agent::McpClientManager::new()),
            bridged_servers: Arc::new(Mutex::new(Vec::new())),
        };
        session.apply_mode();
        session
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
    pub fn model(&self) -> String {
        self.config.lock().unwrap().model.clone()
    }

    /// Current mode id.
    pub fn mode(&self) -> String {
        self.config.lock().unwrap().mode.clone()
    }

    /// Sets the current model.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.config.lock().unwrap().model = model.into();
    }

    /// Installs the resolver used for dynamic model switching: a closure that
    /// builds a live [`Model`](agentscope_core::model::Model) from a model id.
    pub fn set_model_builder(
        &mut self,
        builder: Arc<dyn Fn(&str, &str) -> std::result::Result<Arc<dyn agentscope_core::model::Model>, String> + Send + Sync>,
    ) {
        self.model_builder = Some(builder);
    }

    /// Applies pending session config to the live agent. Config changes
    /// (thought level, model) take effect on the *next* prompt: the model is
    /// rebuilt from the resolved endpoint and swapped into the live agent while
    /// conversation state is preserved (mirrors Java `LocalAgent` rebuilding the
    /// agent per prompt with the new config).
    fn apply_session_config(&mut self) -> Vec<String> {
        // Thought level -> per-call generation defaults. `auto` sends nothing
        // (provider default); explicit levels send only `reasoning_effort`
        // (see `thought_level_params`).
        let level = self.config.lock().unwrap().thought_level.clone();
        let opts = thought_level_params(&level);
        self.agent.set_generate_options_defaults(opts);

        // Dynamic model switching (next prompt). Failures are returned so the
        // caller can notify the client instead of failing silently.
        let mut errors = Vec::new();
        if let Some(build) = &self.model_builder {
            let id = self.config.lock().unwrap().model.clone();
            if !id.is_empty() && id != "default" && id != self.applied_model {
                match build(&id, self.session_id.to_string().as_str()) {
                    Ok(model) => {
                        self.agent.set_model(model);
                        self.applied_model = id.clone();
                        tracing::info!(session = %self.session_id, model = %id,
                            "switched model for session");
                    }
                    Err(e) => {
                        tracing::warn!(session = %self.session_id, model = %id, error = %e,
                            "failed to switch model; keeping current model");
                        errors.push(format!(
                            "⚠️ model switch to '{id}' failed: {e} (keeping current model)"
                        ));
                    }
                }
            }
        }
        errors
    }


    /// Current thought level.
    pub fn thought_level(&self) -> String {
        self.config.lock().unwrap().thought_level.clone()
    }

    /// Sets the thought level (`auto` / `disabled` / `low` / `medium` / `high`
    /// / `max`). Applied on the next prompt.
    pub fn set_thought_level(&mut self, level: impl Into<String>) {
        self.config.lock().unwrap().thought_level = level.into();
    }
    /// Sets the current mode.
    pub fn set_mode(&mut self, mode: impl Into<String>) {
        self.config.lock().unwrap().mode = mode.into();
        // Apply the mode's permission context to the live agent so the *next*
        // request already runs under the new rules.
        self.apply_mode();
    }

    /// Reconfigures the underlying agent's permission engine to match the
    /// current session mode (`plan` / `accept_edits` / `yolo`), then layers the
    /// persisted "always allow / always reject" user rules on top so they
    /// survive mode switches and session restarts.
    fn apply_mode(&mut self) {
        let mode = self.config.lock().unwrap().mode.clone();
        let ctx = config::permission_context_for_mode(&mode);
        self.agent.set_permission_context(ctx);
        let persisted = self.permission_store.load();
        let persisted_count = persisted.len();
        for rule in persisted {
            self.agent.add_permission_rule(rule);
        }
        tracing::debug!(
            session = %self.session_id,
            mode,
            persisted_rules = persisted_count,
            "applied permission context for ACP session mode"
        );
    }

    /// Binds the live client connection for this prompt/connection, so
    /// `Ask` tool calls can push `session/request_permission` to the user.
    pub fn set_client(&mut self, client: AcpClientHandle) {
        self.client = Some(Arc::new(client));
    }

    /// Installs (or removes) the confirm handler that performs the ACP
    /// permission round-trip. Called at the start of every prompt run.
    fn install_confirm_handler(&self) {
        match &self.client {
            Some(client) => {
                let handler = acp_confirm_handler(
                    self.session_id.clone(),
                    Arc::clone(client),
                    Arc::clone(&self.agent),
                    Arc::clone(&self.permission_store),
                );
                self.agent.set_confirm_handler(Some(handler));
            }
            None => {
                // No client handle (e.g. an embedded run that never connected
                // to an ACP peer): Ask decisions stay unsupported and error.
                self.agent.set_confirm_handler(None);
            }
        }
    }

    /// Connects the MCP servers declared by the ACP client (`mcp_servers` on
    /// new/load/resume session) and bridges their tools into the session's
    /// toolkit so the core agent can call them.
    ///
    /// Supports stdio (`McpServer::Stdio`), SSE (`McpServer::Sse`) and HTTP
    /// (`McpServer::Http`) transports. Re-declaring an already bridged server is
    /// a no-op.
    pub async fn apply_mcp_servers(&self, servers: &[agent_client_protocol::schema::v1::McpServer]) {
        use agent_client_protocol::schema::v1::McpServer;
        for server in servers {
            let (name, config) = match server {
                McpServer::Stdio(stdio) => {
                    let name = stdio.name.clone();
                    // Build the stdio launch command: `command args...`.
                    let command = std::iter::once(stdio.command.display().to_string())
                        .chain(stdio.args.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" ");
                    (name.clone(), crate::agent::McpServerConfig::new(name).command(command))
                }
                McpServer::Sse(sse) => {
                    let headers = sse
                        .headers
                        .iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect::<Vec<_>>();
                    (
                        sse.name.clone(),
                        crate::agent::McpServerConfig::new(sse.name.clone())
                            .url(sse.url.clone())
                            .headers(headers),
                    )
                }
                McpServer::Http(http) => {
                    let headers = http
                        .headers
                        .iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect::<Vec<_>>();
                    (
                        http.name.clone(),
                        crate::agent::McpServerConfig::new(http.name.clone())
                            .url(http.url.clone())
                            .headers(headers),
                    )
                }
                _other => {
                    tracing::warn!(
                        session = %self.session_id,
                        "MCP server transport not supported; skipping"
                    );
                    continue;
                }
            };
            // Dedup: skip servers already bridged this session.
            {
                let mut bridged = self.bridged_servers.lock().unwrap();
                if bridged.contains(&name) {
                    continue;
                }
                bridged.push(name.clone());
            }
            match self.mcp_manager.create_client(config).await {
                Ok(client_id) => {
                    if let Err(e) = self.mcp_manager.connect(&client_id).await {
                        tracing::warn!(
                            session = %self.session_id,
                            server = %name,
                            %e,
                            "failed to connect MCP server"
                        );
                        continue;
                    }
                    let toolkit = self.agent.toolkit();
                    crate::agent::bridge_mcp_tools_to_toolkit(&self.mcp_manager, toolkit).await;
                    tracing::info!(
                        session = %self.session_id,
                        server = %name,
                        "bridged MCP server tools into session toolkit"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session = %self.session_id,
                        server = %name,
                        %e,
                        "failed to create MCP client"
                    );
                }
            }
        }
    }

    /// Returns the session mode state advertised to the client.
    pub fn session_mode_state(&self) -> SessionModeState {
        let modes: Vec<SessionMode> = self
            .modes
            .iter()
            .map(|m| SessionMode::new(m.id.clone(), m.name.clone()))
            .collect();
        SessionModeState::new(self.config.lock().unwrap().mode.clone(), modes)
    }

    /// Returns the session config options advertised to the client.
    pub fn config_options(&self) -> Vec<SessionConfigOption> {
        let cfg = self.config.lock().unwrap();
        super::config::default_config_options_with_thought(
            &cfg.model,
            &self.models,
            &cfg.mode,
            &self.modes,
            &cfg.thought_level,
        )
    }

    /// Cancels any active prompt request.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Runs a prompt against the core agent and streams updates, honouring the
    /// SDK's per-request cancellation marker (`$/cancel_request`) in addition to
    /// the session-level [`cancel`](Self::cancel) flag.
    ///
    /// The `emit` closure is invoked for each ACP `SessionNotification` to send
    /// back to the client. Returns the `StopReason` for the turn.
    pub async fn run_prompt_cancellable<F>(
        &mut self,
        content: Vec<ContentBlock>,
        emit: F,
        cancellation: Option<agent_client_protocol::RequestCancellation>,
    ) -> StopReason
    where
        F: Fn(agent_client_protocol::schema::v1::SessionNotification) + Send + Sync,
    {
        self.run_prompt_inner(content, emit, cancellation).await
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
        self.run_prompt_inner(content, emit, None).await
    }

    /// Shared implementation for [`run_prompt`](Self::run_prompt) and
    /// [`run_prompt_cancellable`](Self::run_prompt_cancellable).
    async fn run_prompt_inner<F>(
        &mut self,
        content: Vec<ContentBlock>,
        emit: F,
        cancellation: Option<agent_client_protocol::RequestCancellation>,
    ) -> StopReason
    where
        F: Fn(agent_client_protocol::schema::v1::SessionNotification) + Send + Sync,
    {
        self.cancel.store(false, Ordering::SeqCst);
        let cancel_flag = self.cancel.clone();

        // Bind the confirm handler to the current client connection so tool
        // calls that need approval push an ACP request_permission this turn.
        self.install_confirm_handler();


        // Dynamic model switching + thought level: config changes (setModel /
        // setConfigOption thought_level) take effect on the *next* prompt. The
        // model is rebuilt from the resolved endpoint and swapped into the live
        // agent while conversation state is preserved. Switch failures are
        // surfaced to the client as a message chunk instead of failing silently.
        for err in self.apply_session_config() {
            emit(event::message_chunk(
                self.session_id.clone(),
                format!("{err}\n"),
                None,
            ));
        }
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

        use agentscope_core::event::AgentEventType;
        use futures::StreamExt;
        // Tracks whether any incremental text delta was already emitted, so we
        // don't duplicate the final reply text when it was streamed token-by-token.
        let mut emitted_text = false;
        // One stable message id per turn: every text/thinking chunk and tool call
        // emitted this turn shares it, so ACP clients group them into one
        // assistant message and render tool calls / thoughts correctly.
        let message_id = MessageId::new(format!(
            "turn-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let message_id_opt: Option<MessageId> = Some(message_id);
        let mut tool_events =
            super::event::ToolEventMapper::new().with_message_id(message_id_opt.clone());

        // Push the initial context-window usage so clients render the gauge
        // before the first model call completes. Without a prior model call
        // there is no real "in-context" count, so report zero rather than the
        // (inflated) cumulative session usage.
        emit(self.usage_notification(0));

        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                    if cancel_flag.load(Ordering::SeqCst) {
                        return StopReason::Cancelled;
                    }
                }
                // The SDK sets this marker when the peer sends `$/cancel_request`
                // for the prompt's JSON-RPC request id.
                _ = async {
                    if let Some(c) = &cancellation {
                        c.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    return StopReason::Cancelled;
                }
                item = stream.next() => {
                    match item {
                        Some(Ok(ev)) => match ev.event_type() {
                            AgentEventType::TextBlockDelta => {
                                if let Some(notif) = super::event::map_delta_event(
                                    self.session_id.clone(),
                                    &ev,
                                    message_id_opt.clone(),
                                ) {
                                    emitted_text = true;
                                    emit(notif);
                                }
                            }
                            AgentEventType::AgentResult => {
                                // Emit the final reply only if it wasn't already
                                // streamed incrementally (avoids duplicate output).
                                if !emitted_text {
                                    if let Some(notif) = super::event::map_result_event(
                                        self.session_id.clone(),
                                        &ev,
                                        message_id_opt.clone(),
                                    ) {
                                        emit(notif);
                                    }
                                }
                            }
                            AgentEventType::ModelCallEnd => {
                                // Accumulate usage and push a context-window
                                // UsageUpdate after every model call. `used`
                                // reflects the tokens in the current context
                                // (latest call's input tokens), NOT the
                                // cumulative session count.
                                if let Some(u) = ev
                                    .data()
                                    .fields
                                    .get("usage")
                                    .and_then(|v| serde_json::from_value::<ChatUsage>(v.clone()).ok())
                                {
                                    self.accumulate_usage(u);
                                    emit(self.usage_notification(u.input_tokens));
                                } else {
                                    emit(self.usage_notification(0));
                                }
                            }
                            _ => {
                                if let Some(notif) = super::event::map_delta_event(
                                    self.session_id.clone(),
                                    &ev,
                                    message_id_opt.clone(),
                                ) {
                                    emit(notif);
                                }
                                if let Some(notif) =
                                    tool_events.map(self.session_id.clone(), &ev)
                                {
                                    emit(notif);
                                }
                            }
                        },
                        Some(Err(e)) => {
                            emit(super::event::message_chunk(
                                self.session_id.clone(),
                                format!("Agent error: {e}"),
                                message_id_opt.clone(),
                            ));
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

    /// Builds the ACP `UsageUpdate` notification: `used` is the tokens in
    /// context (latest model-call input tokens, falling back to the cumulative
    /// count), `size` is the live model's context window.
    fn usage_notification(
        &self,
        used: u64,
    ) -> agent_client_protocol::schema::v1::SessionNotification {
        use agent_client_protocol::schema::v1::{SessionNotification, SessionUpdate, UsageUpdate};
        let size = self
            .agent
            .agent()
            .map(|a| a.model().context_window_size() as u64)
            .unwrap_or(128_000);
        SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::thought_level_params;

    #[test]
    fn auto_sends_nothing() {
        let opts = thought_level_params("auto");
        assert!(opts.reasoning_effort.is_none());
        assert!(opts.additional_body_params.is_empty());
    }

    /// Regression: a strict OpenAI-compatible gateway rejects vendor-specific
    /// body params with `json: unknown field "thinking_budget"`, so explicit
    /// levels must send `reasoning_effort` *only*.
    #[test]
    fn explicit_levels_send_only_reasoning_effort() {
        for (level, expected) in [
            ("disabled", Some("minimal")),
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("max", Some("high")),
        ] {
            let opts = thought_level_params(level);
            assert_eq!(opts.reasoning_effort.as_deref(), expected, "level {level}");
            assert!(
                opts.additional_body_params.is_empty(),
                "level {level} must not add body params"
            );
        }
    }
}
