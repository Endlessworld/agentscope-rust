//! Model layer: `Model` trait, chat response, generation options, tool schema,
//! execution config and related types. Mirrors `io.agentscope.core.model`.
//!
//! This module only carries the **interface abstraction** (trait, SPI, DTOs).
//! Concrete provider implementations live in
//! `agentscope-extensions-model` (see `docs/extensions/MODEL__GAP_TRACKER.md`).
//!
//! ## Model identifier vocabulary
//!
//! Mirrors the concept dictionary Java `c05e4b91f` added to `ModelRegistry`:
//!
//! * **provider id** — the routing key and model-id prefix (`dashscope`, `openai`); see
//!   [`ModelProvider::provider_id`](provider::ModelProvider::provider_id).
//! * **model name** — the bare catalog name sent to the upstream API (`qwen-max`);
//!   [`Model::model_name`](model::Model::model_name).
//! * **model id** — the recipe string `provider_id + ":" + model_name`
//!   (`dashscope:qwen-max`), or a bare model name routed by asking every provider.
//! * **[`ModelCreationContext`](provider::ModelCreationContext)** — the creation inputs.
//! * **model identity** — the pair (model id, creation context): a model id alone never
//!   identifies an instance. Rust has no registry cache, so this is only a conceptual
//!   distinction here (every [`ModelRegistry::resolve`](provider::ModelRegistry::resolve)
//!   builds a fresh instance).
//!
//! On the agent side, [`MODEL_ID_CONTEXT_KEY`](crate::agent::MODEL_ID_CONTEXT_KEY) carries a
//! model id to resolve per call, while [`MODEL_CONTEXT_KEY`](crate::agent::MODEL_CONTEXT_KEY)
//! carries the already-decided [`Model`] instance for a run.

pub mod cache_policy;
pub mod catalog;
pub mod chat_response;
pub mod context_windows;
pub mod execution_config;
pub mod finish_reasons;
pub mod generate_options;
pub mod model;
pub mod provider;
pub mod tool_choice;
pub mod tool_schema;
pub mod tool_search_mode;

pub use cache_policy::CachePolicy;
pub use catalog::{
    CatalogConfig, CatalogEvent, CatalogSnapshot, ModelCatalog, ModelDescriptor, ModelKind,
    ModelStatus, ReasoningOption, Resolution, ResolveRequest,
};
pub use chat_response::{ChatResponse, ChatResponseBuilder};
pub use context_windows::{resolve_context_window, DEFAULT_CONTEXT_WINDOW};
pub use execution_config::{ExecutionConfig, ExecutionConfigBuilder};
pub use finish_reasons::indicates_output_limit;
pub use generate_options::{GenerateOptions, GenerateOptionsBuilder};
pub use model::{Model, ModelCapabilities};
pub use provider::{
    ApiProtocol, ModelCreationContext, ModelProvider, ModelProviderSupport, ModelRegistry,
    ModelUtils, StructuredOutputReminder, ANTHROPIC_PROVIDER_ID, DEFAULT_PROVIDER_ID,
    RESPONSES_PROVIDER_ID,
};
pub use tool_choice::ToolChoice;
pub use tool_schema::ToolSchema;
pub use tool_search_mode::ToolSearchMode;
