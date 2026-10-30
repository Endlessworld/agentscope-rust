//! The shared evidence projection the three Decisions middlewares use
//! (`openaidecisions/example/DecisionSupport`).
//!
//! Java keeps these helpers package-private next to the middlewares because all three need the
//! same things: the latest user turn (the routing input), a bounded role/text projection of the
//! recent conversation (the evidence payload), deterministic JSON, and the `[0, 1]` probability
//! check `DecisionSupport.requireProbability` performs.
//!
//! Projection notes (tracker §12.3):
//!
//! * Java measures the budget in `String.length()` (UTF-16 code units) and steps back one unit to
//!   avoid splitting a surrogate pair. Rust counts `char`s, which never splits a scalar value, so
//!   the same call cannot produce broken UTF-8 either.
//! * A `TOOL` message contributes the text blocks of its `ToolResultBlock`s, joined with `\n`, so
//!   the Decision sees what a tool returned rather than a serialized block.

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use serde::Serialize;

use crate::openai_decisions::error::DecisionsError;

/// The maximum number of named questions one Decisions request may carry
/// (`DecisionSupport.MAX_QUESTIONS`).
pub const MAX_QUESTIONS: usize = crate::openai_decisions::MAX_QUESTIONS;

/// One projected conversation entry (`Map.of("role", ..., "text", ...)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecentMessage {
    /// The role name, lowercased (`DecisionSupport.recentMessages`).
    pub role: String,
    /// The message's text, already clipped to the remaining budget.
    pub text: String,
}

/// The text of the last USER turn, trimmed; empty when there is none
/// (`DecisionSupport.latestUserText`).
pub fn latest_user_text(messages: &[Msg]) -> String {
    for message in messages.iter().rev() {
        if message.role() == MsgRole::User {
            return message.text_content().trim().to_string();
        }
    }
    String::new()
}

/// The recent conversation as role/text entries, newest last
/// (`DecisionSupport.recentMessages`).
///
/// Walks backwards from the newest message, skipping blank ones and stopping once
/// `max_messages` entries or `max_chars` characters are spent. A message longer than the remaining
/// budget is clipped and kept when the clipped text is non-blank.
pub fn recent_messages(
    messages: &[Msg],
    max_messages: usize,
    max_chars: usize,
) -> Vec<RecentMessage> {
    let mut entries: Vec<RecentMessage> = Vec::new();
    let mut remaining = max_chars;
    if max_messages == 0 {
        return entries;
    }
    for message in messages.iter().rev() {
        if remaining == 0 || entries.len() >= max_messages {
            break;
        }
        let text = message_text(message);
        if text.trim().is_empty() {
            continue;
        }
        let text = if text.chars().count() > remaining {
            let clipped: String = text.chars().take(remaining).collect();
            if clipped.trim().is_empty() {
                continue;
            }
            clipped
        } else {
            text
        };
        remaining -= text.chars().count();
        entries.insert(
            0,
            RecentMessage {
                role: role_name(message.role()),
                text,
            },
        );
    }
    entries
}

/// Serializes a projection the way `JsonUtils.getJsonCodec().toJson(evidence)` does.
pub fn json<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Rejects a probability that is not finite or outside `[0, 1]`
/// (`DecisionSupport.requireProbability`, whose message is reused verbatim).
pub fn require_probability(value: f64, name: &str) -> Result<f64, DecisionsError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(DecisionsError::invalid(format!(
            "{name} must be finite and between 0 and 1"
        )));
    }
    Ok(value)
}

/// The per-invocation key the middlewares scope their decisions under.
///
/// Java stores the routing decision in Reactor's `Context` (`ctx.put(RoutingDecision.class, ...)`),
/// which `&RuntimeContext` cannot offer here. The middlewares keep their own map keyed by `run_id`,
/// falling back to `session_id`; hosts that reuse a context without either id should set one, or
/// the default key deliberately keeps the last decision for every such call.
pub fn run_key(ctx: &RuntimeContext) -> String {
    ctx.run_id()
        .or_else(|| ctx.session_id())
        .map(str::to_string)
        .unwrap_or_else(|| "openai-decisions-default-run".to_string())
}

/// The flattened text of one message: text blocks plus the text blocks of any tool result.
fn message_text(message: &Msg) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.as_str()),
            ContentBlock::ToolResult(result) => {
                for inner in &result.output {
                    if let ContentBlock::Text(text) = inner {
                        parts.push(text.text.as_str());
                    }
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// The lowercase role name `DecisionSupport.recentMessages` writes.
fn role_name(role: MsgRole) -> String {
    match role {
        MsgRole::User => "user",
        MsgRole::Assistant => "assistant",
        MsgRole::System => "system",
        MsgRole::Tool => "tool",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{TextBlock, ToolResultBlock, ToolResultState};

    fn tool_message(text: &str) -> Msg {
        Msg::tool_blocks(vec![ToolResultBlock {
            id: Some("call-1".into()),
            name: Some("lookup".into()),
            output: vec![ContentBlock::Text(TextBlock::new(text))],
            metadata: Default::default(),
            state: ToolResultState::Success,
        }])
    }

    #[test]
    fn the_latest_user_turn_is_trimmed_and_skips_other_roles() {
        let messages = vec![
            Msg::user("first"),
            Msg::assistant("reply"),
            Msg::user("  second  "),
            Msg::tool("call-1", "lookup", "tool output"),
        ];
        assert_eq!(latest_user_text(&messages), "second");
        assert_eq!(latest_user_text(&[]), "");
        assert_eq!(latest_user_text(&[Msg::assistant("only")]), "");
    }

    #[test]
    fn recent_messages_flatten_tool_results_and_keep_newest_last() {
        let messages = vec![Msg::user("hello"), tool_message("fetched page")];
        let entries = recent_messages(&messages, 8, 8000);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].text, "hello");
        assert_eq!(entries[1].role, "tool");
        assert_eq!(entries[1].text, "fetched page");
    }

    #[test]
    fn the_projection_respects_both_budgets_without_splitting_characters() {
        let messages = vec![
            Msg::user("older message that should be dropped"),
            Msg::assistant("0123456789"),
            Msg::user("abcdef"),
        ];
        let entries = recent_messages(&messages, 2, 10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].text, "abcdef");
        assert_eq!(entries[0].text, "0123");

        // Multi-byte characters survive the clip as whole characters.
        let entries = recent_messages(&[Msg::user("\u{4f60}\u{597d}\u{4e16}\u{754c}")], 8, 2);
        assert_eq!(entries[0].text, "\u{4f60}\u{597d}");
    }

    #[test]
    fn blank_messages_are_skipped() {
        let messages = vec![Msg::user("   "), Msg::assistant("kept")];
        let entries = recent_messages(&messages, 8, 8000);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "kept");
    }

    #[test]
    fn probabilities_must_be_finite_and_bounded() {
        assert_eq!(require_probability(0.0, "confidence").unwrap(), 0.0);
        assert_eq!(require_probability(1.0, "confidence").unwrap(), 1.0);
        for value in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            assert_eq!(
                require_probability(value, "confidence")
                    .unwrap_err()
                    .message(),
                "confidence must be finite and between 0 and 1"
            );
        }
    }

    #[test]
    fn the_run_key_prefers_the_run_then_the_session() {
        let mut ctx = RuntimeContext::new();
        assert_eq!(run_key(&ctx), "openai-decisions-default-run");
        ctx.session_id = Some("s-1".into());
        assert_eq!(run_key(&ctx), "s-1");
        ctx.run_id = Some("r-1".into());
        assert_eq!(run_key(&ctx), "r-1");
    }

    #[test]
    fn json_serializes_the_projection_like_the_java_codec() {
        let entries = vec![RecentMessage {
            role: "user".into(),
            text: "hi".into(),
        }];
        assert_eq!(json(&entries), r#"[{"role":"user","text":"hi"}]"#);
    }
}
