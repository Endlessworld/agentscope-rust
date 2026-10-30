//! Call-scoped model routing (`io.agentscope.extensions.judge.jev.routing`).
//!
//! * [`catalog`] — host-owned capabilities, prices and the deterministic exclusion rules
//!   (`JevRouteCatalog`), fully ported.
//! * [`phase_routing`] — `JevPhaseRouting`: route offers, pinned model / explicit phase handling,
//!   the `route`/`effort` judge questions, response validation, the fail-closed fallbacks and the
//!   fresh-snapshot re-validation. The reactive `onAgent`/`onModelCall` dispatch loop lives in
//!   [`crate::jev::runtime::middleware`], because it needs the core middleware traits.
//!
//! Deviation: Java's `Candidate` carries a core `Model` object; the port keeps the model id and
//! leaves resolution to the host, so no credentials or model instances enter JEV decisions.

pub mod catalog;
pub mod phase_routing;

pub use catalog::{
    excluded, effort, Candidate, Effort, Price, Quota, Route, RouteRequest, Snapshot, TokenUsage,
};
pub use phase_routing::{
    candidate_state, input_restriction, route_reason, CallRecord, InputRestriction, JevPhaseRouting,
    Revalidation, RouteOffer, RoutingInput, RoutingReport, Skipped, EFFORT_INSTRUCTIONS,
    NONE_ROUTE_RUBRIC, PHASE_ROUTING_UNAVAILABLE, PINNED_ROUTE_ID, ROUTING_STATE_MAX_CHARS,
    ROUTE_INSTRUCTIONS,
};
