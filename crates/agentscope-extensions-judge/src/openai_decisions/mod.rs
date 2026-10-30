//! OpenAI *Decisions* port (Java `agentscope-extensions-openai-decisions`, 8 classes / 795
//! lines).
//!
//! Decisions is the natural sibling of System One: the caller declares a closed set of named
//! questions about one shared input (text and/or images) and the provider answers each of them,
//! either with a calibrated value or with an explicit refusal.
//!
//! * [`dto`] — [`DecisionInput`], [`DecisionQuestion`], [`DecisionRequest`], [`DecisionAnswer`],
//!   [`DecisionResult`], [`DecisionUsage`], all validated like Java's record constructors.
//! * [`protocol`] — request-body encoding plus response parsing/validation
//!   (`OpenAIDecisionsClient.execute` / `parseAnswer` / `parseChoice` / `parseScore`).
//! * [`client`] — [`OpenAIDecisionsClient`] over the injectable
//!   [`JudgeHttpTransport`](crate::http::JudgeHttpTransport), with the wrapper retry policy
//!   disabled by default.
//! * [`error`] — [`DecisionsError`], the sanitized `OpenAIDecisionsException` analogue.
//! * [`support`] — the shared evidence projection (latest user turn, bounded role/text history,
//!   deterministic JSON, probability checks) Java keeps in `DecisionSupport`.
//! * [`middleware`] — the three opt-in middlewares over core's agent loop
//!   ([`ModelRouterMiddleware`], [`ToolSelectionMiddleware`], [`AutoModeMiddleware`]).
//!
//! ## Host bindings
//!
//! The middlewares live in [`middleware`] because they implement
//! [`agentscope_core::middleware::Middleware`]; nothing else in this module depends on a host. A
//! Decisions call reaches them through the injected [`DecisionCaller`] seam, so every test is
//! offline: production wires [`caller_from_client`], tests answer with a fixed
//! [`DecisionResult`].

use std::ops::RangeInclusive;

pub mod client;
pub mod dto;
pub mod error;
pub mod middleware;
pub mod protocol;
pub mod support;

pub use client::{
    is_retryable_status, normalize_decision_base_url, DecisionsRetryPolicy, OpenAIDecisionsClient,
    OpenAIDecisionsClientBuilder,
};
pub use dto::{
    ChoiceProbability, DecisionAnswer, DecisionInput, DecisionLevel, DecisionOption,
    DecisionQuestion, DecisionRequest, DecisionRequestBuilder, DecisionResult, DecisionUsage,
    DecisionValue, ScoreProbability, IMAGE_URL_MESSAGE,
};
pub use error::DecisionsError;
pub use middleware::{
    caller_from_client, AutoModeMiddleware, AutoModeMiddlewareBuilder, Compatibility,
    DecisionCaller, DecisionFuture, ModelChoice, ModelRouterMiddleware,
    ModelRouterMiddlewareBuilder, RoutingDecision, ToolSelectionMiddleware,
    ToolSelectionMiddlewareBuilder, DEFAULT_DENY_MESSAGE, DEFAULT_MAX_CONTEXT_CHARS,
    DEFAULT_MAX_CONTEXT_MESSAGES, DEFAULT_MAX_TOOLS, DEFAULT_REJECTION_THRESHOLD,
    DEFAULT_ROUTING_CONFIDENCE, DEFAULT_ROUTING_INSTRUCTIONS, DEFAULT_SAFETY_THRESHOLD,
    DEFAULT_SELECTION_CONFIDENCE, GUARD_CONTEXT_CHARS, GUARD_CONTEXT_MESSAGES, GUARD_REPLY_ID,
    ROUTING_QUESTION,
};
pub use protocol::{decision_body, parse_decision_response};
pub use support::{json as support_json, latest_user_text, recent_messages, RecentMessage};

/// Default OpenAI API base URL (`OpenAIDecisionsClient.DEFAULT_BASE_URL`).
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Default model (`OpenAIDecisionsClient.DEFAULT_MODEL`).
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

/// Default request timeout in milliseconds (`DEFAULT_TIMEOUT` = 30s).
pub const DEFAULT_TIMEOUT_MILLIS: u64 = 30_000;

/// Endpoint appended to the base URL.
pub const DECISIONS_ENDPOINT: &str = "/decisions";

/// Per-value tolerance for independently serialized probabilities
/// (`OpenAIDecisionsClient.TOLERANCE`).
pub const PROBABILITY_TOLERANCE: f64 = 0.000_001;

/// Maximum named questions per request (`DecisionRequest`: 1–400 entries).
pub const MAX_QUESTIONS: usize = 400;

/// Maximum images per input (`DecisionInput`: at most 128).
pub const MAX_IMAGES: usize = 128;

/// Legal number of choice options (`DecisionQuestion.Choice`: 2–255).
pub const CHOICE_OPTION_RANGE: RangeInclusive<usize> = 2..=255;

/// Environment variable the API key falls back to (`Builder.apiKey`).
pub const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";

/// Default retries for [`DecisionsRetryPolicy::defaults`]
/// (`OpenAIDecisionsRetryPolicy.defaults()`).
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Default initial backoff for [`DecisionsRetryPolicy`] constructors.
pub const DEFAULT_INITIAL_BACKOFF_MILLIS: u64 = 500;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_java() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.openai.com/v1");
        assert_eq!(DEFAULT_MODEL, "gpt-6-luna");
        assert_eq!(DEFAULT_TIMEOUT_MILLIS, 30_000);
        assert_eq!(DECISIONS_ENDPOINT, "/decisions");
        assert_eq!(PROBABILITY_TOLERANCE, 1e-6);
        assert_eq!(MAX_QUESTIONS, 400);
        assert_eq!(MAX_IMAGES, 128);
        assert_eq!(CHOICE_OPTION_RANGE, 2..=255);
        assert_eq!(OPENAI_API_KEY_ENV_VAR, "OPENAI_API_KEY");
        assert_eq!(DEFAULT_MAX_RETRIES, 2);
        assert_eq!(DEFAULT_INITIAL_BACKOFF_MILLIS, 500);
    }
}
