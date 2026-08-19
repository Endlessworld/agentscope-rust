//! Harness agent runtime, mirroring `io.agentscope.harness.agent`.

pub mod bus;
pub mod distributed_store;
pub mod gateway;
pub mod harness_agent;
pub mod isolation_scope;
pub mod mcp;
pub mod mcp_client_manager;
pub mod memory;
pub mod middleware;
pub mod sandbox;
pub mod skill;
pub mod subagent;
pub mod workspace;

pub use bus::{
    AsyncToolRecord, AsyncToolRegistry, BusEntry, MessageBus, WorkspaceAsyncToolRegistry,
    WorkspaceMessageBus,
};
pub use distributed_store::{DistributedStore, DistributedStoreBuilder};
pub use gateway::{
    Channel, ChannelBinding, ChannelConfig, ChannelManager, ChannelRouter, ChannelSink,
    ChatUiChannel, ChatUiRequest, create_completion_callback, deterministic_hash, DmScope,
    DefaultSubagentMaterializer, Gateway, GatewayAgent, GatewayBootstrap,
    GatewayBootstrapBuilder, GatewayTurnContext, HarnessGateway, InboundMessage,
    InMemorySubagentRegistry, MsgContext, OutboundAddress, OutboundEnvelope, Peer, PeerKind,
    RouteResult, SendOptions, SessionTurnGate, SubagentMaterializer, SubagentRecord,
    SubagentRegistry, TaskCompletionBridge, wire_task_completion,
    WakeupDispatcher, WakeupTarget,
};
pub use harness_agent::{HarnessAgent, HarnessAgentBuilder};
pub use isolation_scope::IsolationScope;
pub use mcp::{McpServerConfig, McpServerRegistrar, Registrar};
pub use mcp_client_manager::{McpClientManager, McpClientInstance, McpToolInfo};
pub use memory::{
    compaction::{
        build_summary_message, calculate_token, filter_summary_messages, format_messages_for_summary,
        prune_tool_results, truncate_args, CompactionConfig, CompactionConfigBuilder,
        ConversationCompactor, DEFAULT_SUMMARY_PROMPT, PruneConfig, SUMMARY_MSG_NAME,
        TruncateArgsConfig, TruncateArgsConfigBuilder,
    },
    session::{SessionEntry, SessionFreshnessEvaluator, SessionTree},
    MemoryConfig, MemoryConfigBuilder, MemoryConsolidator, MemoryFlushManager,
    ToolResultEvictionConfig,
};
pub use middleware::{
    AgentTraceMiddleware, AsyncToolMiddleware, AtPathExpansionMiddleware, build_delivery_reminder,
    build_task_summary,
    CompactionMiddleware, CompactionMiddlewareConfig, DynamicSubagentsMiddleware,
    InboxMiddleware, MemoryFlushMiddleware, MemoryMaintenanceMiddleware, PlanModeConfig,
    PlanModeMiddleware, prepend_to_system_message, render_subagent_section, SubagentsMiddleware,
    ToolResultEvictionMiddleware,
};
pub use sandbox::{
    DefaultWorkspaceSpecApplier, ExecResult, Sandbox, SandboxClient, SandboxClientOptions,
    SandboxContext, SandboxError, SandboxExecutionGuard, SandboxIsolationKey, SandboxLease,
    SandboxManager, SandboxSnapshotSpec, SandboxState, SandboxStateStore, WorkspaceSpec,
    WorkspaceSpecApplier,
};
pub use skill::{
    AgentSkill, AgentSkillBuilder, HarnessSkillEntry, ShellPathPolicy, SkillCatalog, SkillCurator,
    SkillCuratorConfig, SkillCuratorState, SkillFilter, SkillPromptBuilder, SkillRuntime, SkillState,
    StageResult,
};
pub use subagent::{
    load_from_directory, load_from_filesystem, parse, BackgroundTask, DefaultAgentManager,
    SubagentDeclaration, SubagentEntry, SubagentFactory, SubagentMode, SubagentWorkspaceMode,
    SUBAGENTS_DIR, SUBAGENT_GLOB, TaskCompletionCallback, TaskDelivery, TaskRecord,
    TaskRepository, TaskRunSpec, TaskStatus, WorkspaceTaskRepository,
};
pub use workspace::{
    NamespaceFactory, NoNamespace, PlanModeManager, UserSessionNamespace, WorkspaceConstants,
    WorkspaceIndex, WorkspaceManager, WorkspacePathNormalizer,
};
