//! Harness agent runtime, mirroring `io.agentscope.harness.agent`.

pub mod artifact;
pub mod bus;
pub mod context;
pub mod coordination;
pub mod distributed_store;
pub mod gateway;
pub mod harness_agent;
pub mod isolation_scope;
pub mod mcp;
pub mod mcp_client_manager;
pub mod memory;
pub mod middleware;
pub mod sandbox;
pub mod session;
pub mod skill;
pub mod subagent;
pub mod team;
pub mod verification;
pub mod workspace;

pub use artifact::{ArtifactDeliveryRequest, ArtifactDeliveryResult, ArtifactDeliveryTarget};
pub use bus::{
    AsyncToolRecord, AsyncToolRegistry, BusEntry, MessageBus, WorkspaceAsyncToolRegistry,
    WorkspaceMessageBus,
};
pub use context::{
    context_usage, strip_provider_usage, ApproximateEstimator, BoxedContextSource, ContextBlock,
    ContextItem, ContextItemPlacement, ContextManifest, ContextManifestItem, ContextMiddleware,
    ContextPipeline, ContextPipelineConfig, ContextPolicy, ContextPurpose, ContextRequest,
    ContextSelectionPolicy, ContextSource, ContextSourceOptions, ContextSources, ContextUsage,
    DefaultSelectionPolicy, Estimate, SourceFailurePolicy, TaskContextOptions, TokenEstimator,
    UsageSource,
};
pub use distributed_store::{DistributedStore, DistributedStoreBuilder};
pub use gateway::{
    create_completion_callback, deterministic_hash, wire_task_completion, Channel, ChannelBinding,
    ChannelConfig, ChannelManager, ChannelRouter, ChannelSink, ChatUiChannel, ChatUiRequest,
    DefaultSubagentMaterializer, DmScope, Gateway, GatewayAgent, GatewayBootstrap,
    GatewayBootstrapBuilder, GatewayTurnContext, HarnessGateway, InMemorySubagentRegistry,
    InboundMessage, MsgContext, OutboundAddress, OutboundEnvelope, Peer, PeerKind, PlatformChannel,
    PlatformCtx, RouteResult, SendOptions, SessionTurnGate, SubagentMaterializer, SubagentRecord,
    SubagentRegistry, TaskCompletionBridge, WakeupDispatcher, WakeupTarget,
};
pub use harness_agent::{HarnessAgent, HarnessAgentBuilder};
pub use isolation_scope::IsolationScope;
pub use mcp::{
    bridge_mcp_tools_to_toolkit, try_bridge_mcp_tools_to_toolkit, McpMetaPropagationOptions,
    McpServerConfig, McpServerRegistrar, McpServerRegistrationListener,
    McpServerRegistrationResult, McpServerRegistrationStatus, Registrar,
};
pub use mcp_client_manager::{McpClientInstance, McpClientManager, McpToolInfo};
pub use memory::{
    compaction::{
        build_summary_message, calculate_token, filter_summary_messages,
        format_messages_for_summary, prune_tool_results, trigger_for_input_limit,
        trigger_for_window, truncate_args, CompactionConfig, CompactionConfigBuilder,
        CompactionMetrics, CompactionStatus, ConversationCompactor, PruneConfig,
        TruncateArgsConfig, TruncateArgsConfigBuilder, DEFAULT_SUMMARY_PROMPT, SUMMARY_MSG_NAME,
    },
    session::SessionFreshnessEvaluator,
    MemoryConfig, MemoryConfigBuilder, MemoryConsolidator, MemoryFlushManager,
    ToolResultEvictionConfig,
};
pub use middleware::{
    build_delivery_reminder, build_task_summary, prepend_to_system_message,
    render_subagent_section, AgentTraceMiddleware, AsyncToolMiddleware, AtPathExpansionMiddleware,
    CompactionMiddleware, CompactionMiddlewareConfig, ConversationCompactionMiddleware,
    DynamicSubagentsMiddleware, InboxMiddleware, MemoryFlushMiddleware,
    MemoryMaintenanceMiddleware, PlanModeConfig, PlanModeMiddleware, SubagentsMiddleware,
    ToolResultEvictionMiddleware,
};
pub use sandbox::{
    DefaultWorkspaceSpecApplier, DockerSandbox, DockerSandboxClient, ExecResult,
    InMemorySandboxStateStore, LocalSandboxState, Sandbox, SandboxClient, SandboxClientOptions,
    SandboxContext, SandboxError, SandboxExecutionGuard, SandboxIsolationKey, SandboxLease,
    SandboxManager, SandboxSnapshotSpec, SandboxState, SandboxStateStore, VolumeMount,
    WorkspaceSpec, WorkspaceSpecApplier,
};
pub use session::{
    AgentSession, InputReceipt, SessionAnswer, SessionConfirm, Task as SessionTask,
    WorkspaceSessionLogStore, WorkspaceSessionStorage,
};
pub use skill::{
    build_promoter, compose_skill_repositories, default_gate, AgentSkill, AgentSkillBuilder,
    AllowListFilter, AllowListGate, AuditEntry, DenyListFilter, FileSystemSkillRepository,
    FilesystemSkillUsageStore, HarnessSkillEntry, LoadSkillTool, PromotionDecision,
    PromotionResult, ProposeSkillTool, RejectAllGate, ShellPathPolicy, SkillAuditLog, SkillCatalog,
    SkillCurator, SkillCuratorConfig, SkillCuratorState, SkillFilter, SkillManageConfig,
    SkillManageTool, SkillPromoter, SkillPromotionGate, SkillPromptBuilder, SkillRepository,
    SkillRuntime, SkillState, SkillUsageStore, SkillVisibilityFilter, StageResult,
    WorkspaceSkillRepository,
};
pub use subagent::{
    load_from_directory, load_from_file, load_from_filesystem, parse, BackgroundTask,
    DefaultAgentManager, RemoteAskPolicy, SubagentDeclaration, SubagentEntry, SubagentFactory,
    SubagentMode, SubagentWorkspaceMode, TaskCompletionCallback, TaskDelivery, TaskRecord,
    TaskRepository, TaskRunSpec, TaskStatus, WorkspaceTaskRepository, SUBAGENTS_DIR, SUBAGENT_GLOB,
    SUPPRESS_COMPLETION_CALLBACK,
};
pub use verification::{verify, DeterministicVerifier, SettledAction, Verdict};
pub use workspace::{
    NamespaceFactory, NoNamespace, PlanModeManager, UserSessionNamespace, WorkspaceConstants,
    WorkspaceIndex, WorkspaceManager, WorkspacePathNormalizer,
};
