//! Model layer: `Model` trait, chat response, generation options, tool schema,
//! execution config and related types. Mirrors `io.agentscope.core.model`.
//!
//! This module only carries the **interface abstraction** (trait, SPI, DTOs).
//! Concrete provider implementations live in
//! `agentscope-extensions-model` (see `docs/extensions/MODEL__GAP_TRACKER.md`).

pub mod cache_policy;
pub mod catalog;
pub mod chat_response;
pub mod context_windows;
pub mod execution_config;
pub mod generate_options;
pub mod model;
pub mod provider;
pub mod tool_choice;
pub mod tool_schema;

pub use cache_policy::CachePolicy;
pub use catalog::{
    CatalogConfig, CatalogEvent, CatalogSnapshot, ModelCatalog, ModelDescriptor, ModelKind,
    ModelStatus, ReasoningOption, ResolveRequest, Resolution,
};
pub use chat_response::{ChatResponse, ChatResponseBuilder};
pub use context_windows::{resolve_context_window, DEFAULT_CONTEXT_WINDOW};
pub use execution_config::{ExecutionConfig, ExecutionConfigBuilder};
pub use generate_options::{GenerateOptions, GenerateOptionsBuilder};
pub use model::{Model, ModelCapabilities};
pub use provider::{
    ApiProtocol, ModelCreationContext, ModelProvider, ModelProviderSupport, ModelRegistry,
    ModelUtils, StructuredOutputReminder, ANTHROPIC_PROVIDER_ID, DEFAULT_PROVIDER_ID,
    RESPONSES_PROVIDER_ID,
};
pub use tool_choice::ToolChoice;
pub use tool_schema::ToolSchema;
