//! Workspace management, mirroring `io.agentscope.harness.agent.workspace`.
//!
//! Path constants, prefix-based path normalization, the persisted plan-mode
//! coordinator (`PlanModeManager`), the best-effort SQLite index (`WorkspaceIndex`),
//! the runtime-context namespace factory, and the two-layer `WorkspaceManager`
//! accessor that reads through the configured `Filesystem` with local-disk fallback.

pub mod constants;
pub mod index;
pub mod manager;
pub mod namespace;
pub mod path_normalizer;
pub mod plan;

pub use constants::WorkspaceConstants;
pub use index::WorkspaceIndex;
pub use manager::WorkspaceManager;
pub use namespace::{NamespaceFactory, NoNamespace, UserSessionNamespace};
pub use path_normalizer::WorkspacePathNormalizer;
pub use plan::PlanModeManager;
