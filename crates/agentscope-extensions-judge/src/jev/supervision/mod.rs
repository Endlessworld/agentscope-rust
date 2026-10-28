//! Read-only supervision (`io.agentscope.extensions.judge.jev.supervision`).
//!
//! * [`evidence`] — host-supplied evidence and its scope-bound verification (`SupervisionEvidence`).
//! * [`supervisor`] — independent checks and deterministic arbitration (`JevTaskSupervisor`).
//! * [`middleware`] — `JevSupervisionMiddleware`'s schedule, observation record and the bounded
//!   output-tail helper; the reactive observer loop itself needs the core middleware/event types
//!   and is a documented skeleton (`docs/extensions/JUDGE__GAP_TRACKER.md` §4.2).
//!
//! Every advice value is inert: nothing here interrupts, denies, finishes or writes.

pub mod evidence;
pub mod middleware;
pub mod supervisor;

pub use evidence::{SupervisionEvidence, Verification};
pub use middleware::{
    append_tail, EvidenceRequest, EvidenceSource, Observation, Schedule, SupervisionMiddlewareUnavailable,
    SUPERVISION_MIDDLEWARE_UNAVAILABLE,
};
pub use supervisor::{
    Check, Finding, JevTaskSupervisor, Limits, Proposal, Report, Scope, Snapshot, TaskAdvice,
    Thresholds, Truth,
};
