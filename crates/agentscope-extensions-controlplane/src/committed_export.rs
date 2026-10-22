//! Native committed-event export, mirroring
//! `ControlPlaneObserverMiddleware.publishNative` + `SessionBridge.onCommittedEvent`.
//!
//! The core session log records immutable facts (`message/user`, `tool/result`, …). Java's
//! controlplane observer installs a [`SessionExportSink`] into the runtime context so the
//! recorder drains those committed facts through it; this module is the Rust equivalent.
//! Each accepted fact is translated into a Level-2 [`SessionEvent`] carrying a
//! `native_source` / `native_seq` envelope, then handed to [`BridgeReporter::publish_committed`],
//! which drops anything at or below the durable per-source watermark before journaling.
//!
//! Failure never reaches the conversation: [`SessionExportSink::accept_for`] returns `Ok`
//! even when a fact cannot be mapped, and the recorder already treats a failed drain as
//! best-effort.

use std::sync::Arc;

use agentscope_core::session::{
    SessionEvent as NativeSessionEvent, SessionExportSink,
};
use agentscope_core::message::{Msg, MsgRole};

use crate::bridge::BridgeReporter;
use crate::model::session_event::{
    SessionEvent, ROLE_ASSISTANT, ROLE_SYSTEM, ROLE_TOOL, ROLE_USER, TYPE_COMPACTION,
    TYPE_MESSAGE, TYPE_TOOL_CALL, TYPE_TOOL_RESULT,
};

/// Sink name. Java uses `controlplane-native-v1`; the Rust extension keeps its own stable
/// name so a durable export cursor never collides with a Java process sharing a journal.
pub const SINK_NAME: &str = "controlplane-native-v1";

/// Forwards committed native facts into the bridge's durable Level-2 journal.
///
/// One sink may serve every session of an agent: [`SessionExportSink::accept_for`] carries
/// the logical session id, which becomes the watermark source. This is the same shape as the
/// Java sink, which is bound to a session when the middleware installs it per call.
#[derive(Clone)]
pub struct ControlPlaneCommittedEventSink {
    reporter: BridgeReporter,
}

impl std::fmt::Debug for ControlPlaneCommittedEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneCommittedEventSink").finish_non_exhaustive()
    }
}

impl ControlPlaneCommittedEventSink {
    /// Creates a sink that journals through `reporter`.
    pub fn new(reporter: BridgeReporter) -> Self {
        Self { reporter }
    }

    /// The bridge reporter this sink publishes through.
    pub fn reporter(&self) -> &BridgeReporter {
        &self.reporter
    }
}

impl SessionExportSink for ControlPlaneCommittedEventSink {
    fn name(&self) -> &str {
        SINK_NAME
    }

    fn accept(&self, event: &NativeSessionEvent) -> agentscope_core::error::Result<()> {
        self.accept_for("", event)
    }

    fn accept_for(
        &self,
        session_id: &str,
        event: &NativeSessionEvent,
    ) -> agentscope_core::error::Result<()> {
        // Java `sessionIdOf`: an unidentified call falls back to "default" rather than
        // journaling an event with an empty owning session.
        let source = if session_id.is_empty() {
            "default"
        } else {
            session_id
        };
        if let Some(level2) = translate(source, event) {
            // `false` means the watermark already covers this fact. A duplicate delivery is
            // expected after a lost acknowledgement, never an error.
            self.reporter
                .publish_committed(source, event.seq as i64, level2);
        }
        Ok(())
    }
}

/// Wraps `reporter` as a core export sink.
pub fn committed_event_sink(reporter: BridgeReporter) -> Arc<dyn SessionExportSink> {
    Arc::new(ControlPlaneCommittedEventSink::new(reporter))
}

/// Maps a native committed fact into a Level-2 event, or `None` when the fact type is not
/// part of the reported stream.
///
/// Mirrors Java `publishNative`: only message, tool-call/result and compaction facts cross
/// into the control-plane stream; everything else stays native-only.
fn translate(source: &str, fact: &NativeSessionEvent) -> Option<SessionEvent> {
    let event_type = match fact.event_type.as_str() {
        "message/user" | "message/assistant" => TYPE_MESSAGE,
        "tool/requested" => TYPE_TOOL_CALL,
        "tool/result" => TYPE_TOOL_RESULT,
        "context/replaced" => TYPE_COMPACTION,
        _ => return None,
    };

    let data = fact.data().ok().unwrap_or(serde_json::Value::Null);
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "native_source".into(),
        serde_json::Value::String(source.to_string()),
    );
    metadata.insert("native_seq".into(), serde_json::Value::from(fact.seq));
    metadata.insert(
        "native_event_id".into(),
        serde_json::Value::String(fact.event_id.clone()),
    );
    metadata.insert(
        "execution_run_id".into(),
        fact.execution_run_id
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    );

    let mut builder = SessionEvent::builder(source, event_type)
        .occurred_at(fact.occurred_at)
        .framework_meta(serde_json::Value::Object(metadata.clone()));

    if event_type == TYPE_MESSAGE {
        if let Some(message) = data.get("message") {
            if let Ok(msg) = serde_json::from_value::<Msg>(message.clone()) {
                builder = builder
                    .role(role_of(&msg))
                    .content(msg.text_content());
                metadata.insert("message".into(), message.clone());
                builder = builder.framework_meta(serde_json::Value::Object(metadata));
            }
        }
    } else if event_type == TYPE_TOOL_CALL {
        if let Some(call) = data.get("call") {
            if let Some(name) = call.get("name").and_then(|v| v.as_str()) {
                builder = builder.tool_name(name);
            }
            if let Some(input) = call.get("input") {
                builder = builder.tool_input(input.clone());
            }
        }
    }

    Some(builder.build())
}

/// Wire role name for a message.
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
    use agentscope_core::session::SessionEvent as Native;

    fn fact(event_type: &str, seq: u64, payload: serde_json::Value) -> Native {
        Native::new(
            format!("native-{seq}"),
            seq,
            1_700_000_000_000,
            event_type,
            Some("run-1".into()),
            Some("turn-1".into()),
            true,
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn message_facts_map_role_content_and_envelope() {
        let event = fact(
            "message/assistant",
            7,
            serde_json::json!({ "message": Msg::assistant("hi there") }),
        );
        let level2 = translate("s1", &event).expect("message exported");
        assert_eq!(level2.session_id, "s1");
        assert_eq!(level2.event_type, TYPE_MESSAGE);
        assert_eq!(level2.role, ROLE_ASSISTANT);
        assert_eq!(level2.content, "hi there");
        let meta = level2.framework_meta.unwrap();
        assert_eq!(meta["native_source"], "s1");
        assert_eq!(meta["native_seq"], 7);
        assert_eq!(meta["native_event_id"], "native-7");
        assert_eq!(meta["execution_run_id"], "run-1");
    }

    #[test]
    fn tool_and_compaction_facts_map_to_their_level2_types() {
        let call = fact(
            "tool/requested",
            8,
            serde_json::json!({
                "call": { "id": "c1", "name": "search", "input": { "q": "rust" } }
            }),
        );
        let mapped = translate("s1", &call).unwrap();
        assert_eq!(mapped.event_type, TYPE_TOOL_CALL);
        assert_eq!(mapped.tool_name, "search");
        assert_eq!(mapped.tool_input.as_ref().unwrap()["q"], "rust");

        let replaced = fact("context/replaced", 9, serde_json::json!({}));
        assert_eq!(translate("s1", &replaced).unwrap().event_type, TYPE_COMPACTION);
    }

    #[test]
    fn unrouted_facts_are_ignored() {
        // `run/start` is native-only; Java's `publishNative` returns without publishing.
        let event = fact("run/start", 1, serde_json::json!({}));
        assert!(translate("s1", &event).is_none());
    }
}
