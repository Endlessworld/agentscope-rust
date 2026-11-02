//! Host-owned capabilities and estimates (`JevRouteCatalog`). No model name is interpreted as a
//! capability: every limit below is supplied by the host.

use std::collections::BTreeSet;

use crate::error::JevError;

/// Remaining quota of a candidate (`Quota`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quota {
    /// Quota is available.
    Available,
    /// Quota is nearly exhausted.
    Low,
    /// Quota is exhausted; the candidate is excluded.
    Exhausted,
    /// The host does not know.
    #[default]
    Unknown,
}

impl Quota {
    /// The Java enum name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "AVAILABLE",
            Self::Low => "LOW",
            Self::Exhausted => "EXHAUSTED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// Reasoning effort, ordered from lowest to highest (`Effort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effort {
    /// No reasoning.
    None,
    /// Minimal reasoning.
    Minimal,
    /// Low reasoning.
    Low,
    /// Medium reasoning.
    Medium,
    /// High reasoning.
    High,
    /// Extra-high reasoning.
    Xhigh,
    /// Maximum reasoning.
    Max,
    /// Ultra reasoning.
    Ultra,
}

impl Effort {
    /// The lowercase wire value.
    pub fn value(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    /// Parses a case-insensitive effort name; unknown names are `None`.
    pub fn parse(value: Option<&str>) -> Option<Self> {
        let value = value?;
        match value.to_ascii_uppercase().as_str() {
            "NONE" => Some(Self::None),
            "MINIMAL" => Some(Self::Minimal),
            "LOW" => Some(Self::Low),
            "MEDIUM" => Some(Self::Medium),
            "HIGH" => Some(Self::High),
            "XHIGH" => Some(Self::Xhigh),
            "MAX" => Some(Self::Max),
            "ULTRA" => Some(Self::Ultra),
            _ => None,
        }
    }
}

/// Token counters used for price estimation (`ChatUsage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Input tokens, including cached and cache-creation tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Cached input tokens.
    pub cached_tokens: i64,
    /// Cache-creation tokens.
    pub cache_creation_tokens: i64,
}

/// Rates per million tokens (`Price`).
#[derive(Debug, Clone, PartialEq)]
pub struct Price {
    /// The price version, reported with every call record.
    pub version: String,
    /// Input rate per million tokens.
    pub input: f64,
    /// Output rate per million tokens.
    pub output: f64,
    /// Cached-input rate per million tokens.
    pub cached: f64,
    /// Cache-creation rate per million tokens.
    pub creation: f64,
}

impl Price {
    /// Creates and validates a price (`Price` compact constructor).
    pub fn new(
        version: impl Into<String>,
        input: f64,
        output: f64,
        cached: f64,
        creation: f64,
    ) -> Result<Self, JevError> {
        let version = version.into();
        require_text(&version, "price version", 256)?;
        for rate in [input, output, cached, creation] {
            if !rate.is_finite() || rate < 0.0 {
                return Err(JevError::invalid("invalid rate"));
            }
        }
        Ok(Self {
            version,
            input,
            output,
            cached,
            creation,
        })
    }

    /// Estimates the cost of one call; `None` when the counters are inconsistent.
    pub fn estimate_usd(&self, usage: Option<&TokenUsage>) -> Option<f64> {
        let usage = usage?;
        if usage.input_tokens < 0
            || usage.output_tokens < 0
            || usage.cached_tokens < 0
            || usage.cache_creation_tokens < 0
            || usage.cached_tokens + usage.cache_creation_tokens > usage.input_tokens
        {
            return None;
        }
        let total = ((usage.input_tokens - usage.cached_tokens - usage.cache_creation_tokens)
            as f64
            * self.input
            + usage.output_tokens as f64 * self.output
            + usage.cached_tokens as f64 * self.cached
            + usage.cache_creation_tokens as f64 * self.creation)
            / 1_000_000.0;
        total.is_finite().then_some(total)
    }
}

/// A routable model (`Candidate`), described only by host-supplied facts.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The model id.
    pub id: String,
    /// A human readable description.
    pub description: String,
    /// The context window in tokens.
    pub context_window: Option<i64>,
    /// The maximum output in tokens.
    pub max_output: Option<i64>,
    /// Whether the model supports tools.
    pub supports_tools: bool,
    /// The supported reasoning efforts.
    pub efforts: BTreeSet<Effort>,
    /// The remaining quota.
    pub quota: Quota,
    /// The price, when known.
    pub price: Option<Price>,
}

impl Candidate {
    /// Creates and validates a candidate (`Candidate` compact constructor).
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        context_window: Option<i64>,
        max_output: Option<i64>,
        supports_tools: bool,
        efforts: BTreeSet<Effort>,
        quota: Quota,
        price: Option<Price>,
    ) -> Result<Self, JevError> {
        let id = id.into();
        let description = description.into();
        require_text(&id, "model id", 256)?;
        require_text(&description, "model description", 2000)?;
        if context_window.is_some_and(|window| window < 1) || max_output.is_some_and(|max| max < 1)
        {
            return Err(JevError::invalid("positive capability limits required"));
        }
        Ok(Self {
            id,
            description,
            context_window,
            max_output,
            supports_tools,
            efforts,
            quota,
            price,
        })
    }

    /// The candidate state sent to System One (`JevPhaseRouting.candidateState`).
    pub fn state(&self) -> serde_json::Value {
        let mut state = serde_json::Map::new();
        state.insert("id".to_string(), serde_json::json!(self.id));
        state.insert(
            "description".to_string(),
            serde_json::json!(self.description),
        );
        state.insert(
            "context_window".to_string(),
            serde_json::json!(self.context_window),
        );
        state.insert("max_output".to_string(), serde_json::json!(self.max_output));
        state.insert(
            "supports_tools".to_string(),
            serde_json::json!(self.supports_tools),
        );
        state.insert("quota".to_string(), serde_json::json!(self.quota.as_str()));
        state.insert(
            "efforts".to_string(),
            serde_json::json!(self.efforts.iter().map(Effort::value).collect::<Vec<_>>()),
        );
        if let Some(price) = &self.price {
            state.insert(
                "price_usd_per_million".to_string(),
                serde_json::json!({
                    "version": price.version,
                    "input": price.input,
                    "output": price.output,
                    "cached": price.cached,
                    "creation": price.creation,
                }),
            );
        }
        serde_json::Value::Object(state)
    }
}

/// A route's deterministic preference list (`Route`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The route id; `none` is reserved.
    pub id: String,
    /// The route description shown to the judge.
    pub description: String,
    /// The candidate ids, best first.
    pub models: Vec<String>,
}

impl Route {
    /// Creates and validates a route (`Route` compact constructor).
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        models: Vec<String>,
    ) -> Result<Self, JevError> {
        let id = id.into();
        let description = description.into();
        require_text(&id, "route id", 128)?;
        require_text(&description, "route description", 2000)?;
        let unique: BTreeSet<&String> = models.iter().collect();
        if id == "none" || models.is_empty() || models.len() > 64 || unique.len() != models.len() {
            return Err(JevError::invalid(
                "unique route models required; none is reserved",
            ));
        }
        for model in &models {
            require_text(model, "route model", 256)?;
        }
        Ok(Self {
            id,
            description,
            models,
        })
    }
}

/// The trusted per-run routing facts (`Snapshot`).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The candidates the host is willing to use.
    pub candidates: Vec<Candidate>,
    /// Estimated input tokens of the request.
    pub input_tokens: i64,
    /// Reserved output tokens.
    pub output_reserve: i64,
    /// The minimum effort the host demands.
    pub minimum_effort: Option<Effort>,
    /// A pinned model id; when set the judge does not choose.
    pub pinned_model: Option<String>,
    /// Recent consecutive failures, used by the host for its own policy.
    pub consecutive_failures: i32,
}

impl Snapshot {
    /// Creates and validates a snapshot (`Snapshot` compact constructor).
    pub fn new(
        candidates: Vec<Candidate>,
        input_tokens: i64,
        output_reserve: i64,
        minimum_effort: Option<Effort>,
        pinned_model: Option<String>,
        consecutive_failures: i32,
    ) -> Result<Self, JevError> {
        let unique: BTreeSet<&String> = candidates.iter().map(|candidate| &candidate.id).collect();
        if candidates.is_empty()
            || candidates.len() > 128
            || input_tokens < 0
            || output_reserve < 0
            || !(0..=10_000).contains(&consecutive_failures)
            || unique.len() != candidates.len()
        {
            return Err(JevError::invalid("invalid routing snapshot"));
        }
        if let Some(pinned) = &pinned_model {
            require_text(pinned, "pinned model", 256)?;
        }
        Ok(Self {
            candidates,
            input_tokens,
            output_reserve,
            minimum_effort,
            pinned_model,
            consecutive_failures,
        })
    }
}

/// The provider-visible facts of one model call that affect routability.
///
/// Java reads these from `ModelCallInput`; the port takes them directly so routing stays testable
/// without the core message/model types.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteRequest {
    /// Whether the request offers tools.
    pub tools_present: bool,
    /// The requested `max_tokens`.
    pub max_tokens: Option<i64>,
    /// The requested `max_completion_tokens`.
    pub max_completion_tokens: Option<i64>,
    /// The client's requested reasoning effort, if any.
    pub reasoning_effort: Option<String>,
    /// Whether the caller pinned request options (api key, base url, endpoint, model).
    pub pinned_options: bool,
    /// Whether the input contains a non-text block the router cannot send.
    pub non_text_input: bool,
}

/// Why `candidate` cannot serve `request`; `None` means it can (`JevRouteCatalog.excluded`).
pub fn excluded(
    candidate: &Candidate,
    snapshot: &Snapshot,
    request: &RouteRequest,
) -> Option<&'static str> {
    if candidate.quota == Quota::Exhausted {
        return Some("QUOTA_EXHAUSTED");
    }
    if request.tools_present && !candidate.supports_tools {
        return Some("TOOLS_UNSUPPORTED");
    }
    let mut requested = snapshot.output_reserve.max(request.max_tokens.unwrap_or(0));
    if let Some(max_completion) = request.max_completion_tokens {
        requested = requested.max(max_completion);
    }
    let Some(context_window) = candidate.context_window else {
        return Some("CONTEXT_UNKNOWN");
    };
    if snapshot.input_tokens > context_window - requested {
        return Some("CONTEXT_LIMIT");
    }
    if requested > 0 && candidate.max_output.is_none() {
        return Some("OUTPUT_LIMIT_UNKNOWN");
    }
    if candidate.max_output.is_some_and(|max| requested > max) {
        return Some("OUTPUT_LIMIT");
    }
    let client = Effort::parse(request.reasoning_effort.as_deref());
    if let Some(client) = client {
        if !candidate.efforts.contains(&client) {
            return Some("CLIENT_EFFORT_UNSUPPORTED");
        }
    }
    if let Some(floor) = snapshot.minimum_effort {
        if !candidate.efforts.iter().any(|effort| *effort >= floor) {
            return Some("EFFORT_FLOOR");
        }
        if client.is_some_and(|client| client < floor) {
            return Some("CLIENT_EFFORT_BELOW_FLOOR");
        }
    }
    None
}

/// The effort to apply: the smallest supported effort at or above `wanted`/`floor`
/// (`JevRouteCatalog.effort`).
///
/// Falls back to the highest supported effort when the floor is above every supported effort
/// (the host already decided the candidate is usable).
pub fn effort(
    candidate: &Candidate,
    wanted: Option<Effort>,
    floor: Option<Effort>,
) -> Option<Effort> {
    let mut target = wanted;
    if let Some(floor) = floor {
        if target.is_none_or(|target| target < floor) {
            target = Some(floor);
        }
    }
    let target = target?;
    candidate
        .efforts
        .iter()
        .filter(|effort| **effort >= target)
        .min()
        .copied()
        .or_else(|| candidate.efforts.iter().max().copied())
}

fn require_text(value: &str, field: &str, max: usize) -> Result<(), JevError> {
    if value.trim().is_empty() || value.chars().count() > max {
        return Err(JevError::invalid(format!("invalid {field}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn efforts(values: &[Effort]) -> BTreeSet<Effort> {
        values.iter().copied().collect()
    }

    fn sample_candidate(id: &str) -> Candidate {
        Candidate::new(
            id,
            "model",
            Some(200_000),
            Some(8192),
            true,
            efforts(&[Effort::Low, Effort::Medium, Effort::High]),
            Quota::Available,
            None,
        )
        .unwrap()
    }

    fn snapshot(candidates: Vec<Candidate>) -> Snapshot {
        Snapshot::new(candidates, 1_000, 4_000, None, None, 0).unwrap()
    }

    #[test]
    fn effort_values_parse_and_order_like_java_ordinals() {
        assert_eq!(Effort::parse(Some("HIGH")), Some(Effort::High));
        assert_eq!(Effort::parse(Some("high")), Some(Effort::High));
        assert_eq!(Effort::parse(Some("unknown")), None);
        assert_eq!(Effort::parse(None), None);
        assert_eq!(Effort::Xhigh.value(), "xhigh");
        assert!(Effort::None < Effort::Minimal);
        assert!(Effort::Minimal < Effort::Low);
        assert!(Effort::Max < Effort::Ultra);
        assert_eq!(Quota::default(), Quota::Unknown);
        assert_eq!(Quota::Exhausted.as_str(), "EXHAUSTED");
    }

    #[test]
    fn prices_validate_and_estimate_per_million() {
        let price = Price::new("2026-01", 3.0, 15.0, 0.3, 3.75).unwrap();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        };
        assert_eq!(price.estimate_usd(Some(&usage)), Some(18.0));
        let cached = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cached_tokens: 1_000_000,
            cache_creation_tokens: 0,
        };
        assert!((price.estimate_usd(Some(&cached)).unwrap() - 0.3).abs() < 1e-9);
        assert_eq!(price.estimate_usd(None), None);
        assert_eq!(
            price
                .estimate_usd(Some(&TokenUsage {
                    input_tokens: 10,
                    cached_tokens: 11,
                    ..TokenUsage::default()
                }))
                .is_none(),
            true,
            "cached tokens cannot exceed input tokens"
        );
        for (input, cached) in [(-1.0, 0.0), (1.0, -0.1), (f64::NAN, 0.0)] {
            assert_eq!(
                Price::new("v", input, 1.0, cached, 0.0)
                    .unwrap_err()
                    .message(),
                "invalid rate"
            );
        }
        assert_eq!(
            Price::new("", 1.0, 1.0, 0.0, 0.0).unwrap_err().message(),
            "invalid price version"
        );
    }

    #[test]
    fn candidates_routes_and_snapshots_validate() {
        assert_eq!(
            Candidate::new("", "d", None, None, true, BTreeSet::new(), Quota::Low, None)
                .unwrap_err()
                .message(),
            "invalid model id"
        );
        assert_eq!(
            Candidate::new(
                "m",
                "d",
                Some(0),
                None,
                true,
                BTreeSet::new(),
                Quota::Low,
                None
            )
            .unwrap_err()
            .message(),
            "positive capability limits required"
        );
        assert_eq!(
            Route::new("none", "d", vec!["m".to_string()])
                .unwrap_err()
                .message(),
            "unique route models required; none is reserved"
        );
        assert_eq!(
            Route::new("fast", "d", vec!["m".to_string(), "m".to_string()])
                .unwrap_err()
                .message(),
            "unique route models required; none is reserved"
        );
        assert_eq!(
            Snapshot::new(vec![], 0, 0, None, None, 0)
                .unwrap_err()
                .message(),
            "invalid routing snapshot"
        );
        assert_eq!(
            Snapshot::new(
                vec![sample_candidate("m"), sample_candidate("m")],
                0,
                0,
                None,
                None,
                0
            )
            .unwrap_err()
            .message(),
            "invalid routing snapshot"
        );
        let snapshot = snapshot(vec![sample_candidate("m")]);
        assert_eq!(snapshot.input_tokens, 1_000);
        assert_eq!(sample_candidate("m").state()["id"], json!("m"));
        assert_eq!(
            sample_candidate("m").state()["efforts"],
            json!(["low", "medium", "high"])
        );
        assert!(sample_candidate("m")
            .state()
            .get("price_usd_per_million")
            .is_none());
    }

    #[test]
    fn excluded_reports_the_first_blocking_rule() {
        let snapshot = snapshot(vec![sample_candidate("m")]);
        let request = RouteRequest::default();
        assert_eq!(excluded(&sample_candidate("m"), &snapshot, &request), None);

        let mut exhausted = sample_candidate("m");
        exhausted.quota = Quota::Exhausted;
        assert_eq!(
            excluded(&exhausted, &snapshot, &request),
            Some("QUOTA_EXHAUSTED")
        );

        let mut no_tools = sample_candidate("m");
        no_tools.supports_tools = false;
        let with_tools = RouteRequest {
            tools_present: true,
            ..RouteRequest::default()
        };
        assert_eq!(
            excluded(&no_tools, &snapshot, &with_tools),
            Some("TOOLS_UNSUPPORTED")
        );

        let mut unknown_context = sample_candidate("m");
        unknown_context.context_window = None;
        assert_eq!(
            excluded(&unknown_context, &snapshot, &request),
            Some("CONTEXT_UNKNOWN")
        );

        let tight =
            Snapshot::new(vec![sample_candidate("m")], 199_000, 4_000, None, None, 0).unwrap();
        assert_eq!(
            excluded(&sample_candidate("m"), &tight, &request),
            Some("CONTEXT_LIMIT")
        );

        let mut unknown_output = sample_candidate("m");
        unknown_output.max_output = None;
        assert_eq!(
            excluded(&unknown_output, &snapshot, &request),
            Some("OUTPUT_LIMIT_UNKNOWN")
        );

        let big_output = RouteRequest {
            max_tokens: Some(9_000),
            ..RouteRequest::default()
        };
        assert_eq!(
            excluded(&sample_candidate("m"), &snapshot, &big_output),
            Some("OUTPUT_LIMIT")
        );
        let big_completion = RouteRequest {
            max_completion_tokens: Some(9_000),
            ..RouteRequest::default()
        };
        assert_eq!(
            excluded(&sample_candidate("m"), &snapshot, &big_completion),
            Some("OUTPUT_LIMIT")
        );

        let ultra = RouteRequest {
            reasoning_effort: Some("ultra".to_string()),
            ..RouteRequest::default()
        };
        assert_eq!(
            excluded(&sample_candidate("m"), &snapshot, &ultra),
            Some("CLIENT_EFFORT_UNSUPPORTED")
        );

        let mut narrow = sample_candidate("m");
        narrow.efforts = efforts(&[Effort::Minimal]);
        let floor = Snapshot::new(vec![narrow.clone()], 0, 0, Some(Effort::High), None, 0).unwrap();
        assert_eq!(excluded(&narrow, &floor, &request), Some("EFFORT_FLOOR"));

        let mut wide = sample_candidate("m");
        wide.efforts = efforts(&[Effort::Low, Effort::High]);
        let floor = Snapshot::new(vec![wide.clone()], 0, 0, Some(Effort::High), None, 0).unwrap();
        let low = RouteRequest {
            reasoning_effort: Some("low".to_string()),
            ..RouteRequest::default()
        };
        assert_eq!(
            excluded(&wide, &floor, &low),
            Some("CLIENT_EFFORT_BELOW_FLOOR")
        );
        assert_eq!(excluded(&wide, &floor, &request), None);
    }

    #[test]
    fn effort_rounds_up_to_the_smallest_supported_value() {
        let candidate = sample_candidate("m");
        assert_eq!(
            effort(&candidate, Some(Effort::Low), None),
            Some(Effort::Low)
        );
        assert_eq!(
            effort(&candidate, Some(Effort::Minimal), None),
            Some(Effort::Low),
            "rounds up"
        );
        assert_eq!(effort(&candidate, None, None), None);
        assert_eq!(
            effort(&candidate, Some(Effort::Low), Some(Effort::High)),
            Some(Effort::High),
            "the floor wins"
        );
        assert_eq!(
            effort(&candidate, Some(Effort::Ultra), None),
            Some(Effort::High),
            "falls back to the highest supported effort"
        );
        let mut without_efforts = sample_candidate("m");
        without_efforts.efforts = BTreeSet::new();
        assert_eq!(effort(&without_efforts, Some(Effort::Low), None), None);
    }
}
