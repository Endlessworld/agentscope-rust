//! AgentScope Rust - Harness
//!
//! Rust port of `io.agentscope.harness` from the agentscope-java project: enhanced agent
//! runtime with workspace context, filesystem abstraction, sandbox backends, subagent
//! orchestration, memory management and gateway.

pub mod agent;
pub mod filesystem;
pub mod tool;

// The context compiler is written against the core error model; re-export it so the
// `crate::Result` / `crate::Error` paths used across harness modules stay uniform.
pub use agentscope_core::error::{Error, Result};

pub use agent::{
    bridge_mcp_tools_to_toolkit, build_delivery_reminder, build_summary_message,
    build_task_summary, calculate_token, context_usage, create_completion_callback,
    deterministic_hash, filter_summary_messages, format_messages_for_summary, load_from_directory,
    load_from_file, load_from_filesystem, parse, prepend_to_system_message, prune_tool_results,
    render_subagent_section, truncate_args, wire_task_completion, AgentSkill, AgentSkillBuilder,
    AgentTraceMiddleware, ArtifactDeliveryRequest, ArtifactDeliveryResult, ArtifactDeliveryTarget,
    AsyncToolMiddleware, AsyncToolRecord, AsyncToolRegistry, AtPathExpansionMiddleware,
    BackgroundTask, BusEntry, Channel, ChannelBinding, ChannelConfig, ChannelManager,
    ChannelRouter, ChannelSink, ChatUiChannel, ChatUiRequest, CompactionConfig,
    CompactionConfigBuilder, CompactionMetrics, CompactionMiddleware, CompactionMiddlewareConfig,
    CompactionStatus, ContextUsage, ConversationCompactor, DefaultAgentManager,
    DefaultSubagentMaterializer, DefaultWorkspaceSpecApplier, DistributedStore,
    DistributedStoreBuilder, DmScope, DockerSandbox, DockerSandboxClient,
    DynamicSubagentsMiddleware, ExecResult, Gateway, GatewayAgent, GatewayBootstrap,
    GatewayBootstrapBuilder, GatewayTurnContext, HarnessAgent, HarnessAgentBuilder, HarnessGateway,
    HarnessSkillEntry, InMemorySubagentRegistry, InboundMessage, InboxMiddleware, IsolationScope,
    McpServerConfig, McpServerRegistrar, MemoryConfig, MemoryConfigBuilder, MemoryConsolidator,
    MemoryFlushManager, MemoryFlushMiddleware, MemoryMaintenanceMiddleware, MessageBus, MsgContext,
    OutboundAddress, OutboundEnvelope, Peer, PeerKind, PlanModeConfig, PlanModeManager,
    PlanModeMiddleware, PruneConfig, Registrar, RemoteAskPolicy, RouteResult, SendOptions,
    SessionFreshnessEvaluator, SessionTurnGate, ShellPathPolicy, SkillCatalog, SkillCurator,
    SkillCuratorConfig, SkillCuratorState, SkillFilter, SkillPromptBuilder, SkillRuntime,
    SkillState, StageResult, SubagentDeclaration, SubagentEntry, SubagentFactory,
    SubagentMaterializer, SubagentMode, SubagentRecord, SubagentRegistry, SubagentWorkspaceMode,
    SubagentsMiddleware, TaskCompletionBridge, TaskCompletionCallback, TaskDelivery, TaskRecord,
    TaskRepository, TaskRunSpec, TaskStatus, ToolResultEvictionConfig,
    ToolResultEvictionMiddleware, TruncateArgsConfig, TruncateArgsConfigBuilder, WakeupDispatcher,
    WakeupTarget, WorkspaceAsyncToolRegistry, WorkspaceConstants, WorkspaceIndex, WorkspaceManager,
    WorkspaceMessageBus, WorkspacePathNormalizer, WorkspaceSpec, WorkspaceSpecApplier,
    WorkspaceTaskRepository, DEFAULT_SUMMARY_PROMPT, SUBAGENTS_DIR, SUBAGENT_GLOB,
    SUMMARY_MSG_NAME, SUPPRESS_COMPLETION_CALLBACK,
};
pub use filesystem::{
    validate_path, CompositeFilesystem, ExecuteResponse, FileData, FileInfo, Filesystem, GrepMatch,
    LocalFilesystem, LocalFsMode, OverlayFilesystem, RemoteFilesystem, SandboxBackedFilesystem,
    SandboxFilesystem,
};
pub use tool::{
    command_with_working_directory, AgentGenerateTool, ArtifactDeliveryTool, FilesystemTool, Level,
    MatchRange, MemoryGetTool, MemorySaveTool, MemorySearchTool, SearchResult, SessionHistoryTool,
    SessionListTool, SessionSearchTool, ShellExecuteTool, TaskCancelTool, TaskListTool,
    TaskOutputTool, TaskTool, WebFetchTool, WebSearchTool,
};
