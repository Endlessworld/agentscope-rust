//! Middleware that logs a per-call agent execution trace, mirroring
//! `io.agentscope.harness.agent.middleware.AgentTraceMiddleware`.
//!
//! INFO logs a coarse summary at each lifecycle point (pre/post call,
//! reasoning, acting) by observing the event stream — it persists nothing.
//! DEBUG additionally includes tool-call arguments, tool-result bodies and
//! input messages. The middleware does not alter the event stream; it wraps it
//! with a completion callback (`WithDone`) so the post-phase summary fires
//! exactly when the phase stream ends.
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::{AgentEvent, EventData};
use agentscope_core::formatter::render_message_text;
use agentscope_core::middleware::{
    ActingInput, AgentInput, EventStream, Middleware, Next, ReasoningInput,
};
use futures::{Stream, StreamExt};

/// Stream wrapper that runs a completion callback once the inner stream ends.
struct WithDone<S> {
    inner: S,
    done: Option<Box<dyn FnOnce() + Send>>,
}

impl<S: Stream + Unpin> Stream for WithDone<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(None) => {
                if let Some(f) = self.done.take() {
                    f();
                }
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

/// Truncates `s` to at most `max` characters, appending `...` when shortened.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("...");
        out
    }
}

/// Reads a string field from an event payload.
fn field_str(data: &EventData, key: &str) -> Option<String> {
    data.fields
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Accumulates text deltas and tool-call starts observed during reasoning.
#[derive(Default)]
struct ReasoningTrace {
    text: String,
    tool_calls: Vec<(String, String)>,
}

impl ReasoningTrace {
    fn on_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::TextBlockDelta(d) => {
                if let Some(delta) = field_str(d, "delta") {
                    self.text.push_str(&delta);
                }
            }
            AgentEvent::ToolCallStart(d) => {
                self.tool_calls.push((
                    field_str(d, "toolCallId").unwrap_or_default(),
                    field_str(d, "toolCallName").unwrap_or_default(),
                ));
            }
            _ => {}
        }
    }

    fn format(&self) -> String {
        if self.text.trim().is_empty() && self.tool_calls.is_empty() {
            return "POST_REASONING | empty completion — ReAct loop will terminate".to_string();
        }
        let mut out = format!("POST_REASONING | text: {}", truncate(&self.text, 120));
        for (id, name) in &self.tool_calls {
            out.push_str(&format!("\n  tool_call: id={id}, name={name}"));
        }
        out
    }
}

/// Accumulates per-tool results observed during acting.
#[derive(Default)]
struct ActingTrace {
    // toolCallId -> (name, accumulated text)
    results: BTreeMap<String, (String, String)>,
}

impl ActingTrace {
    fn on_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::ToolResultStart(d) => {
                let id = field_str(d, "toolCallId").unwrap_or_default();
                let name = field_str(d, "toolCallName").unwrap_or_default();
                self.results.entry(id).or_insert((name, String::new()));
            }
            AgentEvent::ToolResultTextDelta(d) => {
                let id = field_str(d, "toolCallId").unwrap_or_default();
                if let Some(delta) = field_str(d, "delta") {
                    if let Some(entry) = self.results.get_mut(&id) {
                        entry.1.push_str(&delta);
                    }
                }
            }
            _ => {}
        }
    }

    /// Finalizes one completed tool result, returning its trace line.
    fn finish(&mut self, ev: &AgentEvent) -> Option<String> {
        let AgentEvent::ToolResultEnd(d) = ev else {
            return None;
        };
        let id = field_str(d, "toolCallId").unwrap_or_default();
        let state = field_str(d, "state").unwrap_or_default();
        let (name, text) = self.results.remove(&id).unwrap_or_default();
        Some(format!(
            "POST_ACTING | id={id}, name={name}, result_len={}, state={state}",
            text.len()
        ))
    }

    /// Formats lines for any tool whose end event never arrived.
    fn pending(&self) -> Vec<String> {
        self.results
            .iter()
            .map(|(id, (name, text))| {
                format!(
                    "POST_ACTING | id={id}, name={name}, result_len={}, state=(no end event)",
                    text.len()
                )
            })
            .collect()
    }
}

/// Middleware that logs a per-call execution trace at INFO/DEBUG level.
pub struct AgentTraceMiddleware {
    /// When `true`, DEBUG-level details (tool args, results, messages) are logged.
    debug: bool,
}

impl AgentTraceMiddleware {
    /// Creates a trace middleware with INFO summaries only.
    pub fn new() -> Self {
        Self { debug: false }
    }

    /// Creates a trace middleware with DEBUG-level details enabled.
    pub fn with_debug() -> Self {
        Self { debug: true }
    }

    fn agent_name(ctx: &RuntimeContext) -> String {
        ctx.agent_name
            .clone()
            .unwrap_or_else(|| "unknown".to_string())
    }
}

impl Default for AgentTraceMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for AgentTraceMiddleware {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        tracing::info!(
            "PRE_CALL | agent={}, {} input message(s)",
            Self::agent_name(ctx),
            input.messages.len()
        );
        if self.debug {
            for m in &input.messages {
                tracing::debug!(
                    "  msg: role={:?}, text={}",
                    m.role(),
                    // Documents are invisible to `text_content()`; the reference keeps the
                    // trace showing what the model actually received.
                    truncate(&render_message_text(m, true), 200)
                );
            }
        }

        let text: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let t = Arc::clone(&text);
        let inner = next(input).inspect(move |res| {
            if let Ok(AgentEvent::TextBlockDelta(d)) = res {
                if let Some(delta) = field_str(d, "delta") {
                    t.lock().unwrap().push_str(&delta);
                }
            }
        });
        let text2 = Arc::clone(&text);
        let done = Box::new(move || {
            let text = text2.lock().unwrap().clone();
            tracing::info!("POST_CALL | response: {}", truncate(&text, 120));
        });
        Box::pin(WithDone {
            inner,
            done: Some(done),
        })
    }

    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        tracing::info!(
            "PRE_REASONING | agent={}, messages={}",
            Self::agent_name(ctx),
            input.messages.len()
        );

        let trace = Arc::new(Mutex::new(ReasoningTrace::default()));
        let t = Arc::clone(&trace);
        let inner = next(input).inspect(move |res| {
            if let Ok(ev) = res {
                t.lock().unwrap().on_event(ev);
            }
        });
        let t2 = Arc::clone(&trace);
        let done = Box::new(move || {
            let summary = t2.lock().unwrap().format();
            tracing::info!("{summary}");
        });
        Box::pin(WithDone {
            inner,
            done: Some(done),
        })
    }

    fn on_acting(
        &self,
        ctx: &RuntimeContext,
        input: ActingInput,
        next: Next<ActingInput>,
    ) -> EventStream {
        let _ = ctx;
        for call in &input.tool_calls {
            tracing::info!("PRE_ACTING | id={}, name={}", call.id, call.name);
            if self.debug && !call.input.is_empty() {
                tracing::debug!(
                    "  args: {}",
                    truncate(&serde_json::to_string(&call.input).unwrap_or_default(), 500)
                );
            }
        }

        let trace = Arc::new(Mutex::new(ActingTrace::default()));
        let t = Arc::clone(&trace);
        let inner = next(input).inspect(move |res| {
            let Ok(ev) = res else { return };
            if let Some(line) = t.lock().unwrap().finish(ev) {
                tracing::info!("{line}");
            }
            t.lock().unwrap().on_event(ev);
        });
        let t2 = Arc::clone(&trace);
        let done = Box::new(move || {
            for line in t2.lock().unwrap().pending() {
                tracing::info!("{line}");
            }
        });
        Box::pin(WithDone {
            inner,
            done: Some(done),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::error::Result;
    use agentscope_core::event::AgentEventType;
    use serde_json::{Map, Value};

    fn ev(ty: AgentEventType, fields: &[(&str, &str)]) -> Result<AgentEvent> {
        let mut map = Map::new();
        for (k, v) in fields {
            map.insert(k.to_string(), Value::String(v.to_string()));
        }
        Ok(AgentEvent::with_fields(ty, map))
    }

    fn text_delta(delta: &str) -> Result<AgentEvent> {
        ev(AgentEventType::TextBlockDelta, &[("delta", delta)])
    }

    #[test]
    fn truncate_preserves_short_and_marks_long() {
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn with_done_runs_callback_on_completion() {
        let stream = futures::stream::iter(vec![1, 2, 3]);
        let ran = Arc::new(Mutex::new(false));
        let ran2 = Arc::clone(&ran);
        let mut wd = WithDone {
            inner: stream,
            done: Some(Box::new(move || {
                *ran2.lock().unwrap() = true;
            })),
        };
        futures::executor::block_on(async { while wd.next().await.is_some() {} });
        assert!(*ran.lock().unwrap(), "completion callback should fire");
    }

    #[test]
    fn reasoning_trace_accumulates_text_and_tool_calls() {
        let mut t = ReasoningTrace::default();
        t.on_event(&text_delta("hel").unwrap());
        t.on_event(&text_delta("lo").unwrap());
        t.on_event(
            &ev(
                AgentEventType::ToolCallStart,
                &[("toolCallId", "c1"), ("toolCallName", "add")],
            )
            .unwrap(),
        );
        let out = t.format();
        assert!(out.contains("text: hello"), "got: {out}");
        assert!(out.contains("id=c1"), "got: {out}");
        assert!(out.contains("name=add"), "got: {out}");
    }

    #[test]
    fn reasoning_trace_empty_completion() {
        let t = ReasoningTrace::default();
        assert!(t.format().contains("empty completion"));
    }

    #[test]
    fn acting_trace_finishes_tool_result() {
        let mut t = ActingTrace::default();
        t.on_event(
            &ev(
                AgentEventType::ToolResultStart,
                &[("toolCallId", "c1"), ("toolCallName", "add")],
            )
            .unwrap(),
        );
        t.on_event(
            &ev(
                AgentEventType::ToolResultTextDelta,
                &[("toolCallId", "c1"), ("delta", "42")],
            )
            .unwrap(),
        );
        let line = t
            .finish(
                &ev(
                    AgentEventType::ToolResultEnd,
                    &[("toolCallId", "c1"), ("state", "SUCCESS")],
                )
                .unwrap(),
            )
            .expect("finish returns a line");
        assert!(line.contains("id=c1"), "got: {line}");
        assert!(line.contains("name=add"), "got: {line}");
        assert!(line.contains("result_len=2"), "got: {line}");
        assert!(line.contains("state=SUCCESS"), "got: {line}");
        assert!(t.pending().is_empty());
    }

    #[test]
    fn acting_trace_pending_when_no_end_event() {
        let mut t = ActingTrace::default();
        t.on_event(
            &ev(
                AgentEventType::ToolResultStart,
                &[("toolCallId", "c1"), ("toolCallName", "grep")],
            )
            .unwrap(),
        );
        let pending = t.pending();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].contains("no end event"));
    }
}
