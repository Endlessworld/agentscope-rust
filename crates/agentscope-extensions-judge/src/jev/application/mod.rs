//! Explicit application workflows (`io.agentscope.extensions.judge.jev.application`).
//!
//! Java's ten classes split into one shared engine and nine thin, explicitly-invoked workflows:
//!
//! * [`selector`] — independent candidate classification (`JevCandidateSelector`).
//! * [`planners`] — `JevBrowserPlanner`, `JevContextPlanner`, `JevTeamPlanner`, `JevRag`,
//!   `JevMemoryGate`, `JevSupervisor`, `JevCustomerSupport`.
//! * [`draft_pipeline`] — buffered draft generation with bounded revisions (`JevDraftPipeline`).
//! * [`stage_router`] — the explicit phase boundary skeleton (`JevStageRouter`).
//!
//! Every workflow is advice-only: none of them dispatches a subagent, mutates agent state, persists
//! memory, executes a tool or sends a reply.
//!
//! Deviations, all tracked in `docs/extensions/JUDGE__GAP_TRACKER.md` §4.2:
//!
//! * Java threads `RuntimeContext` for observation; the Rust execution seam has no context, so the
//!   workflows take plain state values and the caller owns observation.
//! * `JevStageRouter` needs `JevModelRouterMiddleware` from the `example` sub-package (not migrated);
//!   it is a documented skeleton that fails closed.

pub mod draft_pipeline;
pub mod planners;
pub mod selector;
pub mod stage_router;

pub use draft_pipeline::{DraftOutcome, DraftSource, JevDraftPipeline, Revision};
pub use planners::{
    BrowserAction, BrowserOperation, BrowserProposal, ContextExchange, ContextPlan,
    CustomerSupportReport, JevBrowserPlanner, JevContextPlanner, JevCustomerSupport, JevMemoryGate,
    JevRag, JevSupervisor, JevTeamPlanner, KnowledgeDocument, MemoryFact, RagRetrieval,
    SupervisorAdvice, SupervisorReport, TeamMember,
};
pub use selector::{JevCandidateSelector, Selection};
pub use stage_router::{JevStageRouter, Phase, STAGE_ROUTER_UNAVAILABLE};
