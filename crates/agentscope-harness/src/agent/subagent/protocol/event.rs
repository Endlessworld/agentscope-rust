//! Stable Protocol DTOs for remote subagent event streaming, mirroring
//! `io.agentscope.harness.agent.subagent.protocol.*`.

/// Stable wire event types for remote subagent streaming over Agent Protocol.
///
/// These values must remain backward-compatible across versions; unknown types are
/// ignored by older clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RemoteEventType {
    RunStarted,
    RunFinished,
    RunError,
    TextDelta,
    ThinkingDelta,
    ToolCallStart,
    ToolCallEnd,
    ToolResult,
    RequireConfirm,
    Status,
    /// Carries an `AgentEvent` with no dedicated wire type, serialized in
    /// [`RemoteAgentEvent::payload`]. Emitted only at `detail=verbose`.
    AgentEvent,
}

/// A single remote tool call awaiting user confirmation (HITL).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePendingConfirm {
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input_json: Option<String>,
}

impl RemotePendingConfirm {
    pub fn new(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_input_json: Option<String>,
    ) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            tool_name: Some(tool_name.into()),
            tool_input_json,
        }
    }
}

/// User decision for a remote pending tool confirmation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConfirmDecision {
    pub tool_call_id: Option<String>,
    pub approved: bool,
    /// Optional human-readable reason (notably for a denial). Omitted from the
    /// wire when `None`; older peers that do not send the key still deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RemoteConfirmDecision {
    pub fn new(tool_call_id: impl Into<String>, approved: bool) -> Self {
        Self { tool_call_id: Some(tool_call_id.into()), approved, reason: None }
    }

    /// Attaches a reason for the decision (e.g. why the user denied the call).
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Stable Protocol DTO for remote subagent event streaming.
///
/// Fields are optional per event type; unknown fields are ignored for forward
/// compatibility. Besides the flat per-type fields, an event may carry
/// [`payload`](Self::payload) — the source `AgentEvent` serialized in full.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAgentEvent {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seq: i64,
    /// Wire type discriminator (JSON key `type`).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RemoteEventType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_confirms: Option<Vec<RemotePendingConfirm>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Name of the source `AgentEventType`, e.g. `MODEL_CALL_END`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
    /// The source `AgentEvent` as JSON, or `None` for server-synthesized events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// How much of a remote subagent's event stream the caller wants, sent as
/// `context.detail`. Each level is a superset of the previous one. The wire value is
/// the lower-case enum name, matching Java `RemoteStreamDetail#wireValue`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum RemoteStreamDetail {
    /// Lifecycle, tool call boundaries, tool results and confirmation requests.
    Status,
    /// `STATUS` plus text and thinking deltas. The default for a streaming parent.
    Full,
    /// `FULL` plus every remaining `AgentEvent`, forwarded as
    /// [`RemoteEventType::AgentEvent`].
    Verbose,
}

impl RemoteStreamDetail {
    /// Lower-case wire value, e.g. `"verbose"`.
    pub fn wire_value(&self) -> &'static str {
        match self {
            RemoteStreamDetail::Status => "status",
            RemoteStreamDetail::Full => "full",
            RemoteStreamDetail::Verbose => "verbose",
        }
    }

    /// Whether this level includes events requiring at least `required`.
    pub fn includes(&self, required: RemoteStreamDetail) -> bool {
        *self >= required
    }

    /// Parses a wire value, falling back to [`RemoteStreamDetail::Status`] for
    /// null/unknown input.
    pub fn parse(value: Option<&str>) -> RemoteStreamDetail {
        let Some(v) = value.map(str::trim).filter(|s| !s.is_empty()) else {
            return RemoteStreamDetail::Status;
        };
        match v.to_ascii_lowercase().as_str() {
            "full" => RemoteStreamDetail::Full,
            "verbose" => RemoteStreamDetail::Verbose,
            _ => RemoteStreamDetail::Status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_event_type_wire_names() {
        assert_eq!(
            serde_json::to_string(&RemoteEventType::ToolResult).unwrap(),
            "\"TOOL_RESULT\""
        );
        assert_eq!(
            serde_json::from_str::<RemoteEventType>("\"RUN_STARTED\"").unwrap(),
            RemoteEventType::RunStarted
        );
    }

    #[test]
    fn event_serde_uses_camel_case_and_omits_nulls() {
        let mut e = RemoteAgentEvent::default();
        e.kind = Some(RemoteEventType::TextDelta);
        e.tool_call_id = Some("c1".into());
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["type"], "TEXT_DELTA");
        assert_eq!(json["toolCallId"], "c1");
        assert!(json.get("toolName").is_none());
        assert!(json.get("agentId").is_none());
    }

    #[test]
    fn event_roundtrips_from_json() {
        let json = serde_json::json!({
            "seq": 7,
            "type": "TOOL_CALL_END",
            "taskId": "t1",
            "toolCallId": "c1",
            "toolName": "read_file"
        });
        let e: RemoteAgentEvent = serde_json::from_value(json).unwrap();
        assert_eq!(e.seq, 7);
        assert_eq!(e.kind, Some(RemoteEventType::ToolCallEnd));
        assert_eq!(e.task_id.as_deref(), Some("t1"));
        assert_eq!(e.tool_name.as_deref(), Some("read_file"));
    }

    #[test]
    fn stream_detail_parse_and_includes() {
        assert_eq!(RemoteStreamDetail::parse(None), RemoteStreamDetail::Status);
        assert_eq!(RemoteStreamDetail::parse(Some("  FULL ")), RemoteStreamDetail::Full);
        assert_eq!(RemoteStreamDetail::parse(Some("bogus")), RemoteStreamDetail::Status);
        assert!(RemoteStreamDetail::Verbose.includes(RemoteStreamDetail::Full));
        assert!(!RemoteStreamDetail::Status.includes(RemoteStreamDetail::Full));
        assert_eq!(RemoteStreamDetail::Verbose.wire_value(), "verbose");
    }

    #[test]
    fn stream_detail_serializes_as_lowercase_wire_value() {
        assert_eq!(serde_json::to_string(&RemoteStreamDetail::Full).unwrap(), "\"full\"");
        assert_eq!(
            serde_json::from_str::<RemoteStreamDetail>("\"verbose\"").unwrap(),
            RemoteStreamDetail::Verbose
        );
    }

    #[test]
    fn pending_confirm_and_decision_constructors() {
        let p = RemotePendingConfirm::new("c1", "read", Some("{}".into()));
        assert_eq!(p.tool_call_id.as_deref(), Some("c1"));
        let d = RemoteConfirmDecision::new("c1", true);
        assert!(d.approved);
        assert!(d.reason.is_none(), "`new` must not invent a reason");
    }

    /// GAP-13 (a): a decision carrying a reason serializes it under `reason`.
    #[test]
    fn confirm_decision_serializes_reason_when_present() {
        let d = RemoteConfirmDecision::new("c1", false).with_reason("user denied rm -rf");
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["toolCallId"], "c1");
        assert_eq!(json["approved"], false);
        assert_eq!(json["reason"], "user denied rm -rf");
    }

    /// GAP-13 (b): a decision without a reason omits the key entirely.
    #[test]
    fn confirm_decision_omits_reason_when_none() {
        let d = RemoteConfirmDecision::new("c1", true);
        let json = serde_json::to_value(&d).unwrap();
        assert!(
            json.get("reason").is_none(),
            "absent reason must not be serialized: {json}"
        );
    }

    /// GAP-13 (c): JSON from an older peer (no `reason` key) still deserializes.
    #[test]
    fn confirm_decision_deserializes_legacy_json() {
        let d: RemoteConfirmDecision =
            serde_json::from_str(r#"{"toolCallId":"c1","approved":false}"#).unwrap();
        assert_eq!(d.tool_call_id.as_deref(), Some("c1"));
        assert!(!d.approved);
        assert!(d.reason.is_none());

        // And a payload that does carry the reason round-trips it.
        let with_reason: RemoteConfirmDecision =
            serde_json::from_str(r#"{"toolCallId":"c2","approved":false,"reason":"no"}"#)
                .unwrap();
        assert_eq!(with_reason.reason.as_deref(), Some("no"));
    }
}
