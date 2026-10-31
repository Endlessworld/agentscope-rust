//! agentscope-extensions-training - see docs/extensions/TRAINING__GAP_TRACKER.md
//!
//! Rust port of `io.agentscope.core.training`: shadow-traffic training support
//! on top of the core agent/model/toolkit abstractions.
//!
//! * [`strategy`] - selection strategies deciding which calls to train on.
//! * [`reward`] - reward calculation for shadow agents.
//! * [`backend`] - Trinity Feedback/Commit client, DTOs and model adapter.
//! * [`runner`] - run/task registries, config, router and lifecycle runner.

pub mod backend;
pub mod reward;
pub mod runner;
pub mod strategy;

pub use backend::{
    dto, TrinityClient, TrinityClientBuilder, TrinityModelAdapter, TrinityModelAdapterBuilder,
};
pub use reward::{FnRewardCalculator, RewardCalculator, ZeroRewardCalculator};
pub use runner::{
    task_id_generator, AgentCloner, AnnotationProvider, RegistryStats, RunExecutionContext,
    RunRegistry, ShadowAgentBlueprint, ShadowAgentSpec, ShadowRunReport, TaskExecutionRegistry,
    TrainingConfig, TrainingConfigBuilder, TrainingHook, TrainingRouter, TrainingRunner,
    TrainingRunnerBuilder, TRAINING_ANNOTATION_CONTEXT_KEY,
};
pub use strategy::{
    is_expired, ExplicitMarkingStrategy, SamplingRateStrategy, SelectionDecision,
    TrainingAnnotation, TrainingAnnotationBuilder, TrainingContext, TrainingSelectionStrategy,
};

// Re-export the core message type for convenience in strategy signatures.
pub use agentscope_core::message::Msg;
