//! Explicit application phase boundaries (`JevStageRouter`).
//!
//! Java's router is a thin wrapper: it stamps `jev.stage` onto a child `RuntimeContext` and then
//! delegates both hooks to a `JevModelRouterMiddleware`. When that middleware is built with a
//! `JevPhaseRouting` (its "advanced" mode) the delegation is a pure pass-through, so the *stage ->
//! model* decision is exactly `JevPhaseRouting`'s explicit-phase branch. That branch is ported, so
//! this module implements the stage router **without** the `example` sub-package:
//!
//! * [`JevStageRouter::begin`] validates the stage/goal pair and returns the [`Phase`] the host
//!   carries through the run (Java's `ctx.put("jev.stage", stage)`), plus the exact prompt Java
//!   sends (`Stage: {stage}; goal: {goal}`).
//! * [`JevStageRouter::resolve`] resolves the phase against a snapshot by setting
//!   [`RoutingInput::phase`], i.e. the `EXPLICIT_PHASE` branch of
//!   [`JevPhaseRouting::resolve`](crate::jev::routing::JevPhaseRouting::resolve). The judge is never
//!   asked, and an unknown or uncapable stage yields `EXPLICIT_TARGET_UNAVAILABLE` (fail-closed:
//!   the call keeps its original target).
//!
//! Not ported: the `example` sub-package's *legacy* mode, which asks Jev to choose between core
//! `Model` handles with `eligible`/`compatible` predicates ([`STAGE_ROUTER_UNAVAILABLE`]). It is a
//! demonstration of the middleware, needs model instances this crate deliberately does not hold,
//! and the phase-routing branch covers the same `stage -> model` requirement.

use std::sync::Arc;

use crate::error::JevError;
use crate::jev::execution::Decision;
use crate::jev::routing::phase_routing::{JevPhaseRouting, RoutingInput, RoutingReport};
use crate::jev::routing::Snapshot;

/// Why the legacy (`example` sub-package) model-choice mode is not ported.
pub const STAGE_ROUTER_UNAVAILABLE: &str =
    "JevStageRouter's legacy model-choice mode lives in the `example` sub-package, needs core Model handles and eligibility/compatibility predicates; use JevPhaseRouting explicit phases instead";

/// The stage metadata key Java stamps on the child context.
pub const STAGE_METADATA_KEY: &str = "jev.stage";

/// An explicit application phase (`JevStageRouter.Phase`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    /// The stage name; also the route id the phase resolves to.
    pub name: String,
    /// The goal the stage was opened for.
    pub goal: String,
}

impl Phase {
    /// The user message Java sends when it opens the phase scope (*"Stage: X; goal: Y"*).
    pub fn prompt(&self) -> String {
        format!("Stage: {}; goal: {}", self.name, self.goal)
    }
}

/// Explicit application phase boundaries with an independent routing scope per phase.
#[derive(Debug, Clone)]
pub struct JevStageRouter {
    routing: Arc<JevPhaseRouting>,
}

impl JevStageRouter {
    /// Creates the router over the phase-routing engine.
    pub fn new(routing: Arc<JevPhaseRouting>) -> Self {
        Self { routing }
    }

    /// Validates the inputs and opens a phase (`JevStageRouter.begin`).
    ///
    /// Java additionally runs `router.onAgent(..., Flux.empty())`, which only stores the routing
    /// scope in the reactor context; the port carries the phase explicitly instead, so there is
    /// nothing to run here.
    pub fn begin(&self, stage: &str, goal: &str) -> Result<Phase, JevError> {
        if stage.trim().is_empty() || goal.trim().is_empty() {
            return Err(JevError::invalid("stage and goal required"));
        }
        Ok(Phase {
            name: stage.to_string(),
            goal: goal.to_string(),
        })
    }

    /// Resolves the phase's model against a snapshot (`JevStageRouter.onModelCall`).
    ///
    /// The phase is resolved as an explicit route, so the judge caller is not used and an
    /// unknown stage fails closed with `EXPLICIT_TARGET_UNAVAILABLE`.
    pub async fn resolve(
        &self,
        phase: &Phase,
        snapshot: &Snapshot,
        input: &RoutingInput,
    ) -> Decision<RoutingReport> {
        let mut input = input.clone();
        input.phase = Some(phase.name.clone());
        self.routing.resolve(snapshot, &input).await
    }

    /// The metadata key Java uses for the stage.
    pub fn metadata_key(&self) -> &'static str {
        STAGE_METADATA_KEY
    }

    /// The phase-routing engine the router delegates to.
    pub fn routing(&self) -> &Arc<JevPhaseRouting> {
        &self.routing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::{Caller, CallFailure};
    use crate::jev::execution::Status;
    use crate::jev::routing::catalog::{
        Candidate, Effort, Quota, Route, RouteRequest,
    };
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    fn candidate(id: &str) -> Candidate {
        Candidate::new(
            id,
            format!("{id} description"),
            Some(100_000),
            Some(8_000),
            true,
            [Effort::Low].into_iter().collect(),
            Quota::Available,
            None,
        )
        .unwrap()
    }

    fn snapshot(ids: &[&str]) -> Snapshot {
        Snapshot::new(
            ids.iter().map(|id| candidate(id)).collect(),
            100,
            0,
            None,
            None,
            0,
        )
        .unwrap()
    }

    fn routing(calls: Arc<Mutex<usize>>) -> JevPhaseRouting {
        let caller: Caller = Arc::new(move |_request| {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                *calls.lock().unwrap() += 1;
                Err(CallFailure::backend("the stage router must not ask the judge"))
            })
        });
        JevPhaseRouting::new(
            vec![
                Route::new("plan", "planning", vec!["m-plan".to_string()]).unwrap(),
                Route::new("code", "coding", vec!["m-code".to_string()]).unwrap(),
            ],
            ["m-plan", "m-code"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<String>>(),
            0.6,
            false,
        )
        .unwrap()
        .with_caller(caller)
    }

    #[test]
    fn stages_and_goals_are_required() {
        let router = JevStageRouter::new(Arc::new(routing(Arc::new(Mutex::new(0)))));
        assert_eq!(
            router.begin("", "goal").unwrap_err().message(),
            "stage and goal required"
        );
        assert_eq!(
            router.begin("plan", "  ").unwrap_err().message(),
            "stage and goal required"
        );
        let phase = router.begin("plan", "ship it").unwrap();
        assert_eq!(phase.name, "plan");
        assert_eq!(phase.goal, "ship it");
        assert_eq!(phase.prompt(), "Stage: plan; goal: ship it");
        assert_eq!(router.metadata_key(), "jev.stage");
        assert_eq!(STAGE_METADATA_KEY, "jev.stage");
        assert!(STAGE_ROUTER_UNAVAILABLE.contains("example"));
    }

    #[tokio::test]
    async fn a_stage_routes_to_its_first_capable_model_without_asking_the_judge() {
        let calls = Arc::new(Mutex::new(0));
        let router = JevStageRouter::new(Arc::new(routing(Arc::clone(&calls))));
        let phase = router.begin("code", "implement the parser").unwrap();
        let decision = router
            .resolve(
                &phase,
                &snapshot(&["m-plan", "m-code"]),
                &RoutingInput::new("implement", RouteRequest::default(), None),
            )
            .await;
        assert_eq!(decision.status, Status::Decided);
        assert_eq!(decision.reason, crate::jev::routing::route_reason::EXPLICIT_PHASE);
        let report = decision.value.unwrap();
        assert_eq!(report.route.as_deref(), Some("code"));
        assert_eq!(report.model.as_deref(), Some("m-code"));
        assert!(!router.routing().has_caller() || *calls.lock().unwrap() == 0);
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn an_unknown_or_uncapable_stage_fails_closed() {
        let calls = Arc::new(Mutex::new(0));
        let router = JevStageRouter::new(Arc::new(routing(Arc::clone(&calls))));
        let unknown = router.begin("review", "check it").unwrap();
        let decision = router
            .resolve(
                &unknown,
                &snapshot(&["m-plan", "m-code"]),
                &RoutingInput::new("hi", RouteRequest::default(), None),
            )
            .await;
        assert_eq!(decision.status, Status::Inconclusive);
        assert_eq!(
            decision.reason,
            crate::jev::routing::route_reason::EXPLICIT_TARGET_UNAVAILABLE
        );
        assert!(decision.value.unwrap().model.is_none());

        // The stage exists, but its only model is not in the fresh snapshot.
        let code = router.begin("code", "implement").unwrap();
        let decision = router
            .resolve(
                &code,
                &snapshot(&["m-plan"]),
                &RoutingInput::new("hi", RouteRequest::default(), None),
            )
            .await;
        assert_eq!(
            decision.reason,
            crate::jev::routing::route_reason::EXPLICIT_TARGET_UNAVAILABLE
        );
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_pinned_request_still_keeps_its_target() {
        let calls = Arc::new(Mutex::new(0));
        let router = JevStageRouter::new(Arc::new(routing(Arc::clone(&calls))));
        let phase = router.begin("code", "implement").unwrap();
        let mut input = RoutingInput::new("hi", RouteRequest::default(), None);
        input.request.pinned_options = true;
        let decision = router.resolve(&phase, &snapshot(&["m-code"]), &input).await;
        assert_eq!(decision.status, Status::Inconclusive);
        assert_eq!(decision.reason, "PINNED_REQUEST_OPTIONS");
    }
}
