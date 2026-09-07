//! It never touches the persisted agent state, so suspended-tool recovery and
//! session history remain intact.

use std::collections::{HashMap, HashSet};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{Msg, MsgRole};
use agentscope_core::middleware::{EventStream, Middleware, Next, ReasoningInput};

/// Middleware that prunes dangling assistant `tool_calls` / orphan TOOL messages
/// from the outgoing model-request view.
#[derive(Debug, Default, Clone)]
pub struct ToolCallPairSanitizerMiddleware;

impl ToolCallPairSanitizerMiddleware {
    /// Creates the sanitizer middleware.
    pub fn new() -> Self {
        Self
    }

    /// Rewrites the message list so every assistant `tool_calls` has a matching
    /// TOOL response after it and every TOOL message answers a preceding call.
    /// Returns `None` when the list is already consistent.
    pub fn sanitize(messages: &[Msg]) -> Option<Vec<Msg>> {
        // Tool-call ids requested so far but not yet answered.
        let mut pending_call_ids: HashSet<String> = HashSet::new();
        // Tool-call id -> assistant message index requesting it.
        let mut owner: HashMap<String, usize> = HashMap::new();
        // Tool-call id -> indices of TOOL messages answering it.
        let mut answers: HashMap<String, Vec<usize>> = HashMap::new();
        // TOOL messages referencing unknown call ids (or duplicate answers).
        let mut dangling: HashSet<usize> = HashSet::new();

        for (i, msg) in messages.iter().enumerate() {
            match msg.role {
                MsgRole::Assistant => {
                    for tu in msg.tool_use_blocks() {
                        pending_call_ids.insert(tu.id.clone());
                        owner.insert(tu.id.clone(), i);
                    }
                }
                MsgRole::Tool => {
                    let mut answered = Vec::new();
                    let mut orphan = false;
                    for tr in msg.tool_result_blocks() {
                        match &tr.id {
                            Some(id) if pending_call_ids.contains(id) => {
                                pending_call_ids.remove(id);
                                answered.push(id.clone());
                            }
                            // A result for an unknown/duplicate id would violate
                            // the pairing rule, so the whole message must go.
                            _ => orphan = true,
                        }
                    }
                    if orphan {
                        dangling.insert(i);
                    } else {
                        for id in answered {
                            answers.entry(id).or_default().push(i);
                        }
                    }
                }
                _ => {}
            }
        }

        // Assistant messages owning unanswered ids are dangling.
        for id in &pending_call_ids {
            if let Some(&o) = owner.get(id) {
                dangling.insert(o);
            }
        }
        // Cascade: dropping an assistant forces dropping its TOOL answers, and
        // dropping a TOOL answer forces dropping the assistant that owns the id
        // (otherwise the remaining pair half would still violate the rule).
        let mut changed = true;
        while changed {
            changed = false;
            for (id, idxs) in &answers {
                let owner_dangling = owner.get(id).is_some_and(|o| dangling.contains(o));
                let answer_dangling = idxs.iter().any(|i| dangling.contains(i));
                if owner_dangling || answer_dangling {
                    if let Some(&o) = owner.get(id) {
                        changed |= dangling.insert(o);
                    }
                    for i in idxs {
                        changed |= dangling.insert(*i);
                    }
                }
            }
        }
        if dangling.is_empty() {
            return None;
        }
        let rebuilt: Vec<Msg> = messages
            .iter()
            .enumerate()
            .filter(|(i, _)| !dangling.contains(i))
            .map(|(_, m)| m.clone())
            .collect();
        Some(rebuilt)
    }
}

impl Middleware for ToolCallPairSanitizerMiddleware {
    fn on_reasoning(
        &self,
        _ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        match Self::sanitize(&input.messages) {
            None => next(input),
            Some(sanitized) => {
                tracing::debug!(
                    before = input.messages.len(),
                    after = sanitized.len(),
                    "sanitized dangling tool-call pairs before model call"
                );
                next(ReasoningInput::new(sanitized, input.tools, input.options))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{ContentBlock, TextBlock, ToolResultBlock, ToolUseBlock};

    fn assistant_with_call(id: &str, name: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                id,
                name,
                serde_json::Map::new(),
            ))])
            .build()
    }

    fn tool_result(id: &str, name: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(
                ToolResultBlock::builder()
                    .id(id)
                    .name(name)
                    .output(ContentBlock::Text(TextBlock::new("ok")))
                    .build(),
            )])
            .build()
    }

    #[test]
    fn keeps_paired_messages_untouched() {
        let msgs = vec![
            Msg::user("hi"),
            assistant_with_call("t1", "ls"),
            tool_result("t1", "ls"),
            Msg::builder().role(MsgRole::Assistant).text_content("done").build(),
        ];
        assert!(ToolCallPairSanitizerMiddleware::sanitize(&msgs).is_none());
    }

    #[test]
    fn strips_trailing_dangling_assistant_tool_calls() {
        let msgs = vec![
            Msg::user("hi"),
            assistant_with_call("t1", "ls"),
            tool_result("t1", "ls"),
            assistant_with_call("t2", "cat"),
        ];
        let out = ToolCallPairSanitizerMiddleware::sanitize(&msgs).expect("sanitized");
        assert_eq!(out.len(), 3);
        assert!(out.last().unwrap().tool_use_blocks().next().is_none());
    }

    #[test]
    fn drops_orphan_tool_results_and_pair_owner() {
        let msgs = vec![
            Msg::user("hi"),
            assistant_with_call("t1", "ls"),
            tool_result("t1", "ls"),
            tool_result("ghost", "cat"),
        ];
        let out = ToolCallPairSanitizerMiddleware::sanitize(&msgs).expect("sanitized");
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|m| m.tool_result_blocks().all(|tr| tr.id.as_deref() != Some("ghost"))));
    }

    #[test]
    fn drops_assistant_whose_partial_calls_were_answered_elsewhere() {
        // Assistant asks two calls; only one TOOL result exists -> the assistant
        // message must go too, otherwise the provider still sees unanswered ids.
        let msgs = vec![
            Msg::user("hi"),
            assistant_with_call("t1", "ls"),
            tool_result("t1", "ls"),
        ];
        let assistant_two = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::ToolUse(ToolUseBlock::new(
                    "a",
                    "x",
                    serde_json::Map::new(),
                )),
                ContentBlock::ToolUse(ToolUseBlock::new(
                    "b",
                    "y",
                    serde_json::Map::new(),
                )),
            ])
            .build();
        let mut msgs = msgs;
        msgs.push(assistant_two);
        msgs.push(tool_result("a", "x"));
        let out = ToolCallPairSanitizerMiddleware::sanitize(&msgs).expect("sanitized");
        assert_eq!(out.len(), 3);
    }
}