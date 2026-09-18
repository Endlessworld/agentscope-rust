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
fn final_message(event: &AgentEvent) -> Option<Msg> {
    match event {
        AgentEvent::AgentResult(data) | AgentEvent::AgentEnd(data) => data
            .fields
            .get("message")
            .and_then(|v| serde_json::from_value::<Msg>(v.clone()).ok()),
        _ => None,
    }
}
