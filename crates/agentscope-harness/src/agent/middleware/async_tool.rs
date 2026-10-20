//! Middleware that offloads long-running tool calls to background execution.
//!
//! When the acting phase exceeds the configured timeout:
//!   1. the underlying tool execution continues in the background (never
//!      cancelled, driven by a spawned task);
//!   2. a placeholder `ToolResultBlock` is emitted so the LLM can continue
//!      reasoning;
//!   3. when the background execution completes, the real result is pushed to
//!      the session inbox as a hint payload and a wakeup is enqueued.
//!
//! `InboxMiddleware` drains the inbox on the next reasoning step, making the
//! real result available to the LLM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::ToolUseBlock;
use agentscope_core::middleware::{ActingInput, EventStream, Middleware, Next};
use futures::StreamExt;
use serde_json::{json, Value};

use crate::agent::bus::{AsyncToolRecord, AsyncToolRegistry, MessageBus};

/// Generates a UUID without dashes, matching the Java side.
fn new_hex() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

/// Builds the placeholder reminder text for an offloaded tool call.
fn placeholder_text(name: &str, id: &str, secs: u64) -> String {
    format!(
        "<system-reminder>Tool '{}' is running in background (id={}) for over {}s. \
         You will be notified automatically when it finishes, so DO NOT poll, \
         query, or wait for the result yourself. You have two options:\n\
         1. Continue with other independent tasks;\n\
         2. If nothing else to do, give a text reply without calling any tool.\
         </system-reminder>",
        name, id, secs
    )
}

/// Extracts the accumulated text from a `ToolResultTextDelta` event.
fn extract_delta(fields: &serde_json::Map<String, Value>) -> Option<&str> {
    for key in ["delta", "text", "content"] {
        if let Some(Value::String(s)) = fields.get(key) {
            return Some(s);
        }
    }
    None
}

/// Converts a `serde_json::json!` object literal into a `JsonMap`.
fn ev_fields(v: Value) -> serde_json::Map<String, Value> {
    serde_json::from_value(v).expect("expected a JSON object")
}

/// Middleware that offloads slow tool executions to the background.
pub struct AsyncToolMiddleware {
    message_bus: Arc<dyn MessageBus>,
    offload_timeout: Duration,
    async_tool_registry: Option<Arc<dyn AsyncToolRegistry>>,
}

impl AsyncToolMiddleware {
    /// Creates a middleware with the default offload timeout (2 minutes).
    pub fn new(message_bus: Arc<dyn MessageBus>, offload_timeout: Duration) -> Self {
        Self::builder(message_bus, offload_timeout).build()
    }

    /// Returns a builder for a more configurable instance.
    pub fn builder(
        message_bus: Arc<dyn MessageBus>,
        offload_timeout: Duration,
    ) -> AsyncToolMiddlewareBuilder {
        AsyncToolMiddlewareBuilder::new(message_bus, offload_timeout)
    }
}

/// Builder for [`AsyncToolMiddleware`].
pub struct AsyncToolMiddlewareBuilder {
    message_bus: Arc<dyn MessageBus>,
    offload_timeout: Duration,
    async_tool_registry: Option<Arc<dyn AsyncToolRegistry>>,
}

impl AsyncToolMiddlewareBuilder {
    /// Creates a builder with the given bus and offload timeout.
    pub fn new(message_bus: Arc<dyn MessageBus>, offload_timeout: Duration) -> Self {
        Self {
            message_bus,
            offload_timeout,
            async_tool_registry: None,
        }
    }

    /// Enables async-tool registry tracking of offloaded executions.
    pub fn async_tool_registry(mut self, registry: Arc<dyn AsyncToolRegistry>) -> Self {
        self.async_tool_registry = Some(registry);
        self
    }

    /// Builds the middleware.
    pub fn build(self) -> AsyncToolMiddleware {
        AsyncToolMiddleware {
            message_bus: self.message_bus,
            offload_timeout: self.offload_timeout,
            async_tool_registry: self.async_tool_registry,
        }
    }
}

impl Middleware for AsyncToolMiddleware {
    fn on_acting(&self, ctx: &RuntimeContext, input: ActingInput, next: Next<ActingInput>) -> EventStream {
        let Some(session_id) = ctx.session_id().map(str::to_string) else {
            return next(input);
        };

        let message_bus = Arc::clone(&self.message_bus);
        let async_registry = self.async_tool_registry.clone();
        let offload_timeout = self.offload_timeout;
        let user_id = ctx.user_id().map(str::to_string).unwrap_or_default();
        let agent_id = ctx.agent_id().map(str::to_string).unwrap_or_default();
        let tool_calls: Vec<ToolUseBlock> = input.tool_calls.clone();
        // Where the placeholder results go: `on_acting` only gets `&RuntimeContext`, so
        // the equivalent of Java's direct `state.contextMutable().add(...)` write in
        // `emitPlaceholderAndComplete` is a write to the acting results handle.
        let results_handle = input.results.clone();

        // Shared state between the background task and the outer stream.
        let buffer = Arc::new(std::sync::Mutex::new(Vec::<Result<AgentEvent>>::new()));
        let result_buf = Arc::new(std::sync::Mutex::new(String::new()));
        let offloaded = Arc::new(AtomicBool::new(false));
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);

        // Spawn the underlying tool execution in the background (never cancelled).
        let buffer_bg = Arc::clone(&buffer);
        let result_bg = Arc::clone(&result_buf);
        let offloaded_bg = Arc::clone(&offloaded);
        let message_bus_bg = Arc::clone(&message_bus);
        let async_registry_bg = async_registry.clone();
        let tool_calls_bg = tool_calls.clone();
        let session_bg = session_id.clone();
        let user_bg = user_id.clone();
        let agent_bg = agent_id.clone();
        let done_tx = done_tx.clone();
        tokio::spawn(async move {
            let mut stream = next(input);
            while let Some(item) = stream.next().await {
                if offloaded_bg.load(Ordering::SeqCst) {
                    // Background mode: capture the eventual result text.
                    match &item {
                        Ok(AgentEvent::ToolResultTextDelta(data)) => {
                            if let Some(text) = extract_delta(&data.fields) {
                                result_bg.lock().unwrap().push_str(text);
                            }
                        }
                        Err(_) => {
                            if let Some(registry) = &async_registry_bg {
                                for tc in &tool_calls_bg {
                                    let _ = registry.fail(&tc.id, "background tool failed").await;
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    buffer_bg.lock().unwrap().push(item);
                }
            }
            let _ = done_tx.send(true);

            // If offloaded, deliver the final result to the inbox + wakeup.
            if offloaded_bg.load(Ordering::SeqCst) {
                let result = result_bg.lock().unwrap().clone();
                let result_str = if result.is_empty() { "(no output)".to_string() } else { result };

                if let Some(registry) = &async_registry_bg {
                    for tc in &tool_calls_bg {
                        let _ = registry.complete(&tc.id, &result_str).await;
                    }
                }

                let tool_names: Vec<String> =
                    tool_calls_bg.iter().map(|t| t.name.clone()).collect();
                let hint_content = format!(
                    "<system-notification>Tool '{}' running in background has completed.\n\nResult:\n\n{}</system-notification>",
                    tool_names.join(", "),
                    result_str
                );
                let payload = json!({
                    "type": "hint",
                    "id": new_hex(),
                    "hint": hint_content,
                    "source": "tool_output",
                });
                let _ = message_bus_bg.inbox_push(&session_bg, payload).await;
                let _ = message_bus_bg
                    .enqueue_wakeup(&user_bg, &session_bg, &agent_bg)
                    .await;
            }
        });

        // Outer stream: race the inner completion against the offload timeout.
        let fut = async move {
            let wait = tokio::time::timeout(offload_timeout, done_rx.changed());
            match wait.await {
                // Inner completed within the timeout: stream the real events.
                Ok(_) => {
                    let events = buffer.lock().unwrap().drain(..).collect::<Vec<_>>();
                    futures::stream::iter(events)
                }
                // Timed out: offload, register async records, emit placeholders.
                Err(_) => {
                    offloaded.store(true, Ordering::SeqCst);
                    if let Some(registry) = &async_registry {
                        for tc in &tool_calls {
                            let _ = registry
                                .register(AsyncToolRecord::new(
                                    &tc.id,
                                    &session_id,
                                    &tc.name,
                                    &tc.id,
                                    chrono::Utc::now(),
                                ))
                                .await;
                        }
                    }
                    let reply_id = new_hex();
                    let mut events: Vec<agentscope_core::error::Result<AgentEvent>> = Vec::new();
                    for tc in &tool_calls {
                        let text = placeholder_text(&tc.name, &tc.id, offload_timeout.as_secs());
                        // Persist the placeholder so the next reasoning step sees a tool
                        // result for this call instead of an unanswered tool_use.
                        results_handle.record_placeholder(tc, &text);
                        events.push(Ok(AgentEvent::with_fields(
                            AgentEventType::ToolResultStart,
                            ev_fields(json!({
                                "replyId": reply_id,
                                "toolCallId": tc.id,
                                "toolName": tc.name,
                            })),
                        )));
                        events.push(Ok(AgentEvent::with_fields(
                            AgentEventType::ToolResultTextDelta,
                            ev_fields(json!({
                                "replyId": reply_id,
                                "toolCallId": tc.id,
                                "toolName": tc.name,
                                "delta": text,
                            })),
                        )));
                        events.push(Ok(AgentEvent::with_fields(
                            AgentEventType::ToolResultEnd,
                            ev_fields(json!({
                                "replyId": reply_id,
                                "toolCallId": tc.id,
                                "toolName": tc.name,
                                "state": "SUCCESS",
                            })),
                        )));
                    }
                    futures::stream::iter(events)
                }
            }
        };

        Box::pin(futures::stream::once(fut).flatten())
    }
}
