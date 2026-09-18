//! Model layer: `Model` trait, chat response, generation options, tool schema,
//! execution config and related types. Mirrors `io.agentscope.core.model`.

pub mod anthropic;
pub mod cache_policy;
pub mod chat_response;
pub mod context_windows;
pub mod execution_config;
pub mod generate_options;
pub mod model;
pub mod openai;
pub mod provider;
pub mod responses;
pub mod tool_choice;
pub mod tool_schema;

pub use anthropic::{
    default_anthropic_registry, AnthropicMessagesModel, AnthropicMessagesProvider,
};
pub use cache_policy::CachePolicy;
pub use chat_response::{ChatResponse, ChatResponseBuilder};
pub use context_windows::resolve_context_window;
pub use execution_config::{ExecutionConfig, ExecutionConfigBuilder};
pub use generate_options::{GenerateOptions, GenerateOptionsBuilder};
pub use model::{Model, ModelCapabilities};
pub use openai::{default_registry, OpenAiChatModel, OpenAiProvider};
pub use responses::{
    default_responses_registry, OpenAiResponsesModel, OpenAiResponsesProvider,
};
pub use provider::{
    ApiProtocol, ModelCreationContext, ModelProvider, ModelProviderSupport, ModelRegistry,
    ModelUtils, StructuredOutputReminder, ANTHROPIC_PROVIDER_ID, DEFAULT_PROVIDER_ID,
    RESPONSES_PROVIDER_ID,
};
pub use tool_choice::ToolChoice;
pub use tool_schema::ToolSchema;
