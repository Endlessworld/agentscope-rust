//! End-to-end coverage for the native committed-event export chain (P0 #13).
//!
//! The unit tests live with their modules; these tests drive a real `ReActAgent` with a fake
//! offline model and assert what reaches the bridge's durable journal.

use std::sync::Arc;

use agentscope_core::agent::react_agent::ReActAgent;
use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::session::{
    InMemorySessionLogStore, SessionEvent as NativeSessionEvent, SessionExportSink,
    SessionExportSinkHandle, SESSION_EXPORT_SINK_CONTEXT_KEY,
};
use agentscope_core::state::AgentState;
use futures::stream::BoxStream;

use agentscope_extensions_controlplane::model::session_event::{
    ROLE_ASSISTANT, ROLE_USER, TYPE_MESSAGE,
};
use agentscope_extensions_controlplane::{ControlPlaneBridge, ControlPlaneConfig};

/// An offline model that always answers with one text block.
struct FakeModel;

impl Model for FakeModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::iter(vec![Ok(ChatResponse::builder()
            .content(vec![ContentBlock::Text(TextBlock::new("hello from fake"))])
            .build())]))
    }

    fn model_name(&self) -> &str {
        "fake-offline"
    }
}

async fn temp_bridge() -> ControlPlaneBridge {
    let cfg = ControlPlaneConfig::builder("friday")
        .instance_key("p0-13")
        .enable_events(true)
        .event_journal_dir(
            std::env::temp_dir()
                .join(format!("controlplane-p013-{}", uuid::Uuid::new_v4()))
                .to_str()
                .unwrap(),
        )
        .build()
        .unwrap();
    ControlPlaneBridge::new(cfg).start().await.unwrap()
}

fn agent_with_sink(sink: Arc<dyn SessionExportSink>) -> ReActAgent {
    ReActAgent::builder()
        .name("friday")
        .model(Arc::new(FakeModel))
        .state(AgentState::builder().session_id("s1").build())
        .session_log_store(Arc::new(InMemorySessionLogStore::new()))
        .extension(
            SESSION_EXPORT_SINK_CONTEXT_KEY,
            SessionExportSinkHandle::new(sink),
        )
        .build()
        .unwrap()
}

fn message_fact(seq: u64, text: &str) -> NativeSessionEvent {
    NativeSessionEvent::new(
        format!("native-{seq}"),
        seq,
        1_700_000_000_000,
        "message/user",
        Some("run-1".into()),
        Some("turn-1".into()),
        true,
        serde_json::to_string(&serde_json::json!({ "message": Msg::user(text) })).unwrap(),
    )
    .unwrap()
}

/// A real run exports its committed message facts and advances the per-session watermark.
///
/// Before this change the agent opened its recorder without an exporter, so the bridge saw
/// nothing and `source_watermark` stayed 0.
#[tokio::test]
async fn committed_events_from_a_real_run_reach_the_journal_once() {
    let bridge = temp_bridge().await;
    let agent = agent_with_sink(bridge.committed_event_sink());

    let reply = agent.call(vec![Msg::user("hello agent")]).await.unwrap();
    assert_eq!(reply.text_content(), "hello from fake");

    let events = bridge.pending_events();
    let user = events
        .iter()
        .find(|e| e.event_type == TYPE_MESSAGE && e.role == ROLE_USER)
        .expect("committed message/user fact exported");
    assert_eq!(user.session_id, "s1");
    assert_eq!(user.content, "hello agent");
    let assistant = events
        .iter()
        .find(|e| e.event_type == TYPE_MESSAGE && e.role == ROLE_ASSISTANT)
        .expect("committed message/assistant fact exported");
    assert_eq!(assistant.content, "hello from fake");

    // Every exported fact carries the native envelope, and the watermark is the highest
    // native sequence that crossed the boundary.
    let max_native_seq = events
        .iter()
        .filter_map(|e| {
            e.framework_meta
                .as_ref()
                .and_then(|m| m.get("native_seq"))
                .and_then(|v| v.as_i64())
        })
        .max()
        .expect("native_seq present on exported facts");
    assert!(max_native_seq > 0);
    assert_eq!(bridge.source_watermark("s1"), max_native_seq);

    // A turn emits each message fact exactly once; the durable export cursor guarantees a
    // second drain does not replay them.
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == TYPE_MESSAGE && e.role == ROLE_USER)
            .count(),
        1
    );
}

/// Re-delivering the same native fact (lost acknowledgement) journals it exactly once.
///
/// Before this change `EventJournal` had no source watermark at all, so every accepted
/// duplicate would append another Level-2 event.
#[tokio::test]
async fn duplicate_native_watermark_exports_once() {
    let bridge = temp_bridge().await;
    let sink = bridge.committed_event_sink();
    let fact = message_fact(5, "dedupe me");

    sink.accept_for("s1", &fact).unwrap();
    sink.accept_for("s1", &fact).unwrap();
    assert_eq!(bridge.pending_event_count(), 1);
    assert_eq!(bridge.source_watermark("s1"), 5);

    // An older fact is also below the watermark and must be dropped.
    sink.accept_for("s1", &message_fact(4, "stale")).unwrap();
    assert_eq!(bridge.pending_event_count(), 1);
    assert_eq!(bridge.source_watermark("s1"), 5);

    // A newer fact is accepted, and moves the watermark forward.
    sink.accept_for("s1", &message_fact(6, "fresh")).unwrap();
    assert_eq!(bridge.pending_event_count(), 2);
    assert_eq!(bridge.source_watermark("s1"), 6);

    // Watermarks are per source: another session starts at zero.
    assert_eq!(bridge.source_watermark("s2"), 0);
}

/// The watermark survives a bridge restart, so a redelivery after crash is still a no-op.
#[tokio::test]
async fn source_watermark_is_durable_across_bridge_reopen() {
    let dir =
        std::env::temp_dir().join(format!("controlplane-p013-reopen-{}", uuid::Uuid::new_v4()));
    let cfg = ControlPlaneConfig::builder("friday")
        .instance_key("p0-13")
        .enable_events(true)
        .event_journal_dir(dir.to_str().unwrap())
        .build()
        .unwrap();
    let fact = message_fact(9, "survive restart");

    {
        let bridge = ControlPlaneBridge::new(cfg.clone()).start().await.unwrap();
        bridge
            .committed_event_sink()
            .accept_for("s1", &fact)
            .unwrap();
        assert_eq!(bridge.pending_event_count(), 1);
    }

    let reopened = ControlPlaneBridge::new(cfg).start().await.unwrap();
    assert_eq!(reopened.source_watermark("s1"), 9);
    reopened
        .committed_event_sink()
        .accept_for("s1", &fact)
        .unwrap();
    assert_eq!(
        reopened.pending_event_count(),
        1,
        "redelivery after restart must not append a second event"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a configured sink the run is a strict no-op for the export chain.
///
/// Before this change the same was true, but the wiring must not make "no sink" an error or
/// start journaling behind the host's back.
#[tokio::test]
async fn no_sink_configured_is_a_noop() {
    let bridge = temp_bridge().await;
    let agent = ReActAgent::builder()
        .name("friday")
        .model(Arc::new(FakeModel))
        .state(AgentState::builder().session_id("s1").build())
        .session_log_store(Arc::new(InMemorySessionLogStore::new()))
        .build()
        .unwrap();

    let reply = agent.call(vec![Msg::user("hello")]).await.unwrap();
    assert_eq!(reply.text_content(), "hello from fake");
    assert_eq!(bridge.pending_event_count(), 0);
    assert_eq!(bridge.source_watermark("s1"), 0);
}
