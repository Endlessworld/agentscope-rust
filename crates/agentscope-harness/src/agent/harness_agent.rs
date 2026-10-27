//! Enhanced agent runtime, mirroring `HarnessAgent`: wires a `Filesystem`, its tools, a
//! middleware chain, workspace context, memory pipeline, subagent orchestration and plan
//! mode around the core agent abstraction.
//!
//! `HarnessAgent` is the user-facing harness API that wraps a [`ReActAgent`] with
//! workspace / filesystem / memory / subagent / plan-mode orchestration. Use
//! [`HarnessAgentBuilder`] to configure. For plain ReAct usage without any of the above,
//! use `ReActAgent::builder()` directly.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::agent::{Agent, CallableAgent, ReActAgent, RuntimeContext, StreamOptions};
use agentscope_core::event::AgentEvent;
use agentscope_core::message::Msg;
use agentscope_core::middleware::MiddlewareChain;
use agentscope_core::model::Model;
use agentscope_core::session::{
    SessionHistoryMode, SessionKey, SessionLog, SessionLogStore, SessionProjection, Transcript,
};
use agentscope_core::shutdown::{GracefulShutdownManager, ShutdownStateSaver};
use agentscope_core::state::AgentState;
use agentscope_core::state::AgentStateStore;
use agentscope_core::state::ConflictPolicy;
use agentscope_core::tool::{Tool, Toolkit};

use crate::filesystem::{Filesystem, LocalFilesystem};
use crate::tool::CompactConfig;

use super::artifact::ArtifactDeliveryTarget;
use super::bus::{AsyncToolRegistry, MessageBus, WorkspaceAsyncToolRegistry, WorkspaceMessageBus};
use super::distributed_store::DistributedStore;
use super::isolation_scope::IsolationScope;
use super::memory::compaction::{CompactionConfig, ConversationCompactor};
use super::memory::{
    MemoryConfig, MemoryConsolidator, MemoryFlushManager, ToolResultEvictionConfig,
};
use super::middleware::{
    AgentTraceMiddleware, AtPathExpansionMiddleware, ConversationCompactionMiddleware,
    DynamicSubagentsMiddleware, GracefulShutdownMiddleware, HarnessSkillMiddleware,
    InboxMiddleware, MemoryFlushMiddleware, MemoryMaintenanceMiddleware, PlanModeConfig,
    PlanModeMiddleware, SkillCuratorMiddleware, SkillUsageMiddleware, StateCheckpointMiddleware,
    SubagentsMiddleware, TeamsMiddleware, ToolCallPairSanitizerMiddleware,
    ToolResultEvictionMiddleware, WorkspaceContextMiddleware,
};
use super::shutdown::GracefulShutdownController;
use super::skill::{
    build_promoter, compose_skill_repositories, default_gate, AuditEntry,
    FilesystemSkillUsageStore, LazyLoadSkillTool, ProposeSkillTool, SkillAuditLog, SkillCurator,
    SkillCuratorConfig, SkillFilter, SkillManageConfig, SkillManageTool, SkillPromoter,
    SkillPromotionGate, SkillRepository, SkillUsageStore, SkillVisibilityFilter,
    WorkspaceSkillRepository,
};
use super::subagent::task::repository::TaskRepository;
use super::subagent::task::WorkspaceTaskRepository;
use super::subagent::{
    CustomSubagentFactory, DefaultAgentManager, SubagentBlueprint, SubagentDeclaration,
    SubagentEntry,
};
use super::team::{TeamClient, TeamContext};
use super::workspace::{
    PlanModeManager, WorkspaceIndex, WorkspaceManager, WorkspacePathNormalizer,
};
use crate::tool::{
    AgentListTool, AgentSendTool, AgentSpawnTool, ArtifactDeliveryTool, FilesystemTool,
    MemoryGetTool, MemorySaveTool, MemorySearchTool, PlanModeHandle, SessionSearchTool,
    ShellExecuteTool, TeamTool, WebFetchTool, WebSearchTool,
};

/// A harness agent that operates on a filesystem with a configured toolkit and middleware chain.
pub struct HarnessAgent {
    name: String,
    /// Stable agent id (mirrors Java `agentId`; defaults to `name` when unset).
    agent_id: String,
    /// Human-readable description forwarded to the embedded ReAct agent.
    description: String,
    model: Option<Arc<dyn Model>>,
    filesystem: Arc<dyn Filesystem>,
    toolkit: Toolkit,
    middleware: MiddlewareChain,
    cwd: PathBuf,
    distributed_store: Option<DistributedStore>,
    /// Whether the embedded agent checks/rejects concurrent runs (stored config).
    check_running: bool,
    /// Timeout applied to async tool executions when configured (stored config).
    async_tool_timeout: Option<Duration>,
    /// Deployment environment label (mirrors Java `environment`).
    environment: String,
    /// Whether the legacy XML workspace-context rendering was requested (stored config).
    use_legacy_xml_workspace_context: bool,
    /// Whether `disableSessionPersistence` was requested (stored/propagated config).
    disable_session_persistence: bool,
    /// Task repository bound to this agent (defaults to a `WorkspaceTaskRepository`).
    task_repository: Arc<dyn TaskRepository>,
    /// The embedded ReAct agent that drives the reasoning→acting loop.
    /// `None` when no model was configured (assembly-only mode).
    agent: Option<ReActAgent>,
    /// Workspace manager bound to this agent (context loading, memory, sessions).
    workspace_manager: Option<Arc<WorkspaceManager>>,
    /// Plan-mode coordinator, when plan mode was enabled.
    plan_mode_manager: Option<Arc<PlanModeManager>>,
    /// The memory configuration used for the flush/consolidation pipeline.
    memory_config: MemoryConfig,
    /// The path normalizer used by filesystem tools (workspace-relative projection).
    path_normalizer: Option<WorkspacePathNormalizer>,
    /// Ordered skill repositories bound to this agent (low-to-high priority).
    skill_repositories: Vec<Arc<dyn SkillRepository>>,
    /// Skill promoter, when the self-learning loop is enabled.
    skill_promoter: Option<Arc<SkillPromoter>>,
    /// Skill usage store, when the self-learning loop is enabled.
    skill_usage_store: Option<Arc<Mutex<dyn SkillUsageStore>>>,
    /// Skill curator, when enabled.
    skill_curator: Option<Arc<SkillCurator>>,
    /// Skill audit log, when the self-learning loop is enabled.
    skill_audit_log: Option<Arc<SkillAuditLog>>,
    /// The journal store the embedded agent resolves its sessions through.
    ///
    /// The core agent owns the recorder/persistence path; this handle is only used by read
    /// APIs (`session/list`) that need to enumerate sessions from the same backend.
    session_log_store: Option<Arc<dyn SessionLogStore>>,
    /// 该 agent 的优雅关闭管理器（默认每个 agent 一个独立实例）。
    shutdown_manager: Arc<GracefulShutdownManager>,
}

/// The default maximum reasoning→acting iterations used when none is configured.
const DEFAULT_MAX_ITERS: i32 = 10;

/// Default workspace-relative directory for plan files.
const DEFAULT_PLAN_DIR: &str = "plans";

impl HarnessAgent {
    pub fn builder() -> HarnessAgentBuilder {
        HarnessAgentBuilder::default()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The stable agent id (mirrors Java `agentId`); defaults to `name`.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The human-readable description forwarded to the embedded ReAct agent.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Whether the embedded agent enforces a single in-flight run (stored config).
    pub fn check_running(&self) -> bool {
        self.check_running
    }

    /// The configured async-tool timeout, if any.
    pub fn async_tool_timeout(&self) -> Option<Duration> {
        self.async_tool_timeout
    }

    /// The deployment environment label (mirrors Java `environment`).
    pub fn environment(&self) -> &str {
        &self.environment
    }

    /// Whether the legacy XML workspace-context rendering was requested.
    pub fn use_legacy_xml_workspace_context(&self) -> bool {
        self.use_legacy_xml_workspace_context
    }

    /// Whether `disableSessionPersistence` was requested. Mirrors Java's currently
    /// inert flag but is retained for configuration parity.
    pub fn disable_session_persistence(&self) -> bool {
        self.disable_session_persistence
    }

    /// The task repository bound to this agent.
    pub fn task_repository(&self) -> &Arc<dyn TaskRepository> {
        &self.task_repository
    }

    pub fn model(&self) -> Option<&Arc<dyn Model>> {
        self.model.as_ref()
    }

    pub fn filesystem(&self) -> &dyn Filesystem {
        self.filesystem.as_ref()
    }

    pub fn toolkit(&self) -> &Toolkit {
        &self.toolkit
    }

    pub fn middleware(&self) -> &MiddlewareChain {
        &self.middleware
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    pub fn distributed_store(&self) -> Option<&DistributedStore> {
        self.distributed_store.as_ref()
    }

    /// The embedded ReAct agent driving the reasoning→acting loop, if a model was configured.
    pub fn agent(&self) -> Option<&ReActAgent> {
        self.agent.as_ref()
    }

    /// The native journal identity for `context`, or `None` when history is not
    /// journal-backed (no model, legacy mode, or no session id yet).
    ///
    /// Mirrors Java `HarnessAgent.sessionKey(RuntimeContext)`.
    pub fn session_key(&self, context: &RuntimeContext) -> Option<SessionKey> {
        self.agent
            .as_ref()
            .filter(|agent| agent.session_log_enabled())
            .and_then(|agent| agent.session_key(context))
    }

    /// Opens the native journal for `context`. Mirrors Java
    /// `HarnessAgent.sessionLog(RuntimeContext)`.
    ///
    /// Opening a reader never acquires a writer or repairs history, so this is safe to call
    /// while another call is running.
    pub async fn session_log(
        &self,
        context: &RuntimeContext,
    ) -> crate::Result<Arc<dyn SessionLog>> {
        match self.agent.as_ref() {
            Some(agent) if agent.session_log_enabled() => agent.session_log(context).await,
            _ => Err(crate::Error::Session(
                "native session history is not enabled for this agent".into(),
            )),
        }
    }

    /// Reads the committed projection of the session bound to `context`. Mirrors Java
    /// `HarnessAgent.inspectSession(RuntimeContext)`.
    pub async fn inspect_session(
        &self,
        context: &RuntimeContext,
    ) -> crate::Result<SessionProjection> {
        let log = self.session_log(context).await?;
        SessionProjection::read(&*log).await
    }

    /// Lists every persisted session for the caller's user, newest first.
    ///
    /// The journal is the durable authority for native sessions; the in-memory registry only
    /// knows sessions this process created. A client that restarts must still see its history,
    /// so this reads the store's session index instead of the registry.
    pub async fn list_session_keys(
        &self,
        context: &RuntimeContext,
    ) -> crate::Result<Vec<SessionKey>> {
        let store = self
            .session_log_store
            .as_ref()
            .ok_or_else(|| crate::Error::Session("native session history is not enabled".into()))?;
        store.list(context).await
    }

    /// Reads the replayable transcript of the session bound to `context`. Mirrors Java
    /// `HarnessAgent.sessionTranscript(RuntimeContext)`.
    pub async fn session_transcript(&self, context: &RuntimeContext) -> crate::Result<Transcript> {
        let log = self.session_log(context).await?;
        agentscope_core::session::transcript(&*log).await
    }

    /// Binds a conversation's commands and read views. Obtaining this handle never starts
    /// inference. Mirrors Java `HarnessAgent.session(RuntimeContext)`.
    ///
    /// Unlike Java this does not memoise the handle: a cached `AgentSession` would hold an
    /// `Arc<HarnessAgent>` inside the agent itself and leak the reference cycle.
    pub async fn session(
        self: &Arc<Self>,
        context: RuntimeContext,
    ) -> crate::Result<super::session::AgentSession> {
        let key = self.session_key(&context).ok_or_else(|| {
            crate::Error::Session("native session history is not enabled for this agent".into())
        })?;
        let log = self.session_log(&context).await?;
        super::session::AgentSession::open(Arc::clone(self), context, key, log)
    }

    /// Returns the workspace manager bound to this agent, if configured.
    pub fn workspace_manager(&self) -> Option<&Arc<WorkspaceManager>> {
        self.workspace_manager.as_ref()
    }

    /// Returns the plan-mode coordinator, if plan mode was enabled.
    pub fn plan_mode_manager(&self) -> Option<&Arc<PlanModeManager>> {
        self.plan_mode_manager.as_ref()
    }

    /// Returns the memory configuration used for the flush/consolidation pipeline.
    pub fn memory_config(&self) -> &MemoryConfig {
        &self.memory_config
    }

    /// Returns the path normalizer used by filesystem tools, if configured.
    pub fn path_normalizer(&self) -> Option<&WorkspacePathNormalizer> {
        self.path_normalizer.as_ref()
    }

    /// Returns the ordered skill repositories bound to this agent.
    pub fn skill_repositories(&self) -> &[Arc<dyn SkillRepository>] {
        &self.skill_repositories
    }

    /// Returns the skill usage store, if the self-learning loop is enabled.
    pub fn skill_usage_store(&self) -> Option<&Arc<Mutex<dyn SkillUsageStore>>> {
        self.skill_usage_store.as_ref()
    }

    /// Returns the skill promoter, if the self-learning loop is enabled.
    pub fn skill_promoter(&self) -> Option<&Arc<SkillPromoter>> {
        self.skill_promoter.as_ref()
    }

    /// Returns the skill curator, if enabled.
    pub fn skill_curator(&self) -> Option<&Arc<SkillCurator>> {
        self.skill_curator.as_ref()
    }

    /// Returns the skill audit log, if the self-learning loop is enabled.
    pub fn skill_audit_log(&self) -> Option<&Arc<SkillAuditLog>> {
        self.skill_audit_log.as_ref()
    }

    /// Queries the skill audit log for the given UTC day (empty when not configured).
    pub fn query_audit(
        &self,
        day_utc: &str,
        predicate: impl Fn(&AuditEntry) -> bool,
    ) -> Vec<AuditEntry> {
        match &self.skill_audit_log {
            Some(log) => log.query(day_utc, predicate),
            None => Vec::new(),
        }
    }

    /// Runs the skill curator once (no-op when the curator is not enabled).
    pub fn run_curator_once(&self) -> Option<super::skill::CuratorRunReport> {
        let curator = self.skill_curator.as_ref()?;
        Some(futures::executor::block_on(
            curator.run_once(chrono::Utc::now()),
        ))
    }

    /// Promotes a draft skill (invalid result when no promotion loop is configured).
    pub fn promote_skill(
        &self,
        name: &str,
        reviewer_id: &str,
    ) -> Option<super::skill::PromotionResult> {
        self.skill_promoter
            .as_ref()
            .map(|p| p.promote(name, reviewer_id))
    }

    /// Streams agent events for the given messages through the embedded ReAct agent.
    ///
    /// This is the primary driver entry: it runs the middleware-wrapped ReAct loop and
    /// yields filtered [`AgentEvent`]s for the given [`StreamOptions`]. Returns a stream
    /// that yields a single error when no model was configured (assembly-only mode).
    pub fn stream_events(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::error::Result<AgentEvent>> {
        match &self.agent {
            Some(agent) => agent.stream_events(messages, options),
            None => Box::pin(futures::stream::once(async move {
                Err(agentscope_core::error::Error::Agent(
                    "HarnessAgent has no model configured; cannot run".into(),
                ))
            })),
        }
    }

    /// Streams agent events with per-run context extensions.
    ///
    /// Extensions reach the embedded [`ReActAgent`]'s `RuntimeContext`, which is how a caller
    /// hands one run a value the build-time configuration cannot express (the AG-UI adapter
    /// passes a per-call `ToolRequestConfig` this way).
    pub fn stream_events_with_extensions(
        &self,
        messages: Vec<Msg>,
        options: StreamOptions,
        extensions: agentscope_core::agent::Extensions,
    ) -> futures::stream::BoxStream<'static, agentscope_core::error::Result<AgentEvent>> {
        match &self.agent {
            Some(agent) => agent.stream_events_with_extensions(messages, options, extensions),
            None => Box::pin(futures::stream::once(async move { Err(no_model_error()) })),
        }
    }

    /// Replaces the embedded agent's permission engine with one built from `ctx`.
    /// No-op when no model was configured (assembly-only mode).
    pub fn set_permission_context(&self, ctx: agentscope_core::PermissionContextState) {
        if let Some(agent) = &self.agent {
            agent.set_permission_context(ctx);
        }
    }

    /// Installs/removes the confirmation handler used for `Ask` tool decisions.
    /// No-op when no model was configured (assembly-only mode).
    pub fn set_confirm_handler(&self, handler: Option<agentscope_core::ConfirmHandler>) {
        if let Some(agent) = &self.agent {
            agent.set_confirm_handler(handler);
        }
    }

    /// Adds a single permission rule to the embedded agent's current engine
    /// (e.g. recording an "always allow/reject" user choice). No-op without a model.
    pub fn add_permission_rule(&self, rule: agentscope_core::PermissionRule) {
        if let Some(agent) = &self.agent {
            agent.add_permission_rule(rule);
        }
    }

    /// Interrupts the current session's execution. No-op without a configured model.
    pub fn interrupt(&self) {
        if let Some(agent) = &self.agent {
            agent.interrupt();
        }
    }

    /// Interrupts execution only when the current session matches `session_id`.
    /// Returns `true` if the interrupt was triggered for that session.
    pub fn interrupt_session(&self, session_id: &str) -> bool {
        self.agent
            .as_ref()
            .map(|a| a.interrupt_session(session_id))
            .unwrap_or(false)
    }

    /// Interrupts one specific run, if it is still the one in flight.
    ///
    /// Returns `true` when the run was found and signalled. A stale request (the run already
    /// finished and a new one started) is dropped, so a cancel cannot leak into the next turn.
    pub fn interrupt_run(&self, run_id: &str) -> bool {
        self.agent
            .as_ref()
            .map(|a| a.interrupt_run(run_id))
            .unwrap_or(false)
    }

    /// Clears the current session's model-visible conversation context without
    /// creating a new session. No-op without a configured model.
    pub fn clear_context(&self) {
        if let Some(agent) = &self.agent {
            agent.clear_context();
        }
    }

    /// Clears the model-visible context for the session identified by `session_id`.
    /// Returns `true` when that session's context was cleared.
    pub fn clear_context_for(&self, session_id: &str) -> bool {
        self.agent
            .as_ref()
            .map(|a| a.clear_context_for(session_id))
            .unwrap_or(false)
    }

    /// Replaces the model bound to the embedded ReAct agent. The next reasoning
    /// call uses the new model while conversation state is preserved (dynamic
    /// model switching). No-op without a configured model.
    pub fn set_model(&self, model: Arc<dyn Model>) {
        if let Some(agent) = &self.agent {
            agent.set_model(model);
        }
    }

    /// Sets per-call generation defaults (reasoning effort, retry policy, ...)
    /// merged into every model request of the embedded agent.
    pub fn set_generate_options_defaults(&self, options: agentscope_core::model::GenerateOptions) {
        if let Some(agent) = &self.agent {
            agent.set_generate_options_defaults(options);
        }
    }

    /// 该 agent 的优雅关闭管理器。
    ///
    /// 中间件与 [`HarnessAgent::shutdown`] 共用同一个实例：关闭时新请求被拒绝，
    /// 活跃请求被等待（或超时强制中断），状态在终止前被保存。
    pub fn shutdown_manager(&self) -> &Arc<GracefulShutdownManager> {
        &self.shutdown_manager
    }

    /// 触发优雅关闭：拒绝新请求 → 等待活跃请求结束 → 保存状态 → 终止。
    ///
    /// 这是 Java `AgentScopeJvmShutdownHook` 的 Rust 等价入口：Rust 没有 JVM，
    /// 也不引入 tokio 的 `signal` feature，宿主（例如
    /// `agentscope-extensions-acp` 的 `spawn_shutdown_handler`）在收到 Ctrl-C /
    /// SIGTERM 后调用本方法即可。返回 `true` 表示已经到达 `Terminated`。
    pub async fn shutdown(&self) -> bool {
        GracefulShutdownController::new(Arc::clone(&self.shutdown_manager))
            .shutdown()
            .await
    }
}

/// `HarnessAgent` is a first-class agent: invocation and inspection delegate to the
/// embedded `ReActAgent`, so an `Arc<HarnessAgent>` can be handed to a
/// `SubagentFactory` (mirrors Java `HarnessAgent implements Agent`).
impl Agent for HarnessAgent {
    fn agent_id(&self) -> &str {
        &self.agent_id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn interrupt(&self) {
        HarnessAgent::interrupt(self);
    }

    fn agent_state(&self) -> AgentState {
        self.agent
            .as_ref()
            .map(|a| a.agent_state())
            .unwrap_or_default()
    }

    fn toolkit(&self) -> &Toolkit {
        &self.toolkit
    }
}

#[async_trait::async_trait]
impl CallableAgent for HarnessAgent {
    async fn call(&self, messages: Vec<Msg>) -> agentscope_core::error::Result<Msg> {
        match &self.agent {
            Some(agent) => agent.call(messages).await,
            None => Err(no_model_error()),
        }
    }

    async fn call_with_schema(
        &self,
        messages: Vec<Msg>,
        schema: agentscope_core::formatter::JsonSchema,
    ) -> agentscope_core::error::Result<Msg> {
        match &self.agent {
            Some(agent) => agent.call_with_schema(messages, schema).await,
            None => Err(no_model_error()),
        }
    }
}

/// Error returned when a `HarnessAgent` was built without a model.
fn no_model_error() -> agentscope_core::error::Error {
    agentscope_core::error::Error::Agent("HarnessAgent has no model configured; cannot run".into())
}

/// Builder for `HarnessAgent`.
pub struct HarnessAgentBuilder {
    name: Option<String>,
    model: Option<Arc<dyn Model>>,
    /// Turns a subagent declaration's `model` id into a model (Java `Builder.modelResolver`).
    model_resolver: Option<Arc<dyn Fn(&str) -> Option<Arc<dyn Model>> + Send + Sync>>,
    filesystem: Option<Arc<dyn Filesystem>>,
    cwd: Option<PathBuf>,
    distributed_store: Option<DistributedStore>,
    /// Optional persistent state store wired into a checkpoint middleware.
    state_store: Option<Arc<dyn AgentStateStore>>,
    /// How a checkpoint save that loses the CAS race is resolved. Mirrors Java
    /// `HarnessAgent.Builder.conflictPolicy` (default `OVERWRITE`).
    conflict_policy: ConflictPolicy,
    /// Stable session key used for checkpoint save/restore (restart-resume).
    session_id: Option<String>,
    /// User namespace for checkpoint save/restore.
    user_id: Option<String>,
    with_filesystem_tools: bool,
    middleware: Option<MiddlewareChain>,
    sys_prompt: String,
    max_iters: i32,
    // ---- Harness orchestration fields ----
    workspace: Option<PathBuf>,
    environment_memory: Option<String>,
    max_context_tokens: usize,
    additional_context_files: Vec<String>,
    memory_config: Option<MemoryConfig>,
    tool_result_eviction_config: Option<ToolResultEvictionConfig>,
    /// Conversation-compaction configuration (`ConversationCompactionMiddleware`).
    /// This is the Java `CompactionMiddleware` equivalent.
    conversation_compaction_config: Option<super::memory::compaction::CompactionConfig>,
    plan_mode_enabled: bool,
    plan_mode_allow_shell: bool,
    plan_file_dir: String,
    disable_workspace_context: bool,
    disable_at_path_expansion: bool,
    disable_memory_hooks: bool,
    disable_tool_result_eviction: bool,
    disable_compaction: bool,
    disable_subagents: bool,
    disable_dynamic_subagents: bool,
    disable_shell_tool: bool,
    /// Compaction of grep-like `execute` output (see [`crate::tool::CompactConfig`]).
    shell_output_compaction: CompactConfig,
    agent_tracing_log_enabled: bool,
    with_shell_tool: bool,
    message_bus: Option<Arc<dyn MessageBus>>,
    async_tool_registry: Option<Arc<dyn AsyncToolRegistry>>,
    subagent_entries: Vec<SubagentEntry>,
    /// Programmatic subagent declarations, materialised into leaf agents at build time.
    subagent_declarations: Vec<SubagentDeclaration>,
    /// Caller-supplied subagent factories (`subagent_factory`).
    subagent_factories: Vec<CustomSubagentFactory>,
    leaf_subagent: bool,
    /// Explicit base toolkit; harness-registered tools are added on top of it.
    toolkit: Option<Toolkit>,
    // ---- Skill orchestration fields ----
    project_global_skills_dir: Option<PathBuf>,
    skill_repositories: Vec<Arc<dyn SkillRepository>>,
    disable_dynamic_skills: bool,
    disable_default_workspace_skills: bool,
    skill_manage_enabled: bool,
    skill_manage_config: Option<SkillManageConfig>,
    skill_curator_enabled: bool,
    skill_curator_config: Option<SkillCuratorConfig>,
    promotion_gate: Option<Arc<dyn SkillPromotionGate>>,
    visibility_filter: Option<Arc<dyn SkillVisibilityFilter>>,
    skill_filter: SkillFilter,
    confirm_handler: Option<agentscope_core::ConfirmHandler>,
    /// Optional initial permission context forwarded to the embedded ReActAgent.
    permission_context: Option<agentscope_core::PermissionContextState>,
    /// Per-call generation defaults (e.g. additional request headers) applied
    /// to the embedded ReActAgent at build time.
    generate_options_defaults: Option<agentscope_core::model::GenerateOptions>,
    // ---- Java parity setters (D2) ----
    agent_id: Option<String>,
    description: String,
    check_running: bool,
    enable_pending_tool_recovery: Option<bool>,
    enable_meta_tool: Option<bool>,
    enable_task_list: Option<bool>,
    use_legacy_xml_workspace_context: bool,
    async_tool_timeout: Option<Duration>,
    environment: Option<String>,
    disable_memory_tools: bool,
    disable_web_tools: bool,
    web_http_client: Option<reqwest::Client>,
    artifact_delivery_target: Option<Arc<dyn ArtifactDeliveryTarget>>,
    task_repository: Option<Arc<dyn TaskRepository>>,
    teams_client: Option<Arc<dyn TeamClient>>,
    teams_context: Option<TeamContext>,
    /// Context material registered for the context compiler. Empty means "not configured".
    context_sources: crate::agent::context::ContextSources,
    /// Budget policy for the context compiler; `None` derives everything from the model.
    context_policy: Option<crate::agent::context::ContextPolicy>,
    /// Deferred error from an invalid context registration, surfaced at `build()`.
    context_source_error: Option<String>,
    /// Native session journal backend. `None` means "use the workspace default", which
    /// `build()` materialises when history is journal-backed. Mirrors Java
    /// `HarnessAgent.Builder.sessionLogStore`.
    session_log_store: Option<Arc<dyn SessionLogStore>>,
    /// Exactly one history authority (default [`SessionHistoryMode::EventLog`]). Mirrors
    /// Java `HarnessAgent.Builder.sessionHistoryMode`.
    session_history_mode: SessionHistoryMode,
    /// Mirrors Java's currently inert `disableSessionPersistence` flag. It is retained
    /// and propagated to subagents so the configuration surface stays aligned.
    disable_session_persistence: bool,
    // ---- Graceful shutdown ----
    /// 是否挂载 `GracefulShutdownMiddleware`（请求登记 + shutdown 期间拒绝新请求）。
    graceful_shutdown_enabled: bool,
    /// 优雅关闭管理器（默认每个 agent 一个独立实例）。
    shutdown_manager: Option<Arc<GracefulShutdownManager>>,
    /// shutdown 收尾时调用的状态保存器。
    shutdown_state_saver: Option<Arc<dyn ShutdownStateSaver>>,
    /// shutdown 超时：超时后强制中断仍在运行的请求。
    shutdown_timeout: Option<Duration>,
}

impl Default for HarnessAgentBuilder {
    fn default() -> Self {
        Self {
            name: None,
            model: None,
            model_resolver: None,
            filesystem: None,
            cwd: None,
            distributed_store: None,
            state_store: None,
            conflict_policy: ConflictPolicy::default(),
            session_id: None,
            user_id: None,
            with_filesystem_tools: false,
            middleware: None,
            sys_prompt: String::new(),
            max_iters: 0,
            workspace: None,
            environment_memory: None,
            max_context_tokens: 8000,
            additional_context_files: Vec::new(),
            memory_config: None,
            tool_result_eviction_config: None,
            conversation_compaction_config: None,
            plan_mode_enabled: false,
            plan_mode_allow_shell: false,
            plan_file_dir: DEFAULT_PLAN_DIR.to_string(),
            disable_workspace_context: false,
            disable_at_path_expansion: false,
            disable_memory_hooks: false,
            disable_tool_result_eviction: false,
            disable_compaction: false,
            disable_subagents: false,
            disable_dynamic_subagents: false,
            disable_shell_tool: false,
            shell_output_compaction: CompactConfig::from_env(),
            agent_tracing_log_enabled: true,
            with_shell_tool: false,
            message_bus: None,
            async_tool_registry: None,
            subagent_entries: Vec::new(),
            subagent_declarations: Vec::new(),
            subagent_factories: Vec::new(),
            leaf_subagent: false,
            toolkit: None,
            project_global_skills_dir: None,
            skill_repositories: Vec::new(),
            disable_dynamic_skills: false,
            disable_default_workspace_skills: false,
            skill_manage_enabled: false,
            skill_manage_config: None,
            skill_curator_enabled: false,
            skill_curator_config: None,
            promotion_gate: None,
            visibility_filter: None,
            skill_filter: SkillFilter::all(),
            confirm_handler: None,
            permission_context: None,
            generate_options_defaults: None,
            agent_id: None,
            description: String::new(),
            check_running: true,
            enable_pending_tool_recovery: None,
            enable_meta_tool: None,
            enable_task_list: None,
            use_legacy_xml_workspace_context: false,
            async_tool_timeout: None,
            environment: None,
            disable_memory_tools: false,
            disable_web_tools: false,
            web_http_client: None,
            artifact_delivery_target: None,
            task_repository: None,
            teams_client: None,
            teams_context: None,
            context_sources: crate::agent::context::ContextSources::empty(),
            context_policy: None,
            context_source_error: None,
            session_log_store: None,
            session_history_mode: SessionHistoryMode::EventLog,
            disable_session_persistence: false,
            graceful_shutdown_enabled: false,
            shutdown_manager: None,
            shutdown_state_saver: None,
            shutdown_timeout: None,
        }
    }
}

impl HarnessAgentBuilder {
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn model(mut self, model: Arc<dyn Model>) -> Self {
        self.model = Some(model);
        self
    }

    /// Sets the resolver used to honour a subagent declaration's `model` override (mirrors Java
    /// `Builder.modelResolver`). Without one, `decl.model` is ignored and the child inherits the
    /// parent model.
    pub fn model_resolver<F>(mut self, resolver: F) -> Self
    where
        F: Fn(&str) -> Option<Arc<dyn Model>> + Send + Sync + 'static,
    {
        self.model_resolver = Some(Arc::new(resolver));
        self
    }
    pub fn filesystem(mut self, fs: Arc<dyn Filesystem>) -> Self {
        self.filesystem = Some(fs);
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn distributed_store(mut self, ds: DistributedStore) -> Self {
        self.distributed_store = Some(ds);
        self
    }

    /// Wires a persistent state store into the agent: the full `AgentState`
    /// (conversation history, task bag, …) is saved after every run and
    /// restored on the first run, enabling checkpoint/resume across restarts.
    ///
    /// Takes precedence over `distributed_store().agent_state_store` when both
    /// are configured. Pair with [`Self::session_id`]/[`Self::user_id`] for a
    /// stable restore key.
    pub fn state_store(mut self, store: Arc<dyn AgentStateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Sets how a checkpoint save resolves an optimistic-concurrency conflict.
    ///
    /// Defaults to [`ConflictPolicy::Overwrite`] (last-writer-wins, no error). Prefer
    /// [`ConflictPolicy::Fail`] when a distributed turn gate already serialises writers,
    /// and [`ConflictPolicy::AppendMerge`] for append-only turns. Mirrors Java
    /// `HarnessAgent.Builder.conflictPolicy`.
    pub fn conflict_policy(mut self, policy: ConflictPolicy) -> Self {
        self.conflict_policy = policy;
        self
    }

    /// Sets the stable session id used as the checkpoint save/restore key.
    /// A stable id lets a restarted process resume the same conversation.
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Sets the user namespace used as the checkpoint save/restore key.
    pub fn user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Sets the system prompt prepended to every reasoning call of the embedded ReAct agent.
    pub fn sys_prompt(mut self, sys_prompt: impl Into<String>) -> Self {
        self.sys_prompt = sys_prompt.into();
        self
    }

    /// Sets the maximum reasoning→acting iterations of the embedded ReAct agent.
    pub fn max_iters(mut self, max_iters: i32) -> Self {
        self.max_iters = max_iters;
        self
    }

    /// Registers the standard filesystem tools (`read_file`, `write_file`, ...) into the toolkit.
    pub fn register_filesystem_tools(mut self, on: bool) -> Self {
        self.with_filesystem_tools = on;
        self
    }

    /// Registers the shell-execution tool (`execute`) into the toolkit. Only meaningful when
    /// the configured filesystem supports shell execution (e.g. `SandboxBackedFilesystem`).
    pub fn register_shell_tool(mut self, on: bool) -> Self {
        self.with_shell_tool = on;
        self
    }

    pub fn middleware(mut self, mw: MiddlewareChain) -> Self {
        self.middleware = Some(mw);
        self
    }

    /// Registers a dynamic context source for the context compiler.
    ///
    /// Registering any source or instruction (or enabling a task projection, see
    /// [`HarnessAgentBuilder::task_context`]) opts this agent into the compiler: every model
    /// request is then assembled from material and fitted into the budget before dispatch.
    pub fn context_source(
        mut self,
        id: impl Into<String>,
        source: crate::agent::context::BoxedContextSource,
        options: crate::agent::context::ContextSourceOptions,
    ) -> Self {
        match self
            .context_sources
            .clone()
            .with_source(id, source, options)
        {
            Ok(next) => self.context_sources = next,
            Err(error) => self.context_source_error = Some(error.to_string()),
        }
        self
    }

    /// Registers a stable instruction (trusted System material) for the context compiler.
    pub fn instruction(mut self, id: impl Into<String>, content: impl Into<String>) -> Self {
        match self.context_sources.clone().with_instruction(id, content) {
            Ok(next) => self.context_sources = next,
            Err(error) => self.context_source_error = Some(error.to_string()),
        }
        self
    }

    /// Overrides the budget/accounting policy of the context compiler.
    pub fn context_policy(mut self, policy: crate::agent::context::ContextPolicy) -> Self {
        self.context_policy = Some(policy);
        self
    }

    /// Enables task projections inside the context compiler. Mirrors Java
    /// `HarnessAgent.Builder.taskContext`.
    ///
    /// Projections only exist inside the compiler, so enabling one opts the agent in even when
    /// no source or instruction is registered.
    pub fn task_context(mut self, options: crate::agent::context::TaskContextOptions) -> Self {
        self.context_sources = self.context_sources.clone().with_task_context(options);
        self
    }

    // ---- Harness orchestration setters ----

    /// Sets the workspace directory. When left unset, defaults to `{cwd}/.agentscope/workspace`.
    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Sets the environment-memory context injected into the workspace system prompt.
    pub fn environment_memory(mut self, environment_memory: impl Into<String>) -> Self {
        self.environment_memory = Some(environment_memory.into());
        self
    }

    /// Sets the maximum token budget for workspace context (default 8000).
    pub fn max_context_tokens(mut self, max_tokens: usize) -> Self {
        self.max_context_tokens = max_tokens;
        self
    }

    /// Adds a custom context file (workspace-relative) loaded into the system prompt.
    pub fn additional_context_file(mut self, relative_path: impl Into<String>) -> Self {
        self.additional_context_files.push(relative_path.into());
        self
    }

    /// Overrides the long-term memory pipeline configuration (flush + consolidation).
    pub fn memory(mut self, config: MemoryConfig) -> Self {
        self.memory_config = Some(config);
        self
    }

    /// Enables conversation compaction with the given config.
    ///
    /// This is the Java `HarnessAgent.Builder.compaction(CompactionConfig)` equivalent:
    /// at the start of each agent call the accumulated history is summarized (LLM)
    /// and replaced with `[summary] + preserved tail` once it exceeds the trigger.
    pub fn compaction(mut self, config: super::memory::compaction::CompactionConfig) -> Self {
        self.conversation_compaction_config = Some(config);
        self
    }

    /// Alias for [`Self::compaction`], kept for callers that used the older name.
    pub fn conversation_compaction(
        self,
        config: super::memory::compaction::CompactionConfig,
    ) -> Self {
        self.compaction(config)
    }

    /// Disables conversation compaction entirely.
    ///
    /// Mirrors Java `HarnessAgent.Builder.disableCompaction()`: it gates the real
    /// compaction middleware, not just a prompt marker.
    pub fn disable_compaction(mut self) -> Self {
        self.disable_compaction = true;
        self
    }

    /// Overrides the tool-result eviction configuration.
    pub fn tool_result_eviction(mut self, config: ToolResultEvictionConfig) -> Self {
        self.tool_result_eviction_config = Some(config);
        self
    }

    /// Disables tool-result eviction entirely.
    pub fn disable_tool_result_eviction(mut self) -> Self {
        self.disable_tool_result_eviction = true;
        self
    }

    /// Enables plan mode (read-only design phase).
    pub fn enable_plan_mode(mut self) -> Self {
        self.plan_mode_enabled = true;
        self
    }

    /// Allows the shell tool to run while plan mode is active (default: denied).
    pub fn allow_shell_in_plan_mode(mut self) -> Self {
        self.plan_mode_allow_shell = true;
        self
    }

    /// Sets the workspace-relative directory for plan files (default `plans`).
    pub fn plan_file_directory(mut self, dir: impl Into<String>) -> Self {
        let dir = dir.into();
        if !dir.trim().is_empty() {
            self.plan_file_dir = dir;
        }
        self
    }

    /// Skips registration of the workspace-context middleware.
    pub fn disable_workspace_context(mut self) -> Self {
        self.disable_workspace_context = true;
        self
    }

    /// Skips registration of the `@`-path expansion middleware.
    pub fn disable_at_path_expansion(mut self) -> Self {
        self.disable_at_path_expansion = true;
        self
    }

    /// Disables the memory flush + consolidation hooks.
    pub fn disable_memory_hooks(mut self) -> Self {
        self.disable_memory_hooks = true;
        self
    }

    /// Skips registration of the shell tool (only relevant when shell is supported).
    pub fn disable_shell_tool(mut self) -> Self {
        self.disable_shell_tool = true;
        self
    }

    /// Overrides how `execute` output is compacted before it reaches the model.
    ///
    /// Compaction collapses repeated `path:line:text` prefixes of search-like output
    /// into per-path headings. It is on by default; [`CompactConfig::disabled`] turns
    /// it off, and `AGENTSCOPE_OUTPUT_COMPACT=0` does the same for the environment.
    pub fn shell_output_compaction(mut self, config: CompactConfig) -> Self {
        self.shell_output_compaction = config;
        self
    }

    /// Skips registration of the subagent orchestration middleware.
    pub fn disable_subagents(mut self) -> Self {
        self.disable_subagents = true;
        self
    }

    /// Uses the static `SubagentsMiddleware` instead of the dynamic variant.
    pub fn disable_dynamic_subagents(mut self) -> Self {
        self.disable_dynamic_subagents = true;
        self
    }

    /// Enables or disables agent execution trace logging (default `true`).
    pub fn enable_agent_tracing_log(mut self, enabled: bool) -> Self {
        self.agent_tracing_log_enabled = enabled;
        self
    }

    /// Sets an explicit message bus for inbox-based delivery.
    pub fn message_bus(mut self, bus: Arc<dyn MessageBus>) -> Self {
        self.message_bus = Some(bus);
        self
    }

    /// Sets an explicit async-tool registry.
    pub fn async_tool_registry(mut self, registry: Arc<dyn AsyncToolRegistry>) -> Self {
        self.async_tool_registry = Some(registry);
        self
    }

    /// Registers static subagent entries.
    pub fn subagent_entries(mut self, entries: Vec<SubagentEntry>) -> Self {
        self.subagent_entries = entries;
        self
    }

    /// Sets the base toolkit every harness tool is registered onto. Children materialised
    /// from declarations inherit a (filtered) copy of it (mirrors Java `Builder.toolkit`).
    pub fn toolkit(mut self, toolkit: Toolkit) -> Self {
        self.toolkit = Some(toolkit);
        self
    }

    /// Adds a subagent declaration, materialised into a leaf agent on spawn
    /// (mirrors Java `Builder.subagent`).
    pub fn subagent(mut self, declaration: SubagentDeclaration) -> Self {
        self.subagent_declarations.push(declaration);
        self
    }

    /// Adds several subagent declarations (mirrors Java `Builder.subagents`).
    pub fn subagents(mut self, declarations: Vec<SubagentDeclaration>) -> Self {
        self.subagent_declarations.extend(declarations);
        self
    }

    /// Adds a fully custom subagent factory for a given agent id, with no description
    /// (mirrors Java `Builder.subagentFactory(name, factory)`).
    pub fn subagent_factory<F>(self, name: impl Into<String>, factory: F) -> Self
    where
        F: Fn(String) -> Arc<dyn CallableAgent> + Send + Sync + 'static,
    {
        self.subagent_factory_with_description(name, "", factory)
    }

    /// Adds a fully custom subagent factory with the description shown to the orchestrator
    /// (mirrors Java `Builder.subagentFactory(name, description, factory)`).
    pub fn subagent_factory_with_description<F>(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        factory: F,
    ) -> Self
    where
        F: Fn(String) -> Arc<dyn CallableAgent> + Send + Sync + 'static,
    {
        self.subagent_factories.push(CustomSubagentFactory {
            name: name.into(),
            description: description.into(),
            factory: Arc::new(factory),
        });
        self
    }

    /// Marks this build as a leaf subagent (no nested subagent orchestration).
    pub fn as_leaf_subagent(mut self) -> Self {
        self.leaf_subagent = true;
        self
    }

    // ---- Skill orchestration setters (mirror the Java HarnessAgent builder) ----

    /// Sets the project-global skills directory (Layer 1).
    pub fn project_global_skills_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.project_global_skills_dir = Some(dir.into());
        self
    }

    /// Adds a marketplace / external skill repository.
    pub fn skill_repository(mut self, repo: Arc<dyn SkillRepository>) -> Self {
        self.skill_repositories.push(repo);
        self
    }

    /// Adds several marketplace / external skill repositories.
    pub fn skill_repositories(mut self, repos: Vec<Arc<dyn SkillRepository>>) -> Self {
        self.skill_repositories.extend(repos);
        self
    }

    /// Disables the dynamic (promotable) skill layer entirely.
    pub fn disable_dynamic_skills(mut self) -> Self {
        self.disable_dynamic_skills = true;
        self
    }

    /// Opts out of the default read-only workspace skill repository (Layer 4).
    pub fn disable_default_workspace_skills(mut self) -> Self {
        self.disable_default_workspace_skills = true;
        self
    }

    /// Enables the self-learning `skill_manage` / `propose_skill` tools.
    pub fn enable_skill_manage_tool(mut self, config: SkillManageConfig) -> Self {
        self.skill_manage_enabled = true;
        self.skill_manage_config = Some(config);
        self
    }

    /// Enables the self-learning tools with the given auto-promote flag.
    pub fn enable_skill_manage_tool_auto_promote(mut self, auto_promote: bool) -> Self {
        self.skill_manage_enabled = true;
        self.skill_manage_config =
            Some(SkillManageConfig::defaults().with_auto_promote(auto_promote));
        self
    }

    /// Enables the background skill curator.
    pub fn enable_skill_curator(mut self, config: SkillCuratorConfig) -> Self {
        self.skill_curator_enabled = true;
        self.skill_curator_config = Some(config);
        self
    }

    /// Configures the promotion gate and visibility filter for skill promotion.
    pub fn enable_skill_promotion_gate(
        mut self,
        gate: Arc<dyn SkillPromotionGate>,
        visibility_filter: Arc<dyn SkillVisibilityFilter>,
    ) -> Self {
        self.promotion_gate = Some(gate);
        self.visibility_filter = Some(visibility_filter);
        self
    }

    /// Sets an explicit base skill filter (allow/deny by name).
    pub fn skill_filter(mut self, filter: SkillFilter) -> Self {
        self.skill_filter = filter;
        self
    }

    /// Limits visible skills to the named set.
    pub fn enable_skills(mut self, names: &[&str]) -> Self {
        self.skill_filter = SkillFilter::only(names);
        self
    }

    /// Hides the named skills.
    pub fn disable_skills(mut self, names: &[&str]) -> Self {
        self.skill_filter = SkillFilter::except(names);
        self
    }

    /// Sets an explicit permission context forwarded to the embedded ReActAgent
    /// (mode + allow/deny/ask rules). When left unset the agent uses the default
    /// trivial context (everything allowed).
    pub fn permission_context(mut self, ctx: agentscope_core::PermissionContextState) -> Self {
        self.permission_context = Some(ctx);
        self
    }

    /// Sets the confirmation handler used when a tool call requires user approval
    /// (`PermissionBehavior::Ask`). Forwarded to the embedded ReActAgent at build.
    pub fn confirm_handler(mut self, handler: agentscope_core::ConfirmHandler) -> Self {
        self.confirm_handler = Some(handler);
        self
    }

    /// Sets per-call generation defaults (e.g. additional request headers)
    /// merged into every model request of the embedded agent.
    pub fn generate_options_defaults(
        mut self,
        options: agentscope_core::model::GenerateOptions,
    ) -> Self {
        self.generate_options_defaults = Some(options);
        self
    }

    // ---- Java-parity setters (D2) ----

    /// Sets the stable agent id (mirrors Java `agentId`). Defaults to `name` when unset.
    /// Used as the state/transcript/task scope key.
    pub fn agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Sets the human-readable description forwarded to the embedded ReAct agent.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Whether the embedded agent rejects a new run while another is in flight
    /// (mirrors Java `checkRunning`; stored config).
    pub fn check_running(mut self, check_running: bool) -> Self {
        self.check_running = check_running;
        self
    }

    /// Enables recovery of pending (unanswered) tool calls across invocations.
    pub fn enable_pending_tool_recovery(mut self, enable: bool) -> Self {
        self.enable_pending_tool_recovery = Some(enable);
        self
    }

    /// Registers the `reset_equipped_tools` meta tool so the agent can manage its own
    /// tool groups (mirrors Java `enableMetaTool`).
    pub fn enable_meta_tool(mut self, enable: bool) -> Self {
        self.enable_meta_tool = Some(enable);
        self
    }

    /// Enables the built-in task list tool (`todo_write` + reminder middleware),
    /// mirroring Java `enableTaskList`.
    pub fn enable_task_list(mut self, enabled: bool) -> Self {
        self.enable_task_list = Some(enabled);
        self
    }

    /// Requests legacy XML workspace-context rendering (stored config; mirrors Java
    /// `useLegacyXmlWorkspaceContext`).
    pub fn use_legacy_xml_workspace_context(mut self, enabled: bool) -> Self {
        self.use_legacy_xml_workspace_context = enabled;
        self
    }

    /// Sets the timeout applied to async tool executions (mirrors Java `asyncToolTimeout`).
    pub fn async_tool_timeout(mut self, timeout: Duration) -> Self {
        self.async_tool_timeout = Some(timeout);
        self
    }

    /// Sets the deployment environment label (mirrors Java `environment`; default `"prod"`).
    pub fn environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }

    /// Disables the built-in long-term memory tools (`memory_search`/`memory_get`/`memory_save`).
    pub fn disable_memory_tools(mut self) -> Self {
        self.disable_memory_tools = true;
        self
    }

    /// Disables the built-in web tools (`web_fetch`/`web_search`).
    pub fn disable_web_tools(mut self) -> Self {
        self.disable_web_tools = true;
        self
    }

    /// Supplies the shared HTTP client used by the built-in web tools.
    pub fn web_http_client(mut self, client: reqwest::Client) -> Self {
        self.web_http_client = Some(client);
        self
    }

    /// Configures the destination used by the `deliver_artifact` tool.
    ///
    /// Mirrors Java `HarnessAgent.Builder.artifactDeliveryTarget`: when set, the
    /// tool is registered and the sandbox workspace prompt receives the File
    /// Isolation Notice.
    pub fn artifact_delivery_target(mut self, target: Arc<dyn ArtifactDeliveryTarget>) -> Self {
        self.artifact_delivery_target = Some(target);
        self
    }

    /// Uses a caller-supplied task repository instead of the default
    /// `WorkspaceTaskRepository`.
    pub fn task_repository(mut self, repo: Arc<dyn TaskRepository>) -> Self {
        self.task_repository = Some(repo);
        self
    }

    /// Installs a native session journal backend. Mirrors Java
    /// `HarnessAgent.Builder.sessionLogStore`.
    ///
    /// Leaving this unset is the common case: `build()` falls back to a
    /// [`WorkspaceSessionLogStore`](crate::agent::session::WorkspaceSessionLogStore) over the
    /// configured filesystem, so journaling is on by default (Java does the same).
    pub fn session_log_store(mut self, store: Arc<dyn SessionLogStore>) -> Self {
        self.session_log_store = Some(store);
        self
    }

    /// Selects the single history authority. Mirrors Java
    /// `HarnessAgent.Builder.sessionHistoryMode`; defaults to
    /// [`SessionHistoryMode::EventLog`].
    ///
    /// [`SessionHistoryMode::Legacy`] exists only as an explicit opt-out: it keeps the
    /// state-store checkpoint as the history authority and installs no journal.
    pub fn session_history_mode(mut self, mode: SessionHistoryMode) -> Self {
        self.session_history_mode = mode;
        self
    }

    /// Mirrors Java `HarnessAgent.Builder.disableSessionPersistence`.
    ///
    /// Java currently retains this flag without consuming it in the main build path;
    /// Rust keeps the same configuration surface and propagates it to subagents.
    pub fn disable_session_persistence(mut self) -> Self {
        self.disable_session_persistence = true;
        self
    }

    // ---- Graceful shutdown setters ----

    /// 挂载优雅关闭中间件。
    ///
    /// 开启后：每一次 agent 调用都会被登记（per-request 粒度），调用结束
    /// （成功 / 失败 / 取消）时自动注销；关闭进行中时新调用会被明确拒绝。
    pub fn graceful_shutdown(mut self, enabled: bool) -> Self {
        self.graceful_shutdown_enabled = enabled;
        self
    }

    /// 指定优雅关闭管理器。
    ///
    /// 默认每个 agent 一个独立实例；传入全局单例
    /// （`GracefulShutdownManager::instance_arc()`）可以让多个 agent 共享同一套
    /// 关闭生命周期。
    pub fn shutdown_manager(mut self, manager: Arc<GracefulShutdownManager>) -> Self {
        self.shutdown_manager = Some(manager);
        self
    }

    /// 指定 shutdown 收尾时调用的状态保存器。
    ///
    /// 保存器在"所有请求结束之后、状态切到 `Terminated` 之前"被调用一次；保存失败
    /// 只记录日志，不会掩盖关闭流程本身的错误。
    pub fn shutdown_state_saver(mut self, saver: Arc<dyn ShutdownStateSaver>) -> Self {
        self.shutdown_state_saver = Some(saver);
        self
    }

    /// 设置 shutdown 超时。
    ///
    /// 超时后仍在运行的请求会被强制中断，状态直接切到 `Terminated`。
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = Some(timeout);
        self
    }

    /// Enables agent-teams mode: attaches `TeamsMiddleware` and registers the
    /// role-clipped `team` tool. Mirrors Java `teamsMode(client, context)`.
    pub fn teams_mode(mut self, client: Arc<dyn TeamClient>, context: TeamContext) -> Self {
        self.teams_client = Some(client);
        self.teams_context = Some(context);
        self
    }

    /// Resolves the workspace directory when none was set explicitly:
    /// `{cwd}/.agentscope/workspace` (mirrors the Java default).
    fn resolve_default_workspace(&self, cwd: &PathBuf) -> PathBuf {
        cwd.join(".agentscope").join("workspace")
    }

    /// Builds the `HarnessAgent`, wiring the filesystem, tools, middleware chain and
    /// embedded ReAct agent according to the configured orchestration.
    pub fn build(mut self) -> HarnessAgent {
        let cwd = self
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let filesystem = self
            .filesystem
            .clone()
            .unwrap_or_else(|| Arc::new(LocalFilesystem::new(cwd.clone())));
        let resolved_workspace = self
            .workspace
            .clone()
            .unwrap_or_else(|| self.resolve_default_workspace(&cwd));
        let name = self.name.clone().unwrap_or_else(|| "agent".to_string());
        let agent_id = self.agent_id.clone().unwrap_or_else(|| name.clone());
        let description = self.description.clone();
        let environment = self
            .environment
            .clone()
            .unwrap_or_else(|| "prod".to_string());

        // Toolkit deep-copy: each agent gets its own toolkit so harness-registered tools
        // never bleed across builds. A caller-supplied base toolkit is copied in first so
        // subagents declared on it inherit exactly the parent's tools (Java `Builder.toolkit`).
        let toolkit = self.toolkit.clone().unwrap_or_default();

        // ---- Workspace + filesystem orchestration ----
        let workspace_index = WorkspaceIndex::open(&resolved_workspace);
        let workspace_manager = Arc::new(WorkspaceManager::with_index(
            resolved_workspace.clone(),
            Some(Arc::clone(&filesystem)),
            workspace_index,
        ));
        let _ = workspace_manager.validate();

        // ---- Native session journal ----
        // History is journal-backed by default. Resolving the store once keeps the ReAct
        // agent and the session tools on the exact same backend, so a tool can never read a
        // different store than the one the agent wrote (Java `HarnessAgent.build()`).
        let session_log_store: Option<Arc<dyn SessionLogStore>> =
            if self.session_history_mode == SessionHistoryMode::EventLog {
                Some(self.session_log_store.clone().unwrap_or_else(|| {
                    Arc::new(super::session::WorkspaceSessionLogStore::new(Arc::clone(
                        &filesystem,
                    ))) as Arc<dyn SessionLogStore>
                }))
            } else {
                None
            };

        // ---- Middleware chain ----
        let mut middleware = self.middleware.unwrap_or_default();

        // ---- Graceful shutdown ----
        // 每个 agent 默认拥有独立的关闭管理器；宿主可以显式共享全局单例。
        let shutdown_manager = self
            .shutdown_manager
            .clone()
            .unwrap_or_else(|| Arc::new(GracefulShutdownManager::new()));
        if let Some(timeout) = self.shutdown_timeout {
            let mut config = shutdown_manager.config_snapshot();
            config.shutdown_timeout = Some(timeout);
            shutdown_manager.set_config_now(config);
        }
        if self.graceful_shutdown_enabled {
            // 中间件的 order 最高，因此它包住整条调用链：先登记请求，最后注销。
            middleware.add(Box::new(GracefulShutdownMiddleware::new(Arc::clone(
                &shutdown_manager,
            ))));
        }

        // A deferred registration error must surface here rather than silently dropping the
        // caller's context source. `build()` is infallible by contract, and an invalid context
        // registration is a programming error (Java throws `IllegalArgumentException`).
        if let Some(error) = self.context_source_error.take() {
            panic!("invalid context registration: {error}");
        }

        // ---- Context compiler ----
        // The compiler is opt-in: an agent with no registered material keeps the plain
        // reasoning path. Once material is configured, every model request is assembled and
        // budgeted here, which is the only place all middleware rewrites are already applied.
        //
        // It is *registered last* (see the end of the chain): `wrap_model_call` wraps in
        // reverse, so the last registration is the innermost model-call middleware — the compiler
        // must measure the request exactly as the provider will receive it. Note that
        // `OnReasoning` rewrites (tool-pair sanitising, eviction, compaction) already run before
        // any `OnModelCall` middleware by extension-point ordering, so the compiler always sees
        // their output regardless of registration order.
        let context_pipeline = if !self.context_sources.is_empty()
            || self.context_sources.task_context().is_enabled()
        {
            let config = crate::agent::context::ContextPipelineConfig {
                policy: self.context_policy.take().unwrap_or_default(),
                sources: self.context_sources.clone(),
                ..Default::default()
            };
            let mut pipeline = crate::agent::context::ContextPipeline::new(config);
            // Compiler-driven eviction uses the same thresholds as the middleware: one
            // policy, two call sites (the compiler runs first).
            if !self.disable_tool_result_eviction {
                if let Some(cfg) = self.tool_result_eviction_config.clone() {
                    pipeline = pipeline.with_eviction(crate::agent::context::ContextEviction {
                        fs: Arc::clone(&filesystem),
                        config: cfg,
                    });
                }
            }
            Some(Arc::new(pipeline))
        } else {
            None
        };

        // Keep every outgoing model request provider-valid: strip trailing assistant
        // `tool_calls` that were never answered and orphan TOOL results (e.g. after
        // checkpoint restore / eviction artifacts) so the next call cannot fail with
        // HTTP 400 "Messages with role 'tool' must be a response to a preceding
        // message with 'tool_calls'".
        middleware.add(Box::new(ToolCallPairSanitizerMiddleware::new()));

        if self.agent_tracing_log_enabled {
            middleware.add(Box::new(AgentTraceMiddleware::new()));
        }
        if !self.disable_workspace_context {
            let mut ws_ctx = WorkspaceContextMiddleware::new_with_config(
                Arc::clone(&workspace_manager),
                &name,
                self.environment_memory.clone(),
                self.max_context_tokens,
            );
            ws_ctx.set_memory_flags(self.disable_memory_tools, self.disable_memory_hooks);
            ws_ctx.set_artifact_delivery_enabled(self.artifact_delivery_target.is_some());
            ws_ctx.set_use_legacy_xml_workspace_context(self.use_legacy_xml_workspace_context);
            ws_ctx.set_additional_context_files(self.additional_context_files.clone());
            middleware.add(Box::new(ws_ctx));
        }
        if !self.disable_at_path_expansion {
            middleware.add(Box::new(AtPathExpansionMiddleware::new(Arc::clone(
                &filesystem,
            ))));
        }

        // ---- Memory pipeline (flush + consolidation + maintenance) ----
        let memory_config = self.memory_config.clone().unwrap_or_default();
        let memory_model = memory_config.model.clone().or_else(|| self.model.clone());
        if !self.disable_memory_hooks {
            if let Some(memory_model) = memory_model {
                let flush_prompt = memory_config
                    .flush_prompt
                    .clone()
                    .unwrap_or_else(|| MemoryFlushManager::default_flush_prompt());
                middleware.add(Box::new(MemoryFlushMiddleware::new_with_config(
                    Arc::clone(&filesystem),
                    Arc::clone(&memory_model),
                    flush_prompt,
                    memory_config.flush_trigger.clone(),
                    IsolationScope::User,
                )));
                let consolidator = MemoryConsolidator::new_with_prompt(
                    Arc::clone(&filesystem),
                    Arc::clone(&memory_model),
                    memory_config.consolidation_prompt.clone(),
                    memory_config.consolidation_max_tokens,
                );
                middleware.add(Box::new(MemoryMaintenanceMiddleware::new_with_config(
                    Arc::clone(&filesystem),
                    Some(Arc::new(consolidator)),
                    memory_config.daily_file_retention_days,
                    memory_config.session_retention_days,
                    memory_config.consolidation_min_gap,
                    IsolationScope::User,
                )));
            }
        }

        // ---- Conversation compaction ----
        // Java `CompactionMiddleware` performs the real summarize-and-replace pass.
        // `disable_compaction()` must gate this path, not a prompt marker.
        //
        // The same compactor is handed to the context compiler as the budget-overflow
        // fallback: if a request still exceeds the model budget (a long tool loop, a
        // threshold drift, a model switch), the request is compacted and retried
        // instead of surfacing a hard error.
        let mut overflow_compaction: Option<(Arc<ConversationCompactor>, CompactionConfig)> = None;
        if !self.disable_compaction {
            if let Some(cfg) = self.conversation_compaction_config.clone() {
                if let Some(model) = self.model.clone() {
                    let compactor = Arc::new(ConversationCompactor::with_model(model));
                    middleware.add(Box::new(ConversationCompactionMiddleware::new(
                        Arc::clone(&compactor),
                        cfg.clone(),
                        name.clone(),
                    )));
                    overflow_compaction = Some((compactor, cfg));
                }
            }
        }

        // ---- Tool-result eviction ----
        if !self.disable_tool_result_eviction {
            if let Some(cfg) = self.tool_result_eviction_config.clone() {
                middleware.add(Box::new(ToolResultEvictionMiddleware::new(
                    Arc::clone(&filesystem),
                    cfg,
                )));
            }
        }

        // ---- Message bus / inbox ----
        let message_bus = self.message_bus.clone().or_else(|| {
            Some(Arc::new(WorkspaceMessageBus::new(
                Arc::clone(&filesystem),
                ".agentscope/bus",
            )) as Arc<dyn MessageBus>)
        });
        let async_tool_registry = self.async_tool_registry.clone().or_else(|| {
            Some(Arc::new(WorkspaceAsyncToolRegistry::new(
                Arc::clone(&filesystem),
                ".agentscope/bus/async-tools",
            )) as Arc<dyn AsyncToolRegistry>)
        });
        if let Some(bus) = message_bus.clone() {
            let mut inbox = InboxMiddleware::builder(bus);
            if let Some(reg) = async_tool_registry.clone() {
                inbox = inbox.async_tool_registry(reg);
            }
            middleware.add(Box::new(inbox.build()));
        }

        // ---- Agent teams (teamsMode) ----
        if let (Some(client), Some(context)) =
            (self.teams_client.clone(), self.teams_context.clone())
        {
            toolkit.register_tool(Arc::new(TeamTool::new(Arc::clone(&client))));
            middleware.add(Box::new(TeamsMiddleware::from_context(
                client,
                &context,
                agent_id.clone(),
            )));
        }

        // ---- Subagent orchestration ----
        // Wired below, after every tool has been registered, so children materialised
        // from declarations inherit a complete (non-empty) toolkit.

        // ---- Skills ----
        let workspace_skills_dir = Some(resolved_workspace.join(".agents").join("skills"));
        let workspace_namespace_dir = resolved_workspace.join(".agents").join("skills");

        let mut ordered_skill_repos: Vec<Arc<dyn SkillRepository>> = compose_skill_repositories(
            self.project_global_skills_dir.clone(),
            &self.skill_repositories,
            workspace_skills_dir.clone(),
            workspace_namespace_dir.clone(),
            self.disable_default_workspace_skills,
        );

        let mut pending_skill_promoter: Option<Arc<SkillPromoter>> = None;
        // Late-bound slot so `skill_manage` can invalidate the skill middleware's
        // lazily-built catalog cache after it writes skills (the middleware is
        // constructed after the manage tool in this builder).
        let skill_mw_slot: Arc<Mutex<Option<Arc<HarnessSkillMiddleware>>>> =
            Arc::new(Mutex::new(None));
        let mut pending_skill_usage_store: Option<Arc<Mutex<dyn SkillUsageStore>>> = None;
        let mut pending_skill_curator: Option<Arc<SkillCurator>> = None;
        let mut pending_skill_audit_log: Option<Arc<SkillAuditLog>> = None;

        if self.skill_manage_enabled {
            let sm_config = self
                .skill_manage_config
                .clone()
                .unwrap_or_else(SkillManageConfig::defaults);
            let main_dir = if sm_config.main_dir.is_absolute() {
                sm_config.main_dir.clone()
            } else {
                resolved_workspace.join(&sm_config.main_dir)
            };
            let drafts_dir = if sm_config.drafts_dir.is_absolute() {
                sm_config.drafts_dir.clone()
            } else {
                resolved_workspace.join(&sm_config.drafts_dir)
            };

            // Replace the read-only workspace layer (Layer 4) with a writable main repo.
            let mut main_writable: Option<Arc<dyn SkillRepository>> = None;
            for i in (0..ordered_skill_repos.len()).rev() {
                if ordered_skill_repos[i].repo_id() == "workspace-namespaced"
                    && !ordered_skill_repos[i].is_writeable()
                {
                    main_writable = Some(Arc::new(WorkspaceSkillRepository::new_writable(
                        main_dir.clone(),
                        "workspace-writable",
                    )));
                    ordered_skill_repos[i] = Arc::clone(main_writable.as_ref().unwrap());
                    break;
                }
            }
            if main_writable.is_none() {
                main_writable = Some(Arc::new(WorkspaceSkillRepository::new_writable(
                    main_dir.clone(),
                    "workspace-writable",
                )));
                ordered_skill_repos.push(Arc::clone(main_writable.as_ref().unwrap()));
            }
            let main_writable = main_writable.unwrap();
            let drafts_writable: Arc<dyn SkillRepository> = Arc::new(
                WorkspaceSkillRepository::new_writable(drafts_dir.clone(), "workspace-drafts"),
            );

            // Persistent usage store (mirrors Java `new SkillUsageStore(filesystem)`).
            let usage_store: Arc<Mutex<dyn SkillUsageStore>> = Arc::new(Mutex::new(
                FilesystemSkillUsageStore::at_workspace(resolved_workspace.clone()),
            ));
            let audit_log = Arc::new(SkillAuditLog::new(resolved_workspace.clone()));

            let gate: Arc<dyn SkillPromotionGate> =
                self.promotion_gate.clone().unwrap_or_else(default_gate);

            let manage_tool = SkillManageTool::new(
                Arc::clone(&main_writable),
                Arc::clone(&drafts_writable),
                sm_config,
            )
            .with_usage_store(Arc::clone(&usage_store))
            .with_audit_log(Arc::clone(&audit_log))
            .with_on_write({
                let slot = Arc::clone(&skill_mw_slot);
                Arc::new(move || {
                    if let Some(mw) = slot.lock().unwrap().as_ref() {
                        mw.invalidate();
                    }
                })
            });
            let promoter = build_promoter(
                Arc::clone(&drafts_writable),
                Arc::clone(&main_writable),
                Arc::clone(&gate),
                Some(Arc::clone(&usage_store)),
                Some(Arc::clone(&audit_log)),
            );
            let manage_tool = Arc::new(manage_tool.with_promoter(Arc::clone(&promoter)));

            let manage_tool_for_registry: Arc<dyn Tool> = manage_tool.clone();
            toolkit.register_tool(manage_tool_for_registry);
            let propose_tool: Arc<dyn Tool> =
                Arc::new(ProposeSkillTool::new(Arc::clone(&manage_tool)));
            toolkit.register_tool(propose_tool);

            middleware.add(Box::new(SkillUsageMiddleware::new(
                Arc::clone(&usage_store),
                ordered_skill_repos
                    .iter()
                    .flat_map(|r| r.load().into_iter().map(|s| s.name().to_string()))
                    .collect(),
            )));

            if self.skill_curator_enabled {
                let main_for_archive = Arc::clone(&main_writable);
                let archive: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |name: &str| {
                    let _ = main_for_archive.delete_skill(name);
                });
                let curator = Arc::new(SkillCurator::new(
                    Arc::clone(&filesystem),
                    Arc::clone(&usage_store),
                    self.skill_curator_config
                        .unwrap_or_else(SkillCuratorConfig::defaults),
                    archive,
                ));
                pending_skill_curator = Some(Arc::clone(&curator));
                middleware.add(Box::new(SkillCuratorMiddleware::new(curator)));
            }

            pending_skill_promoter = Some(promoter);
            pending_skill_usage_store = Some(usage_store);
            pending_skill_audit_log = Some(audit_log);
        }

        if !ordered_skill_repos.is_empty() {
            let shell_policy = if self.disable_shell_tool || !self.with_shell_tool {
                crate::agent::skill::ShellPathPolicy::no_shell()
            } else {
                crate::agent::skill::ShellPathPolicy::local_with_shell(resolved_workspace.clone())
            };
            let skill_mw = if self.disable_dynamic_skills {
                HarnessSkillMiddleware::frozen(
                    ordered_skill_repos.clone(),
                    self.skill_filter.clone(),
                    self.visibility_filter.clone(),
                    shell_policy,
                    Some(resolved_workspace.clone()),
                )
            } else {
                HarnessSkillMiddleware::new(
                    ordered_skill_repos.clone(),
                    self.skill_filter.clone(),
                    self.visibility_filter.clone(),
                    shell_policy,
                    Some(resolved_workspace.clone()),
                )
            };
            // 嵌入式优化：不再于启动时全量扫描技能目录，改为惰性工具，
            // 首次调用 load_skill_through_path 时才构建索引（内部缓存）。
            let skill_mw = Arc::new(skill_mw);
            // Bind the (already-constructed) `skill_manage` tool to this middleware so
            // skill writes drop the cached catalog.
            *skill_mw_slot.lock().unwrap() = Some(Arc::clone(&skill_mw));
            toolkit.register_tool(Arc::new(LazyLoadSkillTool::new(Arc::clone(&skill_mw))));
            middleware.add_arc(skill_mw as Arc<dyn agentscope_core::middleware::Middleware>);
        }

        // ---- Toolkit (filesystem / shell tools) ----
        if self.with_filesystem_tools {
            FilesystemTool::register(&toolkit, Arc::clone(&filesystem));
        }
        if self.with_shell_tool && !self.disable_shell_tool {
            if let Some(sandbox) = sandbox_filesystem(&filesystem, &cwd) {
                toolkit.register_tool(Arc::new(ShellExecuteTool::with_output_compaction(
                    sandbox,
                    self.shell_output_compaction.clone(),
                )));
            }
        }
        if let Some(target) = &self.artifact_delivery_target {
            toolkit.register_tool(Arc::new(ArtifactDeliveryTool::new(
                Arc::clone(&filesystem),
                Arc::clone(target),
            )));
        }

        // ---- Long-term memory tools (Java registers these by default) ----
        // Registered against the `WorkspaceManager` (not the raw filesystem) so they
        // mirror Java's `MEMORY.md` + `memory/*.md` read/append semantics.
        if !self.disable_memory_tools {
            MemorySearchTool::register(&toolkit, Arc::clone(&workspace_manager));
            MemoryGetTool::register(&toolkit, Arc::clone(&workspace_manager));
            MemorySaveTool::register(&toolkit, Arc::clone(&workspace_manager));
            // Session transcript tools (Java registers `SessionSearchTool` by default in the
            // same `!disableMemoryTools` block, and only when history is journal-backed).
            if let Some(store) = &session_log_store {
                SessionSearchTool::register_all(&toolkit, Arc::clone(store));
            }
        }

        // ---- Web tools (Java registers these by default) ----
        if !self.disable_web_tools {
            match &self.web_http_client {
                Some(client) => {
                    toolkit.register_tool(Arc::new(
                        WebFetchTool::from_env().with_client(client.clone()),
                    ));
                    toolkit.register_tool(Arc::new(
                        WebSearchTool::from_env().with_client(client.clone()),
                    ));
                }
                None => crate::tool::register_web_tools(&toolkit),
            }
        }

        // ---- Meta tool (reset_equipped_tools) ----
        if self.enable_meta_tool.unwrap_or(false) {
            let meta =
                agentscope_core::tool::MetaToolFactory::new(&toolkit).create_reset_equipped_tools();
            toolkit.register_tool(Arc::new(meta));
        }

        // ---- Subagent orchestration ----
        // Mirrors Java `buildStaticSubagentEntries` + `buildDynamicSubagentsMiddleware`:
        // `general-purpose` is always registered; declarations, custom factories and the
        // `subagent_entries` escape hatch are materialised into leaf agents; the
        // spawn/send/list tools are registered on the toolkit, which is complete here.
        let task_repo: Arc<dyn super::subagent::task::TaskRepository> =
            self.task_repository.clone().unwrap_or_else(|| {
                Arc::new(WorkspaceTaskRepository::new(
                    Arc::clone(&filesystem),
                    agent_id.clone(),
                ))
            });
        if !self.leaf_subagent && !self.disable_subagents && self.model.is_some() {
            let blueprint = Arc::new(SubagentBlueprint {
                model: self.model.clone(),
                model_resolver: self.model_resolver.clone(),
                skill_filter: self.skill_filter.clone(),
                generate_options: self.generate_options_defaults.clone(),
                // Snapshot taken before the agent tools join the toolkit: a child must not
                // inherit `agent_spawn`, since it is a leaf worker.
                toolkit: toolkit.copy(),
                cwd: self.cwd.clone(),
                main_workspace: resolved_workspace.clone(),
                max_iters: if self.max_iters > 0 {
                    self.max_iters
                } else {
                    DEFAULT_MAX_ITERS
                },
                state_store: if self.session_history_mode == SessionHistoryMode::Legacy {
                    self.state_store.clone().or_else(|| {
                        self.distributed_store
                            .as_ref()
                            .and_then(|d| d.agent_state_store.clone())
                    })
                } else {
                    // EventLog children recover from the journal like their parent; handing
                    // them a snapshot store would configure a second history authority that
                    // `build()` correctly ignores, which only invites confusion.
                    None
                },
                enable_pending_tool_recovery: self.enable_pending_tool_recovery.unwrap_or(false),
                disable_memory_tools: self.disable_memory_tools,
                disable_memory_hooks: self.disable_memory_hooks,
                memory_config: self.memory_config.clone(),
                disable_workspace_context: self.disable_workspace_context,
                plan_mode_enabled: self.plan_mode_enabled,
                plan_mode_allow_shell: self.plan_mode_allow_shell,
                plan_file_dir: self.plan_file_dir.clone(),
                compaction_config: self.conversation_compaction_config.clone(),
                disable_compaction: self.disable_compaction,
                tool_result_eviction_config: self.tool_result_eviction_config.clone(),
                disable_tool_result_eviction: self.disable_tool_result_eviction,
                agent_tracing_log_enabled: self.agent_tracing_log_enabled,
                additional_context_files: self.additional_context_files.clone(),
                max_context_tokens: self.max_context_tokens,
                environment_memory: self.environment_memory.clone(),
                use_legacy_xml_workspace_context: self.use_legacy_xml_workspace_context,
                disable_session_persistence: self.disable_session_persistence,
            });
            // --- subagent entries ---
            let mut entries: Vec<SubagentEntry> = Vec::new();
            entries.push(SubagentEntry::new(
                "general-purpose",
                "General-purpose subagent with same capabilities as the main agent. Use \
                 for any isolated task that can be fully delegated.",
                blueprint.general_purpose_factory(),
            ));
            for decl in &self.subagent_declarations {
                if let Some(factory) = blueprint.declared_factory(decl) {
                    entries.push(
                        SubagentEntry::new(decl.name.clone(), decl.description.clone(), factory)
                            .with_declaration(decl.clone()),
                    );
                }
            }
            for custom in self.subagent_factories {
                let name = custom.name.clone();
                let description = custom.display_description().to_string();
                entries.push(SubagentEntry::new(name, description, Arc::new(custom)));
            }
            if self.disable_dynamic_subagents {
                // The static middleware never re-materialises declarations, so the
                // `workspace/subagents/*.md` scan happens here (Java `buildSubagentEntries`).
                let ws = resolved_workspace.to_string_lossy().to_string();
                let dir = resolved_workspace.join(super::subagent::SUBAGENTS_DIR);
                for decl in super::subagent::load_from_directory(&dir.to_string_lossy(), Some(&ws))
                {
                    if let Some(factory) = blueprint.declared_factory(&decl) {
                        entries.push(
                            SubagentEntry::new(
                                decl.name.clone(),
                                decl.description.clone(),
                                factory,
                            )
                            .with_declaration(decl),
                        );
                    }
                }
            }
            entries.extend(self.subagent_entries.clone());
            let manager = Arc::new(DefaultAgentManager::new(&entries));
            // --- agent tools ---
            let spawn_tool = Arc::new(
                AgentSpawnTool::new(Arc::clone(&manager))
                    .with_task_repository(Arc::clone(&task_repo)),
            );
            let agent_tools: Vec<Arc<dyn Tool>> = vec![
                Arc::clone(&spawn_tool) as Arc<dyn Tool>,
                Arc::new(AgentSendTool::new(Arc::clone(&spawn_tool))) as Arc<dyn Tool>,
                Arc::new(AgentListTool::new(Arc::clone(&spawn_tool))) as Arc<dyn Tool>,
            ];

            if self.disable_dynamic_subagents {
                let mw = SubagentsMiddleware::new(entries, Arc::clone(&task_repo))
                    .with_extra_tools(agent_tools.clone());
                for t in mw.get_tools() {
                    toolkit.register_tool(t);
                }
                middleware.add(Box::new(mw));
            } else {
                // Java `buildDynamicSubagentsMiddleware`: the middleware re-scans
                // `subagents/*.md` every step and re-materialises them through this builder.
                let factory_blueprint = Arc::clone(&blueprint);
                let mw = DynamicSubagentsMiddleware::new(entries, Arc::clone(&task_repo))
                    .with_filesystem(Arc::clone(&filesystem))
                    .with_main_workspace(resolved_workspace.to_string_lossy().to_string())
                    .with_agent_manager(Arc::clone(&manager))
                    .with_factory_builder(move |decl: &SubagentDeclaration| {
                        factory_blueprint.declared_factory(decl)
                    })
                    .with_extra_tools(agent_tools.clone());
                for t in mw.get_tools() {
                    toolkit.register_tool(t);
                }
                middleware.add(Box::new(mw));
            }
        }

        // ---- Plan mode ----
        let mut plan_mode_manager = None;
        if self.plan_mode_enabled {
            let pm = Arc::new(PlanModeManager::new(
                Arc::clone(&filesystem),
                Some(&self.plan_file_dir),
            ));
            // The middleware must observe runtime mode switches, so it holds the
            // manager (and reads `AgentState`) instead of a compile-time string.
            // Java equivalent: `PlanModeMiddleware` reads `PlanModeManager` on both
            // `onSystemPrompt` and `onModelCall`.
            let config = PlanModeConfig::with_manager(Arc::clone(&pm));
            middleware.add(Box::new(PlanModeMiddleware::new(config)));
            plan_mode_manager = Some(pm);
        }

        // ---- Path normalizer ----
        let path_normalizer = Some(WorkspacePathNormalizer::of(
            &resolved_workspace.to_string_lossy(),
        ));

        // ---- State checkpoint: persist/restore the full AgentState ----
        // Effective store: explicit `state_store` wins over `distributed_store`'s.
        //
        // Exactly one history authority. In [`SessionHistoryMode::EventLog`] the journal owns
        // recovery, so a snapshot store must not also write state: two writers that disagree
        // after a crash make the reader pick a winner it cannot verify (Java keeps the same
        // rule — `if (legacySessionHistory) inner.stateStore(...)`).
        let state_store = self.state_store.clone().or_else(|| {
            self.distributed_store
                .as_ref()
                .and_then(|d| d.agent_state_store.clone())
        });
        if self.session_history_mode == SessionHistoryMode::Legacy {
            if let Some(store) = &state_store {
                // Added last -> outermost: restore runs before (and save after) all
                // inner middlewares (compaction, plan mode, …).
                middleware.add(Box::new(StateCheckpointMiddleware::with_conflict_policy(
                    Arc::clone(store),
                    self.conflict_policy,
                )));
            }
        }

        // The compiler is registered last on purpose: the chain wraps in reverse, so the last
        // registration is the innermost model-call middleware. It validates and budgets the final
        // request, after every other rewrite has been applied.
        if let Some(pipeline) = context_pipeline {
            let mut context_middleware = crate::agent::context::ContextMiddleware::new(pipeline);
            if let Some((compactor, config)) = overflow_compaction {
                context_middleware = context_middleware.with_overflow_compaction(compactor, config);
            }
            middleware.add(Box::new(context_middleware));
        }

        // Initial state: pin a stable session/user id so restart-resume can
        // find the checkpoint under the same key.
        let initial_state = if self.session_id.is_some() || self.user_id.is_some() {
            let mut s = AgentState::default();
            if let Some(sid) = &self.session_id {
                s.session_id = sid.clone();
            }
            if let Some(uid) = &self.user_id {
                s.user_id = Some(uid.clone());
            }
            Some(s)
        } else {
            None
        };

        // Build the embedded ReAct agent from the configured model/toolkit/middleware.
        // Only built when a model is provided (assembly-only mode leaves it `None`).
        let model = self.model.clone();
        let agent = model.as_ref().map(|model| {
            let mut agent_builder = ReActAgent::builder()
                .id(agent_id.clone())
                .name(name.clone())
                .description(description.clone())
                .model(Arc::clone(model))
                .toolkit(toolkit.clone())
                .middleware_chain(middleware.clone());
            if let Some(state) = &initial_state {
                agent_builder = agent_builder.state(state.clone());
            }
            if let Some(ctx) = &self.permission_context {
                agent_builder = agent_builder.permission_context(ctx.clone());
            }
            if let Some(handler) = &self.confirm_handler {
                agent_builder = agent_builder.confirm_handler(handler.clone());
            }
            if !self.sys_prompt.is_empty() {
                agent_builder = agent_builder.sys_prompt(self.sys_prompt.clone());
            }
            let max_iters = if self.max_iters > 0 {
                self.max_iters
            } else {
                DEFAULT_MAX_ITERS
            };
            agent_builder = agent_builder.max_iters(max_iters);
            if let Some(recovery) = self.enable_pending_tool_recovery {
                agent_builder = agent_builder.enable_pending_tool_recovery(recovery);
            }
            if let Some(task_list) = self.enable_task_list {
                agent_builder = agent_builder.todo_tool(task_list);
            }
            // Native committed history is the default authority. The store is resolved in
            // `build()` rather than left to the caller, so a bare `HarnessAgent` journals
            // without any host-side `AgentSession` (Java does the same).
            match &session_log_store {
                Some(store) => {
                    agent_builder = agent_builder
                        .session_log_agent_id(agent_id.clone())
                        .session_log_store(Arc::clone(store))
                        .session_history_mode(SessionHistoryMode::EventLog);
                }
                None => {
                    agent_builder = agent_builder.session_history_mode(SessionHistoryMode::Legacy);
                }
            }
            let agent = agent_builder.build().expect("build ReActAgent");
            if let Some(opts) = &self.generate_options_defaults {
                agent.set_generate_options_defaults(opts.clone());
            }
            // Register the plan-control tools bound to the session state, so
            // `plan_enter`/`plan_exit` flip the persisted `plan_active` flag that core's
            // acting phase enforces. Like Java `PlanModeTools`, they mutate
            // `RuntimeContext`-resolved agent state (here the shared state handle).
            if self.plan_mode_enabled {
                PlanModeHandle::with_state(
                    Arc::clone(&filesystem),
                    Some(&self.plan_file_dir),
                    agent.state_handle(),
                )
                .register(&toolkit);
            }
            agent
        });

        // The shutdown state saver persists the agent's shared state before the
        // manager flips to `Terminated`. It needs the live state handle, so it is
        // bound after the embedded agent exists.
        if let Some(saver) = self.shutdown_state_saver.take() {
            if let Some(agent) = &agent {
                shutdown_manager.bind_state_saver(saver, agent.state_handle());
            }
        }

        HarnessAgent {
            name,
            agent_id,
            description,
            model,
            filesystem,
            toolkit,
            middleware,
            cwd,
            distributed_store: self.distributed_store,
            check_running: self.check_running,
            async_tool_timeout: self.async_tool_timeout,
            environment,
            use_legacy_xml_workspace_context: self.use_legacy_xml_workspace_context,
            disable_session_persistence: self.disable_session_persistence,
            task_repository: task_repo,
            agent,
            workspace_manager: Some(workspace_manager),
            plan_mode_manager,
            memory_config,
            path_normalizer,
            skill_repositories: ordered_skill_repos,
            skill_promoter: pending_skill_promoter,
            skill_usage_store: pending_skill_usage_store,
            skill_curator: pending_skill_curator,
            skill_audit_log: pending_skill_audit_log,
            session_log_store,
            shutdown_manager,
        }
    }
}

/// Returns a sandbox filesystem wrapper when the configured filesystem can be promoted to
/// support shell execution. `LocalFilesystem` is wrapped in a `SandboxBackedFilesystem`.
fn sandbox_filesystem(
    filesystem: &Arc<dyn Filesystem>,
    cwd: &PathBuf,
) -> Option<Arc<dyn crate::filesystem::SandboxFilesystem>> {
    use crate::filesystem::{SandboxBackedFilesystem, SandboxFilesystem};
    let sandbox = SandboxBackedFilesystem::new(Arc::clone(filesystem), cwd.clone());
    Some(Arc::new(sandbox) as Arc<dyn SandboxFilesystem>)
}

impl MemoryFlushManager {
    /// Returns the default flush prompt (re-exposed for the builder).
    pub fn default_flush_prompt() -> String {
        super::memory::flush::DEFAULT_FLUSH_PROMPT.to_string()
    }
}
