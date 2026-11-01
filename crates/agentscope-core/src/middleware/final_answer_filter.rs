//! Final-answer text filtering, mirroring
//! `io.agentscope.core.middleware.FinalAnswerFilterMiddleware`.

use futures::StreamExt;

use super::input::{EventStream, ReasoningInput};
use super::middleware::{ExtensionPoint, Middleware, Next};
use crate::agent::RuntimeContext;
use crate::event::{AgentEvent, AgentEventType};

/// Exposes only the text of the final reasoning round of a ReAct stream.
///
/// Text events are buffered until the current model call completes. If the model produces a
/// tool call during that round, the buffered and subsequent text events of the round are
/// suppressed; otherwise the buffered events are emitted right before the corresponding
/// `ModelCallEnd`.
///
/// This middleware is opt-in and does not change the default behaviour of `ReActAgent`.
/// Because a round can only be identified as intermediate after a tool call is observed, the
/// text of the current round is not emitted until the model call completes.
#[derive(Debug, Default, Clone, Copy)]
pub struct FinalAnswerFilterMiddleware;

impl FinalAnswerFilterMiddleware {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl Middleware for FinalAnswerFilterMiddleware {
    /// Narrow declaration: only the reasoning stream is wrapped.
    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnReasoning]
    }

    fn on_reasoning(
        &self,
        _ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        filter_final_answer(next(input))
    }
}

/// Wraps `inner` so only the final round's text survives.
fn filter_final_answer(inner: EventStream) -> EventStream {
    Box::pin(futures::stream::unfold(
        RoundFilter {
            inner,
            ready: Vec::new(),
            reply_id: None,
            tool_call_seen: false,
            buffered: Vec::new(),
        },
        |mut state| async move {
            loop {
                if !state.ready.is_empty() {
                    let event = state.ready.remove(0);
                    return Some((Ok(event), state));
                }
                match state.inner.next().await {
                    None => return None,
                    Some(Err(error)) => return Some((Err(error), state)),
                    Some(Ok(event)) => state.handle(event),
                }
            }
        },
    ))
}

/// Per-round buffer state, mirroring Java `FinalAnswerFilterMiddleware.RoundState`.
struct RoundFilter {
    inner: EventStream,
    /// Events released to the consumer on the next poll.
    ready: Vec<AgentEvent>,
    /// The reply id of the round currently being buffered.
    reply_id: Option<String>,
    /// Whether a tool call was observed in this round (the round is intermediate).
    tool_call_seen: bool,
    /// Text lifecycle events held back until the round is known to be final.
    buffered: Vec<AgentEvent>,
}

impl RoundFilter {
    fn handle(&mut self, event: AgentEvent) {
        match event.event_type() {
            AgentEventType::ModelCallStart => {
                self.reply_id = reply_id_of(&event);
                self.tool_call_seen = false;
                self.buffered.clear();
                self.ready.push(event);
            }
            AgentEventType::TextBlockStart
            | AgentEventType::TextBlockDelta
            | AgentEventType::TextBlockEnd => {
                if self.is_current_reply(&event) {
                    // Held back: either released with `ModelCallEnd` or dropped once the
                    // round turns out to carry a tool call.
                    if !self.tool_call_seen {
                        self.buffered.push(event);
                    }
                } else {
                    self.ready.push(event);
                }
            }
            AgentEventType::ToolCallStart => {
                if self.is_current_reply(&event) {
                    self.tool_call_seen = true;
                    self.buffered.clear();
                }
                self.ready.push(event);
            }
            AgentEventType::ModelCallEnd => {
                if self.is_current_reply(&event) {
                    if self.tool_call_seen {
                        self.buffered.clear();
                    } else {
                        self.ready.append(&mut self.buffered);
                    }
                    self.reply_id = None;
                    self.tool_call_seen = false;
                }
                self.ready.push(event);
            }
            _ => self.ready.push(event),
        }
    }

    fn is_current_reply(&self, event: &AgentEvent) -> bool {
        let Some(current) = &self.reply_id else {
            return false;
        };
        match reply_id_of(event) {
            Some(event_reply) => *current == event_reply,
            // Text deltas carry no reply id (`delta_event`), so while a round is open they
            // belong to it. Every other event this filter inspects carries one.
            None => true,
        }
    }
}

fn reply_id_of(event: &AgentEvent) -> Option<String> {
    event
        .data()
        .fields
        .get("reply_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::JsonMap;

    fn with_reply(event_type: AgentEventType, reply_id: &str) -> AgentEvent {
        let mut fields = JsonMap::new();
        if !reply_id.is_empty() {
            fields.insert("reply_id".into(), serde_json::json!(reply_id));
        }
        AgentEvent::with_fields(event_type, fields)
    }

    fn delta(text: &str) -> AgentEvent {
        let mut fields = JsonMap::new();
        fields.insert("delta".into(), serde_json::json!(text));
        AgentEvent::with_fields(AgentEventType::TextBlockDelta, fields)
    }

    /// Runs the filter over `events` exactly as `on_reasoning` would.
    fn filter(events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let inner: EventStream = Box::pin(futures::stream::iter(events.into_iter().map(Ok)));
        futures::executor::block_on(async {
            let mut out = Vec::new();
            let mut stream = filter_final_answer(inner);
            while let Some(item) = stream.next().await {
                out.push(item.unwrap());
            }
            out
        })
    }

    #[test]
    fn pure_text_round_keeps_every_text_event_in_order() {
        let events = filter(vec![
            with_reply(AgentEventType::ModelCallStart, "r1"),
            with_reply(AgentEventType::TextBlockStart, "r1"),
            delta("hello"),
            with_reply(AgentEventType::TextBlockEnd, "r1"),
            with_reply(AgentEventType::ModelCallEnd, "r1"),
        ]);
        let types: Vec<AgentEventType> = events.iter().map(AgentEvent::event_type).collect();
        assert_eq!(
            types,
            vec![
                AgentEventType::ModelCallStart,
                // Held back until `ModelCallEnd`, then released in order.
                AgentEventType::TextBlockStart,
                AgentEventType::TextBlockDelta,
                AgentEventType::TextBlockEnd,
                AgentEventType::ModelCallEnd,
            ]
        );
    }

    #[test]
    fn text_from_a_tool_calling_round_is_suppressed() {
        let events = filter(vec![
            with_reply(AgentEventType::ModelCallStart, "r1"),
            with_reply(AgentEventType::TextBlockStart, "r1"),
            delta("thinking out loud"),
            with_reply(AgentEventType::ToolCallStart, "r1"),
            delta("more text"),
            with_reply(AgentEventType::TextBlockEnd, "r1"),
            with_reply(AgentEventType::ModelCallEnd, "r1"),
        ]);
        let types: Vec<AgentEventType> = events.iter().map(AgentEvent::event_type).collect();
        assert_eq!(
            types,
            vec![
                AgentEventType::ModelCallStart,
                AgentEventType::ToolCallStart,
                AgentEventType::ModelCallEnd,
            ]
        );
    }

    #[test]
    fn only_the_last_round_survives_across_rounds() {
        let events = filter(vec![
            with_reply(AgentEventType::ModelCallStart, "r1"),
            delta("intermediate"),
            with_reply(AgentEventType::ToolCallStart, "r1"),
            with_reply(AgentEventType::ModelCallEnd, "r1"),
            with_reply(AgentEventType::ModelCallStart, "r2"),
            delta("final answer"),
            with_reply(AgentEventType::ModelCallEnd, "r2"),
        ]);
        let texts: Vec<String> = events
            .iter()
            .filter(|e| e.event_type() == AgentEventType::TextBlockDelta)
            .map(|e| {
                e.data().fields["delta"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(texts, vec!["final answer"]);
    }

    #[test]
    fn text_of_assistant_entries_is_forwarded_untouched() {
        // Events from a different reply (e.g. an assistant message event) are not the
        // model round's text and must pass through unchanged.
        let events = filter(vec![
            with_reply(AgentEventType::ModelCallStart, "r1"),
            delta("primary"),
            with_reply(AgentEventType::ModelCallEnd, "r1"),
            delta("assistant-entry"),
            AgentEvent::with_fields(AgentEventType::TextBlockStart, JsonMap::new()),
        ]);
        let texts: Vec<String> = events
            .iter()
            .filter(|e| e.event_type() == AgentEventType::TextBlockDelta)
            .map(|e| {
                e.data().fields["delta"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(texts, vec!["primary", "assistant-entry"]);
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type() == AgentEventType::TextBlockStart)
                .count(),
            1
        );
        // The sample above has no `reply_id`, so it cannot be mistaken for the round.
    }
}
