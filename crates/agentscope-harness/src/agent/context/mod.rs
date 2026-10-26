//! Context assembly, mirroring `io.agentscope.harness.agent.context`.
//!
//! Harness assembles the final model request from four kinds of material: stable
//! instructions (System), the conversation, transient runtime state and reference
//! documents. This module defines that material, its deterministic rendering and the budget
//! policy; [`ContextPipeline`] turns them into an actual request.
//!
//! Dynamic blocks can never create System instructions: authority comes from the provider of
//! the material, not from the text it contains.

pub mod builder;
pub mod context_middleware;
pub mod item;
pub mod model_calls;
pub mod policy;
pub mod render;
pub mod source;
pub mod usage;

pub use builder::{
    BudgetExceeded, ContextEviction, ContextPipeline, ContextPipelineConfig, PrepareOutcome,
    PreparedRequest, MANIFEST_ATTRIBUTE_PREFIX,
};
pub use context_middleware::ContextMiddleware;
pub use item::{
    content_hash, ContextBlock, ContextItem, ContextItemPlacement, ContextPurpose,
};
pub use model_calls::{auxiliary, auxiliary_text};
pub use policy::{
    ApproximateEstimator, ContextManifest, ContextManifestItem, ContextPolicy,
    ContextSelectionPolicy, DefaultSelectionPolicy, Estimate, TaskContextOptions, TokenEstimator,
};
pub use render::{
    layout_messages, render_context_blocks, synthetic, MessageKind, ORIGINAL_SYSTEM_INPUTS,
};
pub use source::{
    BoxedContextSource, CollectedMaterials, ContextRequest, ContextSource, ContextSourceOptions,
    ContextSources, SourceFailurePolicy,
};
pub use usage::{
    context_usage, strip_provider_usage, ContextUsage, UsageSource, LOCAL_METHOD, PROVIDER_METHOD,
};
