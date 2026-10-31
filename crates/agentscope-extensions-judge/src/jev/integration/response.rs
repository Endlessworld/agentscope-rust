//! `JevResponseMiddleware` shapes: the quality state, the conversation text and the limits.
//!
//! Java screens model turns before their events are released, refines tool-free final drafts and
//! can replace a text delta. That reactive part lives in
//! [`ResponseMiddleware`](crate::jev::runtime::middleware::ResponseMiddleware); the quality state
//! builder — the evidence the judge sees — is pure and lives here.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::evaluation::{JevTrace, TraceMessage};

/// Why the fail-closed placeholder cannot drive the middleware itself.
pub const RESPONSE_MIDDLEWARE_UNAVAILABLE: &str = "this placeholder is fail-closed; the reactive middleware is jev::runtime::middleware::ResponseMiddleware";

/// The middleware order Java declares (`order()`).
pub const MIDDLEWARE_ORDER: i32 = 200;

/// Java's default limits (`JevResponseMiddleware.Builder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseLimits {
    /// Maximum characters of the draft / judge state.
    pub max_chars: usize,
    /// Maximum captured events.
    pub max_events: usize,
    /// Maximum revisions (0..=10).
    pub max_revisions: usize,
    /// The per-round budget in milliseconds.
    pub round_budget_millis: u64,
}

impl ResponseLimits {
    /// Validates the limits (`invalid response limits`).
    pub fn new(
        max_chars: usize,
        max_events: usize,
        max_revisions: usize,
        round_budget_millis: u64,
    ) -> Result<Self, JevError> {
        if max_chars < 1 || max_events < 1 || max_revisions > 10 || round_budget_millis == 0 {
            return Err(JevError::invalid("invalid response limits"));
        }
        Ok(Self {
            max_chars,
            max_events,
            max_revisions,
            round_budget_millis,
        })
    }

    /// The Java defaults: 64000 chars, 8192 events, 1 revision, 120s.
    pub fn defaults() -> Self {
        Self {
            max_chars: 64_000,
            max_events: 8_192,
            max_revisions: 1,
            round_budget_millis: 120_000,
        }
    }

    /// The round budget as a [`Duration`].
    pub fn round_budget(&self) -> Duration {
        Duration::from_millis(self.round_budget_millis)
    }
}

/// Renders the conversation the way Java's `messages(...)` helper does.
///
/// Every message contributes `ROLE: text`; every text part of a tool result contributes
/// `tool evidence: ...`.
pub fn conversation_text(messages: &[TraceMessage]) -> String {
    use crate::jev::evaluation::TraceBlock;
    let mut out = String::new();
    for message in messages {
        out.push_str(&format!("{}: {}\n", message.role, message.text));
        for block in &message.blocks {
            if let TraceBlock::ToolResult(result) = block {
                for part in &result.parts {
                    if let crate::jev::evaluation::TraceResultPart::Text(text) = part {
                        out.push_str(&format!("tool evidence: {text}\n"));
                    }
                }
            }
        }
    }
    out
}

/// Builds the judge state for a draft (`JevResponseMiddleware.qualityState`).
///
/// Named question, answer and paired tool evidence are provided, and the legacy `prompt`/`answer`
/// fields stay compatible with older definitions.
pub fn quality_state(messages: &[TraceMessage], draft: &str) -> Result<JevTrace, JevError> {
    let trace = JevTrace::from_messages("answer-review", messages, None)?;
    let user_question = messages
        .iter()
        .filter(|message| !message.text.trim().is_empty())
        .map(|message| format!("{}: {}", message.role, message.text))
        .collect::<Vec<_>>()
        .join("\n");
    let supporting_context: Vec<Value> = trace
        .list("tool_results")
        .iter()
        .filter(|value| value.get("result_state").and_then(Value::as_str) == Some("SUCCESS"))
        .filter_map(|value| value.get("result").cloned())
        .collect();
    let mut state: BTreeMap<String, Value> = BTreeMap::new();
    state.insert("prompt".to_string(), json!(conversation_text(messages)));
    state.insert("answer".to_string(), json!(draft));
    state.insert("user_question".to_string(), json!(user_question));
    state.insert("assistant_answer".to_string(), json!(draft));
    state.insert("tool_calls".to_string(), Value::Array(trace.list("tool_calls").to_vec()));
    state.insert(
        "supporting_context".to_string(),
        Value::Array(supporting_context),
    );
    state.insert("trace_issues".to_string(), json!(trace.issues));
    JevTrace::without_issues("answer-review", state)
}

/// The fail-closed placeholder kept for hosts that expect Java's type names.
///
/// The reactive middleware is
/// [`ResponseMiddleware`](crate::jev::runtime::middleware::ResponseMiddleware); using this type
/// keeps `ENFORCE` fail-closed instead of silently releasing an unscreened draft.
#[derive(Debug, Clone)]
pub struct ResponseMiddlewareUnavailable {
    reason: String,
}

impl ResponseMiddlewareUnavailable {
    /// Creates the placeholder with an explicit reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The default placeholder.
    pub fn model_call_hook_missing() -> Self {
        Self::new(RESPONSE_MIDDLEWARE_UNAVAILABLE)
    }

    /// The Java middleware order.
    pub fn order(&self) -> i32 {
        MIDDLEWARE_ORDER
    }

    /// Fails closed with the documented reason.
    pub fn screen(&self, _draft: &str) -> Result<(), JevError> {
        Err(JevError::protocol(self.reason.clone()))
    }

    /// The rejection message Java prepends (`Rejected`).
    pub fn rejection(reason: &str) -> String {
        format!("JEV response withheld: {reason}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::evaluation::{TraceBlock, TraceResultPart, TraceToolResult, TraceToolUse};

    fn messages() -> Vec<TraceMessage> {
        let mut assistant = TraceMessage::new("ASSISTANT", "");
        assistant.blocks = vec![
            TraceBlock::ToolUse(TraceToolUse {
                id: Some("c1".to_string()),
                name: "read".to_string(),
                arguments: json!({}),
            }),
            TraceBlock::ToolResult(TraceToolResult {
                id: Some("c1".to_string()),
                name: "read".to_string(),
                parts: vec![TraceResultPart::Text("body".to_string())],
                state: Some("SUCCESS".to_string()),
                metadata: BTreeMap::new(),
            }),
        ];
        vec![TraceMessage::user("where?"), assistant]
    }

    #[test]
    fn the_quality_state_carries_named_and_legacy_evidence() {
        let state = quality_state(&messages(), "here").unwrap();
        assert_eq!(state.id, "answer-review");
        assert_eq!(state.text("answer"), "here");
        assert_eq!(state.text("assistant_answer"), "here");
        assert!(state.text("user_question").contains("USER: where?"));
        assert!(state.text("prompt").contains("tool evidence: body"));
        assert_eq!(state.list("tool_calls").len(), 1);
        assert_eq!(state.list("supporting_context"), &[json!("body")]);
        assert_eq!(state.list("trace_issues").len(), 0);
    }

    #[test]
    fn failed_tool_results_are_not_supporting_context() {
        let mut messages = messages();
        messages[1].blocks[1] = TraceBlock::ToolResult(TraceToolResult {
            id: Some("c1".to_string()),
            name: "read".to_string(),
            parts: vec![TraceResultPart::Text("boom".to_string())],
            state: Some("ERROR".to_string()),
            metadata: BTreeMap::new(),
        });
        let state = quality_state(&messages, "draft").unwrap();
        assert!(state.list("supporting_context").is_empty());
    }

    #[test]
    fn limits_and_the_placeholder_match_java() {
        assert_eq!(
            ResponseLimits::new(0, 1, 0, 1).unwrap_err().message(),
            "invalid response limits"
        );
        assert_eq!(
            ResponseLimits::new(1, 0, 0, 1).unwrap_err().message(),
            "invalid response limits"
        );
        assert_eq!(
            ResponseLimits::new(1, 1, 11, 1).unwrap_err().message(),
            "invalid response limits"
        );
        assert_eq!(
            ResponseLimits::new(1, 1, 0, 0).unwrap_err().message(),
            "invalid response limits"
        );
        let limits = ResponseLimits::defaults();
        assert_eq!(limits.max_chars, 64_000);
        assert_eq!(limits.max_events, 8_192);
        assert_eq!(limits.max_revisions, 1);
        assert_eq!(limits.round_budget(), Duration::from_secs(120));

        let placeholder = ResponseMiddlewareUnavailable::model_call_hook_missing();
        assert_eq!(placeholder.order(), 200);
        assert_eq!(
            placeholder.screen("draft").unwrap_err().message(),
            RESPONSE_MIDDLEWARE_UNAVAILABLE
        );
        assert_eq!(
            ResponseMiddlewareUnavailable::rejection("OUTPUT_BLOCK"),
            "JEV response withheld: OUTPUT_BLOCK"
        );
        assert!(conversation_text(&messages()).starts_with("USER: where?\n"));
    }
}
