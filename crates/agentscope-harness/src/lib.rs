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
    AgentSkill, AgentSkillBuilder, AgentTraceMiddleware, ArtifactDeliveryRequest,
    ArtifactDeliveryResult, ArtifactDeliveryTarget, AsyncToolMiddleware, AsyncToolRecord,
    AsyncToolRegistry,
    AtPathExpansionMiddleware, BusEntry, build_delivery_reminder, build_summary_message,
    build_task_summary, calculate_token, context_usage, ContextUsage,
    Channel, ChannelBinding, ChannelConfig, ChannelManager, ChannelRouter, ChannelSink,
    ChatUiChannel, ChatUiRequest, create_completion_callback, CompactionConfig, CompactionConfigBuilder,
    CompactionMiddleware, CompactionMiddlewareConfig, CompactionMetrics, CompactionStatus,
    ConversationCompactor, DEFAULT_SUMMARY_PROMPT,
    DefaultSubagentMaterializer, DefaultWorkspaceSpecApplier, deterministic_hash, DistributedStore, DistributedStoreBuilder,
    DockerSandbox, DockerSandboxClient,
    ExecResult, Gateway, GatewayAgent, GatewayBootstrap, GatewayBootstrapBuilder, GatewayTurnContext,
    filter_summary_messages, format_messages_for_summary, HarnessAgent, HarnessAgentBuilder,
    DefaultAgentManager, HarnessGateway, HarnessSkillEntry, InMemorySubagentRegistry,
    InboxMiddleware, IsolationScope, bridge_mcp_tools_to_toolkit, McpServerConfig, McpServerRegistrar, MemoryConfig,
    MemoryConfigBuilder, MemoryConsolidator, MemoryFlushManager, MemoryFlushMiddleware,
    MemoryMaintenanceMiddleware, DmScope, InboundMessage, MessageBus, MsgContext, OutboundAddress,
    OutboundEnvelope, Peer, PeerKind, PlanModeConfig, prepend_to_system_message, render_subagent_section,
    RouteResult, SendOptions,
    PlanModeManager, PlanModeMiddleware, prune_tool_results, PruneConfig, Registrar,
    SessionFreshnessEvaluator, SessionTurnGate, ShellPathPolicy, SkillCatalog,
    SkillCurator, SkillCuratorConfig, SkillCuratorState, SkillFilter, SkillPromptBuilder,
    BackgroundTask, RemoteAskPolicy, SkillRuntime, SkillState, StageResult, SubagentDeclaration,
    SubagentEntry,
    SubagentFactory, SubagentMaterializer, SubagentMode, SubagentRecord, SubagentRegistry,
    SubagentWorkspaceMode, SubagentsMiddleware, DynamicSubagentsMiddleware, TaskCompletionBridge, load_from_directory,
    load_from_file, load_from_filesystem, parse, SUBAGENTS_DIR, SUBAGENT_GLOB, SUMMARY_MSG_NAME,
    TaskCompletionCallback, TaskDelivery, TaskRecord, TaskRepository, TaskRunSpec, TaskStatus,
    SUPPRESS_COMPLETION_CALLBACK,
    ToolResultEvictionConfig, ToolResultEvictionMiddleware, wire_task_completion,
    truncate_args, TruncateArgsConfig,
    TruncateArgsConfigBuilder, WorkspaceTaskRepository,
    WakeupDispatcher, WakeupTarget, WorkspaceAsyncToolRegistry, WorkspaceConstants,
    WorkspaceIndex, WorkspaceManager, WorkspaceMessageBus, WorkspacePathNormalizer,
    WorkspaceSpec, WorkspaceSpecApplier,
};
pub use filesystem::{
    validate_path, CompositeFilesystem, ExecuteResponse, FileData, FileInfo, Filesystem,
    GrepMatch, LocalFilesystem, LocalFsMode, OverlayFilesystem, RemoteFilesystem,
    SandboxBackedFilesystem, SandboxFilesystem,
};
pub use tool::{
    command_with_working_directory, AgentGenerateTool, ArtifactDeliveryTool, FilesystemTool, Level, MatchRange,
    MemoryGetTool, MemorySaveTool, MemorySearchTool, SearchResult, SessionHistoryTool,
    SessionListTool, SessionSearchTool, ShellExecuteTool, TaskCancelTool,
    TaskListTool, TaskOutputTool, TaskTool, WebFetchTool, WebSearchTool,
};
