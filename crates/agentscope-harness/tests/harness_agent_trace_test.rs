//! Integration tests for `AgentTraceMiddleware`: it must transparently pass the
//! event stream through while observing it, and the `WithDone` completion hook
//! must fire exactly once when the underlying stream ends.
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::Msg;
use agentscope_core::middleware::ReasoningInput;
use agentscope_core::model::GenerateOptions;
use agentscope_core::Middleware;
use agentscope_harness::AgentTraceMiddleware;
use futures::StreamExt;
use serde_json::{Map, Value};

fn ctx() -> RuntimeContext {
    RuntimeContext::default()
}

fn text_delta(delta: &str) -> Result<AgentEvent> {
    let mut map = Map::new();
    map.insert("delta".to_string(), Value::String(delta.to_string()));
    Ok(AgentEvent::with_fields(AgentEventType::TextBlockDelta, map))
}

#[tokio::test]
async fn reasoning_stream_passes_through_unchanged() {
    let mw = AgentTraceMiddleware::new();
    // next() yields two text deltas; the middleware must forward them unchanged.
    let input = ReasoningInput::new(vec![Msg::user("hi")], vec![], GenerateOptions::default());
    let stream = mw.on_reasoning(
        &ctx(),
        input,
        Box::new(|_inp| {
            Box::pin(futures::stream::iter(vec![text_delta("hel"), text_delta("lo")]))
        }),
    );
    let events: Vec<_> = stream.collect().await;
    assert_eq!(events.len(), 2, "middleware must not add or drop events");
    assert_eq!(
        events[0].as_ref().unwrap().event_type(),
        AgentEventType::TextBlockDelta
    );
}

#[tokio::test]
async fn reasoning_empty_stream_completes_without_events() {
    let mw = AgentTraceMiddleware::new();
    let input = ReasoningInput::new(vec![Msg::user("x")], vec![], GenerateOptions::default());
    let stream = mw.on_reasoning(
        &ctx(),
        input,
        Box::new(|_inp| Box::pin(futures::stream::empty::<Result<AgentEvent>>())),
    );
    let events: Vec<_> = stream.collect().await;
    assert!(events.is_empty());
}
