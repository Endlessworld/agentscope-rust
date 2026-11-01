//! Shared interpretation of the free-form finish reason carried by
//! [`ChatResponse`](super::ChatResponse), mirroring `io.agentscope.core.model.FinishReasons`.
//!
//! Providers report termination with raw strings (`"stop"`, `"length"`, `"MAX_TOKENS"`, …)
//! and some report none at all. This module names those spellings once, so provider adapters
//! and callers compare against constants instead of literals.

use serde_json::Value;

use crate::message::JsonMap;

/// OpenAI Chat Completions and DashScope: the model stopped normally.
pub const STOP: &str = "stop";
/// OpenAI Chat Completions and DashScope: the output hit the token limit.
pub const LENGTH: &str = "length";
/// OpenAI Chat Completions: the model asked for tool calls.
pub const TOOL_CALLS: &str = "tool_calls";
/// Anthropic: the model stopped normally.
pub const END_TURN: &str = "end_turn";
/// Anthropic: the model asked for a (client or server) tool.
pub const TOOL_USE: &str = "tool_use";
/// Anthropic and Gemini: the output hit the token limit.
pub const MAX_TOKENS: &str = "max_tokens";
/// OpenAI Responses: the response is incomplete; the cause is in metadata.
pub const INCOMPLETE: &str = "incomplete";
/// [`INCOMPLETE`]'s metadata cause for an output-token limit.
pub const MAX_OUTPUT_TOKENS: &str = "max_output_tokens";
/// Metadata key naming the cause of an [`INCOMPLETE`] response
/// (Java `"openai.response.incomplete_reason"`).
pub const METADATA_OPENAI_INCOMPLETE_REASON: &str = "openai.response.incomplete_reason";

/// Whether the reported termination means the output was cut at an output-token limit.
///
/// A missing or unrecognised reason never indicates truncation, so providers that report
/// none keep being accepted. Mirrors Java `FinishReasons.indicatesOutputLimit`.
pub fn indicates_output_limit(
    finish_reason: Option<&str>,
    response_metadata: Option<&JsonMap>,
) -> bool {
    let Some(reason) = finish_reason else {
        return false;
    };
    if reason.eq_ignore_ascii_case(LENGTH) || reason.eq_ignore_ascii_case(MAX_TOKENS) {
        return true;
    }
    // The Responses API reports `incomplete` and names the cause in metadata; only the
    // output-token cause means truncation (a content filter does not).
    if !reason.eq_ignore_ascii_case(INCOMPLETE) {
        return false;
    }
    response_metadata
        .and_then(|metadata| metadata.get(METADATA_OPENAI_INCOMPLETE_REASON))
        .and_then(Value::as_str)
        .is_some_and(|cause| cause.eq_ignore_ascii_case(MAX_OUTPUT_TOKENS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn constants_match_the_java_literals() {
        // Values are the wire strings Java compares against, verbatim.
        assert_eq!(STOP, "stop");
        assert_eq!(LENGTH, "length");
        assert_eq!(TOOL_CALLS, "tool_calls");
        assert_eq!(END_TURN, "end_turn");
        assert_eq!(TOOL_USE, "tool_use");
        assert_eq!(MAX_TOKENS, "max_tokens");
        assert_eq!(INCOMPLETE, "incomplete");
        assert_eq!(MAX_OUTPUT_TOKENS, "max_output_tokens");
        assert_eq!(
            METADATA_OPENAI_INCOMPLETE_REASON,
            "openai.response.incomplete_reason"
        );
    }

    #[test]
    fn only_output_limits_indicate_truncation() {
        assert!(indicates_output_limit(Some("length"), None));
        assert!(indicates_output_limit(Some("MAX_TOKENS"), None));
        assert!(!indicates_output_limit(Some("stop"), None));
        assert!(!indicates_output_limit(Some("tool_calls"), None));
        // A missing reason never means truncation.
        assert!(!indicates_output_limit(None, None));

        let mut metadata = JsonMap::new();
        metadata.insert(
            METADATA_OPENAI_INCOMPLETE_REASON.to_string(),
            json!(MAX_OUTPUT_TOKENS),
        );
        assert!(indicates_output_limit(Some("incomplete"), Some(&metadata)));

        metadata.insert(
            METADATA_OPENAI_INCOMPLETE_REASON.to_string(),
            json!("content_filter"),
        );
        assert!(!indicates_output_limit(Some("incomplete"), Some(&metadata)));
        assert!(!indicates_output_limit(Some("incomplete"), None));
    }
}
