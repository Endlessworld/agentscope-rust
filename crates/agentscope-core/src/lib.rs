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
pub mod permission;
pub mod shutdown;
pub mod state;
pub mod tool;
pub mod tracing;

pub use agent::{
    Agent, CallableAgent, CallGate, EventCategory, ExecutionGuard, ObservableAgent, ReActAgent,
    ReActAgentBuilder, RuntimeContext, StreamOptions, StreamOptionsBuilder,
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
    is_file_exists, is_local_file, JsonSchema, JsonSchemaBuilder, resolve_mime_type, ResponseFormat,
    ResponseFormatBuilder, save_base64_data_to_temp_file, should_bypass_history,
    to_file_protocol_url, url_to_base64_data_url, url_to_protocol_url, validate_audio_extension,
    validate_image_extension, validate_video_extension, BYPASS_MULTIAGENT_HISTORY_MERGE,
    MAX_SIZE_BYTES, SUPPORTED_AUDIO_EXTENSIONS, SUPPORTED_IMAGE_EXTENSIONS,
    SUPPORTED_VIDEO_EXTENSIONS, WARN_SIZE_BYTES,
};
pub use message::{
    metadata_keys, AudioBlock, Base64Source, ChatUsage, ContentBlock, DataBlock, GenerateReason,
    HintBlock, ImageBlock, JsonMap, Msg, MsgBuilder, MsgRole, Source, TextBlock, ThinkingBlock,
    ToolCallState, ToolResultBlock, ToolResultState, ToolUseBlock, URLSource, VideoBlock,
};
pub use middleware::{ActingInput, AgentInput, Middleware, MiddlewareChain, ModelCallInput, ReasoningInput, TaskReminderMiddleware};
pub use model::{
    default_registry, resolve_context_window, CachePolicy, ChatResponse, ChatResponseBuilder,
    ExecutionConfig, ExecutionConfigBuilder, GenerateOptions, GenerateOptionsBuilder, Model,
    ModelCapabilities, ModelCreationContext, ModelProvider, ModelProviderSupport, ModelRegistry,
    ModelUtils, OpenAiChatModel, OpenAiProvider, StructuredOutputReminder, ToolChoice, ToolSchema,
    DEFAULT_PROVIDER_ID,
};
pub use tool::{
    DefaultToolResultConverter, ExtendedModel, FunctionTool, MetaToolFactory, is_dangerous_command,
    is_dangerous_path, SchemaOnlyTool, SimpleExtendedModel, TodoWriteTool, Tool, ToolCallParam, ToolExecutionContext,
    Toolkit, ToolkitConfig, ToolkitConfigBuilder, ToolExecutor, ToolGroup, ToolGroupBuilder,
    ToolGroupManager, ToolGroupScope, ToolRegistration, ToolResultConverter, ToolResultMessageBuilder,
    ToolSuspendException, ToolValidator,
};
pub use interruption::{InterruptContext, InterruptControl, InterruptSource};
pub use memory::{InMemoryMemory, Memory, StateBackedMemory};
pub use permission::{
    AdditionalWorkingDirectory, PermissionBehavior, PermissionContextState, PermissionDecision,
    PermissionEngine, PermissionMode, PermissionRule,
};
pub use shutdown::{
    GracefulShutdownConfig, GracefulShutdownManager, PartialReasoningPolicy, ShutdownState,
};
pub use state::{
    AgentState, AgentStateStore, InMemoryAgentStateStore, JsonFileAgentStateStore,
    PlanModeContextState, ReadCacheEntry, SessionInfo, Task, TaskContextState, TaskState,
    ToolContextState, compute_hash, has_changed, needs_full_rewrite,
};
pub use tracing::{init_tracing, init_tracing_with_filter};
