//! `TranscriptMiddleware` — persists each agent turn to a `TranscriptStore` after the
//! call completes, mirroring `io.agentscope.harness.agent.middleware.TranscriptMiddleware`.
//! Independent of the memory flush: it keeps a complete session history even when memory
//! hooks are disabled.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::event::AgentEvent;
use agentscope_core::middleware::{EventStream, Middleware, Next};
use futures::stream::{self, StreamExt};

use crate::agent::transcript::{TranscriptRef, TranscriptStore};

/// Middleware that records each agent turn into a transcript store.
pub struct TranscriptMiddleware {
    store: Arc<dyn TranscriptStore>,
    tenant: Option<String>,
}

impl TranscriptMiddleware {
    pub fn new(store: Arc<dyn TranscriptStore>) -> Self {
        Self { store, tenant: None }
    }

    /// Pins the tenant used for transcript routing (defaults to `user_id` or `"default"`).
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    fn ref_for(&self, ctx: &RuntimeContext) -> TranscriptRef {
        let tenant = self
            .tenant
            .clone()
            .or_else(|| ctx.user_id().map(String::from))
            .unwrap_or_else(|| "default".into());
        let agent_id = ctx
            .agent_id()
            .map(String::from)
            .or_else(|| ctx.agent_name.clone())
            .unwrap_or_else(|| "agent".into());
        let session_id = ctx.session_id().unwrap_or("default").to_string();
        TranscriptRef::new(tenant, agent_id, session_id)
    }
}

impl Middleware for TranscriptMiddleware {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: agentscope_core::middleware::AgentInput,
        next: Next<agentscope_core::middleware::AgentInput>,
    ) -> EventStream {
        let store = Arc::clone(&self.store);
        let r#ref = self.ref_for(ctx);
        let input_messages: Vec<serde_json::Value> = input
            .messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
            .collect();
        let stream = next(input);
        let captured = async move {
            let events: Vec<Result<AgentEvent>> = stream.collect().await;
            let out_events: Vec<serde_json::Value> = events
                .iter()
                .filter_map(|e| e.as_ref().ok().map(|ev| serde_json::to_value(ev).unwrap_or(serde_json::Value::Null)))
                .collect();
            let _ = store.append_segment(&r#ref, &input_messages, &out_events).await;
            events
        };
        stream::once(captured)
            .map(stream::iter)
            .flatten()
            .boxed()
    }
}
