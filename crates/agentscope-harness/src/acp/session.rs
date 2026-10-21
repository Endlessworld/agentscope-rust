//! ACP agent session: holds per-session state and drives the core [`ReActAgent`]
//! on each prompt, streaming ACP `SessionUpdate` notifications back to the client.
//!
//! Mirrors `AgiAgentSession` from the Kotlin `AgiHarnessAgent`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    ContentBlock, CreateElicitationRequest, CreateElicitationResponse, MessageId,
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigOption, SessionId,
    SessionMode, SessionModeState, StopReason, ToolCallUpdate,
};
use agent_client_protocol::{Client, ConnectionTo};
use agentscope_core::event::{AgentEvent, ConfirmResult};
use agentscope_core::message::{ChatUsage, ToolUseBlock};
use agentscope_core::permission::{PermissionBehavior, PermissionRule};
use agentscope_core::ConfirmHandler;
use agentscope_core::model::GenerateOptions;

use crate::agent::HarnessAgent;

use super::config::{self, ModeChoice, ModelChoice};
use super::elicitation::{ElicitationClient, ElicitationSlot};
use super::event;
use super::permission_store::PermissionRuleStore;
use super::support::ClientSupport;

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

    /// Pushes an `elicitation/create` form to the user and awaits their answer.
    ///
    /// Only safe to await from a task outside the dispatch loop (the agent run
    /// already lives in a `ConnectionTo::spawn`ed task), which is how the
    /// `ask_user` tool invokes it.
    pub(crate) async fn elicit(
        &self,
        request: CreateElicitationRequest,
    ) -> std::result::Result<CreateElicitationResponse, String> {
        self.connection
            .send_request(request)
            .block_task()
            .await
            .map_err(|error| error.to_string())
    }

    /// Opens an MCP-over-ACP connection to a client-declared MCP server
    /// (`mcp/connect`) and returns a handle that can list/call its tools.
    pub(crate) async fn connect_mcp_acp(
        &self,
        name: &str,
        server_id: agent_client_protocol::schema::v1::McpServerAcpId,
    ) -> Result<super::mcp_over_acp::McpAcpConnection, agent_client_protocol::Error> {
        super::mcp_over_acp::McpAcpConnection::connect(
            self.connection.clone(),
            name,
            server_id,
        )
        .await
    }
}

/// Bridges the live ACP connection into the `ask_user` tool.
#[async_trait::async_trait]
impl ElicitationClient for AcpClientHandle {
    async fn elicit(
        &self,
        request: CreateElicitationRequest,
    ) -> std::result::Result<CreateElicitationResponse, String> {
        AcpClientHandle::elicit(self, request).await
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

/// Maps a turn's accumulated [`ChatUsage`] to the protocol `Usage` reported in
/// `PromptResponse`. Returns `None` when the turn saw no usage at all, so an
/// untouched turn does not report a misleading zero-token payload.
fn turn_usage_to_response(usage: &ChatUsage) -> Option<agent_client_protocol::schema::v1::Usage> {
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return None;
    }
    Some(
        agent_client_protocol::schema::v1::Usage::new(
            usage.total_tokens(),
            usage.input_tokens,
            usage.output_tokens,
        )
        .cached_read_tokens(Some(usage.cached_tokens)),
    )
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
    /// Shared slot the `ask_user` tool reads the live connection from. It is
    /// filled whenever [`set_client`](Self::set_client) binds a connection; the
    /// tool holds the same `Arc`, so it sees the update without being rebuilt.
    elicitation_slot: ElicitationSlot,
    /// Persistent "always allow / always reject" rule store for this workspace.
    permission_store: Arc<PermissionRuleStore>,
    /// Manages MCP client connections for this session (stdio servers declared
    /// by the ACP client via `mcp_servers`). Shared across clones so tools keep
    /// working after `register_session` write-back.
    mcp_manager: Arc<crate::agent::McpClientManager>,
    /// Server names already bridged into the toolkit this session (dedup).
    bridged_servers: Arc<Mutex<Vec<String>>>,
    /// Capabilities the connected client advertised at `initialize`; several
    /// `session/update` variants are only legal when the matching capability was
    /// advertised, so every optional update is gated on this.
    support: ClientSupport,
    /// Live MCP-over-ACP connections (and their bridged tools) for this session,
    /// so they are released when the session is replaced/dropped. Shared across
    /// clones because `apply_mcp_servers` takes `&self`.
    mcp_acp_connections: Arc<Mutex<Vec<super::mcp_over_acp::McpAcpConnection>>>,
    /// Token usage accumulated during the turn that is running (reset per turn),
    /// reported back as `PromptResponse.usage`.
    turn_usage: ChatUsage,
    /// Workspace roots beyond `cwd`, reported in `session/list`.
    additional_directories: Vec<PathBuf>,
    /// Client-visible session title, if one was set.
    title: Option<String>,
    /// ISO-8601 timestamp of the last prompt turn.
    updated_at: Option<String>,
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
            elicitation_slot: Arc::new(Mutex::new(None)),
            permission_store,
            mcp_manager: Arc::new(crate::agent::McpClientManager::new()),
            bridged_servers: Arc::new(Mutex::new(Vec::new())),
            support: ClientSupport::new(),
            turn_usage: ChatUsage::default(),
            mcp_acp_connections: Arc::new(Mutex::new(Vec::new())),
            additional_directories: Vec::new(),
            title: None,
            updated_at: None,
        };
        session.apply_mode();
        session
    }

    /// The session id.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The runtime context that identifies this ACP session in the journal.
    ///
    /// The user id is left unset on purpose: `HarnessAgent::session_key` falls back to the
    /// agent's pinned state, so a host that configured a tenant keeps it.
    fn session_context(&self) -> agentscope_core::agent::RuntimeContext {
        agentscope_core::agent::RuntimeContext::new().with_session_id(self.session_id.to_string())
    }

    /// The working directory.
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// The underlying agent.
    pub fn agent(&self) -> &HarnessAgent {
        &self.agent
    }

    /// Capabilities the connected client advertised at `initialize`.
    pub fn client_support(&self) -> &ClientSupport {
        &self.support
    }

    /// Shares the agent-level client-capability view with this session (clones
    /// share the same underlying store, so a later `initialize` is visible
    /// without re-registering the session).
    pub fn set_client_support(&mut self, support: ClientSupport) {
        self.support = support;
    }

    /// Records the working directory the session was opened with, plus any
    /// additional workspace roots the client declared.
    pub fn set_additional_directories(&mut self, dirs: Vec<PathBuf>) {
        self.additional_directories = dirs;
    }

    /// The additional workspace roots reported for this session.
    pub fn additional_directories(&self) -> &[PathBuf] {
        &self.additional_directories
    }

    /// Builds an advisory notification for a non-fatal condition (e.g. a failed
    /// model switch or an MCP server that could not be bridged).
    ///
    /// Emits a `SessionUpdate::Notice` when the client advertised
    /// `session.notices`, otherwise degrades to an ordinary agent message so the
    /// information is never silently dropped.
    fn advisory_notification(
        &self,
        message: impl Into<String>,
    ) -> agent_client_protocol::schema::v1::SessionNotification {
        let message = message.into();
        if self.support.supports_notices() {
            event::notice(
                self.session_id.clone(),
                agent_client_protocol::schema::v1::NoticeSeverity::Warning,
                message,
                None,
            )
        } else {
            event::message_chunk(self.session_id.clone(), format!("{message}\n"), None)
        }
    }

    /// Builds this session's `SessionInfo` (see [`Self::session_info`]) and
    /// emits it as a `SessionInfoUpdate` so the client's session list stays in
    /// sync with the latest activity time.
    fn session_info_update(&self) -> agent_client_protocol::schema::v1::SessionNotification {
        use agent_client_protocol::schema::v1::{SessionInfoUpdate, SessionNotification, SessionUpdate};
        let mut update = SessionInfoUpdate::new();
        if let Some(title) = &self.title {
            update = update.title(title.clone());
        }
        if let Some(updated_at) = &self.updated_at {
            update = update.updated_at(updated_at.clone());
        }
        SessionNotification::new(self.session_id.clone(), SessionUpdate::SessionInfoUpdate(update))
    }

    /// Sets the client-visible session title (subsequent turns advertise it).
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = Some(title.into());
    }


    /// Projects this session into the protocol `SessionInfo` returned by
    /// `session/list` (cwd, additional roots, title and last-activity time).
    pub fn session_info(&self) -> agent_client_protocol::schema::v1::SessionInfo {
        let mut info = agent_client_protocol::schema::v1::SessionInfo::new(
            self.session_id.clone(),
            self.cwd.clone(),
        )
        .additional_directories(self.additional_directories.clone());
        if let Some(title) = &self.title {
            info = info.title(title.clone());
        }
        if let Some(updated_at) = &self.updated_at {
            info = info.updated_at(updated_at.clone());
        }
        info
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
    /// `Ask` tool calls can push `session/request_permission` to the user and
    /// `ask_user` can push an `elicitation/create` form.
    pub fn set_client(&mut self, client: AcpClientHandle) {
        let handle = Arc::new(client);
        // Publish the connection to the elicitation tool (which holds the same
        // slot across prompts, so it never needs rebuilding).
        let erased: Arc<dyn ElicitationClient> = handle.clone();
        *self.elicitation_slot.lock().unwrap() = Some(erased);
        self.client = Some(handle);
    }

    /// The slot the `ask_user` tool reads the live connection from. Shared with
    /// the tool built at session-creation time.
    pub fn elicitation_slot(&self) -> ElicitationSlot {
        Arc::clone(&self.elicitation_slot)
    }

    /// Installs the slot the `ask_user` tool was built with. Called with the
    /// very slot handed to the session factory, so the tool and the session
    /// share one handle.
    pub fn set_elicitation_slot(&mut self, slot: ElicitationSlot) {
        self.elicitation_slot = slot;
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
    /// Supports stdio (`McpServer::Stdio`), SSE (`McpServer::Sse`), HTTP
    /// (`McpServer::Http`) and native ACP (`McpServer::Acp`, MCP-over-ACP)
    /// transports. Re-declaring an already bridged server is a no-op.
    pub async fn apply_mcp_servers(&self, servers: &[agent_client_protocol::schema::v1::McpServer]) {
        use agent_client_protocol::schema::v1::McpServer;
        for server in servers {
            // MCP-over-ACP is driven by a different transport (the ACP channel
            // itself) and needs `&self` interior mutability, so it is handled
            // before the config-based transports.
            if let McpServer::Acp(acp) = server {
                self.apply_acp_mcp_server(acp).await;
                continue;
            }
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

    /// Connects a client-declared MCP-over-ACP server and bridges its tools.
    async fn apply_acp_mcp_server(
        &self,
        acp: &agent_client_protocol::schema::v1::McpServerAcp,
    ) {
        let name = acp.name.clone();
        {
            let mut bridged = self.bridged_servers.lock().unwrap();
            if bridged.contains(&name) {
                return;
            }
            bridged.push(name.clone());
        }
        let Some(client) = self.client.clone() else {
            tracing::warn!(
                session = %self.session_id,
                server = %name,
                "MCP-over-ACP server declared but no client connection is bound"
            );
            return;
        };
        match client.connect_mcp_acp(&name, acp.server_id.clone()).await {
            Ok(connection) => {
                if let Err(e) = connection.bridge_tools(self.agent.toolkit()).await {
                    tracing::warn!(
                        session = %self.session_id,
                        server = %name,
                        %e,
                        "failed to list MCP-over-ACP tools"
                    );
                }
                self.mcp_acp_connections.lock().unwrap().push(connection);
                tracing::info!(
                    session = %self.session_id,
                    server = %name,
                    "bridged MCP-over-ACP server tools into session toolkit"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session = %self.session_id,
                    server = %name,
                    %e,
                    "failed to connect MCP-over-ACP server"
                );
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
        // Turn-level usage: reset at the start of each prompt turn, accumulate
        // from every model call, and report back as `PromptResponse.usage`.
        self.turn_usage = ChatUsage::default();

        // Bind the confirm handler to the current client connection so tool
        // calls that need approval push an ACP request_permission this turn.
        self.install_confirm_handler();


        // Dynamic model switching + thought level: config changes (setModel /
        // setConfigOption thought_level) take effect on the *next* prompt. The
        // model is rebuilt from the resolved endpoint and swapped into the live
        // agent while conversation state is preserved. Switch failures are
        // surfaced to the client as an advisory notice (or a message chunk when
        // the client did not advertise `session.notices`).
        for err in self.apply_session_config() {
            emit(self.advisory_notification(err));
        }
        // Record last-activity time and tell the client so its session list
        // shows the current `updatedAt` without a separate `session/list` call.
        self.updated_at = Some(chrono::Utc::now().to_rfc3339());
        emit(self.session_info_update());
        // Convert ACP content blocks into one prompt string; the session runtime wraps it in
        // the user message that becomes the turn's admitted input.
        let mut text = String::new();
        for block in content {
            if let ContentBlock::Text(t) = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
        }
        // Run the turn through the session runtime instead of driving `stream_events`
        // directly: the runtime owns the journal writer, restores the committed state from
        // the journal before the run, and admits steering/interrupts. This client only
        // renders — events arrive through the sink below.
        let session = match self.agent.session(self.session_context()).await {
            Ok(session) => session,
            Err(error) => {
                emit(super::event::message_chunk(
                    self.session_id.clone(),
                    format!("Agent error: {error}"),
                    None,
                ));
                return StopReason::Refusal;
            }
        };
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        session.set_event_sink(Arc::new(move |event: &AgentEvent| {
            // A closed receiver only means the prompt already returned; the run itself is
            // unaffected, so dropping the event here is safe.
            let _ = events_tx.send(event.clone());
        }));
        let task = match session.submit(text).await {
            Ok(task) => task,
            Err(error) => {
                emit(super::event::message_chunk(
                    self.session_id.clone(),
                    format!("Agent error: {error}"),
                    None,
                ));
                return StopReason::Refusal;
            }
        };

        use agentscope_core::event::AgentEventType;
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
        // Clients that advertised plan operations get the ID-addressed
        // `plan_update` form for `todo_write`-derived plans.
        tool_events = tool_events.with_plan_operations(self.support.supports_plan_operations());

        // Push the initial context-window usage so clients render the gauge
        // before the first model call completes. Without a prior model call
        // there is no real "in-context" count, so report zero rather than the
        // (inflated) cumulative session usage.
        emit(self.usage_notification(0));

        // Settlement is the exit condition: the sink keeps the channel open, so "no more
        // events" never happens on its own. Events keep rendering in this loop until the
        // queue is drained after settlement.
        let mut settled_task: Option<crate::agent::session::Task> = None;
        let mut cancelled = false;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                    if cancel_flag.load(Ordering::SeqCst) {
                        cancelled = true;
                        break;
                    }
                    if settled_task.is_none() {
                        if let Ok(current) = session.task(&task.command_id).await {
                            if current.settled() {
                                settled_task = Some(current);
                            }
                        }
                    }
                    if settled_task.is_some() && events_rx.is_empty() {
                        break;
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
                    cancelled = true;
                    break;
                }
                event = events_rx.recv() => {
                    match event {
                        Some(ev) => match ev.event_type() {
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
                                    self.accumulate_usage(u.clone());
                                    self.accumulate_turn_usage(&u);
                                    emit(self.usage_notification(u.input_tokens));
                                } else {
                                    emit(self.usage_notification(0));
                                }
                            }
                            AgentEventType::Custom => {
                                // The compaction middleware announces a completed
                                // summarization in-band; translate it into the
                                // capability-gated `compaction_update` only when
                                // the client advertised `session.compaction`.
                                if let Some((_, summary)) =
                                    super::event::compaction_marker(&ev)
                                {
                                    if self.support.supports_compaction() {
                                        let compaction_id = format!(
                                            "compaction-{}",
                                            std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .map(|d| d.as_nanos())
                                                .unwrap_or(0)
                                        );
                                        for notif in super::event::compaction_notifications(
                                            self.session_id.clone(),
                                            &compaction_id,
                                            &summary,
                                        ) {
                                            emit(notif);
                                        }
                                    }
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
                                for notif in
                                    tool_events.map(self.session_id.clone(), &ev)
                                {
                                    emit(notif);
                                }
                            }
                        },
                        None => break,
                    }
                }
            }
        }

        if cancelled {
            // The ACP cancel flag is the only client-facing cancellation, so stop the run and
            // close its turn: an interrupted turn stays open in the journal (only
            // `completed`/`cancelled` end one), and the next prompt must be able to submit.
            //
            // Signal the agent directly, not only through the session: `AgentSession::interrupt`
            // needs the journal to already carry a run id, so a prompt cancelled while the
            // dispatcher is still admitting it would otherwise never reach the run at all.
            self.agent.interrupt();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                let _ = session.interrupt().await;
                match session.task(&task.command_id).await {
                    Ok(current) if current.settled() => break,
                    _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                }
            }

            let log = session.log();
            let turn_id = task.turn_id.clone();
            tokio::spawn(async move {
                // Waiting here would hold the prompt open for as long as the run's writer
                // lease lasts; the turn must be closed either way, just not on this path.
                let _ = agentscope_core::session::cancel_pending(log, &turn_id).await;
            });
            return StopReason::Cancelled;
        }

        if let Some(current) = &settled_task {
            if current.status == "failed" {
                let detail = current
                    .error
                    .clone()
                    .unwrap_or_else(|| "run failed".to_string());
                emit(super::event::message_chunk(
                    self.session_id.clone(),
                    format!("Agent error: {detail}"),
                    message_id_opt.clone(),
                ));
                return StopReason::Refusal;
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

    /// Accumulates usage for the currently running turn (reported back as
    /// `PromptResponse.usage`).
    fn accumulate_turn_usage(&mut self, usage: &ChatUsage) {
        self.turn_usage.input_tokens += usage.input_tokens;
        self.turn_usage.output_tokens += usage.output_tokens;
        self.turn_usage.cached_tokens += usage.cached_tokens;
    }

    /// Builds the ACP `Usage` for the running turn, or `None` when no model
    /// call contributed usage (so an empty turn does not report a bogus 0).
    pub fn turn_usage_response(&self) -> Option<agent_client_protocol::schema::v1::Usage> {
        turn_usage_to_response(&self.turn_usage)
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
    use super::{thought_level_params, turn_usage_to_response};
    use agentscope_core::message::ChatUsage;

    #[test]
    fn an_empty_turn_reports_no_usage() {
        assert!(turn_usage_to_response(&ChatUsage::default()).is_none());
    }

    #[test]
    fn turn_usage_maps_to_the_protocol_shape() {
        let usage = ChatUsage::builder()
            .input_tokens(100)
            .output_tokens(50)
            .cached_tokens(20)
            .build();
        let response = turn_usage_to_response(&usage).expect("usage reported");
        assert_eq!(response.total_tokens, 150);
        assert_eq!(response.input_tokens, 100);
        assert_eq!(response.output_tokens, 50);
        assert_eq!(response.cached_read_tokens, Some(20));
    }

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
