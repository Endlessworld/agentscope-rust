//! One Level-2 session event, mirroring the ASDP `SessionEventMsg`.
//!
//! The durable event stream is the canonical conversation history, so message and tool
//! payloads are preserved rather than depending on the optional Level-3 message-query
//! capability. `seq` increases monotonically within a session and is the control plane's
//! idempotency key; it is assigned by the bridge, not by adapters.

use serde::{Deserialize, Serialize};

/// `session_start` event type.
pub const TYPE_SESSION_START: &str = "session_start";
/// `message` event type.
pub const TYPE_MESSAGE: &str = "message";
/// `tool_call` event type.
pub const TYPE_TOOL_CALL: &str = "tool_call";
/// `tool_result` event type.
pub const TYPE_TOOL_RESULT: &str = "tool_result";
/// `session_end` event type.
pub const TYPE_SESSION_END: &str = "session_end";
/// `compaction` event type.
pub const TYPE_COMPACTION: &str = "compaction";

/// `user` role.
pub const ROLE_USER: &str = "user";
/// `assistant` role.
pub const ROLE_ASSISTANT: &str = "assistant";
/// `system` role.
pub const ROLE_SYSTEM: &str = "system";
/// `tool` role.
pub const ROLE_TOOL: &str = "tool";

/// One Level-2 session event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    /// Owning session id.
    pub session_id: String,
    /// Event type (one of the `TYPE_*` constants).
    pub event_type: String,
    /// Monotonic sequence within the session (assigned by the bridge).
    #[serde(default)]
    pub seq: i64,
    /// Wall-clock time the event occurred, epoch millis.
    pub occurred_at: i64,
    /// Message / tool role.
    #[serde(default)]
    pub role: String,
    /// Message content.
    #[serde(default)]
    pub content: String,
    /// Tool name, for `tool_call` / `tool_result`.
    #[serde(default)]
    pub tool_name: String,
    /// Raw tool input (opaque JSON), for `tool_call`.
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Tool output, for `tool_result`.
    #[serde(default)]
    pub tool_output: String,
    /// Input tokens reported by the model call.
    #[serde(default)]
    pub tokens_in: i64,
    /// Output tokens reported by the model call.
    #[serde(default)]
    pub tokens_out: i64,
    /// Wall-clock duration of the observed operation, millis.
    #[serde(default)]
    pub duration_ms: i64,
    /// Framework-private metadata blob (opaque JSON).
    #[serde(default)]
    pub framework_meta: Option<serde_json::Value>,
}

impl SessionEvent {
    /// Starts building an event for `session_id` / `event_type`.
    pub fn builder(session_id: impl Into<String>, event_type: impl Into<String>) -> SessionEventBuilder {
        SessionEventBuilder {
            event: SessionEvent {
                session_id: session_id.into(),
                event_type: event_type.into(),
                seq: 0,
                occurred_at: 0,
                role: String::new(),
                content: String::new(),
                tool_name: String::new(),
                tool_input: None,
                tool_output: String::new(),
                tokens_in: 0,
                tokens_out: 0,
                duration_ms: 0,
                framework_meta: None,
            },
        }
    }

    /// Sets the bridge-assigned sequence.
    pub fn set_seq(&mut self, seq: i64) {
        self.seq = seq;
    }
}

/// Builder for [`SessionEvent`].
#[derive(Debug, Clone)]
pub struct SessionEventBuilder {
    event: SessionEvent,
}

impl SessionEventBuilder {
    /// Message / tool role.
    pub fn role(mut self, v: impl Into<String>) -> Self {
        self.event.role = v.into();
        self
    }

    /// Message content.
    pub fn content(mut self, v: impl Into<String>) -> Self {
        self.event.content = v.into();
        self
    }

    /// Tool name.
    pub fn tool_name(mut self, v: impl Into<String>) -> Self {
        self.event.tool_name = v.into();
        self
    }

    /// Raw tool input.
    pub fn tool_input(mut self, v: serde_json::Value) -> Self {
        self.event.tool_input = Some(v);
        self
    }

    /// Tool output.
    pub fn tool_output(mut self, v: impl Into<String>) -> Self {
        self.event.tool_output = v.into();
        self
    }

    /// Input tokens.
    pub fn tokens_in(mut self, v: i64) -> Self {
        self.event.tokens_in = v.max(0);
        self
    }

    /// Output tokens.
    pub fn tokens_out(mut self, v: i64) -> Self {
        self.event.tokens_out = v.max(0);
        self
    }

    /// Duration in millis.
    pub fn duration_ms(mut self, v: i64) -> Self {
        self.event.duration_ms = v.max(0);
        self
    }

    /// Occurred-at epoch millis.
    pub fn occurred_at(mut self, v: i64) -> Self {
        self.event.occurred_at = v;
        self
    }

    /// Framework metadata.
    pub fn framework_meta(mut self, v: serde_json::Value) -> Self {
        self.event.framework_meta = Some(v);
        self
    }

    /// Builds the event, defaulting `occurred_at` to now and clamping negatives.
    pub fn build(mut self) -> SessionEvent {
        if self.event.occurred_at <= 0 {
            self.event.occurred_at = chrono::Utc::now().timestamp_millis();
        }
        self.event.tokens_in = self.event.tokens_in.max(0);
        self.event.tokens_out = self.event.tokens_out.max(0);
        self.event.duration_ms = self.event.duration_ms.max(0);
        self.event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_defaults_timestamp_and_clamps() {
        let ev = SessionEvent::builder("s1", TYPE_MESSAGE)
            .role(ROLE_USER)
            .content("hi")
            .tokens_in(-5)
            .build();
        assert!(ev.occurred_at > 0);
        assert_eq!(ev.tokens_in, 0);
        assert_eq!(ev.role, ROLE_USER);
    }

    #[test]
    fn serde_roundtrip_camel_case() {
        let ev = SessionEvent::builder("s1", TYPE_TOOL_CALL)
            .tool_name("shell")
            .tool_input(serde_json::json!({ "cmd": "ls" }))
            .build();
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"sessionId\":\"s1\""));
        assert!(json.contains("\"toolName\":\"shell\""));
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }
}
