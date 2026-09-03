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

use agentscope_core::agent::{ReActAgent, StreamOptions};
use agentscope_core::event::AgentEvent;
use agentscope_core::message::Msg;
use agentscope_core::middleware::MiddlewareChain;
use agentscope_core::model::Model;
use agentscope_core::tool::{Tool, Toolkit};

use crate::filesystem::{Filesystem, LocalFilesystem};

use super::distributed_store::DistributedStore;
use super::isolation_scope::IsolationScope;
use super::memory::{MemoryConfig, MemoryConsolidator, MemoryFlushManager, ToolResultEvictionConfig};
use super::middleware::{
    AgentTraceMiddleware, AtPathExpansionMiddleware, CompactionMiddleware,
    CompactionMiddlewareConfig, ConversationCompactionMiddleware, DynamicSubagentsMiddleware,
    HarnessSkillMiddleware, InboxMiddleware, MemoryFlushMiddleware,
    MemoryMaintenanceMiddleware, PlanModeConfig, PlanModeMiddleware, SkillCuratorMiddleware,
    SkillUsageMiddleware, SubagentsMiddleware, ToolResultEvictionMiddleware,
    WorkspaceContextMiddleware,
};
use super::skill::{
    build_promoter, compose_skill_repositories, AuditEntry, default_gate, FilesystemSkillUsageStore,
    LazyLoadSkillTool, ProposeSkillTool, SkillAuditLog, SkillCurator, SkillCuratorConfig, SkillFilter,
    SkillManageConfig, SkillManageTool, SkillPromoter, SkillPromotionGate, SkillRepository,
    SkillUsageStore, SkillVisibilityFilter, WorkspaceSkillRepository,
};
use super::subagent::task::WorkspaceTaskRepository;
use super::subagent::SubagentEntry;
use super::workspace::{
    PlanModeManager, WorkspaceIndex, WorkspaceManager, WorkspacePathNormalizer,
};
use super::bus::{MessageBus, WorkspaceMessageBus, WorkspaceAsyncToolRegistry, AsyncToolRegistry};
use crate::tool::{FilesystemTool, ShellExecuteTool};

/// A harness agent that operates on a filesystem with a configured toolkit and middleware chain.
pub struct HarnessAgent {
    name: String,
    model: Option<Arc<dyn Model>>,
    filesystem: Arc<dyn Filesystem>,
    toolkit: Toolkit,
    middleware: MiddlewareChain,
    cwd: PathBuf,
    distributed_store: Option<DistributedStore>,
    /// The embedded ReAct agent that drives the reasoning→acting loop.
    /// `None` when no model was configured (assembly-only mode).
    agent: Option<ReActAgent>,
    /// Workspace manager bound to this agent (context loading, memory, sessions).
    workspace_manager: Option<Arc<WorkspaceManager>>,
    /// Plan-mode coordinator, when plan mode was enabled.
    plan_mode_manager: Option<PlanModeManager>,
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

    /// Returns the workspace manager bound to this agent, if configured.
    pub fn workspace_manager(&self) -> Option<&Arc<WorkspaceManager>> {
        self.workspace_manager.as_ref()
    }

    /// Returns the plan-mode coordinator, if plan mode was enabled.
    pub fn plan_mode_manager(&self) -> Option<&PlanModeManager> {
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
        Some(futures::executor::block_on(curator.run_once(chrono::Utc::now())))
    }

    /// Promotes a draft skill (invalid result when no promotion loop is configured).
    pub fn promote_skill(&self, name: &str, reviewer_id: &str) -> Option<super::skill::PromotionResult> {
        self.skill_promoter.as_ref().map(|p| p.promote(name, reviewer_id))
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
}

/// Builder for `HarnessAgent`.
pub struct HarnessAgentBuilder {
    name: Option<String>,
    model: Option<Arc<dyn Model>>,
    filesystem: Option<Arc<dyn Filesystem>>,
    cwd: Option<PathBuf>,
    distributed_store: Option<DistributedStore>,
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
    compaction_config: Option<CompactionMiddlewareConfig>,
    tool_result_eviction_config: Option<ToolResultEvictionConfig>,
    /// Conversation-compaction configuration (`ConversationCompactionMiddleware`).
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
    agent_tracing_log_enabled: bool,
    with_shell_tool: bool,
    message_bus: Option<Arc<dyn MessageBus>>,
    async_tool_registry: Option<Arc<dyn AsyncToolRegistry>>,
    subagent_entries: Vec<SubagentEntry>,
    leaf_subagent: bool,
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
}

impl Default for HarnessAgentBuilder {
    fn default() -> Self {
        Self {
            name: None,
            model: None,
            filesystem: None,
            cwd: None,
            distributed_store: None,
            with_filesystem_tools: false,
            middleware: None,
            sys_prompt: String::new(),
            max_iters: 0,
            workspace: None,
            environment_memory: None,
            max_context_tokens: 8000,
            additional_context_files: Vec::new(),
            memory_config: None,
            compaction_config: None,
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
            agent_tracing_log_enabled: true,
            with_shell_tool: false,
            message_bus: None,
            async_tool_registry: None,
            subagent_entries: Vec::new(),
            leaf_subagent: false,
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

    /// Enables the compaction middleware with the given config.
    pub fn compaction(mut self, config: CompactionMiddlewareConfig) -> Self {
        self.compaction_config = Some(config);
        self
    }

    /// Disables the compaction middleware entirely.
    pub fn disable_compaction(mut self) -> Self {
        self.disable_compaction = true;
        self
    }

    /// Overrides the tool-result eviction configuration.
    pub fn tool_result_eviction(mut self, config: ToolResultEvictionConfig) -> Self {
        self.tool_result_eviction_config = Some(config);
        self
    }

    /// Enables conversation compaction: at the start of each agent call the
    /// accumulated history is summarized (LLM) and replaced with
    /// `[summary] + preserved tail` once it exceeds the configured trigger.
    pub fn conversation_compaction(
        mut self,
        config: super::memory::compaction::CompactionConfig,
    ) -> Self {
        self.conversation_compaction_config = Some(config);
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
        self.skill_manage_config = Some(SkillManageConfig::defaults().with_auto_promote(auto_promote));
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

    /// Resolves the workspace directory when none was set explicitly:
    /// `{cwd}/.agentscope/workspace` (mirrors the Java default).
    fn resolve_default_workspace(&self, cwd: &PathBuf) -> PathBuf {
        cwd.join(".agentscope").join("workspace")
    }

    /// Builds the `HarnessAgent`, wiring the filesystem, tools, middleware chain and
    /// embedded ReAct agent according to the configured orchestration.
    pub fn build(self) -> HarnessAgent {
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

        // Toolkit deep-copy: each agent gets its own toolkit so harness-registered tools
        // never bleed across builds.
        let toolkit = Toolkit::new();

        // ---- Workspace + filesystem orchestration ----
        let workspace_index = WorkspaceIndex::open(&resolved_workspace);
        let workspace_manager = Arc::new(WorkspaceManager::with_index(
            resolved_workspace.clone(),
            Some(Arc::clone(&filesystem)),
            workspace_index,
        ));
        let _ = workspace_manager.validate();

        // ---- Middleware chain ----
        let mut middleware = self.middleware.unwrap_or_default();

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
            ws_ctx.set_additional_context_files(self.additional_context_files.clone());
            middleware.add(Box::new(ws_ctx));
        }
        if !self.disable_at_path_expansion {
            middleware.add(Box::new(AtPathExpansionMiddleware::new(Arc::clone(
                &filesystem,
            ))));
        }

        // ---- Memory pipeline (flush + consolidation + maintenance) ----
        let memory_config = self.memory_config.unwrap_or_default();
        let memory_model = memory_config
            .model
            .clone()
            .or_else(|| self.model.clone());
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
                middleware.add(Box::new(MemoryMaintenanceMiddleware::new(
                    Arc::clone(&filesystem),
                    Some(consolidator),
                )));
            }
        }

        // ---- Compaction ----
        if !self.disable_compaction {
            if let Some(cfg) = self.compaction_config.clone() {
                middleware.add(Box::new(CompactionMiddleware::new(cfg)));
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

        // ---- Conversation compaction ----
        if let Some(cfg) = self.conversation_compaction_config.clone() {
            if let Some(model) = self.model.clone() {
                middleware.add(Box::new(ConversationCompactionMiddleware::with_model(
                    model,
                    cfg,
                    name.clone(),
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

        // ---- Subagent orchestration ----
        let task_repo: Arc<dyn super::subagent::task::TaskRepository> =
            Arc::new(WorkspaceTaskRepository::new(Arc::clone(&filesystem), name.clone()));
        if !self.leaf_subagent && !self.disable_subagents && self.model.is_some() {
            if !self.subagent_entries.is_empty() || !self.disable_dynamic_subagents {
                if self.disable_dynamic_subagents {
                    let mw = SubagentsMiddleware::new(self.subagent_entries.clone(), Arc::clone(&task_repo));
                    for t in mw.get_tools() {
                        toolkit.register_tool(t);
                    }
                    middleware.add(Box::new(mw));
                } else {
                    let mw = DynamicSubagentsMiddleware::new(
                        self.subagent_entries.clone(),
                        Arc::clone(&task_repo),
                    )
                    .with_filesystem(Arc::clone(&filesystem))
                    .with_main_workspace(resolved_workspace.to_string_lossy().to_string());
                    for t in mw.get_tools() {
                        toolkit.register_tool(t);
                    }
                    middleware.add(Box::new(mw));
                }
            }
        }

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
            .with_audit_log(Arc::clone(&audit_log));
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
                    self.skill_curator_config.unwrap_or_else(SkillCuratorConfig::defaults),
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
            toolkit.register_tool(Arc::new(LazyLoadSkillTool::new(Arc::clone(&skill_mw))));
            middleware.add_arc(skill_mw as Arc<dyn agentscope_core::middleware::Middleware>);
        }

        // ---- Toolkit (filesystem / shell tools) ----
        if self.with_filesystem_tools {
            FilesystemTool::register(&toolkit, Arc::clone(&filesystem));
        }
        if self.with_shell_tool && !self.disable_shell_tool {
            if let Some(sandbox) = sandbox_filesystem(&filesystem, &cwd) {
                ShellExecuteTool::register(&toolkit, sandbox);
            }
        }

        // ---- Plan mode ----
        let mut plan_mode_manager = None;
        if self.plan_mode_enabled {
            let pm = PlanModeManager::new(
                Arc::clone(&filesystem),
                Some(&self.plan_file_dir),
            );
            let config = PlanModeConfig {
                enabled: true,
                plan: String::new(),
            };
            middleware.add(Box::new(PlanModeMiddleware::new(config)));
            plan_mode_manager = Some(pm);
        }

        // ---- Path normalizer ----
        let path_normalizer = Some(WorkspacePathNormalizer::of(
            &resolved_workspace.to_string_lossy(),
        ));

        // Build the embedded ReAct agent from the configured model/toolkit/middleware.
        // Only built when a model is provided (assembly-only mode leaves it `None`).
        let model = self.model.clone();
        let agent = model.as_ref().map(|model| {
            let mut agent_builder = ReActAgent::builder()
                .name(name.clone())
                .model(Arc::clone(model))
                .toolkit(toolkit.clone())
                .middleware_chain(middleware.clone());
            if let Some(ctx) = &self.permission_context {
                agent_builder = agent_builder.permission_context(ctx.clone());
            }
            if let Some(handler) = &self.confirm_handler {
                agent_builder = agent_builder.confirm_handler(handler.clone());
            }
            if !self.sys_prompt.is_empty() {
                agent_builder = agent_builder.sys_prompt(self.sys_prompt.clone());
            }
            let max_iters = if self.max_iters > 0 { self.max_iters } else { DEFAULT_MAX_ITERS };
            agent_builder = agent_builder.max_iters(max_iters);
            agent_builder.build().expect("build ReActAgent")
        });

        HarnessAgent {
            name,
            model,
            filesystem,
            toolkit,
            middleware,
            cwd,
            distributed_store: self.distributed_store,
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
