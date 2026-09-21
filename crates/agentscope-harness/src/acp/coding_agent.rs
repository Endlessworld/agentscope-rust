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
    DeleteSessionRequest, DeleteSessionResponse, InitializeRequest, InitializeResponse,
    Implementation, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, PromptCapabilities, ResumeSessionRequest, ResumeSessionResponse,
    SessionCapabilities, SessionCloseCapabilities, SessionDeleteCapabilities, SessionId,
    SessionInfo, SessionListCapabilities, SessionResumeCapabilities, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
};
use agent_client_protocol::{Agent, Stdio};

use agentscope_core::model::Model;
use agentscope_extensions_model::OpenAiChatModel;

use crate::agent::{HarnessAgent, HarnessAgentBuilder};
use crate::CompactionMiddlewareConfig;

use super::config::{ModeChoice, ModelChoice};
use super::session::{AcpAgentSession, AcpClientHandle};

/// Default agent info advertised during initialization.
const AGENT_NAME: &str = "agentscope-coding-agent";
const AGENT_VERSION: &str = "0.1.0";


/// Default model id used when `CODING_MODEL_ID` is not set.
const DEFAULT_MODEL_ID: &str = "gpt-4o";

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
/// [`HarnessAgentBuilder`]'s complete orchestration (workspace, filesystem tools, shell,
/// compaction, plan mode). Mirrors the Java `HarnessCodingAgent`/`CodingAgentFactory`.
fn default_model_factory(cwd: std::path::PathBuf, _session_id: &str) -> HarnessAgent {
    let model = build_env_model();
    let compaction = CompactionMiddlewareConfig {
        enabled: true,
        max_tokens: 16_000,
    };
    let workspace = cwd.join(".agentscope").join("workspace");
    HarnessAgent::builder()
        .name("agentscope-coding-agent")
        .model(model)
        .cwd(cwd)
        .workspace(workspace)
        .register_filesystem_tools(true)
        .register_shell_tool(true)
        .compaction(compaction)
        .max_iters(50)
        .build()
}

/// An ACP CodingAgent with a session registry and a factory for building the
/// underlying [`HarnessAgent`] per session.
pub struct AcpCodingAgent {
    name: String,
    model_factory: Arc<dyn Fn(PathBuf, &str) -> HarnessAgent + Send + Sync>,
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
            models,
            modes,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            model_builder: None,
            max_sessions: 1,
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
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true))
            .session_capabilities(
                SessionCapabilities::new()
                    .list(SessionListCapabilities::new())
                    .delete(SessionDeleteCapabilities::new())
                    .resume(SessionResumeCapabilities::new())
                    .close(SessionCloseCapabilities::new()),
            )
            .auth(AgentAuthCapabilities::new())
    }

    /// Creates a new session using the model factory.
    fn create_session(&self, session_id: SessionId, cwd: PathBuf) -> AcpAgentSession {
        let agent = (self.model_factory)(cwd.clone(), session_id.to_string().as_str());
        let mut session = AcpAgentSession::new(
            session_id,
            cwd,
            agent,
            self.models.clone(),
            self.modes.clone(),
        );
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

    /// Currently registered session ids.
    fn session_ids(&self) -> Vec<SessionId> {
        self.sessions
            .lock()
            .unwrap()
            .keys()
            .map(|k| SessionId::new(k.clone()))
            .collect()
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
        Agent
            .builder()
            .on_receive_request(
                async move |initialize: InitializeRequest, responder, _cx| {
                    let response = InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(a_init.capabilities())
                        .agent_info(Implementation::new(AGENT_NAME, AGENT_VERSION));
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: NewSessionRequest, responder, _cx| {
                    let cwd = req.cwd;
                    let session_id = SessionId::new(format!(
                        "session-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    ));
                    let session = a_new.create_session(session_id.clone(), cwd);
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
                        registrar.register_session(session);
                        responder.respond(PromptResponse::new(stop))
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
                    a_cfg.register_session(session);
                    responder.respond(SetSessionConfigOptionResponse::new(config_options))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
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
                async move |_req: ListSessionsRequest, responder, _cx| {
                    let infos: Vec<SessionInfo> = a_list
                        .session_ids()
                        .into_iter()
                        .map(|id| SessionInfo::new(id, PathBuf::from(".")))
                        .collect();
                    responder.respond(ListSessionsResponse::new(infos))
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
            .connect_to(transport)
            .await
    }
}

/// Builder for [`AcpCodingAgent`].
#[derive(Default)]
pub struct AcpCodingAgentBuilder {
    name: Option<String>,
    model_factory: Option<Box<dyn Fn(PathBuf, &str) -> HarnessAgent + Send + Sync>>,
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


