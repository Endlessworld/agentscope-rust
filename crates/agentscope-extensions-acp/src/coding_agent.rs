//! ACP CodingAgent: an ACP **agent** (server role) that wraps the core
//! [`ReActAgent`] and speaks the Agent Client Protocol over stdio.
//!
//! Mirrors the Kotlin `AgiAgent` / `AgiAgentSession` and Java
//! `HarnessCodingAgent`: handles `initialize`, session lifecycle, prompts
//! (streaming `SessionUpdate` notifications), config options, and cancellation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{
    AgentAuthCapabilities, AgentCapabilities, ArchiveSessionRequest, ArchiveSessionResponse,
    AuthMethod, AuthMethodTerminal, AuthState, AuthStatusCapabilities, AuthStatusRequest,
    AuthStatusResponse, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    ContentBlock, DeleteSessionRequest, DeleteSessionResponse, DisableProviderRequest,
    DisableProviderResponse, Implementation, InitializeRequest, InitializeResponse,
    ListProvidersRequest, ListProvidersResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, McpCapabilities, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionAdditionalDirectoriesCapabilities, SessionArchiveCapabilities,
    SessionArchiveFilter, SessionCapabilities, SessionCloseCapabilities, SessionDeleteCapabilities,
    SessionId, SessionInfo, SessionListCapabilities, SessionResumeCapabilities, SessionStatus,
    SessionStatusCapabilities, SessionStatusRequest, SessionStatusResponse,
    SessionSystemPromptCapabilities, SessionTitleCapabilities, SetProviderRequest,
    SetProviderResponse, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse, SetSessionTitleRequest, SetSessionTitleResponse,
    StopReason, SystemPrompt, SystemPromptMode, TextContent, UnarchiveSessionRequest,
    UnarchiveSessionResponse,
};
use agent_client_protocol::{Agent, Stdio};

use agentscope_core::model::Model;
use agentscope_extensions_model::OpenAiChatModel;

use agentscope_harness::agent::context::TaskContextOptions;
use agentscope_harness::agent::{trigger_for_window, CompactionConfig, HarnessAgent};

use super::config::{ModeChoice, ModelChoice};
use super::elicitation::ElicitationSlot;
use super::provider::ProviderRegistry;
use super::session::{AcpAgentSession, AcpClientHandle};
use super::support::ClientSupport;

/// Default agent info advertised during initialization.
const AGENT_NAME: &str = "agentscope-coding-agent";
const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How many per-`cwd` index agents stay cached. Each one is a full `HarnessAgent`.
const MAX_CATALOG_AGENTS: usize = 4;

/// Default model id used when `CODING_MODEL_ID` is not set.
const DEFAULT_MODEL_ID: &str = "gpt-4o";

/// Default context window the ACP agents compact against.
///
/// Deliberately **independent of the selected model's window**: a model that advertises a
/// 1M-token window (the measured case: `opencode-go/deepseek-v4.1-flash`) would otherwise
/// push the trigger to ~878k tokens, so the conversation would be re-sent for hundreds of
/// thousands of tokens before anything is summarized. Override with
/// `AGENTSCOPE_ACP_COMPACTION_WINDOW` (tokens; `0`/unset keeps the default).
pub const ACP_CONTEXT_WINDOW_TOKENS: usize = 256_000;

/// The window the conversation compactor sizes itself against.
pub fn compaction_window_tokens() -> usize {
    parse_compaction_window(
        std::env::var("AGENTSCOPE_ACP_COMPACTION_WINDOW")
            .ok()
            .as_deref(),
    )
}

/// Parses the `AGENTSCOPE_ACP_COMPACTION_WINDOW` override.
///
/// Anything unusable (unset, non-numeric, `0`) falls back to the default so a typo cannot
/// silently disable compaction.
fn parse_compaction_window(raw: Option<&str>) -> usize {
    raw.map(str::trim)
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(ACP_CONTEXT_WINDOW_TOKENS)
}

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
    let model =
        OpenAiChatModel::new(model_id, api_key, base_url).expect("failed to build OpenAiChatModel");
    Arc::new(model)
}

/// The default per-session factory: builds a fully-wired [`HarnessAgent`] using the
/// [`HarnessAgent::builder`]'s complete orchestration (workspace, filesystem tools, shell,
/// compaction, plan mode). Mirrors the Java `HarnessCodingAgent`/`CodingAgentFactory`.
fn default_model_factory(cwd: std::path::PathBuf, _session_id: &str) -> HarnessAgent {
    let model = build_env_model();
    // The trigger comes from the ACP's own window attribute, never from the model's
    // advertised window (see `ACP_CONTEXT_WINDOW_TOKENS`).
    let trigger = trigger_for_window(compaction_window_tokens());
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
                .trigger_tokens(trigger)
                .build(),
        )
        .max_iters(50)
        // `request/prepared` 默认不记（恢复不依赖它）；`AGENTSCOPE_ACP_RECORD_MODEL_REQUESTS=1` 恢复。
        .record_model_requests(crate::recording::record_model_requests())
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
    /// Text the client asked to **append** to our own system prompt
    /// (`systemPrompt.mode = append`). Already validated: never empty, and only
    /// `append` reaches here — `override` is rejected by the handler.
    pub system_prompt: Option<String>,
    /// Out-of-band channel the harness streams subagent frames into.
    ///
    /// The factory forwards it to the agent builder (`HarnessAgentBuilder::subagent_events`);
    /// [`AcpAgentSession`] keeps the receiving end and starts the forwarder on the first prompt.
    /// `None` for the agents built only to probe a workspace (`session/exists`, `session/list`):
    /// they never spawn a subagent.
    pub subagent_events: Option<agentscope_harness::tool::SubagentEventSink>,
    /// Slot the live client connection is published to (filled on the first prompt).
    ///
    /// Shared with the session and with the subagent permission bridge: a child's
    /// `session/request_permission` must be pushed over the same connection, under the child's
    /// own session id. `None` for workspace-probing agents.
    pub client_slot: Option<Arc<Mutex<Option<Arc<super::session::AcpClientHandle>>>>>,
    /// Persistent permission-rule store of the session being built.
    ///
    /// Shared with the subagent permission bridge so an "always allow" answer given for a child
    /// is persisted by the same store this session reloads from. `None` for probing agents.
    pub permission_store: Option<Arc<super::permission_store::PermissionRuleStore>>,
    /// Every subagent this session's tree spawned, keyed by child session id.
    ///
    /// The root's spawn tool knows only its direct children, so a `session/cancel` addressed to
    /// a grandchild is routed here instead. Shared with the session and the permission bridge
    /// (which is handed every child, nested ones included).
    pub subagent_agents:
        Option<Arc<Mutex<HashMap<String, Arc<dyn agentscope_core::agent::CallableAgent>>>>>,
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
    model_builder: Option<Arc<dyn Fn(&str, &str) -> Result<Arc<dyn Model>, String> + Send + Sync>>,
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
    /// Agents built only to read the durable session index per `cwd`, so
    /// `session/list` does not rebuild one on every call.
    catalog_agents: Mutex<HashMap<PathBuf, Arc<HarnessAgent>>>,
    /// Workspaces this process has seen a session in (created, resumed or listed).
    ///
    /// `session/status`, `session/archive` and `session/unarchive` carry no `cwd`, while the
    /// creation records, archive markers and journals are all per-workspace. Without this set a
    /// session that is no longer in the registry — archived, or created by a previous process —
    /// could not be found at all. Only paths are kept, so the growth is negligible.
    known_workspaces: Mutex<Vec<PathBuf>>,
    /// Advertised `prompt.image` support. See [`ImageSupport`].
    image_support: ImageSupport,
    /// Answers `auth/status` for the connected client.
    ///
    /// The protocol forbids using `auth/status` to *demand* authentication, so the
    /// default answer is `not_required`; a host that owns the credentials (the ACP
    /// binary reads `~/.agentscope/models.json`) supplies the honest state.
    auth_probe: Arc<dyn Fn() -> AuthState + Send + Sync>,
    /// Child session id → the ACP session that spawned it.
    ///
    /// A child session is not a registered session: a client cancels it by sending
    /// `session/cancel` addressed to the child id, which only this map can route to the
    /// session that owns the subagent.
    subagent_children: Arc<Mutex<HashMap<String, String>>>,
}

const IMAGE_SUPPORT_NO: u8 = 1;
const IMAGE_SUPPORT_YES: u8 = 2;

/// Cached answer to 「does any selectable model accept image input」.
///
/// The catalog table that answers it costs ~15 MB, and `initialize` only needs the boolean. The
/// host refreshes the value while the table is resident (startup and after every catalog refresh);
/// `initialize` then reads it without parsing anything.
#[derive(Clone, Default)]
pub struct ImageSupport(Arc<AtomicU8>);

impl ImageSupport {
    /// Stores the answer.
    pub fn set(&self, supported: bool) {
        let value = if supported {
            IMAGE_SUPPORT_YES
        } else {
            IMAGE_SUPPORT_NO
        };
        self.0.store(value, Ordering::Relaxed);
    }

    /// The cached answer, or `None` when it was never computed.
    pub fn get(&self) -> Option<bool> {
        match self.0.load(Ordering::Relaxed) {
            IMAGE_SUPPORT_YES => Some(true),
            IMAGE_SUPPORT_NO => Some(false),
            _ => None,
        }
    }

    /// Recomputes the answer from the catalog for `models` and caches it.
    ///
    /// Cheap while the table is already resident (the normal call sites); when it is not, this
    /// parses it exactly like `initialize` used to.
    pub fn refresh(&self, models: &[ModelChoice]) -> bool {
        let supported = super::config::image_support(models);
        self.set(supported);
        supported
    }
}

/// Picks which cached catalog agent to drop when the map is full: any entry other than the one
/// being inserted. Insertion order carries no meaning here — the bound is what matters.
fn catalog_eviction<'a>(
    keys: impl Iterator<Item = &'a PathBuf>,
    cwd: &std::path::Path,
) -> Option<&'a PathBuf> {
    keys.into_iter().find(|key| key.as_path() != cwd)
}

/// Hands the parsed model catalog back to the OS once no live session still needs it.
///
/// The catalog table is ~15 MB for ~9k models and is only needed to answer *which* models exist:
/// `initialize`, `session/new`, `session/load`/`resume` and a model switch. Once every registered
/// session holds the descriptor of its selected model, the per-turn path never touches the table
/// again, so this drops it (the JSON file stays on disk) and `ModelCatalog::ensure_loaded` re-parses
/// it on the next listing. Skipped while any session has no descriptor yet, because releasing then
/// would force a re-parse before that session can serve its first prompt.
pub fn release_catalog_if_unused(sessions: &Mutex<HashMap<String, AcpAgentSession>>) {
    let bound = {
        let guard = sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.values().all(AcpAgentSession::has_cached_descriptor)
    };
    if !bound {
        return;
    }
    if agentscope_core::model::catalog::ModelCatalog::global().release_snapshot() {
        tracing::debug!("model catalog released: every session has bound its model");
        // Freed blocks are still committed heap pages until the idle trim compacts them.
        crate::mem::schedule_idle_trim();
    }
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
            catalog_agents: Mutex::new(HashMap::new()),
            known_workspaces: Mutex::new(Vec::new()),
            image_support: ImageSupport::default(),
            auth_probe: Arc::new(|| AuthState::NotRequired),
            subagent_children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Overrides how `auth/status` answers.
    pub fn with_auth_probe(
        mut self,
        probe: impl Fn() -> AuthState + Send + Sync + 'static,
    ) -> Self {
        self.auth_probe = Arc::new(probe);
        self
    }

    /// The current authentication state reported by `auth/status`.
    pub fn auth_state(&self) -> AuthState {
        (self.auth_probe)()
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

    /// Handle to the cached `prompt.image` answer advertised by [`capabilities`](Self::capabilities).
    ///
    /// Hosts refresh it while the catalog table is resident (startup, every refresh) so
    /// `initialize` never parses the table for one boolean.
    pub fn image_support(&self) -> ImageSupport {
        self.image_support.clone()
    }

    /// Recomputes the advertised `prompt.image` answer from the catalog.
    ///
    /// Cheap while the table is resident; when it is not, this parses it once (the behaviour
    /// `initialize` had before the value was cached).
    pub fn refresh_image_support(&self) -> bool {
        self.image_support.refresh(&self.models)
    }

    pub fn builder() -> AcpCodingAgentBuilder {
        AcpCodingAgentBuilder::default()
    }

    /// Builds the ACP agent capabilities advertised during initialization.
    pub fn capabilities(&self) -> AgentCapabilities {
        // Read the cached answer: parsing the model table here costs ~15 MB, and this runs on
        // every `initialize` while only a boolean is needed. A host that never refreshed the
        // cache pays the parse once, exactly like before.
        let image = self
            .image_support
            .get()
            .unwrap_or_else(|| self.refresh_image_support());
        let mut session_capabilities = SessionCapabilities::new()
            .list(SessionListCapabilities::new())
            .delete(SessionDeleteCapabilities::new())
            .resume(SessionResumeCapabilities::new())
            .close(SessionCloseCapabilities::new())
            // The handler already honoured `additionalDirectories`; without this
            // advertisement a compliant client never sends them (ACP-37).
            .additional_directories(SessionAdditionalDirectoriesCapabilities::new())
            // Clients may retitle a session (ACP-33) and probe whether we still hold
            // it in memory (ACP-34).
            .title(SessionTitleCapabilities::new())
            .status(SessionStatusCapabilities::new())
            // `session/archive` / `session/unarchive` + the `session/list` filter (ACP-36).
            .archive(SessionArchiveCapabilities::new());
        // Only `append` mode is implemented; advertising `{}` is exactly the spec's way of
        // saying "append supported" (ACP-38). It is advertised only when a `session_factory`
        // can carry the text into the agent -- `model_factory` has no channel for it, so
        // advertising there would promise something we cannot honour (and we reject the
        // request instead of dropping it silently).
        if self.session_factory.is_some() {
            session_capabilities =
                session_capabilities.system_prompt(SessionSystemPromptCapabilities::new());
        }
        AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                PromptCapabilities::new()
                    .image(image)
                    .audio(false)
                    .embedded_context(true),
            )
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true).acp(true))
            .session_capabilities(session_capabilities)
            // We never require a sign-in, so `auth/status` answers `not_required` (ACP-35).
            .auth(AgentAuthCapabilities::new().status(AuthStatusCapabilities::new()))
    }

    /// Creates a new session using the session factory.
    fn create_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        system_prompt: Option<String>,
    ) -> AcpAgentSession {
        // The slot is created *before* the agent so a capability-gated `ask_user`
        // tool can be baked into the toolkit at build time — that is what makes
        // subagents inherit it, since the blueprint snapshots the toolkit once.
        let elicitation_slot: ElicitationSlot = Arc::new(Mutex::new(None));
        // Subagent progress: the harness streams `(child session, event)` frames, the session
        // forwards them to the client. Only a real session gets the channel -- the agents built
        // to probe a workspace never run anything.
        let (subagent_events, subagent_frames) = tokio::sync::mpsc::unbounded_channel();
        // The subagent permission bridge is wired into the agent builder, so the connection slot
        // and the rule store must exist *before* the agent: a spawned child asks for its
        // permission under its own session id, over this connection, and persists "always"
        // answers where this session reloads them from.
        let client_slot: Arc<Mutex<Option<Arc<super::session::AcpClientHandle>>>> =
            Arc::new(Mutex::new(None));
        let permission_store = Arc::new(super::permission_store::PermissionRuleStore::at_dir(&cwd));
        // Every child of this session's subagent tree, so a cancel naming a nested child still
        // reaches its agent (the root tool only owns its direct children).
        let subagent_agents: Arc<
            Mutex<HashMap<String, Arc<dyn agentscope_core::agent::CallableAgent>>>,
        > = Arc::new(Mutex::new(HashMap::new()));
        let agent = match &self.session_factory {
            Some(factory) => factory(SessionBuild {
                cwd: cwd.clone(),
                session_id: session_id.to_string(),
                client_support: self.client_support.clone(),
                elicitation_slot: Arc::clone(&elicitation_slot),
                system_prompt,
                subagent_events: Some(subagent_events),
                client_slot: Some(Arc::clone(&client_slot)),
                permission_store: Some(Arc::clone(&permission_store)),
                subagent_agents: Some(Arc::clone(&subagent_agents)),
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
        session.set_client_slot(client_slot);
        session.set_permission_store(permission_store);
        session.set_subagent_events(subagent_frames);
        session.set_subagent_children(Arc::clone(&self.subagent_children));
        session.set_subagent_agents(subagent_agents);
        // Clones share the same underlying capability store, so a late
        // `initialize` still gates this session's optional updates correctly.
        session.set_client_support(self.client_support.clone());
        if let Some(builder) = &self.model_builder {
            session.set_model_builder(Arc::clone(builder));
        }
        session
    }

    /// Validates a client-supplied `systemPrompt` for a session lifecycle request (ACP-38).
    ///
    /// Beyond the mode/content rules of [`system_prompt_text`], a `model_factory`-built agent
    /// has no channel for the text: reject the request instead of accepting it and dropping it
    /// (that configuration does not advertise the capability either).
    fn client_system_prompt(
        &self,
        prompt: Option<SystemPrompt>,
    ) -> Result<Option<String>, agent_client_protocol::Error> {
        let text = system_prompt_text(prompt)?;
        if text.is_some() && self.session_factory.is_none() {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("systemPrompt requires a session factory"));
        }
        Ok(text)
    }

    /// Registers the session in the registry.
    /// 嵌入式优化：超过 `max_sessions` 上限时淘汰最旧会话，
    /// 避免多会话同时持有完整 `HarnessAgent` 成倍放大内存。
    fn register_session(&self, session: AcpAgentSession) {
        let id = session.session_id().to_string();
        self.note_workspace(session.cwd());
        // A prompt that finishes after `session/close` must not resurrect the session.
        if session.is_closed() {
            self.sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return;
        }
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() >= self.max_sessions && !guard.contains_key(&id) {
            // 淘汰最久未活动的会话：`updated_at` 是提示轮次写入的 RFC-3339 时间戳，
            // 字典序即时间序；新建而未提示过的会话排在最前（最旧）。会话 id 形如
            // `session-<uuid>`，无法解析出时间戳，绝不能按 id 猜旧。
            //
            // 优先淘汰空闲会话：运行中的会话仍被其 prompt handler 使用，淘汰后客户端
            // 对它的后续请求（例如 `session/cancel`）会变成 "unknown session"。
            // 当所有会话都在运行时容量上限仍然优先（否则无法接纳新会话）；正在执行的
            // 提示持有自己的 clone，不会因此中断，只是后续定向请求可能落空。
            let mut oldest: Option<(String, String, bool)> = None;
            for (sid, candidate) in guard.iter() {
                let idle = !candidate.is_running();
                let key = (sid.clone(), candidate.updated_at_sort_key(), idle);
                let replace = match &oldest {
                    None => true,
                    Some((_, ot, oidle)) => {
                        // Prefer an idle session over a running one, then the least recently
                        // used.
                        (idle && !*oidle) || (idle == *oidle && key.1 < *ot)
                    }
                };
                if replace {
                    oldest = Some(key);
                }
            }
            if let Some((oldest_id, _, _)) = oldest {
                guard.remove(&oldest_id);
            }
        }
        guard.insert(id, session);
    }

    /// Removes a session from the registry.
    fn remove_session(&self, session_id: &SessionId) {
        self.sessions
            .lock()
            .unwrap()
            .remove(&session_id.to_string());
    }

    /// Records that this process has seen sessions in `cwd`.
    fn note_workspace(&self, cwd: &std::path::Path) {
        let mut guard = self
            .known_workspaces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !guard.iter().any(|known| known == cwd) {
            guard.push(cwd.to_path_buf());
        }
    }

    /// Removes every durable trace of `session_id` under `cwd`: the journal, the creation
    /// record and the archive marker.
    ///
    /// A journal survives the protocol call only when the backend refuses to delete it; that is
    /// logged rather than failing a `session/delete` whose other effects already landed.
    async fn forget_session(
        &self,
        agent: &HarnessAgent,
        cwd: &std::path::Path,
        session_id: &SessionId,
        context: &agentscope_core::agent::RuntimeContext,
    ) {
        if let Err(error) = agent.delete_session(context).await {
            tracing::warn!(session = %session_id, %error, "could not delete the session journal");
        }
        super::session::forget_session_on_disk(cwd, session_id);
    }

    /// Returns a clone of the session (for handlers).
    fn get_session(&self, session_id: &SessionId) -> Option<AcpAgentSession> {
        self.sessions
            .lock()
            .unwrap()
            .get(&session_id.to_string())
            .cloned()
    }

    /// The ACP session that owns `child`, for a `session/cancel` addressed to a subagent.
    fn subagent_parent(&self, child: &SessionId) -> Option<SessionId> {
        self.subagent_children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&child.to_string())
            .map(|parent| SessionId::new(parent.clone()))
    }

    /// Releases every session once the connection is gone (ACP-42).
    ///
    /// A prompt turn runs on a spawned task, so a client that disconnects mid-turn would
    /// otherwise leave the agent reasoning with nowhere to send the result. Closing each
    /// session cancels the running turn (the same path `session/close` takes) and drops the
    /// `HarnessAgent`; the cached `session/list` agents go too. No checkpoint write is needed
    /// here: the harness commits the journal per turn, so the last completed step is already
    /// durable.
    async fn shutdown_sessions(&self) {
        let sessions: Vec<AcpAgentSession> = {
            let mut guard = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.drain().map(|(_, session)| session).collect()
        };
        // Released concurrently: the per-session wait is bounded, and serialising N sessions
        // would multiply that bound by N.
        let releases = sessions.into_iter().map(|session| async move {
            session.close();
            // Bounded: a wedged turn must not keep the process alive after the client left.
            let _ = session
                .cancel_and_wait(std::time::Duration::from_secs(5))
                .await;
        });
        futures::future::join_all(releases).await;
        self.catalog_agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Whether a persisted journal exists for `session_id`.
    ///
    /// `session/load` and `session/resume` must not silently create an empty session for a
    /// mistyped or deleted id: the client would see a successful load with no history. The
    /// workspace is the durable authority, so probe it before building anything: the creation
    /// record covers a session that never ran a turn, the journal one that did.
    async fn session_exists(&self, session_id: &SessionId, cwd: PathBuf) -> bool {
        // A subagent's session journals like a user session, but it is not one: loading it
        // would open a child transcript as a top-level conversation.
        if is_subagent_session(session_id) {
            return false;
        }
        // A session created but never prompted has no journal yet: the creation record the
        // workspace wrote on `session/new` is the durable proof that the id is not a typo.
        if super::session::is_recorded_on_disk(&cwd, session_id) {
            return true;
        }
        let agent = match &self.session_factory {
            Some(factory) => factory(SessionBuild {
                cwd: cwd.clone(),
                session_id: session_id.to_string(),
                client_support: self.client_support.clone(),
                elicitation_slot: Arc::new(Mutex::new(None)),
                system_prompt: None,
                subagent_events: None,
                client_slot: None,
                permission_store: None,
                subagent_agents: None,
            }),
            None => (self.model_factory)(cwd.clone(), session_id.to_string().as_str()),
        };
        let context =
            agentscope_core::agent::RuntimeContext::new().with_session_id(session_id.to_string());
        match agent.session_log(&context).await {
            Ok(log) => match log.head().await {
                Ok(head) => head.seq > 0 || head.commit_id.is_some(),
                Err(_) => false,
            },
            Err(_) => false,
        }
    }

    /// Resolves `session/status` for `session_id` (ACP-34).
    ///
    /// `open` when this process holds the session, `saved` when only its workspace knows it, and
    /// `None` (→ `session_not_found`) otherwise.
    async fn session_status(&self, session_id: &SessionId) -> Option<SessionStatus> {
        if self.get_session(session_id).is_some() {
            return Some(SessionStatus::Open);
        }
        // A live subagent runs in a background task this process owns, so it is an open session
        // even though it never enters the top-level registry: a client polling the card it is
        // rendering learns that its subagent is still working. A finished child stays unknown
        // (its transcript belongs to the parent's session, not to a conversation of its own).
        if self
            .subagent_children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&session_id.to_string())
        {
            return Some(SessionStatus::Open);
        }
        self.session_workspace(session_id)
            .await
            .map(|_| SessionStatus::Saved)
    }

    /// Finds the workspace that knows `session_id` when the registry does not hold it.
    ///
    /// The journal store, the creation records and the archive markers are per-workspace, while
    /// `session/status` and `session/archive` carry no `cwd`. So the persisted probe only covers
    /// workspaces this process has already seen: the live registry plus every workspace a
    /// `session/new`, `session/load`, `session/resume` or `session/list` touched. A session
    /// persisted somewhere never seen here reads as unknown, which is what a client that never
    /// named that directory should conclude anyway.
    async fn session_workspace(&self, session_id: &SessionId) -> Option<PathBuf> {
        // Subagent sessions live in the same journal as their parent but are not user sessions:
        // `session/status` on one must read as unknown, not as a loadable session.
        if is_subagent_session(session_id) {
            return None;
        }
        let mut cwds: Vec<PathBuf> = {
            let guard = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .values()
                .map(|session| session.cwd().clone())
                .collect()
        };
        cwds.extend(
            self.known_workspaces
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .cloned(),
        );
        cwds.sort();
        cwds.dedup();
        let wanted = session_id.to_string();
        let context = agentscope_core::agent::RuntimeContext::new();
        for cwd in cwds {
            // A creation record or an archive marker without a journal entry still names a
            // session this workspace owns: the record is written on `session/new`, before the
            // first prompt, and it outlives the marker an unarchive clears.
            if super::session::is_recorded_on_disk(&cwd, session_id)
                || super::session::is_archived_on_disk(&cwd, session_id)
            {
                return Some(cwd);
            }
            let agent = self.catalog_agent(&cwd);
            match agent.list_session_keys(&context).await {
                Ok(keys) => {
                    if keys.iter().any(|key| key.session_id == wanted) {
                        return Some(cwd);
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not read the persisted session index")
                }
            }
        }
        None
    }

    /// Archives or unarchives a session (ACP-36).
    ///
    /// A live session is released exactly like `session/close`: an archived session must not
    /// keep holding an agent while being invisible to the default `session/list`. A session that
    /// only exists on disk is marked in its workspace's marker file.
    async fn archive_session(
        &self,
        session_id: &SessionId,
        archived: bool,
    ) -> Result<(), agent_client_protocol::Error> {
        if let Some(mut session) = self.get_session(session_id) {
            if session.is_archived() == archived {
                // Idempotent: no state change, and no reason to release a live session.
                return Ok(());
            }
            session.set_archived(archived);
            if archived {
                session.close();
                let _ = session
                    .cancel_and_wait(std::time::Duration::from_secs(15))
                    .await;
                self.remove_session(session_id);
            } else {
                self.register_session(session);
            }
            return Ok(());
        }
        match self.session_workspace(session_id).await {
            Some(cwd) => {
                super::session::set_archived_on_disk(&cwd, session_id, archived);
                Ok(())
            }
            None => Err(agent_client_protocol::v1::schema::Error::session_not_found(
                session_id.clone(),
            )
            .into()),
        }
    }

    /// Builds the `session/list` response, honouring the request's `cursor`.
    ///
    /// The in-memory registry only knows sessions this process created. The workspace is the
    /// durable authority, so when the client names a `cwd` the list also includes sessions a
    /// previous process left there — otherwise a restart showed an empty history and the client
    /// could not offer `session/load`. Two sources are merged: the journal (sessions that ran at
    /// least one turn) and the creation records (sessions created but never prompted, which have
    /// no journal yet).
    ///
    /// Sessions are ordered by id (stable) so pagination is deterministic. The archive filter
    /// is applied before pagination, as the spec requires (ACP-36).
    async fn session_infos(
        &self,
        cwd: Option<&std::path::Path>,
        cursor: Option<&str>,
        filter: SessionArchiveFilter,
    ) -> ListSessionsResponse {
        const PAGE_SIZE: usize = 50;
        let mut infos: Vec<SessionInfo> = {
            let guard = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .values()
                .filter(|session| cwd.is_none_or(|cwd| session.cwd() == cwd))
                .map(AcpAgentSession::session_info)
                .collect()
        };
        let known: std::collections::HashSet<String> = infos
            .iter()
            .map(|info| info.session_id.to_string())
            .collect();
        if let Some(cwd) = cwd {
            let agent = self.catalog_agent(cwd);
            let context = agentscope_core::agent::RuntimeContext::new();
            // Journal first, then the creation records: a session that never prompted has no
            // journal at all, so the records are the only source that knows it exists.
            let mut discovered: std::collections::BTreeSet<String> =
                super::session::recorded_session_ids(cwd)
                    .into_iter()
                    .collect();
            match agent.list_session_keys(&context).await {
                Ok(keys) => {
                    discovered.extend(keys.into_iter().map(|key| key.session_id));
                }
                Err(error) => {
                    tracing::warn!(%error, "could not read the persisted session index")
                }
            }
            for id in discovered {
                if known.contains(&id) {
                    continue;
                }
                let session_id = SessionId::new(id);
                // Persisted-only sessions report their archive state from the marker file, so
                // `exclude` cannot leak an archived session that is on disk.
                let archived = super::session::is_archived_on_disk(cwd, &session_id);
                infos.push(SessionInfo::new(session_id, cwd.to_path_buf()).archived(archived));
            }
        }
        // Drop duplicates by id *before* sorting: the two orders disagree, so `dedup_by`
        // after the sort would leave a persisted duplicate of an in-memory session in place.
        let mut seen_ids = std::collections::HashSet::new();
        infos.retain(|info| seen_ids.insert(info.session_id.to_string()));
        // Subagent sessions (`sub-*`) journal like any other agent, but they are not user
        // sessions: a client must never offer one for load / resume / delete.
        infos.retain(|info| !is_subagent_session(&info.session_id));
        // Archive filter first: the spec applies it before pagination and combines it with cwd.
        // `Other(_)` is rejected by the handler, so here it falls through as "no filtering".
        match filter {
            SessionArchiveFilter::Exclude => infos.retain(|info| info.archived != Some(true)),
            SessionArchiveFilter::Only => infos.retain(|info| info.archived == Some(true)),
            _ => {}
        }
        sort_session_infos(&mut infos);

        // The cursor is the exclusive lower bound (last id of the prior page).
        let start = match cursor {
            Some(c) => infos
                .iter()
                .position(|info| info.session_id.to_string() == c)
                .map(|i| i + 1)
                .unwrap_or(0),
            None => 0,
        };
        let total = infos.len();
        let page: Vec<SessionInfo> = infos.into_iter().skip(start).take(PAGE_SIZE).collect();
        let has_more = start + page.len() < total;
        let next_cursor = if has_more {
            page.last().map(|info| info.session_id.to_string())
        } else {
            None
        };
        ListSessionsResponse::new(page).next_cursor(next_cursor)
    }

    /// Builds (and caches) the agent used only to read the durable session index for `cwd`.
    ///
    /// `HarnessAgent::list_session_keys` reaches the session store through a built agent, so a
    /// process that has not created a session yet still needs one to enumerate history. The
    /// synthetic session id never reaches the journal: only the store's index is read.
    fn catalog_agent(&self, cwd: &std::path::Path) -> Arc<HarnessAgent> {
        const CATALOG_SESSION_ID: &str = "session-catalog";
        self.note_workspace(cwd);
        if let Some(agent) = self
            .catalog_agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(cwd)
        {
            return Arc::clone(agent);
        }
        let agent = Arc::new(match &self.session_factory {
            Some(factory) => factory(SessionBuild {
                cwd: cwd.to_path_buf(),
                session_id: CATALOG_SESSION_ID.to_string(),
                client_support: self.client_support.clone(),
                elicitation_slot: Arc::new(Mutex::new(None)),
                system_prompt: None,
                subagent_events: None,
                client_slot: None,
                permission_store: None,
                subagent_agents: None,
            }),
            None => (self.model_factory)(cwd.to_path_buf(), CATALOG_SESSION_ID),
        });
        // Each cached catalog agent is a full `HarnessAgent`, so the map is bounded: a client
        // that lists sessions in many directories must not accumulate one agent per directory.
        let mut guard = self
            .catalog_agents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() >= MAX_CATALOG_AGENTS {
            if let Some(stale) = catalog_eviction(guard.keys(), cwd).cloned() {
                guard.remove(&stale);
            }
        }
        guard.insert(cwd.to_path_buf(), Arc::clone(&agent));
        agent
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
        // Start the idle-wakeup loop before serving requests: a session only exists after
        // `session/new`, and the loop picks it up from the registry on its next tick.
        let wakeup_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wakeup = spawn_wakeup_loop(Arc::clone(&agent), Arc::clone(&wakeup_stop));
        let a_init = agent.clone();
        let a_new = agent.clone();
        let a_prompt = agent.clone();
        let a_cancel = agent.clone();
        let a_cfg = agent.clone();
        let a_mode = agent.clone();
        let a_close = agent.clone();
        let a_list = agent.clone();
        let a_load = agent.clone();
        let a_resume = agent.clone();
        let a_delete = agent.clone();
        let a_title = agent.clone();
        let a_status = agent.clone();
        let a_auth = agent.clone();
        let a_archive = agent.clone();
        let a_unarchive = agent.clone();
        let a_respond = agent.clone();
        let a_providers = agent.clone();
        let a_providers_set = agent.clone();
        let a_providers_disable = agent.clone();
        let result = Agent
            .v1()
            .on_receive_request(
                async move |initialize: InitializeRequest, responder, _cx| {
                    // Record what the client advertised so capability-gated
                    // session updates (`session/notice`, compaction, plan ops)
                    // can be suppressed when unsupported.
                    a_init
                        .client_support
                        .set_capabilities(initialize.client_capabilities.clone());
                    // The client identity drives option rendering (e.g. IntelliJ
                    // needs a flat model list instead of grouped options).
                    a_init
                        .client_support
                        .set_client_info(initialize.client_info.clone());
                    let mut caps = a_init.capabilities();
                    if !a_init.providers.is_empty() {
                        caps = caps.providers(
                            agent_client_protocol::v1::schema::ProvidersCapabilities::new(),
                        );
                    }
                    // Always advertised, not gated on `clientCapabilities.auth.terminal`:
                    // the registry auth check sends `initialize` with empty capabilities and
                    // requires a non-empty list, and a client that cannot open a terminal
                    // simply does not offer the method to the user.
                    let auth_methods = vec![AuthMethod::Terminal(
                        AuthMethodTerminal::new("setup", "Configure a model provider")
                            .description(
                                "Runs `acp_harness_agent --setup` in a terminal: pick a model \
                                 provider and paste its API key. Providers whose key is already \
                                 exported need no answers at all.",
                            )
                            .args(vec!["--setup".to_string()]),
                    )];
                    let response = InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(caps)
                        .agent_info(Implementation::new(AGENT_NAME, AGENT_VERSION))
                        .auth_methods(auth_methods);
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: NewSessionRequest, responder, _cx| {
                    let cwd = req.cwd;
                    let system_prompt = match a_new.client_system_prompt(req.system_prompt) {
                        Ok(prompt) => prompt,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    // A session id is the journal key: two sessions created in the same
                    // millisecond used to collide on `session-<millis>`, so the second
                    // recorder fenced the first one's writer lease ("Session writer ownership
                    // expired or fenced"). Java allocates a UUID here; do the same and keep the
                    // readable prefix for logs.
                    let session_id = SessionId::new(format!("session-{}", uuid::Uuid::new_v4()));
                    let mut session = a_new.create_session(session_id.clone(), cwd, system_prompt);
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
                    // The model list is out; drop the catalog table unless a session still needs it.
                    release_catalog_if_unused(&a_new.sessions);
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
                        // A panic inside the agent loop must not leave the client waiting for a
                        // response that will never come: the SDK sends nothing when a responder
                        // is dropped, so the prompt would hang until the client gave up.
                        let outcome = futures::FutureExt::catch_unwind(
                            std::panic::AssertUnwindSafe(session.run_prompt_cancellable(
                                req.prompt,
                                move |notif| {
                                    let _ = emit_cx.send_notification(notif);
                                },
                                Some(cancellation),
                            )),
                        )
                        .await;
                        // Turn-level token usage must be read before the session
                        // is moved back into the registry.
                        let usage = session.turn_usage_response();
                        registrar.register_session(session);
                        match outcome {
                            Ok(stop) => responder.respond(PromptResponse::new(stop).usage(usage)),
                            Err(panic) => {
                                let detail = panic
                                    .downcast_ref::<&str>()
                                    .map(|message| message.to_string())
                                    .or_else(|| panic.downcast_ref::<String>().cloned())
                                    .unwrap_or_else(|| "unknown panic".to_string());
                                tracing::error!(%detail, "prompt panicked; reporting to the client");
                                responder.respond_with_error(
                                    agent_client_protocol::Error::internal_error()
                                        .data(format!("prompt panicked: {detail}")),
                                )
                            }
                        }
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            // ACP clients cancel an in-flight turn with `session/cancel`. Without this handler
            // "Stop" was a no-op: the run kept streaming (and billing) while the client had
            // already abandoned the prompt. The prompt loop watches this flag and returns
            // `StopReason::Cancelled`.
            .on_receive_notification(
                async move |notif: CancelNotification, _cx| {
                    if let Some(session) = a_cancel.get_session(&notif.session_id) {
                        session.cancel();
                    } else if let Some(parent) = a_cancel.subagent_parent(&notif.session_id) {
                        // A child session is not a registered session: the client cancels it by
                        // naming the child, and the parent owns the agent that must be stopped.
                        if let Some(session) = a_cancel.get_session(&parent) {
                            session.cancel_subagent(&notif.session_id);
                        }
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: SetSessionConfigOptionRequest, responder, cx| {
                    let sid = req.session_id;
                    let mut session = match a_cfg.get_session(&sid) {
                        Some(s) => s,
                        None => {
                            return responder.respond_with_error(
                                agent_client_protocol::v1::schema::Error::session_not_found(
                                    sid.clone(),
                                )
                                .into(),
                            );
                        }
                    };
                    session.set_client(AcpClientHandle::new(cx.clone()));
                    let Some(value) = req.value.as_value_id().map(|v| v.to_string()) else {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data("config option requires a value id"),
                        );
                    };
                    match req.config_id.to_string().as_str() {
                        "model" => {
                            if !session.has_model(&value) {
                                return responder.respond_with_error(
                                    agent_client_protocol::Error::invalid_params()
                                        .data(format!("unknown model '{value}'")),
                                );
                            }
                            session.set_model(value);
                            // The live agent keeps the previous model until the
                            // next prompt, so refresh the context-window gauge
                            // from the newly selected model's descriptor now.
                            session.push_usage_for_selected_model();
                        }
                        "thought_level" => {
                            if !session.has_thought_level(&value) {
                                return responder.respond_with_error(
                                    agent_client_protocol::Error::invalid_params().data(format!(
                                        "thought level '{value}' is not supported by the selected model"
                                    )),
                                );
                            }
                            session.set_thought_level(value);
                        }
                        "mode" => {
                            session.set_mode(value);
                        }
                        other => {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data(format!("unknown config option '{other}'")),
                            );
                        }
                    }
                    let config_options = session.config_options();
                    let update = agent_client_protocol::v1::schema::SessionNotification::new(
                        session.session_id().clone(),
                        agent_client_protocol::v1::schema::SessionUpdate::ConfigOptionUpdate(
                            agent_client_protocol::v1::schema::ConfigOptionUpdate::new(
                                config_options.clone(),
                            ),
                        ),
                    );
                    let _ = cx.send_notification(update);
                    a_cfg.register_session(session);
                    // A model switch re-parsed the catalog; hand it back right away.
                    release_catalog_if_unused(&a_cfg.sessions);
                    responder.respond(SetSessionConfigOptionResponse::new(config_options))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: crate::ext::ExtRespondRequest, responder, _cx| {
                    handle_ext_respond(a_respond.clone(), req, responder).await
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
                    // Reflect the mode change to the client so its UI updates
                    // without waiting for the next prompt turn.
                    let update = agent_client_protocol::v1::schema::SessionNotification::new(
                        session.session_id().clone(),
                        agent_client_protocol::v1::schema::SessionUpdate::CurrentModeUpdate(
                            agent_client_protocol::v1::schema::CurrentModeUpdate::new(
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
                        // Mark the session closed *first*: a prompt that finishes later must
                        // not re-insert it into the registry, and a turn stuck on a provider
                        // that never answers must stop at once instead of waiting out the
                        // settle grace (the closed flag short-circuits that wait).
                        session.close();
                        // Then let the cancelled run release the journal before the session
                        // leaves the registry.
                        let _ = session
                            .cancel_and_wait(std::time::Duration::from_secs(15))
                            .await;
                    }
                    a_close.remove_session(&sid);
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ListSessionsRequest, responder, _cx| {
                    // An unknown filter value is reserved for a future protocol variant: reject it
                    // rather than silently listing the wrong set (ACP-36).
                    if matches!(req.archived, Some(SessionArchiveFilter::Other(_))) {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data("unsupported session archive filter"),
                        );
                    }
                    let infos = a_list
                        .session_infos(
                            req.cwd.as_deref(),
                            req.cursor.as_deref(),
                            req.archived.unwrap_or_default(),
                        )
                        .await;
                    responder.respond(infos)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ArchiveSessionRequest, responder, _cx| {
                    match a_archive.archive_session(&req.session_id, true).await {
                        Ok(()) => responder.respond(ArchiveSessionResponse::new()),
                        Err(error) => responder.respond_with_error(error),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: UnarchiveSessionRequest, responder, _cx| {
                    match a_unarchive.archive_session(&req.session_id, false).await {
                        Ok(()) => responder.respond(UnarchiveSessionResponse::new()),
                        Err(error) => responder.respond_with_error(error),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest, responder, cx| {
                    let sid = req.session_id;
                    let system_prompt = match a_load.client_system_prompt(req.system_prompt) {
                        Ok(prompt) => prompt,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let session = match a_load.get_session(&sid) {
                        Some(s) => {
                            // A live session already owns its agent; the prompt cannot be
                            // layered on without rebuilding it (which would lose the turn).
                            if system_prompt.is_some() {
                                tracing::warn!(
                                    session = %sid,
                                    "systemPrompt ignored: the session is already live"
                                );
                            }
                            s
                        }
                        None => {
                            if !a_load.session_exists(&sid, req.cwd.clone()).await {
                                return responder.respond_with_error(
                                    agent_client_protocol::v1::schema::Error::session_not_found(
                                        sid.clone(),
                                    )
                                    .into(),
                                );
                            }
                            let session = a_load.create_session(
                                sid.clone(),
                                req.cwd.clone(),
                                system_prompt.clone(),
                            );
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
                    // ACP v1 `loadSession` requires the agent to replay the whole conversation
                    // before answering: the client has no state for this session, so it renders
                    // exactly the `session/update`s that arrive. Without this the IDE showed an
                    // empty transcript while the agent still answered with the full context.
                    for notification in session.replay_notifications().await {
                        let _ = cx.send_notification(notification);
                    }
                    let response = LoadSessionResponse::new()
                        .modes(modes)
                        .config_options(config_options);
                    release_catalog_if_unused(&a_load.sessions);
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ResumeSessionRequest, responder, _cx| {
                    let sid = req.session_id;
                    let system_prompt = match a_resume.client_system_prompt(req.system_prompt) {
                        Ok(prompt) => prompt,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    let session = match a_resume.get_session(&sid) {
                        Some(s) => {
                            // Same as `session/load`: a live session keeps the prompt it was
                            // built with, so a new one cannot be applied.
                            if system_prompt.is_some() {
                                tracing::warn!(
                                    session = %sid,
                                    "systemPrompt ignored: the session is already live"
                                );
                            }
                            s
                        }
                        None => {
                            if !a_resume.session_exists(&sid, req.cwd.clone()).await {
                                return responder.respond_with_error(
                                    agent_client_protocol::v1::schema::Error::session_not_found(
                                        sid.clone(),
                                    )
                                    .into(),
                                );
                            }
                            let session = a_resume.create_session(
                                sid.clone(),
                                req.cwd.clone(),
                                system_prompt,
                            );
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
                    release_catalog_if_unused(&a_resume.sessions);
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DeleteSessionRequest, responder, _cx| {
                    let context = agentscope_core::agent::RuntimeContext::new()
                        .with_session_id(req.session_id.to_string());
                    if let Some(session) = a_delete.get_session(&req.session_id) {
                        let _ = session
                            .cancel_and_wait(std::time::Duration::from_secs(10))
                            .await;
                        session.close();
                        // The live agent is the one that owns the journal: deleting through it
                        // cannot address a different agent identity than the turn wrote.
                        a_delete
                            .forget_session(session.agent(), session.cwd(), &req.session_id, &context)
                            .await;
                    } else if let Some(cwd) = a_delete.session_workspace(&req.session_id).await {
                        // The registry no longer holds the session (restart, or archived): the
                        // workspace files still have to go, or the next `session/list` shows it.
                        let agent = a_delete.catalog_agent(&cwd);
                        a_delete
                            .forget_session(&agent, &cwd, &req.session_id, &context)
                            .await;
                    }
                    a_delete.remove_session(&req.session_id);
                    responder.respond(DeleteSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: SetSessionTitleRequest, responder, cx| {
                    let sid = req.session_id;
                    // The spec requires rejecting a title that is empty after normalization
                    // with `-32602`; whitespace-only counts as empty. We store the trimmed form
                    // and hand it back, which is the normalisation the spec allows.
                    let title = req.title.trim().to_string();
                    if title.is_empty() {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params()
                                .data("session title must not be empty"),
                        );
                    }
                    let Some(mut session) = a_title.get_session(&sid) else {
                        return responder.respond_with_error(
                            agent_client_protocol::v1::schema::Error::session_not_found(sid.clone())
                                .into(),
                        );
                    };
                    session.set_title(title.clone());
                    // Reflect the change now, like the mode/config handlers: the client's
                    // session list must not wait for the next prompt turn.
                    let _ = cx.send_notification(session.session_info_update());
                    a_title.register_session(session);
                    responder.respond(SetSessionTitleResponse::new(title))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: SessionStatusRequest, responder, _cx| {
                    match a_status.session_status(&req.session_id).await {
                        Some(status) => responder.respond(SessionStatusResponse::new(status)),
                        None => responder.respond_with_error(
                            agent_client_protocol::v1::schema::Error::session_not_found(
                                req.session_id.clone(),
                            )
                            .into(),
                        ),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: AuthStatusRequest, responder, _cx| {
                    // The protocol forbids using `auth/status` to *demand* authentication, so
                    // this only reports what the host says: credentials present -> `authenticated`,
                    // none at all -> `unauthenticated` (the client may then offer `--setup`).
                    responder.respond(AuthStatusResponse::new(a_auth.auth_state()))
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
                            agent_client_protocol::Error::invalid_params()
                                .data(format!("unknown provider '{}'", req.provider_id)),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DisableProviderRequest, responder, _cx| {
                    let ok = a_providers_disable
                        .providers
                        .disable(&req.provider_id.to_string());
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
            .await;
        // The connection is gone: stop waking sessions nobody is listening to, and release
        // the sessions so a turn that was still running is cancelled instead of finishing
        // into a closed transport (ACP-42).
        wakeup_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        wakeup.abort();
        agent.shutdown_sessions().await;
        result
    }
}

/// Runs the idle-wakeup loop for the lifetime of the connection.
///
/// Background results (subagent tasks, offloaded tools, teammate messages) only reach the
/// model on a reasoning step, so a session that went idle with a pending result would stay
/// silent until the user typed again. This loop is the ACP counterpart of the harness
/// `WakeupDispatcher`: it drains the wakeup queue each producer writes to and gives the
/// named session the turn it is missing.
///
/// A wakeup that lands while the session is running is **deferred, not dropped**: the running
/// turn only reads the result if it reaches another reasoning step, and a turn that is
/// already wrapping up would leave it unserved. Deferred entries are retried each tick until
/// the session goes idle; the turn's own `run_wakeup` guard drops the retry when that turn
/// already consumed the result, so an empty turn is never started. One attempt per idle
/// period, like the dispatcher, so a failing turn is not retried in a loop.
fn spawn_wakeup_loop(
    agent: Arc<AcpCodingAgent>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Long enough to be free when nothing is pending, short enough that a finished
        // subagent's summary shows up while the user is still looking at the session.
        const TICK: std::time::Duration = std::time::Duration::from_millis(500);
        // Sessions that asked to be woken while their turn was still running.
        let mut deferred: std::collections::HashMap<String, ()> = std::collections::HashMap::new();
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(TICK).await;
            let sessions: Vec<AcpAgentSession> = {
                let registry = agent
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                registry.values().cloned().collect()
            };
            // Sessions built from the same workspace share one bus, so whichever of them
            // drains first collects every entry; the second drain is empty.
            for session in sessions {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                for target_id in session.drain_wakeup_requests().await {
                    deferred.insert(target_id, ());
                }
            }
            // Serve everything that is now idle; keep the sessions still running.
            for target_id in std::mem::take(&mut deferred).into_keys() {
                let target = {
                    let registry = agent
                        .sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    registry.get(&target_id).cloned()
                };
                let Some(mut target) = target else {
                    // Unknown or evicted session: nothing to wake.
                    continue;
                };
                if target.is_closed() {
                    continue;
                }
                if target.is_running() {
                    deferred.insert(target_id, ());
                    continue;
                }
                target.run_wakeup().await;
            }
        }
    })
}

/// Handles `_agentscope/session/respond`: converts the wire answers and hands them to the
/// session runtime, which continues the suspended turn.
async fn handle_ext_respond(
    agent: Arc<AcpCodingAgent>,
    req: crate::ext::ExtRespondRequest,
    responder: agent_client_protocol::v1::Responder<serde_json::Value>,
) -> Result<(), agent_client_protocol::Error> {
    let params = req.params;
    let answers = match params.to_answers() {
        Ok(answers) => answers,
        Err(error) => {
            return responder
                .respond_with_error(agent_client_protocol::Error::invalid_params().data(error));
        }
    };
    let session_id = SessionId::new(params.session_id);
    let session = match agent.get_session(&session_id) {
        Some(session) => session,
        None => {
            return responder.respond_with_error(
                agent_client_protocol::v1::schema::Error::session_not_found(session_id).into(),
            );
        }
    };
    match session.respond(answers).await {
        Ok(_) => responder.respond(serde_json::Value::Object(serde_json::Map::new())),
        Err(error) => responder.respond_with_error(
            agent_client_protocol::Error::internal_error().data(error.to_string()),
        ),
    }
}

/// Whether `session_id` names a subagent's session (`sub-<uuid>`, as minted by `agent_spawn`)
/// rather than a user session.
///
/// Subagents journal under their own key, so a workspace's session index contains them. They
/// are not user-facing sessions: listing one would offer `session/load` on a transcript the
/// client never opened, and `session/delete` would remove a child a live parent still uses.
fn is_subagent_session(session_id: &SessionId) -> bool {
    session_id.to_string().starts_with("sub-")
}

/// Orders `session/list` entries: most recently active first.
///
/// Session ids are opaque (`session-<uuid>`), so ordering by id put an arbitrary session at
/// the top of the client's picker. The id is the tie-breaker, which keeps pagination
/// deterministic for sessions written in the same millisecond. A session without a recorded
/// activity time (never prompted) sorts last.
fn sort_session_infos(infos: &mut [SessionInfo]) {
    infos.sort_by(|a, b| match (&a.updated_at, &b.updated_at) {
        (Some(left), Some(right)) => right
            .cmp(left)
            .then_with(|| a.session_id.to_string().cmp(&b.session_id.to_string())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.session_id.to_string().cmp(&b.session_id.to_string()),
    });
}

/// Validates a client-supplied `systemPrompt` on a session lifecycle request (ACP-38).
///
/// Only `append` mode is implemented: the text is layered on top of our own
/// instructions, so the coding rules and tool guidance stay in force. Any other mode
/// is rejected with `-32602` instead of being silently downgraded — the spec forbids
/// the downgrade, and silently honouring `override` would hand the client's prompt
/// authority over our safety instructions.
fn system_prompt_text(
    prompt: Option<SystemPrompt>,
) -> Result<Option<String>, agent_client_protocol::Error> {
    let Some(prompt) = prompt else {
        return Ok(None);
    };
    if prompt.mode != SystemPromptMode::Append {
        return Err(agent_client_protocol::Error::new(
            agent_client_protocol::ErrorCode::INVALID_PARAMS,
            format!(
                "unsupported systemPrompt mode {:?}; only `append` is supported",
                prompt.mode
            ),
        )
        .data(serde_json::json!({ "supportedModes": ["append"] })));
    }
    let mut text = String::new();
    for block in prompt.content {
        match block {
            ContentBlock::Text(TextContent { text: chunk, .. }) => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&chunk);
            }
            other => {
                return Err(agent_client_protocol::Error::new(
                    agent_client_protocol::ErrorCode::INVALID_PARAMS,
                    format!(
                        "unsupported systemPrompt content block; only `text` is supported ({other:?})"
                    ),
                )
                .data(serde_json::json!({ "supportedContentBlocks": ["text"] })));
            }
        }
    }
    if text.trim().is_empty() {
        return Err(agent_client_protocol::Error::new(
            agent_client_protocol::ErrorCode::INVALID_PARAMS,
            "systemPrompt content must contain non-empty text",
        ));
    }
    Ok(Some(text))
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
    model_builder: Option<Arc<dyn Fn(&str, &str) -> Result<Arc<dyn Model>, String> + Send + Sync>>,
    auth_probe: Option<Arc<dyn Fn() -> AuthState + Send + Sync>>,
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
    ///
    /// This factory does not receive the client-supplied `systemPrompt` (ACP-38);
    /// use [`session_factory`](Self::session_factory) if the session must honour it.
    /// Consequently the `systemPrompt` capability is **not** advertised for an agent
    /// built this way, and a request that carries one is rejected with `-32602`.
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

    /// Installs the answer `auth/status` reports (`not_required` by default).
    pub fn auth_probe(mut self, probe: impl Fn() -> AuthState + Send + Sync + 'static) -> Self {
        self.auth_probe = Some(Arc::new(probe));
        self
    }

    /// Builds the [`AcpCodingAgent`].
    pub fn build(self) -> Result<AcpCodingAgent, Box<dyn std::error::Error>> {
        let factory = self
            .model_factory
            .map(|f| -> Box<dyn Fn(PathBuf, &str) -> HarnessAgent + Send + Sync> { f })
            .unwrap_or_else(|| Box::new(default_model_factory));
        let mut agent =
            AcpCodingAgent::new(move |cwd, sid| factory(cwd, sid), self.models, self.modes);
        agent.session_factory = self.session_factory;
        agent.model_builder = self.model_builder;
        if let Some(name) = self.name {
            agent = agent.with_name(name);
        }
        if let Some(max) = self.max_sessions {
            agent = agent.with_max_sessions(max);
        }
        if let Some(probe) = self.auth_probe {
            agent = agent.with_auth_probe(move || probe());
        }
        Ok(agent)
    }
}

#[cfg(test)]
mod tests {
    use super::sort_session_infos;
    use agent_client_protocol::v1::schema::{SessionId, SessionInfo};

    fn info(id: &str, updated_at: Option<&str>) -> SessionInfo {
        let info = SessionInfo::new(SessionId::new(id), std::path::PathBuf::from("/tmp"));
        match updated_at {
            Some(at) => info.updated_at(at),
            None => info,
        }
    }

    #[test]
    fn the_compaction_window_is_its_own_attribute_not_the_model_window() {
        // The window a 1M-token model advertises must not set the compaction trigger: the
        // conversation would grow past ~878k tokens before anything is summarized.
        assert_eq!(
            super::parse_compaction_window(None),
            super::ACP_CONTEXT_WINDOW_TOKENS
        );
        assert_eq!(super::parse_compaction_window(Some(" 300000 ")), 300_000);
        // A typo or a zero must not silently disable compaction.
        assert_eq!(
            super::parse_compaction_window(Some("0")),
            super::ACP_CONTEXT_WINDOW_TOKENS
        );
        assert_eq!(
            super::parse_compaction_window(Some("256k")),
            super::ACP_CONTEXT_WINDOW_TOKENS
        );
    }

    #[test]
    fn the_catalog_cache_drops_another_directory_when_it_is_full() {
        // Each cached catalog agent is a whole `HarnessAgent`; without a bound, listing sessions
        // in many directories would keep one agent alive per directory.
        let keys = vec![
            std::path::PathBuf::from("/a"),
            std::path::PathBuf::from("/b"),
        ];
        let cwd = std::path::PathBuf::from("/c");
        let stale = super::catalog_eviction(keys.iter(), &cwd).expect("an eviction candidate");
        assert_ne!(stale.as_path(), cwd.as_path());
        assert!(keys.contains(stale));
    }

    #[test]
    fn the_catalog_cache_keeps_the_only_entry_being_replaced() {
        let keys = vec![std::path::PathBuf::from("/a")];
        let cwd = std::path::PathBuf::from("/a");
        assert!(super::catalog_eviction(keys.iter(), &cwd).is_none());
    }

    #[test]
    fn the_session_list_orders_by_last_activity_not_by_id() {
        // Regression: ordering by session id put an arbitrary session first, because the id is
        // an opaque uuid (`session-<uuid>`) with no time in it.
        let mut infos = vec![
            info("session-aaa", Some("2026-10-27T10:00:00Z")),
            info("session-zzz", Some("2026-10-28T10:00:00Z")),
            info("session-never-prompted", None),
            info("session-mmm", Some("2026-10-27T10:00:00Z")),
        ];
        sort_session_infos(&mut infos);
        let ids: Vec<String> = infos
            .iter()
            .map(|info| info.session_id.to_string())
            .collect();
        assert_eq!(
            ids,
            vec![
                "session-zzz",
                // Same timestamp: the id is the tie-breaker, so pagination stays stable.
                "session-aaa",
                "session-mmm",
                "session-never-prompted",
            ]
        );
    }

    /// ACP-38: only `append` is implemented, and the text is joined from text blocks.
    #[test]
    fn a_client_system_prompt_is_only_accepted_in_append_mode() {
        use agent_client_protocol::v1::schema::{
            ContentBlock, SystemPrompt, SystemPromptMode, TextContent,
        };

        assert_eq!(super::system_prompt_text(None).unwrap(), None);

        let append = SystemPrompt::new(vec![
            ContentBlock::Text(TextContent::new("rule one")),
            ContentBlock::Text(TextContent::new("rule two")),
        ]);
        assert_eq!(
            super::system_prompt_text(Some(append)).unwrap().as_deref(),
            Some("rule one\n\nrule two")
        );

        // `override` would hand the client authority over our coding rules, so it is
        // rejected rather than downgraded to `append` (the spec forbids the downgrade).
        let overridden = SystemPrompt::new(vec![ContentBlock::Text(TextContent::new("hijack"))])
            .mode(SystemPromptMode::Override);
        let error = super::system_prompt_text(Some(overridden)).unwrap_err();
        assert_eq!(error.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);

        // Empty / whitespace-only content must not create a session with no instructions.
        let blank = SystemPrompt::new(vec![ContentBlock::Text(TextContent::new("   "))]);
        let error = super::system_prompt_text(Some(blank)).unwrap_err();
        assert_eq!(error.code, agent_client_protocol::ErrorCode::INVALID_PARAMS);
    }

    /// ACP-42: once the connection is gone nothing can consume a turn's output, so every
    /// session must be closed (which cancels its running turn) and dropped.
    #[tokio::test]
    async fn a_terminated_connection_releases_its_sessions() {
        use agent_client_protocol::v1::schema::SessionId;

        struct StubModel;

        #[async_trait::async_trait]
        impl agentscope_core::model::Model for StubModel {
            fn stream(
                &self,
                _messages: Vec<agentscope_core::message::Msg>,
                _tools: Vec<agentscope_core::model::ToolSchema>,
                _options: agentscope_core::model::GenerateOptions,
            ) -> futures::stream::BoxStream<
                '_,
                agentscope_core::error::Result<agentscope_core::model::ChatResponse>,
            > {
                Box::pin(futures::stream::once(async {
                    Ok(agentscope_core::model::ChatResponse::builder()
                        .content(vec![agentscope_core::message::ContentBlock::Text(
                            agentscope_core::message::TextBlock::new("done"),
                        )])
                        .build())
                }))
            }

            fn model_name(&self) -> &str {
                "stub"
            }
        }

        let root =
            std::env::temp_dir().join(format!("agentscope_acp_shutdown_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root_for_factory = root.clone();
        let agent = super::AcpCodingAgent::builder()
            .model_factory(move |cwd: std::path::PathBuf, session_id: &str| {
                agentscope_harness::agent::HarnessAgent::builder()
                    .name("shutdown-test")
                    .model(std::sync::Arc::new(StubModel))
                    .session_id(session_id)
                    .cwd(cwd)
                    .workspace(root_for_factory.join(".agentscope").join("workspace"))
                    .disable_memory_hooks()
                    .max_iters(1)
                    .build()
            })
            .build()
            .expect("build AcpCodingAgent");

        let session_id = SessionId::new("session-shutdown");
        let session = agent.create_session(session_id.clone(), root.clone(), None);
        let handle = session.clone();
        agent.register_session(session);
        assert!(agent.get_session(&session_id).is_some());

        agent.shutdown_sessions().await;

        assert!(
            handle.is_closed(),
            "shutdown must close the session so its running turn is cancelled"
        );
        assert!(
            agent.get_session(&session_id).is_none(),
            "the registry must be drained"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
