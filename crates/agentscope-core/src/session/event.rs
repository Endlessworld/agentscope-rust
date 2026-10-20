//! Immutable session facts, mirroring `SessionEvent`, `SessionEventTypes` and
//! `SessionEventCodecRegistry`.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use super::session_error;
use crate::state::AgentState;

/// An immutable native fact. Payload is JSON captured at acceptance, never a mutable
/// message reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    pub schema_version: i32,
    pub event_id: String,
    pub seq: u64,
    pub occurred_at: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub required: bool,
    pub payload_json: String,
}

impl SessionEvent {
    /// Creates an event, validating the envelope and that the payload is JSON.
    pub fn new(
        event_id: impl Into<String>,
        seq: u64,
        occurred_at: i64,
        event_type: impl Into<String>,
        execution_run_id: Option<String>,
        turn_id: Option<String>,
        required: bool,
        payload_json: impl Into<String>,
    ) -> crate::error::Result<Self> {
        let event = Self {
            schema_version: 1,
            event_id: event_id.into(),
            seq,
            occurred_at,
            event_type: event_type.into(),
            execution_run_id,
            turn_id,
            required,
            payload_json: payload_json.into(),
        };
        event.validate_envelope()?;
        Ok(event)
    }

    /// Creates an event from a structured payload.
    pub fn from_payload<T: Serialize>(
        event_id: impl Into<String>,
        seq: u64,
        occurred_at: i64,
        event_type: impl Into<String>,
        execution_run_id: Option<String>,
        turn_id: Option<String>,
        required: bool,
        payload: &T,
    ) -> crate::error::Result<Self> {
        Self::new(
            event_id,
            seq,
            occurred_at,
            event_type,
            execution_run_id,
            turn_id,
            required,
            serde_json::to_string(payload)?,
        )
    }

    fn validate_envelope(&self) -> crate::error::Result<()> {
        if self.schema_version != 1 || self.seq < 1 {
            return Err(session_error("Unsupported event envelope"));
        }
        if self.event_id.is_empty() {
            return Err(session_error("Session event requires an id"));
        }
        if self.event_type.is_empty() {
            return Err(session_error("Session event requires a type"));
        }
        serde_json::from_str::<serde_json::Value>(&self.payload_json)?;
        Ok(())
    }

    /// Decodes the payload into a typed value.
    pub fn payload<T: for<'de> Deserialize<'de>>(&self) -> crate::error::Result<T> {
        Ok(serde_json::from_str(&self.payload_json)?)
    }

    /// Decodes the payload as a JSON object (`{}` when the payload is not an object).
    pub fn data(&self) -> crate::error::Result<serde_json::Value> {
        Ok(serde_json::from_str(&self.payload_json)?)
    }
}

/// Native schema v1 event type names.
pub struct SessionEventTypes;

impl SessionEventTypes {
    /// Every built-in event type, in schema order.
    pub const BUILTIN: [&'static str; 57] = [
        "run/start",
        "run/end",
        "run/stop_requested",
        "turn/start",
        "turn/end",
        "turn/resumed",
        "turn/completed",
        "turn/suspended",
        "turn/failed",
        "turn/cancelled",
        "turn/interrupted",
        "turn/output",
        "step/start",
        "step/end",
        "input/received",
        "input/applied",
        "input/discarded",
        "inbox/accepted",
        "inbox/started",
        "inbox/applied",
        "inbox/opened",
        "inbox/closed",
        "inbox/rejected",
        "inbox/handled",
        "message/system",
        "message/user",
        "message/assistant",
        "context/build",
        "request/prepared",
        "model/dispatch",
        "model/end",
        "model/chunk",
        "model/retry",
        "tool/requested",
        "tool/decision",
        "action/start",
        "action/end",
        "tool/dispatch",
        "tool/chunk",
        "tool/result",
        "interaction/requested",
        "interaction/resolved",
        "compaction/start",
        "compaction/end",
        "context/replaced",
        "task/changed",
        "plan/changed",
        "permission/changed",
        "verification/result",
        "subagent/spawned",
        "subagent/completed",
        "subagent/linked",
        "recovery/applied",
        "migration/baseline",
        "presentation/hint",
        "state/checkpoint",
        "state/restored",
    ];

    /// Whether `event_type` is a reserved native type.
    pub fn is_builtin(event_type: &str) -> bool {
        Self::BUILTIN.contains(&event_type)
    }

    /// Validates an event against the default codec registry.
    pub fn validate(event: &SessionEvent) -> crate::error::Result<()> {
        SessionEventCodecRegistry::default_registry().validate(event)
    }
}

/// An explicit extension schema for a non-reserved, namespaced event type.
pub trait SessionEventCodec: Send + Sync {
    /// Validates the payload; an invalid payload must stop recovery.
    fn validate(&self, data: &serde_json::Value) -> crate::error::Result<()>;

    /// Applies the payload to recovered state.
    fn apply(
        &self,
        state: &mut AgentState,
        data: &serde_json::Value,
    ) -> crate::error::Result<()>;
}

/// Registry of extension codecs. Required unknown versions/types always stop recovery.
#[derive(Default)]
pub struct SessionEventCodecRegistry {
    extensions: RwLock<HashMap<String, Arc<dyn SessionEventCodec>>>,
}

impl std::fmt::Debug for SessionEventCodecRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.extensions.read().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("SessionEventCodecRegistry")
            .field("extensions", &count)
            .finish()
    }
}

impl SessionEventCodecRegistry {
    /// The process-wide registry used by recovery.
    pub fn default_registry() -> &'static SessionEventCodecRegistry {
        static DEFAULT: OnceLock<SessionEventCodecRegistry> = OnceLock::new();
        DEFAULT.get_or_init(SessionEventCodecRegistry::default)
    }

    /// Registers a codec for a non-reserved, namespaced event type.
    pub fn register(
        &self,
        event_type: &str,
        codec: Arc<dyn SessionEventCodec>,
    ) -> crate::error::Result<()> {
        if !event_type.contains('/') || SessionEventTypes::is_builtin(event_type) {
            return Err(session_error(
                "Use a non-reserved namespaced event type",
            ));
        }
        let mut guard = self
            .extensions
            .write()
            .map_err(|_| session_error("Codec registry lock poisoned"))?;
        if guard.contains_key(event_type) {
            return Err(session_error(format!(
                "Event codec already registered: {event_type}"
            )));
        }
        guard.insert(event_type.to_string(), codec);
        Ok(())
    }

    /// Whether an extension codec is registered for `event_type`.
    pub fn has(&self, event_type: &str) -> bool {
        self.extensions
            .read()
            .map(|m| m.contains_key(event_type))
            .unwrap_or(false)
    }

    fn get(&self, event_type: &str) -> Option<Arc<dyn SessionEventCodec>> {
        self.extensions
            .read()
            .ok()
            .and_then(|m| m.get(event_type).cloned())
    }

    /// Validates a registered extension payload; unknown *required* types are rejected.
    pub fn validate(&self, event: &SessionEvent) -> crate::error::Result<()> {
        if let Some(codec) = self.get(&event.event_type) {
            return codec.validate(&event.data()?);
        }
        if event.required && !SessionEventTypes::is_builtin(&event.event_type) {
            return Err(session_error(format!(
                "Unknown required event: {}",
                event.event_type
            )));
        }
        Ok(())
    }

    /// Applies a registered extension payload to recovered state.
    pub fn apply_extension(
        &self,
        state: &mut AgentState,
        event: &SessionEvent,
    ) -> crate::error::Result<()> {
        match self.get(&event.event_type) {
            Some(codec) => codec.apply(state, &event.data()?),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(event_type: &str, required: bool) -> SessionEvent {
        SessionEvent::new(
            "e1",
            1,
            0,
            event_type,
            None,
            None,
            required,
            "{}",
        )
        .unwrap()
    }

    #[test]
    fn envelope_round_trips_with_type_key() {
        let ev = event("run/start", true);
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "run/start");
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["payloadJson"], "{}");
        assert!(json.get("turnId").is_none(), "absent ids stay omitted");
        let back: SessionEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn envelope_rejects_bad_schema_seq_and_payload() {
        let mut ev = event("run/start", true);
        ev.schema_version = 2;
        assert!(ev.validate_envelope().is_err());

        let mut ev = event("run/start", true);
        ev.seq = 0;
        assert!(ev.validate_envelope().is_err());

        assert!(SessionEvent::new("e1", 1, 0, "run/start", None, None, true, "{oops").is_err());
    }

    #[test]
    fn builtin_types_include_identity_and_inbox_facts() {
        for name in ["run/start", "turn/completed", "inbox/accepted", "state/checkpoint"] {
            assert!(SessionEventTypes::is_builtin(name), "{name}");
        }
        assert!(!SessionEventTypes::is_builtin("extension/custom"));
    }

    struct EchoCodec;

    impl SessionEventCodec for EchoCodec {
        fn validate(&self, data: &serde_json::Value) -> crate::error::Result<()> {
            if data.get("ok").is_none() {
                return Err(session_error("missing ok"));
            }
            Ok(())
        }

        fn apply(
            &self,
            state: &mut AgentState,
            data: &serde_json::Value,
        ) -> crate::error::Result<()> {
            state.set_summary(format!("applied:{}", data["ok"]));
            Ok(())
        }
    }

    #[test]
    fn extension_codecs_validate_apply_and_reject_reserved_types() {
        let registry = SessionEventCodecRegistry::default();
        registry
            .register("extension/echo", Arc::new(EchoCodec))
            .unwrap();
        // A duplicate registration is rejected.
        assert!(registry
            .register("extension/echo", Arc::new(EchoCodec))
            .is_err());
        // Reserved/unnamespaced types are rejected.
        assert!(registry.register("run/start", Arc::new(EchoCodec)).is_err());
        assert!(registry.register("noslash", Arc::new(EchoCodec)).is_err());

        let good = SessionEvent::new(
            "e2",
            1,
            0,
            "extension/echo",
            None,
            None,
            true,
            "{\"ok\":true}",
        )
        .unwrap();
        registry.validate(&good).unwrap();

        let bad = SessionEvent::new(
            "e3",
            1,
            0,
            "extension/echo",
            None,
            None,
            true,
            "{}",
        )
        .unwrap();
        assert!(registry.validate(&bad).is_err());

        let mut state = AgentState::builder().session_id("s").build();
        registry.apply_extension(&mut state, &good).unwrap();
        assert_eq!(state.summary(), "applied:true");
    }

    #[test]
    fn unknown_required_type_stops_recovery_but_optional_does_not() {
        let registry = SessionEventCodecRegistry::default();
        let required = event("extension/unknown", true);
        assert!(registry.validate(&required).is_err());
        let optional = event("extension/unknown", false);
        registry.validate(&optional).unwrap();
    }
}
