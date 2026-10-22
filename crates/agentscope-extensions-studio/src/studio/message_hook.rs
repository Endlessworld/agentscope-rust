//! Hook for automatically forwarding agent messages to Studio.
//!
//! Java's `StudioMessageHook` implements the (now deprecated) `Hook` interface
//! and intercepts `PostCallEvent`. Rust's core has no `Hook`; the equivalent is
//! a [`Middleware`](agentscope_core::middleware::Middleware) intercepting
//! `on_agent`: after the core event stream completes we forward the final
//! message to Studio as a fire-and-forget side effect. Failures are logged but
//! never interrupt agent execution.
use std::sync::Arc;

use futures::StreamExt;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::AgentEvent;
use agentscope_core::message::Msg;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};

use crate::studio::client::StudioClient;

/// Middleware that forwards the final output message of each agent invocation
/// to Studio for visualization.
pub struct StudioMessageHook {
    client: Option<Arc<StudioClient>>,
}

impl StudioMessageHook {
    /// Creates a hook backed by the given Studio client.
    pub fn new(client: Arc<StudioClient>) -> Self {
        Self {
            client: Some(client),
        }
    }

    /// Creates a no-op hook (mirrors Java's tolerant handling of a null client).
    pub fn disabled() -> Self {
        Self { client: None }
    }
}

impl Middleware for StudioMessageHook {
    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let stream = next(input);
        let Some(client) = self.client.clone() else {
            tracing::warn!(
                "StudioMessageHook has no Studio client; skipping message forwarding"
            );
            return stream;
        };

        // Capture the last assistant/result message as it streams, then forward it
        // once the stream ends. Failures are logged, never propagated.
        let (tx, rx) = tokio::sync::mpsc::channel::<Msg>(1);
        let forwarded = stream.map(move |item| {
            if let Ok(event) = &item {
                if let Some(msg) = final_message(event) {
                    let _ = tx.try_send(msg);
                }
            }
            item
        });

        let client_for_task = client.clone();
        tokio::spawn(async move {
            let mut rx = rx;
            // Keep only the most recent forwarded message.
            let mut last: Option<Msg> = None;
            while let Some(msg) = rx.recv().await {
                last = Some(msg);
            }
            if let Some(msg) = last {
                if let Err(err) = client_for_task.push_message(&msg).await {
                    tracing::error!(error = %err, "failed to push message to Studio");
                }
            }
        });

        Box::pin(forwarded)
    }
}

/// Extracts a forwardable final message from an agent event.
///
/// Mirrors Java's `PostCallEvent#getFinalMessage`: the `AGENT_RESULT` payload.
///
/// The core writes the final reply under `fields["reply"]`
/// (`react_agent.rs`: `data.fields.insert("reply", ...)`). Reading `"message"`
/// here made `final_message()` always `None`, so Studio forwarding never fired.
/// `"message"` is still accepted as a fallback for third-party producers.
fn final_message(event: &AgentEvent) -> Option<Msg> {
    match event {
        AgentEvent::AgentResult(data) | AgentEvent::AgentEnd(data) => data
            .fields
            .get("reply")
            .or_else(|| data.fields.get("message"))
            .and_then(|v| serde_json::from_value::<Msg>(v.clone()).ok()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::EventData;

    fn result_with(fields: &[(&str, serde_json::Value)]) -> AgentEvent {
        let mut data = EventData::default();
        for (k, v) in fields {
            data.fields.insert((*k).to_string(), v.clone());
        }
        AgentEvent::AgentResult(data)
    }

    /// The key the core actually writes must be the one we read.
    #[test]
    fn reads_reply_key_written_by_core() {
        let msg = Msg::assistant("hello");
        let event = result_with(&[("reply", serde_json::json!(msg))]);
        let extracted = final_message(&event).expect("reply should be extracted");
        assert_eq!(extracted, msg);
    }

    /// Legacy/third-party producers using `message` keep working.
    #[test]
    fn falls_back_to_message_key() {
        let msg = Msg::assistant("legacy");
        let event = result_with(&[("message", serde_json::json!(msg))]);
        assert_eq!(final_message(&event), Some(msg));
    }

    #[test]
    fn non_result_events_yield_none() {
        assert!(final_message(&AgentEvent::AgentStart(EventData::default())).is_none());
    }
}
