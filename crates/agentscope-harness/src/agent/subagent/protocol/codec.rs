//! Bidirectional codec between internal `AgentEvent`s and stable
//! [`RemoteAgentEvent`] wire DTOs, mirroring
//! `io.agentscope.harness.agent.subagent.protocol.RemoteEventCodec`.

use agentscope_core::event::{AgentEvent, AgentEventType, EventData};
use agentscope_core::message::JsonMap;

use super::event::{RemoteAgentEvent, RemoteEventType, RemotePendingConfirm, RemoteStreamDetail};

/// Metadata key stamped onto decoded events carrying a `taskId`.
pub const METADATA_TASK_ID: &str = "taskId";

fn field_str(data: &EventData, key: &str) -> Option<String> {
    data.fields.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Maps an internal agent event to a protocol DTO (without seq/taskId — those are
/// assigned by the server event bus).
///
/// Every event also carries its full serialization in [`RemoteAgentEvent::payload`],
/// so a client can restore the original instance instead of the lossy flat fields.
pub fn from_agent_event(event: &AgentEvent) -> Option<RemoteAgentEvent> {
    let data = event.data();
    let mut dto = RemoteAgentEvent {
        timestamp: Some(event.created_at().to_string()),
        event_type: Some(event.event_type().wire_name().to_string()),
        payload: serde_json::to_string(event).ok(),
        ..Default::default()
    };
    fill_typed_fields(&mut dto, event, data);
    Some(dto)
}

fn fill_typed_fields(dto: &mut RemoteAgentEvent, event: &AgentEvent, data: &EventData) {
    match event {
        AgentEvent::AgentStart(_) => {
            dto.kind = Some(RemoteEventType::RunStarted);
            dto.agent_id = data
                .source
                .clone()
                .or_else(|| field_str(data, "name"))
                .or_else(|| Some("remote".to_string()));
        }
        AgentEvent::AgentEnd(_) => {
            dto.kind = Some(RemoteEventType::RunFinished);
        }
        AgentEvent::TextBlockDelta(_) => {
            dto.kind = Some(RemoteEventType::TextDelta);
            dto.text = Some(field_str(data, "delta").unwrap_or_default());
        }
        AgentEvent::ThinkingBlockDelta(_) => {
            dto.kind = Some(RemoteEventType::ThinkingDelta);
            dto.text = Some(field_str(data, "delta").unwrap_or_default());
        }
        AgentEvent::ToolCallStart(_) => {
            dto.kind = Some(RemoteEventType::ToolCallStart);
            dto.tool_call_id = field_str(data, "tool_call_id");
            dto.tool_name = field_str(data, "tool_name");
        }
        AgentEvent::ToolCallEnd(_) => {
            dto.kind = Some(RemoteEventType::ToolCallEnd);
            dto.tool_call_id = field_str(data, "tool_call_id");
            dto.tool_name = field_str(data, "tool_name");
        }
        AgentEvent::ToolResultEnd(_) => {
            dto.kind = Some(RemoteEventType::ToolResult);
            dto.tool_call_id = field_str(data, "tool_call_id");
            dto.tool_name = field_str(data, "tool_name");
            dto.status = field_str(data, "state");
        }
        AgentEvent::RequireUserConfirm(_) => {
            dto.kind = Some(RemoteEventType::RequireConfirm);
            dto.pending_confirms = Some(extract_pending_confirms(data));
        }
        _ => {
            dto.kind = Some(RemoteEventType::AgentEvent);
        }
    }
}

fn extract_pending_confirms(data: &EventData) -> Vec<RemotePendingConfirm> {
    let Some(serde_json::Value::Array(blocks)) = data.fields.get("tool_calls") else {
        return Vec::new();
    };
    blocks
        .iter()
        .map(|b| {
            let id = b.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let input = b
                .get("input")
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            RemotePendingConfirm::new(id, name, input)
        })
        .collect()
}

/// Maps a protocol DTO back to an internal `AgentEvent`.
///
/// A `payload` is preferred over the flat fields: it restores the original event with
/// its ids, timestamps and metadata intact, and is the only way to recover events sent
/// as [`RemoteEventType::AgentEvent`]. Servers too old to send one still decode through
/// the per-type mapping.
pub fn to_agent_event(remote: &RemoteAgentEvent) -> Option<AgentEvent> {
    if let Some(event) = remote
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_str::<AgentEvent>(p).ok())
    {
        return Some(stamp_task_id(event, remote));
    }
    let kind = remote.kind?;
    let mapped = match kind {
        RemoteEventType::RunStarted => {
            let name = remote.agent_id.clone().unwrap_or_else(|| "remote".to_string());
            Some(AgentEvent::AgentStart(EventData::default()).with_source(name))
        }
        RemoteEventType::RunFinished => Some(AgentEvent::AgentEnd(EventData::default())),
        RemoteEventType::RunError | RemoteEventType::Status | RemoteEventType::AgentEvent => None,
        RemoteEventType::TextDelta => {
            let mut fields = JsonMap::new();
            fields.insert("delta".into(), serde_json::Value::String(remote.text.clone().unwrap_or_default()));
            Some(AgentEvent::with_fields(AgentEventType::TextBlockDelta, fields))
        }
        RemoteEventType::ThinkingDelta => {
            let mut fields = JsonMap::new();
            fields.insert("delta".into(), serde_json::Value::String(remote.text.clone().unwrap_or_default()));
            Some(AgentEvent::with_fields(AgentEventType::ThinkingBlockDelta, fields))
        }
        RemoteEventType::ToolCallStart => {
            Some(AgentEvent::with_fields(AgentEventType::ToolCallStart, tool_fields(remote)))
        }
        RemoteEventType::ToolCallEnd => {
            Some(AgentEvent::with_fields(AgentEventType::ToolCallEnd, tool_fields(remote)))
        }
        RemoteEventType::ToolResult => {
            let mut fields = tool_fields(remote);
            fields.insert(
                "state".into(),
                serde_json::Value::String(remote.status.clone().unwrap_or_else(|| "SUCCESS".into())),
            );
            Some(AgentEvent::with_fields(AgentEventType::ToolResultEnd, fields))
        }
        RemoteEventType::RequireConfirm => Some(to_require_confirm(remote)),
    };
    mapped.map(|event| stamp_task_id(event, remote))
}

fn tool_fields(remote: &RemoteAgentEvent) -> JsonMap {
    let mut fields = JsonMap::new();
    if let Some(id) = &remote.tool_call_id {
        fields.insert("tool_call_id".into(), serde_json::Value::String(id.clone()));
    }
    if let Some(name) = &remote.tool_name {
        fields.insert("tool_name".into(), serde_json::Value::String(name.clone()));
    }
    fields
}

fn to_require_confirm(remote: &RemoteAgentEvent) -> AgentEvent {
    let blocks: Vec<serde_json::Value> = remote
        .pending_confirms
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let input: serde_json::Value = p
                .tool_input_json
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "id": p.tool_call_id,
                "name": p.tool_name,
                "input": input,
                "state": "ASKING",
            })
        })
        .collect();
    let mut fields = JsonMap::new();
    fields.insert("tool_calls".into(), serde_json::Value::Array(blocks));
    AgentEvent::with_fields(AgentEventType::RequireUserConfirm, fields)
}

fn stamp_task_id(event: AgentEvent, remote: &RemoteAgentEvent) -> AgentEvent {
    match remote.task_id.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(task_id) => event.with_metadata(METADATA_TASK_ID, serde_json::Value::String(task_id.to_string())),
        None => event,
    }
}

/// Whether this event type should be emitted for the given detail level.
pub fn matches_detail(kind: RemoteEventType, detail: Option<&str>) -> bool {
    let level = RemoteStreamDetail::parse(detail);
    match kind {
        RemoteEventType::TextDelta | RemoteEventType::ThinkingDelta => {
            level.includes(RemoteStreamDetail::Full)
        }
        RemoteEventType::AgentEvent => level.includes(RemoteStreamDetail::Verbose),
        _ => true,
    }
}

/// Detail check for a whole DTO.
pub fn matches_detail_event(event: &RemoteAgentEvent, detail: Option<&str>) -> bool {
    event.kind.map(|k| matches_detail(k, detail)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_delta() -> AgentEvent {
        let mut fields = JsonMap::new();
        fields.insert("delta".into(), serde_json::Value::String("hello".into()));
        AgentEvent::with_fields(AgentEventType::TextBlockDelta, fields)
    }

    #[test]
    fn text_delta_has_flat_fields_and_payload() {
        let dto = from_agent_event(&text_delta()).unwrap();
        assert_eq!(dto.kind, Some(RemoteEventType::TextDelta));
        assert_eq!(dto.text.as_deref(), Some("hello"));
        assert_eq!(dto.event_type.as_deref(), Some("TEXT_BLOCK_DELTA"));
        assert!(dto.payload.is_some());
    }

    #[test]
    fn payload_roundtrips_original_event() {
        let original = text_delta();
        let dto = from_agent_event(&original).unwrap();
        let restored = to_agent_event(&dto).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn tool_result_carries_id_name_and_state() {
        let mut fields = JsonMap::new();
        fields.insert("tool_call_id".into(), serde_json::Value::String("c1".into()));
        fields.insert("tool_name".into(), serde_json::Value::String("read_file".into()));
        fields.insert("state".into(), serde_json::Value::String("ERROR".into()));
        let event = AgentEvent::with_fields(AgentEventType::ToolResultEnd, fields);
        let dto = from_agent_event(&event).unwrap();
        assert_eq!(dto.kind, Some(RemoteEventType::ToolResult));
        assert_eq!(dto.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(dto.tool_name.as_deref(), Some("read_file"));
        assert_eq!(dto.status.as_deref(), Some("ERROR"));
    }

    #[test]
    fn flat_decode_without_payload_rebuilds_event() {
        let dto = RemoteAgentEvent {
            kind: Some(RemoteEventType::TextDelta),
            text: Some("hi".into()),
            ..Default::default()
        };
        let event = to_agent_event(&dto).unwrap();
        assert_eq!(event.event_type(), AgentEventType::TextBlockDelta);
        assert_eq!(event.data().fields.get("delta").unwrap(), &serde_json::json!("hi"));
    }

    #[test]
    fn task_id_is_stamped_onto_decoded_event_metadata() {
        let mut fields = JsonMap::new();
        fields.insert("delta".into(), serde_json::Value::String("x".into()));
        let event = AgentEvent::with_fields(AgentEventType::TextBlockDelta, fields);
        let mut dto = from_agent_event(&event).unwrap();
        dto.task_id = Some("task-9".into());
        let decoded = to_agent_event(&dto).unwrap();
        assert_eq!(decoded.metadata().get(METADATA_TASK_ID).unwrap(), "task-9");
    }

    #[test]
    fn unknown_event_falls_back_to_agent_event_type() {
        let event = AgentEvent::with_fields(AgentEventType::ModelCallEnd, JsonMap::new());
        let dto = from_agent_event(&event).unwrap();
        assert_eq!(dto.kind, Some(RemoteEventType::AgentEvent));
    }

    #[test]
    fn require_confirm_decodes_pending_confirms() {
        let mut fields = JsonMap::new();
        fields.insert(
            "tool_calls".into(),
            serde_json::json!([{"id": "c1", "name": "shell", "input": {"cmd": "ls"}}]),
        );
        let event = AgentEvent::with_fields(AgentEventType::RequireUserConfirm, fields);
        let dto = from_agent_event(&event).unwrap();
        assert_eq!(dto.kind, Some(RemoteEventType::RequireConfirm));
        let pending = dto.pending_confirms.as_ref().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tool_call_id.as_deref(), Some("c1"));

        let decoded = to_agent_event(&dto).unwrap();
        assert_eq!(decoded.event_type(), AgentEventType::RequireUserConfirm);
    }

    #[test]
    fn detail_filtering() {
        assert!(matches_detail(RemoteEventType::RunStarted, None));
        assert!(!matches_detail(RemoteEventType::TextDelta, Some("status")));
        assert!(matches_detail(RemoteEventType::TextDelta, Some("full")));
        assert!(!matches_detail(RemoteEventType::AgentEvent, Some("full")));
        assert!(matches_detail(RemoteEventType::AgentEvent, Some("verbose")));
    }

    #[test]
    fn run_started_carries_agent_name() {
        let event = AgentEvent::AgentStart(EventData::default()).with_source("researcher");
        let dto = from_agent_event(&event).unwrap();
        assert_eq!(dto.kind, Some(RemoteEventType::RunStarted));
        assert_eq!(dto.agent_id.as_deref(), Some("researcher"));
    }
}
