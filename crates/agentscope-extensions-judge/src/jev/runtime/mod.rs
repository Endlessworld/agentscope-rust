//! The `agentscope-core` / `agentscope-harness` seam bindings for JEV.
//!
//! Every other `jev` sub-module is deliberately framework-free: pure functions, DTOs and state
//! machines that can be tested offline with no agent runtime. This module is the **only** place
//! that binds those state machines to a host, and it owns exactly the seams the tracker listed as
//! blocking:
//!
//! * [`archive`] — `StoreJevContextArchive` over the harness [`BaseStore`](agentscope_harness::filesystem::BaseStore)
//!   (content-addressed keys, create-if-absent CAS, fail-closed reads).
//! * [`tools`] — the `@Tool` registration layer for `search_evidence`, `search_knowledge` and
//!   `review_code_snapshot`, plus [`register_jev_tools`].
//! * [`middleware`] — the four `Middleware` hooks (trace evaluation, response screening,
//!   supervision observation, phase routing dispatch).
//! * [`quality`] — the response middleware's quality/reviser branches: the assess loop, the
//!   revision seam, `replaceText` and the capture/buffer limits.
//! * [`review_driver`] — the staged `JevCodeReviewer` driver
//!   (`screen → profile → locate → mechanism → severity → route`) over an injected System One
//!   caller, with Java's request budget, short-circuits and report aggregation.
//!
//! Keeping the bindings here means the four middlewares and three tools cannot regress the pure
//! layers they wrap: the pure modules keep their signatures and their tests.

pub mod archive;
pub mod middleware;
pub mod quality;
pub mod review_driver;
pub mod tools;

pub use archive::{context_namespace, StoreJevContextArchive, NAMESPACE_PREFIX, SNAPSHOT_KEY};
pub use middleware::{
    PhaseRoutingMiddleware, ResponseMiddleware, RouteResolver, SupervisionMiddleware,
    TraceEvaluationMiddleware,
};
pub use quality::{
    assess, capture, collect_text, judge_status_name, outcome_name, replace_text, revise_with_model,
    QualitySettings, Reviser, Revision, ScreenOutcome, ScreenPolicy, Screener,
};
pub use review_driver::{
    ReviewDriver, EVIDENCE_RULES, FILE_LIMIT, FOLLOW_UP_PENDING, INCOMPLETE_REVIEW,
    NO_SOURCE_FILES, REVIEW_FINISHED,
};
pub use tools::{
    register_jev_tools, CodeReviewDriver, CodeReviewSource, EvidenceSearchTool, EvidenceSource,
    JevCodeReviewerDriver, JevToolConfig, KnowledgeSearchTool, ReviewCodeTool, ToolScope,
    CODE_REVIEW_TOOL_NAME, EVIDENCE_TOOL_NAME, KNOWLEDGE_TOOL_NAME,
};
