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

use agent_client_protocol::schema::v1::{
    AgentAuthCapabilities, AgentCapabilities, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse, DisableProviderRequest,
    DisableProviderResponse, Implementation, InitializeRequest, InitializeResponse,
    ListProvidersRequest, ListProvidersResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, McpCapabilities, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionDeleteCapabilities, SessionId, SessionInfo, SessionListCapabilities,
    SessionResumeCapabilities, SetProviderRequest, SetProviderResponse,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse, StopReason,
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
    /// Advertised `prompt.image` support. See [`ImageSupport`].
    image_support: ImageSupport,
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
            image_support: ImageSupport::default(),
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
        AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                PromptCapabilities::new()
                    .image(image)
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

    /// Returns a clone of the session (for handlers).
    fn get_session(&self, session_id: &SessionId) -> Option<AcpAgentSession> {
        self.sessions
            .lock()
            .unwrap()
            .get(&session_id.to_string())
            .cloned()
    }

    /// Whether a persisted journal exists for `session_id`.
    ///
    /// `session/load` and `session/resume` must not silently create an empty session for a
    /// mistyped or deleted id: the client would see a successful load with no history. The
    /// journal is the durable authority, so probe it before building anything.
    async fn session_exists(&self, session_id: &SessionId, cwd: PathBuf) -> bool {
        let agent = match &self.session_factory {
            Some(factory) => factory(SessionBuild {
                cwd: cwd.clone(),
                session_id: session_id.to_string(),
                client_support: self.client_support.clone(),
                elicitation_slot: Arc::new(Mutex::new(None)),
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

    /// Builds the `session/list` response, honouring the request's `cursor`.
    ///
    /// The in-memory registry only knows sessions this process created. The journal is the
    /// durable authority, so when the client names a `cwd` the list also includes sessions a
    /// previous process persisted there — otherwise a restart showed an empty history and the
    /// client could not offer `session/load`.
    ///
    /// Sessions are ordered by id (stable) so pagination is deterministic.
    async fn session_infos(
        &self,
        cwd: Option<&std::path::Path>,
        cursor: Option<&str>,
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
            match agent.list_session_keys(&context).await {
                Ok(keys) => {
                    for key in keys {
                        if known.contains(&key.session_id) {
                            continue;
                        }
                        infos.push(SessionInfo::new(
                            SessionId::new(key.session_id),
                            cwd.to_path_buf(),
                        ));
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not read the persisted session index")
                }
            }
        }
        // Drop duplicates by id *before* sorting: the two orders disagree, so `dedup_by`
        // after the sort would leave a persisted duplicate of an in-memory session in place.
        let mut seen_ids = std::collections::HashSet::new();
        infos.retain(|info| seen_ids.insert(info.session_id.to_string()));
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
        let a_respond = agent.clone();
        let a_providers = agent.clone();
        let a_providers_set = agent.clone();
        let a_providers_disable = agent.clone();
        let result = Agent
            .builder()
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
                                agent_client_protocol::Error::invalid_params()
                                    .data("unknown session"),
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
                    let infos = a_list
                        .session_infos(req.cwd.as_deref(), req.cursor.as_deref())
                        .await;
                    responder.respond(infos)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest, responder, cx| {
                    let sid = req.session_id;
                    let session = match a_load.get_session(&sid) {
                        Some(s) => s,
                        None => {
                            if !a_load.session_exists(&sid, req.cwd.clone()).await {
                                return responder.respond_with_error(
                                    agent_client_protocol::Error::invalid_params()
                                        .data(format!("unknown session '{sid}'")),
                                );
                            }
                            let session = a_load.create_session(sid.clone(), req.cwd.clone());
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
                    let session = match a_resume.get_session(&sid) {
                        Some(s) => s,
                        None => {
                            if !a_resume.session_exists(&sid, req.cwd.clone()).await {
                                return responder.respond_with_error(
                                    agent_client_protocol::Error::invalid_params()
                                        .data(format!("unknown session '{sid}'")),
                                );
                            }
                            let session = a_resume.create_session(sid.clone(), req.cwd.clone());
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
                    if let Some(session) = a_delete.get_session(&req.session_id) {
                        let _ = session
                            .cancel_and_wait(std::time::Duration::from_secs(10))
                            .await;
                        session.close();
                    }
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
        // The connection is gone: stop waking sessions nobody is listening to.
        wakeup_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        wakeup.abort();
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
    responder: agent_client_protocol::Responder<serde_json::Value>,
) -> Result<(), agent_client_protocol::Error> {
    let params = req.params;
    let answers = match params.to_answers() {
        Ok(answers) => answers,
        Err(error) => {
            return responder
                .respond_with_error(agent_client_protocol::Error::invalid_params().data(error));
        }
    };
    let session = match agent.get_session(&SessionId::new(params.session_id)) {
        Some(session) => session,
        None => {
            return responder.respond_with_error(
                agent_client_protocol::Error::invalid_params().data("unknown session"),
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
        Ok(agent)
    }
}

#[cfg(test)]
mod tests {
    use super::sort_session_infos;
    use agent_client_protocol::schema::v1::{SessionId, SessionInfo};

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
}
