//! Call-scoped model routing (`io.agentscope.extensions.judge.jev.routing`).
//!
//! * [`catalog`] — host-owned capabilities, prices and the deterministic exclusion rules
//!   (`JevRouteCatalog`), fully ported.
//! * [`phase_routing`] — `JevPhaseRouting`. The reactive `onAgent`/`onModelCall` hooks belong to the
//!   core middleware trait, which this crate does not depend on; the pure parts (input restriction
//!   detection and the candidate state projection) are ported and the dispatch loop is a documented
//!   skeleton. See `docs/extensions/JUDGE__GAP_TRACKER.md` §4.2.
//!
//! Deviation: Java's `Candidate` carries a core `Model` object; the port keeps the model id and
//! leaves resolution to the host, so no credentials or model instances enter JEV decisions.

pub mod catalog;
pub mod phase_routing;

pub use catalog::{
    excluded, effort, Candidate, Effort, Price, Quota, Route, RouteRequest, Snapshot, TokenUsage,
};
pub use phase_routing::{
    candidate_state, input_restriction, CallRecord, InputRestriction, JevPhaseRouting, RoutingReport,
    Skipped, PHASE_ROUTING_UNAVAILABLE,
};
