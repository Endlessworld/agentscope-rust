//! AgentScope Rust - Harness
//!
//! Rust port of `io.agentscope.harness` from the agentscope-java project: enhanced agent
//! runtime with workspace context, filesystem abstraction, sandbox backends, subagent
//! orchestration, memory management and gateway.

pub mod acp;
pub mod agent;
pub mod filesystem;
pub mod tool;

pub use acp::{AcpAgentSession, AcpCodingAgent, AcpCodingAgentBuilder};
pub use agent::{
    AgentSkill, AgentSkillBuilder, AgentTraceMiddleware, AsyncToolMiddleware, AsyncToolRecord,
    AsyncToolRegistry,
    AtPathExpansionMiddleware, BusEntry, build_delivery_reminder, build_summary_message,
    build_task_summary, calculate_token,
    Channel, ChannelBinding, ChannelConfig, ChannelManager, ChannelRouter, ChannelSink,
    ChatUiChannel, ChatUiRequest, create_completion_callback, CompactionConfig, CompactionConfigBuilder,
    CompactionMiddleware, CompactionMiddlewareConfig, ConversationCompactor, DEFAULT_SUMMARY_PROMPT,
    DefaultSubagentMaterializer, DefaultWorkspaceSpecApplier, deterministic_hash, DistributedStore, DistributedStoreBuilder,
    ExecResult, Gateway, GatewayAgent, GatewayBootstrap, GatewayBootstrapBuilder, GatewayTurnContext,
    filter_summary_messages, format_messages_for_summary, HarnessAgent, HarnessAgentBuilder,
    DefaultAgentManager, HarnessGateway, HarnessSkillEntry, InMemorySubagentRegistry,
    InboxMiddleware, IsolationScope, McpServerConfig, McpServerRegistrar, MemoryConfig,
    MemoryConfigBuilder, MemoryConsolidator, MemoryFlushManager, MemoryFlushMiddleware,
    MemoryMaintenanceMiddleware, DmScope, InboundMessage, MessageBus, MsgContext, OutboundAddress,
    OutboundEnvelope, Peer, PeerKind, PlanModeConfig, prepend_to_system_message, render_subagent_section,
    RouteResult, SendOptions,
    PlanModeManager, PlanModeMiddleware, prune_tool_results, PruneConfig, Registrar, SessionEntry,
    SessionFreshnessEvaluator, SessionTree, SessionTurnGate, ShellPathPolicy, SkillCatalog,
    SkillCurator, SkillCuratorConfig, SkillCuratorState, SkillFilter, SkillPromptBuilder,
    BackgroundTask, SkillRuntime, SkillState, StageResult, SubagentDeclaration, SubagentEntry,
    SubagentFactory, SubagentMaterializer, SubagentMode, SubagentRecord, SubagentRegistry,
    SubagentWorkspaceMode, SubagentsMiddleware, DynamicSubagentsMiddleware, TaskCompletionBridge, load_from_directory,
    load_from_filesystem, parse, SUBAGENTS_DIR, SUBAGENT_GLOB, SUMMARY_MSG_NAME,
    TaskCompletionCallback, TaskDelivery, TaskRecord, TaskRepository, TaskRunSpec, TaskStatus,
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
    command_with_working_directory, AgentGenerateTool, FilesystemTool, Level, MatchRange,
    MemoryGetTool, MemorySaveTool, MemorySearchTool, SearchResult, SessionHistoryTool,
    SessionListTool, SessionSearchTool, ShellExecuteTool, TaskCancelTool,
    TaskListTool, TaskOutputTool, TaskTool, WebFetchTool, WebSearchTool,
};
