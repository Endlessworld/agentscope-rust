//! Tool system: `Tool` trait, toolkit, groups, validation, executor and the
//! built-in file / coding / mcp / schema-only tools.
//! Mirrors `io.agentscope.core.tool`.

pub mod coding;
pub mod config;
pub mod dangerous;
pub mod emitter;
pub mod executor;
pub mod file;
pub mod group;
pub mod mcp;
pub mod meta;
pub mod registration;
pub mod result;
pub mod schema_only_tool;
pub mod suspend;
pub mod todo;
pub mod tool;
pub mod tool_request_config;
pub mod toolkit;
pub mod validator;

pub use coding::{validate_command, CommandCheck};
pub use config::{ToolkitConfig, ToolkitConfigBuilder};
pub use dangerous::{is_dangerous_command, is_dangerous_path};
pub use emitter::{
    emit_to, tool_emitter_for_call, ChunkCallback, DefaultToolEmitter, NoOpToolEmitter, ToolEmitter,
};
pub use executor::ToolExecutor;
pub use file::FilesystemTool;
pub use group::{ToolGroup, ToolGroupBuilder, ToolGroupManager, ToolGroupScope};
pub use mcp::{McpAsyncHandler, McpTool};
pub use meta::ToolCallParamBuilder;
pub use meta::{ExtendedModel, MetaToolFactory, SimpleExtendedModel, ToolCallParam};
pub use registration::ToolRegistration;
pub use result::{DefaultToolResultConverter, ToolResultConverter, ToolResultMessageBuilder};
pub use schema_only_tool::SchemaOnlyTool;
pub use suspend::ToolSuspendException;
pub use todo::TodoWriteTool;
pub use tool::{FunctionTool, Tool, ToolExecutionContext};
pub use tool_request_config::{ToolMergeMode, ToolRequestConfig, TOOL_REQUEST_CONFIG_CONTEXT_KEY};
pub use toolkit::Toolkit;
pub use validator::ToolValidator;
