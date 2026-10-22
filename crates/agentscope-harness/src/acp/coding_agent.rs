//! ACP CodingAgent: an ACP **agent** (server role) that wraps the core
//! [`ReActAgent`] and speaks the Agent Client Protocol over stdio.
//!
//! Mirrors the Kotlin `AgiAgent` / `AgiAgentSession` and Java
//! `HarnessCodingAgent`: handles `initialize`, session lifecycle, prompts
//! (streaming `SessionUpdate` notifications), config options, and cancellation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    AgentAuthCapabilities, AgentCapabilities, CloseSessionRequest, CloseSessionResponse,
    DeleteSessionRequest, DeleteSessionResponse, DisableProviderRequest, DisableProviderResponse,
    InitializeRequest, InitializeResponse, Implementation, ListProvidersRequest,
    ListProvidersResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, PromptCapabilities, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities, SessionDeleteCapabilities,
    SessionId, SessionInfo, SessionListCapabilities, SessionResumeCapabilities,
    SetProviderRequest, SetProviderResponse, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
};
use agent_client_protocol::{Agent, Stdio};

use agentscope_core::model::Model;
use agentscope_extensions_model::OpenAiChatModel;

use crate::agent::context::TaskContextOptions;
use crate::agent::{CompactionConfig, HarnessAgent};

use super::config::{ModeChoice, ModelChoice};
use super::elicitation::ElicitationSlot;
use super::provider::ProviderRegistry;
use super::session::{AcpAgentSession, AcpClientHandle};
use super::support::ClientSupport;

/// Default agent info advertised during initialization.
const AGENT_NAME: &str = "agentscope-coding-agent";
const AGENT_VERSION: &str = "0.1.0";


/// Default model id used when `CODING_MODEL_ID` is not set.
const DEFAULT_MODEL_ID: &str = "gpt-4o";

/// Context window the ACP agents compact against: conversation compaction triggers once the
/// accumulated context reaches this many tokens.
pub const ACP_CONTEXT_WINDOW_TOKENS: usize = 256_000;

/// Builds an [`OpenAiChatModel`] from the environment (mirrors Java `CodingAgentFactory`).
///
/// Reads `CODING_MODEL_ID` (default `gpt-4o`), `OPENAI_API_KEY` and `OPENAI_BASE_URL`.
fn build_env_model() -> Arc<dyn Model> {
    let model_id = std::env::var("CODING_MODEL_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string());
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    let model = OpenAiChatModel::new(model_id, api_key, base_url)
        .expect("failed to build OpenAiChatModel");
    Arc::new(model)
}

/// The default per-session factory: builds a fully-wired [`HarnessAgent`] using the
/// [`HarnessAgent::builder`]'s complete orchestration (workspace, filesystem tools, shell,
/// compaction, plan mode). Mirrors the Java `HarnessCodingAgent`/`CodingAgentFactory`.
fn default_model_factory(cwd: std::path::PathBuf, _session_id: &str) -> HarnessAgent {
    let model = build_env_model();
    let workspace = cwd.join(".agentscope").join("workspace");
    HarnessAgent::builder()
        .name("agentscope-coding-agent")
        .model(model)
        .cwd(cwd)
        .workspace(workspace)
        .register_filesystem_tools(true)
        .register_shell_tool(true)
        .conversation_compaction(
            CompactionConfig::builder()
                .trigger_messages(0)
                .trigger_tokens(ACP_CONTEXT_WINDOW_TOKENS)
                .build(),
        )
        .max_iters(50)
        // 同 ACP 主路径：中断/崩溃后把悬挂的 tool_use 补成 error result。
        .enable_pending_tool_recovery(true)
        // 与 ACP 主路径一致：开启任务投影，交给上下文编译器产出 `<TASK_STATE>` 与 manifest。
        .task_context(TaskContextOptions {
            requirements: true,
            verification_results: true,
            requirement_proposals: false,
        })
        .build()
}

/// Everything a session factory needs to build a [`HarnessAgent`].
///
/// Capabilities must be known *before* the agent is built: the subagent
/// blueprint snapshots the toolkit at build time (`HarnessAgent::build`), so a
/// tool registered only afterwards would never reach subagents.
pub struct SessionBuild {
    /// Working directory the session was opened with.
    pub cwd: PathBuf,
    /// The freshly assigned ACP session id.
    pub session_id: String,
    /// Capabilities the client advertised at `initialize`.
    pub client_support: ClientSupport,
    /// Slot the `ask_user` tool reads the live connection from. The session
    /// fills it on every prompt; the tool holds the same handle.
    pub elicitation_slot: ElicitationSlot,
}

/// An ACP CodingAgent with a session registry and a factory for building the
/// underlying [`HarnessAgent`] per session.
pub struct AcpCodingAgent {
    name: String,
    model_factory: Arc<dyn Fn(PathBuf, &str) -> HarnessAgent + Send + Sync>,
    /// Capability-aware factory; takes precedence over `model_factory` when set.
    /// Only this variant can gate tools on client capabilities at build time
    /// (and therefore have them inherited by subagents).
    session_factory: Option<Arc<dyn Fn(SessionBuild) -> HarnessAgent + Send + Sync>>,
    models: Vec<ModelChoice>,
    modes: Vec<ModeChoice>,
    sessions: Arc<Mutex<HashMap<String, AcpAgentSession>>>,
    /// Builds a live model from a model id + session id (dynamic model
    /// switching). The session id lets callers derive per-session request
    /// headers (e.g. `x-opencode-session` for prompt-cache routing).
    model_builder: Option<
        Arc<dyn Fn(&str, &str) -> Result<Arc<dyn Model>, String> + Send + Sync>,
    >,
    /// 会话注册表上限（嵌入式优化）：超过上限时淘汰最旧会话，
    /// 避免多会话成倍放大内存。默认 1。
    max_sessions: usize,
    /// Capabilities advertised by the connected client at `initialize`.
    /// Several optional `session/update` variants are capability-gated; every
    /// session shares this view so the gates reflect the live negotiation.
    client_support: ClientSupport,
    /// Provider descriptors advertised through `providers/list`, keyed by
    /// provider id. Empty when the client did not negotiate `providers`.
    providers: ProviderRegistry,
}

impl AcpCodingAgent {
    /// Creates a new `AcpCodingAgent`.
    pub fn new(
        model_factory: impl Fn(PathBuf, &str) -> HarnessAgent + Send + Sync + 'static,
        models: Vec<ModelChoice>,
        modes: Vec<ModeChoice>,
    ) -> Self {
        Self {
            name: AGENT_NAME.to_string(),
            model_factory: Arc::new(model_factory),
            session_factory: None,
            models,
            modes,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            model_builder: None,
            max_sessions: 1,
            client_support: ClientSupport::new(),
            providers: ProviderRegistry::default(),
        }
    }

    /// Overrides the agent display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// 设置会话注册表上限（嵌入式优化，默认 1）。
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max.max(1);
        self
    }

    /// Returns a builder for this agent.
    /// Returns the shared session registry.
    ///
    /// Exposed so host wiring (e.g. the Ctrl-C/SIGTERM handler in the binary
    /// entry point) can observe sessions created *after* the agent started
    /// running and interrupt them, letting every in-flight turn unwind through
    /// the normal path — reconcile + state checkpoint — before the process
    /// exits, instead of being killed mid-turn.
    pub fn session_registry(&self) -> Arc<Mutex<HashMap<String, AcpAgentSession>>> {
        Arc::clone(&self.sessions)
    }

    pub fn builder() -> AcpCodingAgentBuilder {
        AcpCodingAgentBuilder::default()
    }

    /// Builds the ACP agent capabilities advertised during initialization.
    pub fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                PromptCapabilities::new()
                    .image(false)
                    .audio(false)
                    .embedded_context(true),
            )
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true).acp(true))
            .session_capabilities(
                SessionCapabilities::new()
                    .list(SessionListCapabilities::new())
                    .delete(SessionDeleteCapabilities::new())
                    .resume(SessionResumeCapabilities::new())
                    .close(SessionCloseCapabilities::new()),
            )
            .auth(AgentAuthCapabilities::new())
    }

    /// Creates a new session using the session factory.
    fn create_session(&self, session_id: SessionId, cwd: PathBuf) -> AcpAgentSession {
        // The slot is created *before* the agent so a capability-gated `ask_user`
        // tool can be baked into the toolkit at build time — that is what makes
        // subagents inherit it, since the blueprint snapshots the toolkit once.
        let elicitation_slot: ElicitationSlot = Arc::new(Mutex::new(None));
        let agent = match &self.session_factory {
            Some(factory) => factory(SessionBuild {
                cwd: cwd.clone(),
                session_id: session_id.to_string(),
                client_support: self.client_support.clone(),
                elicitation_slot: Arc::clone(&elicitation_slot),
            }),
            None => (self.model_factory)(cwd.clone(), session_id.to_string().as_str()),
        };
        let mut session = AcpAgentSession::new(
            session_id,
            cwd,
            agent,
            self.models.clone(),
            self.modes.clone(),
        );
        session.set_elicitation_slot(elicitation_slot);
        // Clones share the same underlying capability store, so a late
        // `initialize` still gates this session's optional updates correctly.
        session.set_client_support(self.client_support.clone());
        if let Some(builder) = &self.model_builder {
            session.set_model_builder(Arc::clone(builder));
        }
        session
    }

    /// Registers the session in the registry.
    /// 嵌入式优化：超过 `max_sessions` 上限时淘汰最旧会话，
    /// 避免多会话同时持有完整 `HarnessAgent` 成倍放大内存。
    fn register_session(&self, session: AcpAgentSession) {
        let id = session.session_id().to_string();
        let mut guard = self.sessions.lock().unwrap();
        if guard.len() >= self.max_sessions && !guard.contains_key(&id) {
            // 淘汰最旧：session_id 形如 session-<epoch_millis>，取其时间戳最小者。
            let mut oldest: Option<(String, i64)> = None;
            for sid in guard.keys() {
                let t = sid
                    .strip_prefix("session-")
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(i64::MAX);
                match &oldest {
                    Some((_, ot)) if *ot <= t => {}
                    _ => oldest = Some((sid.clone(), t)),
                }
            }
            if let Some((oldest_id, _)) = oldest {
                guard.remove(&oldest_id);
            }
        }
        guard.insert(id, session);
    }

    /// Removes a session from the registry.
    fn remove_session(&self, session_id: &SessionId) {
        self.sessions.lock().unwrap().remove(&session_id.to_string());
    }

    /// Returns a clone of the session (for handlers).
    fn get_session(&self, session_id: &SessionId) -> Option<AcpAgentSession> {
        self.sessions
            .lock()
            .unwrap()
            .get(&session_id.to_string())
            .cloned()
    }

    /// Builds the `session/list` response, honouring the request's `cursor`.
    ///
    /// Sessions are ordered by id (stable, and the id embeds the creation
    /// timestamp) so pagination is deterministic. `next_cursor` is the last id
    /// of a non-final page; the client passes it back to fetch the next page.
    fn session_infos(&self, cursor: Option<&str>) -> ListSessionsResponse {
        const PAGE_SIZE: usize = 50;
        let guard = self.sessions.lock().unwrap();
        let mut entries: Vec<&AcpAgentSession> = guard.values().collect();
        entries.sort_by(|a, b| a.session_id().to_string().cmp(&b.session_id().to_string()));

        // The cursor is the exclusive lower bound (last id of the prior page).
        let start = match cursor {
            Some(c) => entries
                .iter()
                .position(|s| s.session_id().to_string() == c)
                .map(|i| i + 1)
                .unwrap_or(0),
            None => 0,
        };
        let page: Vec<&AcpAgentSession> = entries.into_iter().skip(start).take(PAGE_SIZE).collect();
        let has_more = start + page.len() < guard.len();
        let next_cursor = if has_more {
            page.last().map(|s| s.session_id().to_string())
        } else {
            None
        };
        let infos: Vec<SessionInfo> = page.into_iter().map(|s| s.session_info()).collect();
        ListSessionsResponse::new(infos).next_cursor(next_cursor)
    }

    /// Assembles and runs the ACP agent over stdio until the connection closes.
    pub async fn run_stdio(self) -> agent_client_protocol::Result<()> {
        self.run_with_transport(Stdio::new()).await
    }

    /// Assembles the full ACP agent (all request handlers) and connects it to
    /// the given transport, running until the connection closes.
    ///
    /// This is the transport-agnostic core of [`run_stdio`](Self::run_stdio);
    /// tests can inject an in-process [`Channel`] transport to exercise the
    /// full handler chain without spawning a subprocess.
    ///
    /// [`Channel`]: agent_client_protocol::Channel
    pub async fn run_with_transport(
        self,
        transport: impl agent_client_protocol::ConnectTo<Agent> + 'static,
    ) -> agent_client_protocol::Result<()> {
        let agent = Arc::new(self);
        let a_init = agent.clone();
        let a_new = agent.clone();
        let a_prompt = agent.clone();
        let a_cfg = agent.clone();
        let a_mode = agent.clone();
        let a_close = agent.clone();
        let a_list = agent.clone();
        let a_load = agent.clone();
        let a_resume = agent.clone();
        let a_delete = agent.clone();
        let a_providers = agent.clone();
        let a_providers_set = agent.clone();
        let a_providers_disable = agent.clone();
        Agent
            .builder()
            .on_receive_request(
                async move |initialize: InitializeRequest, responder, _cx| {
                    // Record what the client advertised so capability-gated
                    // session updates (`session/notice`, compaction, plan ops)
                    // can be suppressed when unsupported.
                    a_init
                        .client_support
                        .set_capabilities(initialize.client_capabilities.clone());
                    let mut caps = a_init.capabilities();
                    if !a_init.providers.is_empty() {
                        caps = caps.providers(
                            agent_client_protocol::schema::v1::ProvidersCapabilities::new(),
                        );
                    }
                    let response = InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(caps)
                        .agent_info(Implementation::new(AGENT_NAME, AGENT_VERSION));
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: NewSessionRequest, responder, _cx| {
                    let cwd = req.cwd;
                    // A session id is the journal key: two sessions created in the same
                    // millisecond used to collide on `session-<millis>`, so the second
                    // recorder fenced the first one's writer lease ("Session writer ownership
                    // expired or fenced"). Java allocates a UUID here; do the same and keep the
                    // readable prefix for logs.
                    let session_id = SessionId::new(format!("session-{}", uuid::Uuid::new_v4()));
                    let mut session = a_new.create_session(session_id.clone(), cwd);
                    session.set_additional_directories(req.additional_directories.clone());
                    // Bridge any MCP servers the client declared before serving
                    // the first prompt, so their tools are available immediately.
                    if !req.mcp_servers.is_empty() {
                        session.apply_mcp_servers(&req.mcp_servers).await;
                    }
                    let config_options = session.config_options();
                    let modes = session.session_mode_state();
                    a_new.register_session(session);
                    let response = NewSessionResponse::new(session_id)
                        .modes(modes)
                        .config_options(config_options);
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: PromptRequest, responder, cx| {
                    let sid = req.session_id;
                    let mut session = match a_prompt.get_session(&sid) {
                        Some(s) => s,
                        None => {
                            return responder.respond(PromptResponse::new(StopReason::Refusal));
                        }
                    };
                    // Bind the live client connection so Ask tool calls can push
                    // `session/request_permission` to the user during this prompt.
                    session.set_client(AcpClientHandle::new(cx.clone()));
                    let emit_cx = cx.clone();
                    // The SDK sets this marker when the client sends `$/cancel_request`
                    // for this prompt's JSON-RPC request id; `run_prompt_cancellable`
                    // watches it and returns `StopReason::Cancelled`.
                    let cancellation = responder.cancellation();
                    let registrar = a_prompt.clone();
                    // Run the prompt on a spawned task: a long-running handler would
                    // otherwise block the dispatch loop, so the SDK could never
                    // process the incoming `$/cancel_request` and update the marker.
                    cx.spawn(async move {
                        let stop = session
                            .run_prompt_cancellable(
                                req.prompt,
                                move |notif| {
                                    let _ = emit_cx.send_notification(notif);
                                },
                                Some(cancellation),
                            )
                            .await;
                        // Turn-level token usage must be read before the session
                        // is moved back into the registry.
                        let usage = session.turn_usage_response();
                        registrar.register_session(session);
                        responder.respond(PromptResponse::new(stop).usage(usage))
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: SetSessionConfigOptionRequest, responder, cx| {
                    let sid = req.session_id;
                    let mut session = match a_cfg.get_session(&sid) {
                        Some(s) => s,
                        None => {
                            return responder.respond(SetSessionConfigOptionResponse::new(vec![]));
                        }
                    };
                    session.set_client(AcpClientHandle::new(cx.clone()));
                    let value = req.value.as_value_id().map(|v| v.to_string());
                    match req.config_id.to_string().as_str() {
                        "model" => {
                            if let Some(v) = value {
                                session.set_model(v);
                            }
                        }
                        "thought_level" => {
                            if let Some(v) = value {
                                session.set_thought_level(v);
                            }
                        }
                        "mode" => {
                            if let Some(v) = value {
                                session.set_mode(v);
                            }
                        }
                        _ => {}
                    }
                    let config_options = session.config_options();
                    let update = agent_client_protocol::schema::v1::SessionNotification::new(
                        session.session_id().clone(),
                        agent_client_protocol::schema::v1::SessionUpdate::ConfigOptionUpdate(
                            agent_client_protocol::schema::v1::ConfigOptionUpdate::new(
                                config_options.clone(),
                            ),
                        ),
                    );
                    let _ = cx.send_notification(update);
                    a_cfg.register_session(session);
                    responder.respond(SetSessionConfigOptionResponse::new(config_options))
                },
                agent_client_protocol::on_receive_request!(),
            )            .on_receive_request(
                async move |req: SetSessionModeRequest, responder, cx| {
                    let sid = req.session_id;
                    let mut session = match a_mode.get_session(&sid) {
                        Some(s) => s,
                        None => return responder.respond(SetSessionModeResponse::new()),
                    };
                    // Bind the client connection too: the mode change applies a
                    // new permission context immediately and later Ask tool
                    // calls need the connection to push request_permission.
                    session.set_client(AcpClientHandle::new(cx.clone()));
                    session.set_mode(req.mode_id.to_string());
                    // Reflect the mode change to the client so its UI updates
                    // without waiting for the next prompt turn.
                    let update = agent_client_protocol::schema::v1::SessionNotification::new(
                        session.session_id().clone(),
                        agent_client_protocol::schema::v1::SessionUpdate::CurrentModeUpdate(
                            agent_client_protocol::schema::v1::CurrentModeUpdate::new(
                                req.mode_id.clone(),
                            ),
                        ),
                    );
                    let _ = cx.send_notification(update);
                    a_mode.register_session(session);
                    responder.respond(SetSessionModeResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: CloseSessionRequest, responder, _cx| {
                    let sid = req.session_id;
                    if let Some(session) = a_close.get_session(&sid) {
                        session.cancel();
                    }
                    a_close.remove_session(&sid);
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ListSessionsRequest, responder, _cx| {
                    let infos = a_list.session_infos(req.cursor.as_deref());
                    responder.respond(infos)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest, responder, _cx| {
                    let sid = req.session_id;
                    let session = match a_load.get_session(&sid) {
                        Some(s) => s,
                        None => {
                            let session =
                                a_load.create_session(sid.clone(), req.cwd.clone());
                            a_load.register_session(session);
                            a_load.get_session(&sid).unwrap()
                        }
                    };
                    let mut session = session;
                    session.set_additional_directories(req.additional_directories.clone());
                    // Bridge MCP servers declared on load (idempotent per server).
                    if !req.mcp_servers.is_empty() {
                        session.apply_mcp_servers(&req.mcp_servers).await;
                    }
                    let config_options = session.config_options();
                    let modes = session.session_mode_state();
                    let response = LoadSessionResponse::new()
                        .modes(modes)
                        .config_options(config_options);
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ResumeSessionRequest, responder, _cx| {
                    let sid = req.session_id;
                    let session = match a_resume.get_session(&sid) {
                        Some(s) => s,
                        None => {
                            let session =
                                a_resume.create_session(sid.clone(), req.cwd.clone());
                            a_resume.register_session(session);
                            a_resume.get_session(&sid).unwrap()
                        }
                    };
                    let mut session = session;
                    session.set_additional_directories(req.additional_directories.clone());
                    // Bridge MCP servers declared on resume (idempotent per server).
                    if !req.mcp_servers.is_empty() {
                        session.apply_mcp_servers(&req.mcp_servers).await;
                    }
                    let config_options = session.config_options();
                    let modes = session.session_mode_state();
                    let response = ResumeSessionResponse::new()
                        .modes(modes)
                        .config_options(config_options);
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DeleteSessionRequest, responder, _cx| {
                    a_delete.remove_session(&req.session_id);
                    responder.respond(DeleteSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ListProvidersRequest, responder, _cx| {
                    responder.respond(ListProvidersResponse::new(a_providers.providers.list()))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: SetProviderRequest, responder, _cx| {
                    let ok = a_providers_set.providers.set(
                        &req.provider_id.to_string(),
                        req.api_type,
                        req.base_url,
                        req.headers,
                    );
                    if ok {
                        responder.respond(SetProviderResponse::new())
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data(format!(
                                "unknown provider '{}'",
                                req.provider_id
                            )),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DisableProviderRequest, responder, _cx| {
                    let ok = a_providers_disable.providers.disable(&req.provider_id.to_string());
                    if ok {
                        responder.respond(DisableProviderResponse::new())
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data(format!(
                                "provider '{}' is unknown or required",
                                req.provider_id
                            )),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(transport)
            .await
    }
}

/// Builder for [`AcpCodingAgent`].
#[derive(Default)]
pub struct AcpCodingAgentBuilder {
    name: Option<String>,
    model_factory: Option<Box<dyn Fn(PathBuf, &str) -> HarnessAgent + Send + Sync>>,
    session_factory: Option<Arc<dyn Fn(SessionBuild) -> HarnessAgent + Send + Sync>>,
    models: Vec<ModelChoice>,
    modes: Vec<ModeChoice>,
    max_sessions: Option<usize>,
    model_builder: Option<
        Arc<dyn Fn(&str, &str) -> Result<Arc<dyn Model>, String> + Send + Sync>,
    >,
}

impl AcpCodingAgentBuilder {
    /// Sets the agent display name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the factory used to build a [`HarnessAgent`] per session. The
    /// second argument is the freshly assigned ACP session id, so callers can
    /// derive per-session request headers (e.g. `x-opencode-session`).
    pub fn model_factory(
        mut self,
        factory: impl Fn(PathBuf, &str) -> HarnessAgent + Send + Sync + 'static,
    ) -> Self {
        self.model_factory = Some(Box::new(factory));
        self
    }

    /// Sets the capability-aware factory used to build a [`HarnessAgent`] per
    /// session. Takes precedence over [`model_factory`](Self::model_factory).
    ///
    /// Use this instead of [`model_factory`](Self::model_factory) when the agent
    /// must expose capability-gated tools (e.g. `ask_user`, which requires
    /// `elicitation.form`) that subagents should inherit.
    pub fn session_factory(
        mut self,
        factory: impl Fn(SessionBuild) -> HarnessAgent + Send + Sync + 'static,
    ) -> Self {
        self.session_factory = Some(Arc::new(factory));
        self
    }

    /// Sets the available models advertised to the client.
    pub fn models(mut self, models: Vec<ModelChoice>) -> Self {
        self.models = models;
        self
    }

    /// Sets the available modes advertised to the client.
    pub fn modes(mut self, modes: Vec<ModeChoice>) -> Self {
        self.modes = modes;
        self
    }

    /// 设置会话注册表上限（嵌入式优化，默认 1）。
    pub fn max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = Some(max);
        self
    }

    /// Installs the resolver used for dynamic model switching: builds a live
    /// [`Model`] from a model id when the client selects a different model
    /// (takes effect on the next prompt).
    pub fn model_builder(
        mut self,
        builder: impl Fn(&str, &str) -> Result<Arc<dyn Model>, String> + Send + Sync + 'static,
    ) -> Self {
        self.model_builder = Some(Arc::new(builder));
        self
    }

    /// Builds the [`AcpCodingAgent`].
    pub fn build(self) -> Result<AcpCodingAgent, Box<dyn std::error::Error>> {
        let factory = self
            .model_factory
            .map(|f| -> Box<dyn Fn(PathBuf, &str) -> HarnessAgent + Send + Sync> { f })
            .unwrap_or_else(|| Box::new(default_model_factory));
        let mut agent = AcpCodingAgent::new(
            move |cwd, sid| factory(cwd, sid),
            self.models,
            self.modes,
        );
        agent.session_factory = self.session_factory;
        agent.model_builder = self.model_builder;
        if let Some(name) = self.name {
            agent = agent.with_name(name);
        }
        if let Some(max) = self.max_sessions {
            agent = agent.with_max_sessions(max);
        }
        Ok(agent)
    }
}


