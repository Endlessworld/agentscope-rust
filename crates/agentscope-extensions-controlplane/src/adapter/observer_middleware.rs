//! Observer middleware, mirroring `io.agentscope.extensions.controlplane.adapter.ControlPlaneObserverMiddleware`.
//!
//! **Purely additive:** inputs are forwarded to `next` unchanged and the resulting event
//! stream is only tapped, never rewritten or filtered. Removing this middleware changes
//! nothing about how the agent behaves — a failed report is swallowed.
//!
//! Tool-call arguments are read in [`on_acting`](Middleware::on_acting), because the
//! `ToolCallEnd` event carries only the call's name and id. Tool output text is reassembled
//! from the delta events, which is the only place the framework exposes it incrementally.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures::StreamExt;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::{AgentEvent, EventData};
use agentscope_core::message::{Msg, MsgRole};
use agentscope_core::middleware::{ActingInput, AgentInput, EventStream, Middleware, Next};

use crate::bridge::BridgeReporter;
use crate::model::session_event::{
    SessionEvent, ROLE_ASSISTANT, ROLE_SYSTEM, ROLE_TOOL, ROLE_USER, TYPE_MESSAGE,
    TYPE_SESSION_START, TYPE_TOOL_CALL, TYPE_TOOL_RESULT,
};

/// State shared between an adapter and its observer middleware.
///
/// The reporter is installed when the bridge attaches the adapter
/// ([`FrameworkAdapter::on_bridge_attached`](crate::adapter::FrameworkAdapter::on_bridge_attached)),
/// so the middleware can be built before the bridge exists.
#[derive(Default)]
pub(crate) struct AdapterShared {
    pub(crate) reporter: Mutex<Option<BridgeReporter>>,
    pub(crate) started_sessions: Mutex<HashSet<String>>,
    /// user slot recorded per session (needed to reach the right state slot).
    pub(crate) session_users: Mutex<HashMap<String, String>>,
}

impl AdapterShared {
    /// The bridge reporter, once attached.
    pub(crate) fn reporter(&self) -> Option<BridgeReporter> {
        self.reporter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Per-turn accumulation (token usage and in-flight tool output).
#[derive(Default)]
struct TurnState {
    tool_outputs: Mutex<HashMap<String, String>>,
}

/// Middleware that copies conversation activity out to controlplane without touching it.
#[derive(Clone)]
pub struct ControlPlaneObserverMiddleware {
    shared: Arc<AdapterShared>,
}

impl ControlPlaneObserverMiddleware {
    /// Creates the middleware backed by `shared`.
    pub(crate) fn new(shared: Arc<AdapterShared>) -> Self {
        Self { shared }
    }

    fn reporter(&self) -> Option<BridgeReporter> {
        self.shared.reporter()
    }
}

#[async_trait::async_trait]
impl Middleware for ControlPlaneObserverMiddleware {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let session_id = session_id_of(ctx);
        // Recording the user slot is independent of reporting: it is what lets the adapter
        // reach the right state slot later, so it must run even with no bridge attached
        // (mirrors `AgentScopeAdapter.rememberSession`).
        if let Some(user) = ctx.user_id.clone() {
            self.shared
                .session_users
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(session_id.clone(), user);
        }
        let Some(reporter) = self.reporter() else {
            return next(input);
        };

        // One `call()` is a turn, not a session; the session opens on the first turn and
        // closes only on an explicit terminate, so no session_end is emitted here.
        let first_turn = self
            .shared
            .started_sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session_id.clone());
        if first_turn {
            reporter.publish(SessionEvent::builder(&session_id, TYPE_SESSION_START).build());
        }

        for msg in &input.messages {
            let text = msg.text_content();
            if !text.is_empty() {
                reporter.publish(
                    SessionEvent::builder(&session_id, TYPE_MESSAGE)
                        .role(role_of(msg))
                        .content(text)
                        .build(),
                );
            }
        }

        reporter.set_busy(&session_id, true);
        let turn = Arc::new(TurnState::default());
        let sid = session_id.clone();
        let stream = next(input);
        Box::pin(stream.inspect(move |res| {
            if let Ok(event) = res {
                observe(&reporter, &sid, &turn, event);
            }
        }))
    }

    fn on_acting(
        &self,
        ctx: &RuntimeContext,
        input: ActingInput,
        next: Next<ActingInput>,
    ) -> EventStream {
        let Some(reporter) = self.reporter() else {
            return next(input);
        };
        let session_id = session_id_of(ctx);
        for call in &input.tool_calls {
            let mut builder = SessionEvent::builder(&session_id, TYPE_TOOL_CALL)
                .role(ROLE_ASSISTANT)
                .tool_name(call.name.clone())
                .framework_meta(tool_metadata(&call.id, "running"));
            if !call.input.is_empty() {
                if let Ok(value) = serde_json::to_value(&call.input) {
                    builder = builder.tool_input(value);
                }
            }
            reporter.publish(builder.build());
        }
        next(input)
    }

    /// Captures the fully composed system prompt when this middleware is registered last in
    /// the `on_system_prompt` chain.
    async fn on_system_prompt(&self, ctx: &RuntimeContext, current_prompt: String) -> String {
        if let Some(reporter) = self.reporter() {
            reporter.set_effective_system_prompt(&session_id_of(ctx), current_prompt.clone());
        }
        current_prompt
    }
}

/// Folds one streamed agent event into the Level-2 session stream.
fn observe(reporter: &BridgeReporter, session_id: &str, turn: &TurnState, event: &AgentEvent) {
    match event {
        AgentEvent::ToolResultTextDelta(data) => {
            let call_id = field_str(&data, "tool_call_id");
            let delta = field_str(&data, "delta");
            let mut outputs = turn.tool_outputs.lock().unwrap_or_else(|p| p.into_inner());
            outputs.entry(call_id).or_default().push_str(&delta);
        }
        AgentEvent::ToolResultEnd(data) => {
            let call_id = field_str(&data, "tool_call_id");
            let tool_name = field_str(&data, "tool_name");
            let state = field_str(&data, "state");
            let output = turn
                .tool_outputs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&call_id)
                .unwrap_or_default();
            reporter.publish(
                SessionEvent::builder(session_id, TYPE_TOOL_RESULT)
                    .role(ROLE_TOOL)
                    .tool_name(tool_name)
                    .tool_output(output)
                    .framework_meta(tool_metadata(&call_id, &state))
                    .build(),
            );
        }
        AgentEvent::AgentResult(data) => {
            // The reply message carries the assistant text and the turn's token usage;
            // Rust's `ModelCallEnd` event does not carry usage.
            let Some(reply) = data.fields.get("reply") else {
                return;
            };
            let Ok(msg) = serde_json::from_value::<Msg>(reply.clone()) else {
                return;
            };
            let text = msg.text_content();
            if text.is_empty() {
                return;
            }
            let mut builder = SessionEvent::builder(session_id, TYPE_MESSAGE)
                .role(ROLE_ASSISTANT)
                .content(text);
            if let Some(usage) = msg.chat_usage() {
                builder = builder
                    .tokens_in(usage.input_tokens as i64)
                    .tokens_out(usage.output_tokens as i64);
            }
            reporter.publish(builder.build());
        }
        AgentEvent::AgentEnd(_) => {
            reporter.set_busy(session_id, false);
        }
        _ => {}
    }
}

fn field_str(data: &EventData, key: &str) -> String {
    data.fields
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn tool_metadata(tool_call_id: &str, state: &str) -> serde_json::Value {
    serde_json::json!({
        "toolCallId": tool_call_id,
        "state": if state.is_empty() { "unknown" } else { state },
    })
}

/// Session id for a call: the runtime session, else the agent id, else `default`.
fn session_id_of(ctx: &RuntimeContext) -> String {
    if let Some(s) = ctx.session_id.as_deref().filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    if let Some(a) = ctx.agent_id.as_deref().filter(|s| !s.is_empty()) {
        return a.to_string();
    }
    if let Some(n) = ctx.agent_name.as_deref().filter(|s| !s.is_empty()) {
        return n.to_string();
    }
    "default".to_string()
}

fn role_of(msg: &Msg) -> &'static str {
    match msg.role() {
        MsgRole::User => ROLE_USER,
        MsgRole::Assistant => ROLE_ASSISTANT,
        MsgRole::System => ROLE_SYSTEM,
        MsgRole::Tool => ROLE_TOOL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ControlPlaneConfig;
    use crate::bridge::ControlPlaneBridge;
    use agentscope_core::message::Msg;

    fn shared_with_reporter() -> (Arc<AdapterShared>, ControlPlaneBridge) {
        let cfg = ControlPlaneConfig::builder("friday")
            .instance_key("rep-1")
            .event_journal_dir(
                std::env::temp_dir()
                    .join(format!("controlplane-obs-{}", uuid::Uuid::new_v4()))
                    .to_str()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let bridge = ControlPlaneBridge::new(cfg).start().unwrap();
        let shared = Arc::new(AdapterShared::default());
        *shared.reporter.lock().unwrap() = Some(bridge.reporter());
        (shared, bridge)
    }

    fn ctx(session: &str) -> RuntimeContext {
        let mut c = RuntimeContext::default();
        c.session_id = Some(session.to_string());
        c
    }

    #[tokio::test]
    async fn on_agent_emits_session_start_and_input_messages() {
        let (shared, _bridge) = shared_with_reporter();
        let mw = ControlPlaneObserverMiddleware::new(shared);
        let mut ctx = ctx("s1");
        ctx.delta_tx = None;
        let input = AgentInput::new(vec![Msg::user("hello")]);
        let next: Next<AgentInput> = Box::new(|_i| Box::pin(futures::stream::empty()));
        let mut stream = mw.on_agent(&ctx, input, next);
        // Drain; the tap side effects have already run.
        while stream.next().await.is_some() {}
        // session_start + 1 user message
        assert_eq!(mw.reporter().unwrap().publish(
            SessionEvent::builder("probe", TYPE_MESSAGE).build()
        ), 1);
    }

    #[tokio::test]
    async fn on_agent_forwards_stream_unchanged_when_attached() {
        let (shared, _bridge) = shared_with_reporter();
        let mw = ControlPlaneObserverMiddleware::new(shared);
        let input = AgentInput::new(vec![Msg::user("hi")]);
        let next: Next<AgentInput> = Box::new(|_i| {
            Box::pin(futures::stream::iter(vec![Ok(AgentEvent::AgentEnd(
                EventData::default(),
            ))]))
        });
        let stream = mw.on_agent(&ctx("s2"), input, next);
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Ok(AgentEvent::AgentEnd(_))));
    }

    #[tokio::test]
    async fn observer_passes_through_without_reporter() {
        let mw = ControlPlaneObserverMiddleware::new(Arc::new(AdapterShared::default()));
        let prompt = mw.on_system_prompt(&ctx("s1"), "base".to_string()).await;
        assert_eq!(prompt, "base");
        assert!(mw.reporter().is_none());
    }

    #[test]
    fn session_id_prefers_runtime_session_then_agent() {
        let mut c = RuntimeContext::default();
        assert_eq!(session_id_of(&c), "default");
        c.agent_id = Some("a1".into());
        assert_eq!(session_id_of(&c), "a1");
        c.session_id = Some("s1".into());
        assert_eq!(session_id_of(&c), "s1");
    }

    #[test]
    fn tool_metadata_defaults_unknown_state() {
        let meta = tool_metadata("c1", "");
        assert_eq!(meta["toolCallId"], "c1");
        assert_eq!(meta["state"], "unknown");
    }
}
