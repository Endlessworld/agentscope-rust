//! AgentScope Rust - Core
//!
//! Rust port of `io.agentscope.core` from the agentscope-java project: agent abstraction,
//! message model, model providers/transports, tool system, middleware, memory, state and
//! tracing.

pub mod agent;
pub mod credential;
pub mod error;
pub mod event;
pub mod formatter;
pub mod interruption;
pub mod memory;
pub mod message;
pub mod middleware;
pub mod model;
pub mod observation;
pub mod permission;
pub mod session;
pub mod shutdown;
pub mod state;
pub mod tls;
pub mod tool;
pub mod tracing;

pub use agent::{
    normalize_agent_id, Agent, CallGate, CallableAgent, ConfirmHandler, EventCategory,
    ExecutionGuard, Extensions, ObservableAgent, ReActAgent, ReActAgentBuilder, RuntimeContext,
    StreamOptions, StreamOptionsBuilder, DEFAULT_AGENT_ID, MAX_AGENT_ID_LENGTH,
};
pub use credential::{
    Credential, DeepSeekCredential, DeepSeekCredentialBuilder, KimiCredential,
    KimiCredentialBuilder, ModelCard, XAICredential, XAICredentialBuilder,
};
pub use error::{AgentExceptionInfo, CompositeAgentException, Error, Result};
pub use event::{AgentEvent, AgentEventEmitter, AgentEventType, ConfirmResult, EventData};
pub use formatter::{
    convert_tool_result_to_string, determine_audio_format, determine_media_type,
    download_url_to_base64, extract_text_content, extract_text_content_from_block, file_to_base64,
    format_role_label, get_extension, has_media_content, infer_audio_format_from_media_type,
    is_file_exists, is_local_file, resolve_mime_type, save_base64_data_to_temp_file,
    should_bypass_history, to_file_protocol_url, url_to_base64_data_url, url_to_protocol_url,
    validate_audio_extension, validate_image_extension, validate_video_extension, JsonSchema,
    JsonSchemaBuilder, ResponseFormat, ResponseFormatBuilder, BYPASS_MULTIAGENT_HISTORY_MERGE,
    MAX_SIZE_BYTES, SUPPORTED_AUDIO_EXTENSIONS, SUPPORTED_IMAGE_EXTENSIONS,
    SUPPORTED_VIDEO_EXTENSIONS, WARN_SIZE_BYTES,
};
pub use interruption::{InterruptContext, InterruptControl, InterruptSource};
// `Memory` is a deprecated compatibility layer (Java `Memory` is `@Deprecated since 2.0.0`:
// the conversation is owned by `AgentState.context`). Re-exported unchanged so existing
// hosts keep compiling for one minor cycle.
#[allow(deprecated)]
pub use memory::{InMemoryMemory, Memory, StateBackedMemory};
pub use message::{
    metadata_keys, AudioBlock, Base64Source, ChatUsage, ContentBlock, DataBlock, DocumentBlock,
    DocumentBlockBuilder, FileIdSource, FileIdSourceBuilder, GenerateReason, HintBlock, ImageBlock,
    ImageDetail, JsonMap, Msg, MsgBuilder, MsgRole, Source, TextBlock, ThinkingBlock, ToolCallState,
    ToolExecutionDetails, ToolExecutionOutcome, ToolResultBlock, ToolResultState, ToolUseBlock,
    URLSource, VideoBlock,
};
pub use middleware::{
    ActingInput, ActingResultsHandle, ActingResultsSnapshot, AgentInput, ExtensionPoint,
    FinalAnswerFilterMiddleware, Middleware, MiddlewareChain, ModelCallInput, Next, ReasoningInput,
    TaskReminderMiddleware,
};
pub use model::{
    indicates_output_limit, resolve_context_window, CachePolicy, ChatResponse, ChatResponseBuilder,
    ExecutionConfig, ExecutionConfigBuilder, GenerateOptions, GenerateOptionsBuilder, Model,
    ModelCapabilities, ModelCreationContext, ModelProvider, ModelProviderSupport, ModelRegistry,
    ModelUtils, StructuredOutputReminder, ToolChoice, ToolSchema, ToolSearchMode,
    DEFAULT_PROVIDER_ID,
};
pub use observation::{
    observe_action, status_of, ActionObservation, ActionObserver, ActionStatus, ObserverHandle,
    SessionActionObserver, EVENT_NAME as ACTION_OBSERVATION_EVENT, OBSERVER_CONTEXT_KEY,
};
pub use permission::{
    AdditionalWorkingDirectory, PermissionBehavior, PermissionContextState, PermissionDecision,
    PermissionEngine, PermissionMode, PermissionRule,
};
pub use shutdown::{
    ActiveRequestInfo, GracefulShutdownConfig, GracefulShutdownManager, PartialReasoningPolicy,
    ShutdownState, ShutdownStateSaver,
};
pub use state::{
    compute_hash, has_changed, needs_full_rewrite, AgentState, AgentStateStore, ConflictPolicy,
    EvidenceBinding, InMemoryAgentStateStore, JsonFileAgentStateStore, LegacyLoadResult,
    PlanModeContextState, ReadCacheEntry, SessionInfo, Task, TaskContextState, TaskState,
    TaskVerification, ToolkitState, ToolContextState, VerificationOutcome,
};
pub use tool::{
    is_dangerous_command, is_dangerous_path, DefaultToolResultConverter, ExtendedModel,
    FunctionTool, KeywordToolSearchStrategy, MetaToolFactory, RequirementProposeTool,
    SchemaOnlyTool, SimpleExtendedModel,
    TodoWriteTool, Tool, ToolCallParam, ToolCallInterceptor, ToolExecutionContext, ToolExecutor,
    ToolGroup, ToolGroupBuilder, ToolGroupManager, ToolGroupScope, ToolInvocation,
    ToolRegistration, ToolResultConverter, ToolResultMessageBuilder, ToolSearchMiddleware,
    ToolSearchRequest, ToolSearchResult, ToolSearchStrategy, ToolSearchTool, ToolSuspendException,
    ToolValidator, Toolkit, ToolkitConfig, ToolkitConfigBuilder,
};
pub use tracing::{init_tracing, init_tracing_with_filter};
