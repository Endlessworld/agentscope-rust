//! Agent state, sub-contexts and persistent state stores, mirroring
//! `io.agentscope.core.state`.

pub mod agent_state;
pub mod conflict_policy;
pub mod context;
pub mod list_hash;
pub mod session_info;
pub mod store;
pub mod verification;

pub use agent_state::AgentState;
pub use conflict_policy::ConflictPolicy;
pub use context::{
    PlanModeContextState, ReadCacheEntry, Task, TaskContextState, TaskState, ToolContextState,
};
pub use list_hash::{compute_hash, has_changed, needs_full_rewrite, EMPTY_HASH, SAMPLING_THRESHOLD};
pub use session_info::SessionInfo;
pub use store::{
    AgentStateStore, InMemoryAgentStateStore, JsonFileAgentStateStore, VersionedState, UNVERSIONED,
};
pub use verification::{EvidenceBinding, TaskVerification, VerificationOutcome};
