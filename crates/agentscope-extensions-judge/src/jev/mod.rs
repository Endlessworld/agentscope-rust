//! The JEV (*System One*) decision-model port.
//!
//! Sub-modules, in dependency order:
//!
//! * [`dto`] — wire types (questions, answers, request/response envelopes, usage).
//! * [`probability`] — the 4-decimal rounding-aware probability-sum rule.
//! * [`validation`] — request/response contract checks with Java-compatible messages.
//! * [`guardrail`] — content-safety policy + pure verdict fold.
//! * [`review`] — deterministic review regions and the review snapshot type.
//! * [`execution`] — per-subscription budget/observation DTOs.

pub mod application;
pub mod call;
pub mod client;
pub mod code_review;
pub mod confirmed_calls;
pub mod context;
pub mod dto;
pub mod evaluation;
pub mod evidence;
pub mod execution;
pub mod guardrail;
pub mod integration;
pub mod judge;
pub mod probability;
pub mod review;
pub mod routing;
pub mod runtime;
pub mod screening;
pub mod supervision;
pub mod validation;

pub use client::{is_retryable_status, JevClient, JevClientBuilder};
pub use confirmed_calls::{
    contains as confirmed_call_contains, matches as confirmed_call_matches,
    remember as remember_confirmed_call, Approval, JevConfirmedCalls, CONFIRMED_CALLS_KEY,
};
pub use dto::{
    Answer, ChoiceAnswer, ChoiceQuestion, JsonMap, NoulAnswer, NoulCriteria, NoulQuestion,
    Question, ScoreAnswer, ScoreQuestion, SystemOneRequest, SystemOneRequestBuilder,
    SystemOneResult, Usage,
};
pub use execution::{Decision, JevExecution, Mode, Options, Record, Status};
pub use guardrail::{Hazard, JevGuardrail, Outcome, Verdict};
pub use probability::{
    probability_sum_tolerance, sum_keyed, sum_probabilities, sum_within_tolerance,
    API_PROBABILITY_DECIMAL_PLACES, API_PROBABILITY_ROUNDING_BOUND,
    PROBABILITY_SUM_TOLERANCE_PER_OPTION,
};
pub use review::{
    patch_regions, source_regions, JevReviewInput, Region, ReviewFile, ReviewMode, Side,
    TestEvidence,
};
pub use screening::GuardrailScreener;
pub use validation::{
    level_keys, require_confidence, require_probability, validate_answer, validate_probabilities,
    validate_question, validate_request, validate_response, validate_result, CHOICE_OPTION_RANGE,
    SCORE_LEVEL_RANGE,
};

/// Base URL of the hosted System One API (Java `JevClient.DEFAULT_BASE_URL`).
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";

/// Default model name (Java `JevClient.DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "jev-latest";

/// System One endpoint path (Java `JevClient.SYSTEM_ONE_ENDPOINT`).
pub const SYSTEM_ONE_ENDPOINT: &str = "/v1/systemone";

/// Default request timeout in milliseconds (Java `JevClient.DEFAULT_TIMEOUT` = 5s).
pub const DEFAULT_TIMEOUT_MILLIS: u64 = 5_000;

/// Default retry attempts (Java `JevRetryPolicy.defaults().maxRetries()`).
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Default initial backoff in milliseconds (Java `JevRetryPolicy.defaults().initialBackoff()`).
pub const DEFAULT_INITIAL_BACKOFF_MILLIS: u64 = 500;

/// Retry settings for transient Jev System One HTTP failures (`JevRetryPolicy`).
///
/// Java's compact constructor rejects negative retries and non-positive backoff, which is what
/// [`RetryPolicy::new`] reproduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum retries after the first attempt.
    pub max_retries: u32,
    /// Initial backoff before the first retry.
    pub initial_backoff_ms: u64,
}

impl RetryPolicy {
    /// Creates a retry policy, rejecting a zero backoff.
    pub fn new(max_retries: u32, initial_backoff_ms: u64) -> Result<Self, crate::error::JevError> {
        if initial_backoff_ms == 0 {
            return Err(crate::error::JevError::invalid(
                "initialBackoff must be positive",
            ));
        }
        Ok(Self {
            max_retries,
            initial_backoff_ms,
        })
    }

    /// The Java `JevRetryPolicy.defaults()` policy: 2 retries after 500ms.
    pub fn defaults() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MILLIS,
        }
    }

    /// The maximum backoff Java uses (`initialBackoff * 8`).
    pub fn max_backoff_ms(&self) -> u64 {
        self.initial_backoff_ms.saturating_mul(8)
    }

    /// Whether `status_code` is retryable (`429`, `529` or `5xx`, per `JevClient.httpException`).
    pub fn is_retryable_status(status_code: u16) -> bool {
        status_code == 429 || status_code == 529 || status_code >= 500
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Builds the System One URL for a base URL (`JevClient.systemOneUrl`).
///
/// Trailing slashes are trimmed, an already-versioned base gets `/systemone`, and a base that
/// already ends in the endpoint is used as-is.
pub fn system_one_url(base_url: &str) -> String {
    let mut normalized = base_url.trim();
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized = &normalized[..normalized.len() - 1];
    }
    if normalized.ends_with(SYSTEM_ONE_ENDPOINT) {
        return normalized.to_string();
    }
    if normalized.ends_with("/v1") {
        return format!("{normalized}/systemone");
    }
    format!("{normalized}{SYSTEM_ONE_ENDPOINT}")
}

/// Picks the API key from the two environment values Java reads, preferring the non-blank one.
///
/// Wraps `JevClient.Builder.defaultApiKey(typesafe, jev)`; the caller supplies the (already
/// read) values so this stays a pure function.
pub fn default_api_key(
    typesafe_api_key: Option<&str>,
    jev_api_key: Option<&str>,
) -> Option<String> {
    match typesafe_api_key {
        Some(key) if !key.trim().is_empty() => Some(key.to_string()),
        _ => jev_api_key.map(str::to_string),
    }
}

/// The names of the environment variables Java checks, in precedence order.
pub const API_KEY_ENV_VARS: [&str; 2] = ["TYPESAFE_API_KEY", "JEV_API_KEY"];

/// Reads the API key from the environment the way `JevClient.Builder.defaultApiKey` does.
pub fn api_key_from_env() -> Option<String> {
    let typesafe = std::env::var(API_KEY_ENV_VARS[0]).ok();
    let jev = std::env::var(API_KEY_ENV_VARS[1]).ok();
    default_api_key(typesafe.as_deref(), jev.as_deref())
}

/// Extracts an HTTP error message the way `JevClient.errorMessage` does: `message`, then `error`
/// (possibly nested at `error.<field>`), then `detail`, else the raw body.
pub fn error_message(body: Option<&str>) -> Option<String> {
    let body = body?;
    if body.trim().is_empty() {
        return None;
    }
    let Ok(root) = serde_json::from_str::<serde_json::Value>(body) else {
        return Some(body.to_string());
    };
    for field in ["message", "error", "detail"] {
        if let Some(text) = root.get(field).and_then(serde_json::Value::as_str) {
            return Some(text.to_string());
        }
    }
    if let Some(nested) = root.get("error").and_then(|error| error.as_object()) {
        for field in ["message", "error", "detail"] {
            if let Some(text) = nested.get(field).and_then(serde_json::Value::as_str) {
                return Some(text.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_one_url_matches_java_normalization() {
        assert_eq!(
            system_one_url("https://api.typesafe.ai"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            system_one_url("https://api.typesafe.ai/"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            system_one_url("https://api.typesafe.ai/v1"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            system_one_url("https://api.typesafe.ai/v1/"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            system_one_url("https://api.typesafe.ai/v1/systemone"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(system_one_url("/"), "//v1/systemone");
    }

    #[test]
    fn retry_policy_defaults_and_backoff_cap() {
        let policy = RetryPolicy::defaults();
        assert_eq!(policy.max_retries, 2);
        assert_eq!(policy.initial_backoff_ms, 500);
        assert_eq!(policy.max_backoff_ms(), 4_000);
        assert_eq!(RetryPolicy::default(), policy);
        assert_eq!(
            RetryPolicy::new(1, 0).unwrap_err().message(),
            "initialBackoff must be positive"
        );
        assert!(RetryPolicy::new(0, 1).is_ok());
    }

    #[test]
    fn retryable_statuses_match_java() {
        for status in [429u16, 529, 500, 502, 599] {
            assert!(RetryPolicy::is_retryable_status(status), "{status}");
        }
        for status in [200u16, 400, 401, 404, 422, 428] {
            assert!(!RetryPolicy::is_retryable_status(status), "{status}");
        }
    }

    #[test]
    fn api_key_prefers_the_non_blank_primary() {
        assert_eq!(
            default_api_key(Some("typesafe"), Some("jev")),
            Some("typesafe".to_string())
        );
        assert_eq!(
            default_api_key(Some("  "), Some("jev")),
            Some("jev".to_string())
        );
        assert_eq!(default_api_key(None, Some("jev")), Some("jev".to_string()));
        assert_eq!(default_api_key(None, None), None);
        assert_eq!(
            default_api_key(Some(""), None),
            None,
            "a blank primary falls through to the (absent) secondary"
        );
        assert_eq!(API_KEY_ENV_VARS, ["TYPESAFE_API_KEY", "JEV_API_KEY"]);
    }

    #[test]
    fn error_message_probes_java_field_order() {
        assert_eq!(
            error_message(Some("{\"message\":\"invalid key\"}")).as_deref(),
            Some("invalid key")
        );
        assert_eq!(
            error_message(Some("{\"error\":\"boom\"}")).as_deref(),
            Some("boom")
        );
        assert_eq!(
            error_message(Some("{\"detail\":\"detailed\"}")).as_deref(),
            Some("detailed")
        );
        assert_eq!(
            error_message(Some("{\"error\":{\"message\":\"nested\"}}")).as_deref(),
            Some("nested")
        );
        assert_eq!(error_message(Some("not json")).as_deref(), Some("not json"));
        assert_eq!(error_message(Some("   ")), None);
        assert_eq!(error_message(None), None);
        assert_eq!(
            error_message(Some("{\"other\":1}")),
            None,
            "a JSON body without a known field yields no message"
        );
    }
}
