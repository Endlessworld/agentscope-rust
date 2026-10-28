//! `JevPhaseRouting`: call-scoped routing with explicit phase boundaries.
//!
//! Java's router is a core `MiddlewareBase`: `onAgent` opens a routing scope and `onModelCall`
//! resolves a decision, re-validates it against a fresh snapshot, then dispatches the model call
//! with the chosen model and effort. Both hooks need the core middleware, `ModelCallInput`,
//! `AgentEvent` and `GenerateOptions` types, which this crate does not depend on, so the dispatch
//! loop is **not** ported.
//!
//! What *is* ported are the two pure helpers the loop depends on:
//!
//! * [`input_restriction`] — the reasons a request must not be re-routed at all.
//! * [`candidate_state`] — the candidate projection sent to System One.
//!
//! [`JevPhaseRouting::resolve`] therefore fails closed with [`PHASE_ROUTING_UNAVAILABLE`].

use serde_json::{json, Value};

use crate::error::JevError;

use super::catalog::{Candidate, Effort};

/// Why a model call must keep its original target.
pub const PHASE_ROUTING_UNAVAILABLE: &str =
    "JevPhaseRouting needs the core middleware (onAgent/onModelCall, ModelCallInput, AgentEvent); the dispatch loop is not ported";

/// A reason a request cannot be re-routed (`JevPhaseRouting.inputRestriction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRestriction {
    /// The caller pinned api key/base url/endpoint/model.
    PinnedRequestOptions,
    /// The input contains a block the router cannot forward.
    NonTextInput,
}

impl InputRestriction {
    /// The Java reason string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PinnedRequestOptions => "PINNED_REQUEST_OPTIONS",
            Self::NonTextInput => "NON_TEXT_INPUT",
        }
    }
}

/// One candidate the router refused, and why (`JevPhaseRouting.Skipped`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The candidate id.
    pub model: String,
    /// The exclusion reason.
    pub reason: String,
}

/// The routing decision reported to the host (`JevPhaseRouting.Report`).
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingReport {
    /// The chosen route id, when the judge chose one.
    pub route: Option<String>,
    /// The chosen model id.
    pub model: Option<String>,
    /// The effort that will be applied.
    pub effort: Option<String>,
    /// The effort the judge suggested before the floor was applied.
    pub suggested_effort: Option<String>,
    /// The judge's confidence in the route.
    pub confidence: Option<f64>,
    /// The route probabilities.
    pub probabilities: std::collections::BTreeMap<String, f64>,
    /// The candidates that were skipped.
    pub skipped: Vec<Skipped>,
    /// The model that answered the routing question.
    pub judge_model: Option<String>,
}

/// One dispatched call observation (`JevPhaseRouting.CallRecord`).
#[derive(Debug, Clone, PartialEq)]
pub struct CallRecord {
    /// The phase or route the call was dispatched for.
    pub phase: Option<String>,
    /// The model the router suggested.
    pub suggested_model: Option<String>,
    /// The model actually dispatched.
    pub dispatched_model: Option<String>,
    /// The effort the caller requested.
    pub requested_effort: Option<String>,
    /// `COMPLETED`, `CANCELLED` or `ERROR`.
    pub status: String,
    /// Why the call had this shape.
    pub reason: Option<String>,
    /// Wall-clock time spent.
    pub elapsed_millis: u64,
    /// The price version used for the estimate.
    pub price_version: Option<String>,
    /// The estimated cost, when the host supplied a price and usage.
    pub estimated_usd: Option<f64>,
}

/// Call-scoped routing (`JevPhaseRouting`) — fail-closed skeleton.
#[derive(Debug, Clone)]
pub struct JevPhaseRouting {
    routes: Vec<super::catalog::Route>,
    allowed_models: std::collections::BTreeSet<String>,
    confidence: f64,
    picks_effort: bool,
}

impl JevPhaseRouting {
    /// Validates the routing configuration (`JevPhaseRouting` constructor).
    pub fn new(
        routes: Vec<super::catalog::Route>,
        allowed_models: std::collections::BTreeSet<String>,
        confidence: f64,
        picks_effort: bool,
    ) -> Result<Self, JevError> {
        let route_ids: std::collections::BTreeSet<&String> =
            routes.iter().map(|route| &route.id).collect();
        let invalid = routes.is_empty()
            || routes.len() > 64
            || allowed_models.is_empty()
            || allowed_models.len() > 128
            || route_ids.len() != routes.len()
            || routes
                .iter()
                .flat_map(|route| route.models.iter())
                .any(|model| !allowed_models.contains(model))
            || !confidence.is_finite()
            || !(0.0..=1.0).contains(&confidence);
        if invalid {
            return Err(JevError::invalid("invalid phase routing configuration"));
        }
        Ok(Self {
            routes,
            allowed_models,
            confidence,
            picks_effort,
        })
    }

    /// The configured confidence floor.
    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    /// Whether the router may pick a reasoning effort.
    pub fn picks_effort(&self) -> bool {
        self.picks_effort
    }

    /// The configured route ids, in order.
    pub fn route_ids(&self) -> Vec<&str> {
        self.routes.iter().map(|route| route.id.as_str()).collect()
    }

    /// The models the router may use.
    pub fn allowed_models(&self) -> &std::collections::BTreeSet<String> {
        &self.allowed_models
    }

    /// Resolves a routing decision.
    ///
    /// Java would open a routing scope here; the port fails closed until the middleware seam
    /// exists, so a caller never silently routes to a model it did not choose.
    pub fn resolve(&self) -> Result<RoutingReport, JevError> {
        Err(JevError::protocol(PHASE_ROUTING_UNAVAILABLE))
    }
}

/// The reasons a request must keep its original target (`inputRestriction`).
///
/// `pinned_options` mirrors Java's check for an api key, base url, endpoint path or model name on
/// the call options; `has_non_text` mirrors its block-type scan (text, thinking and tool calls are
/// forwardable; any other block, and any non-text tool result part, is not).
pub fn input_restriction(
    pinned_options: bool,
    has_non_text: bool,
) -> Option<InputRestriction> {
    if pinned_options {
        return Some(InputRestriction::PinnedRequestOptions);
    }
    if has_non_text {
        return Some(InputRestriction::NonTextInput);
    }
    None
}

/// The candidate projection sent to System One (`candidateState`).
pub fn candidate_state(candidate: &Candidate) -> Value {
    let mut state = serde_json::Map::new();
    state.insert("id".to_string(), json!(candidate.id));
    state.insert("description".to_string(), json!(candidate.description));
    state.insert("context_window".to_string(), json!(candidate.context_window));
    state.insert("max_output".to_string(), json!(candidate.max_output));
    state.insert("supports_tools".to_string(), json!(candidate.supports_tools));
    state.insert("quota".to_string(), json!(candidate.quota.as_str()));
    let efforts: Vec<&str> = candidate.efforts.iter().map(Effort::value).collect();
    state.insert("efforts".to_string(), json!(efforts));
    if let Some(price) = &candidate.price {
        state.insert(
            "price_usd_per_million".to_string(),
            json!({
                "version": price.version,
                "input": price.input,
                "output": price.output,
                "cached": price.cached,
                "creation": price.creation,
            }),
        );
    }
    Value::Object(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::routing::catalog::{Price, Quota};
    use std::collections::BTreeSet;

    fn route(id: &str, models: &[&str]) -> super::super::catalog::Route {
        super::super::catalog::Route::new(
            id,
            "description",
            models.iter().map(|model| (*model).to_string()).collect(),
        )
        .unwrap()
    }

    fn allowed(models: &[&str]) -> BTreeSet<String> {
        models.iter().map(|model| (*model).to_string()).collect()
    }

    #[test]
    fn configuration_is_validated() {
        assert_eq!(
            JevPhaseRouting::new(vec![], allowed(&["m"]), 0.5, false)
                .unwrap_err()
                .message(),
            "invalid phase routing configuration"
        );
        assert_eq!(
            JevPhaseRouting::new(vec![route("fast", &["m"])], BTreeSet::new(), 0.5, false)
                .unwrap_err()
                .message(),
            "invalid phase routing configuration"
        );
        assert_eq!(
            JevPhaseRouting::new(vec![route("fast", &["other"])], allowed(&["m"]), 0.5, false)
                .unwrap_err()
                .message(),
            "invalid phase routing configuration"
        );
        for confidence in [-0.1, 1.1, f64::NAN] {
            assert_eq!(
                JevPhaseRouting::new(vec![route("fast", &["m"])], allowed(&["m"]), confidence, false)
                    .unwrap_err()
                    .message(),
                "invalid phase routing configuration"
            );
        }
        let duplicated = vec![route("fast", &["m"]), route("fast", &["m"])];
        assert!(JevPhaseRouting::new(duplicated, allowed(&["m"]), 0.5, false).is_err());

        let router = JevPhaseRouting::new(
            vec![route("fast", &["m"])],
            allowed(&["m"]),
            0.6,
            true,
        )
        .unwrap();
        assert_eq!(router.confidence(), 0.6);
        assert!(router.picks_effort());
        assert_eq!(router.route_ids(), vec!["fast"]);
        assert!(router.allowed_models().contains("m"));
    }

    #[test]
    fn resolution_fails_closed() {
        let router = JevPhaseRouting::new(
            vec![route("fast", &["m"])],
            allowed(&["m"]),
            0.6,
            false,
        )
        .unwrap();
        let error = router.resolve().unwrap_err();
        assert_eq!(error.message(), PHASE_ROUTING_UNAVAILABLE);
        assert!(error.message().contains("middleware"));
    }

    #[test]
    fn input_restrictions_and_candidate_state_are_pure() {
        assert_eq!(input_restriction(false, false), None);
        assert_eq!(
            input_restriction(true, false),
            Some(InputRestriction::PinnedRequestOptions)
        );
        assert_eq!(
            input_restriction(false, true),
            Some(InputRestriction::NonTextInput)
        );
        assert_eq!(InputRestriction::PinnedRequestOptions.as_str(), "PINNED_REQUEST_OPTIONS");
        assert_eq!(InputRestriction::NonTextInput.as_str(), "NON_TEXT_INPUT");

        let candidate = Candidate::new(
            "m",
            "model",
            Some(1000),
            Some(10),
            true,
            BTreeSet::from([Effort::High, Effort::Low]),
            Quota::Low,
            Some(Price::new("v1", 1.0, 2.0, 0.1, 0.2).unwrap()),
        )
        .unwrap();
        let state = candidate_state(&candidate);
        assert_eq!(state["quota"], json!("LOW"));
        assert_eq!(state["efforts"], json!(["low", "high"]));
        assert_eq!(state["price_usd_per_million"]["cached"], json!(0.1));
        assert_eq!(state["context_window"], json!(1000));
    }
}
