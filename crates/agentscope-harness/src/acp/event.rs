//! Conversion of core [`AgentEvent`] into ACP `SessionUpdate` notifications.
//!
//! Mirrors `emitOutput` / `recursiveAgentFlux` streaming logic from the Kotlin
//! `AgiHarnessAgent`: maps text/thinking deltas and tool-call progress into ACP
//! session updates.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agentscope_core::event::{AgentEvent, AgentEventType};

/// Emits a plain-text agent message chunk update.
pub fn message_chunk(session_id: impl Into<agent_client_protocol::schema::v1::SessionId>, text: String) -> SessionNotification {
    SessionNotification::new(
        session_id,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    )
}

/// Emits a thinking/reasoning chunk update.
pub fn thought_chunk(session_id: impl Into<agent_client_protocol::schema::v1::SessionId>, text: String) -> SessionNotification {
    SessionNotification::new(
        session_id,
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    )
}

/// Builds a `ToolCall` (creation) update.
pub fn tool_call_start(
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    id: String,
    title: String,
    kind: ToolKind,
) -> SessionNotification {
    SessionNotification::new(
        session_id,
        SessionUpdate::ToolCall(
            ToolCall::new(id, title)
                .kind(kind)
                .status(ToolCallStatus::InProgress),
        ),
    )
}

/// Builds a `ToolCallUpdate` (progress/completion) update.
pub fn tool_call_update(
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    id: String,
    kind: ToolKind,
    status: ToolCallStatus,
) -> SessionNotification {
    SessionNotification::new(
        session_id,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id,
            ToolCallUpdateFields::new().kind(kind).status(status),
        )),
    )
}

/// Maps a core tool-call event into the appropriate ACP session update.
///
/// Returns `None` for events that don't map to a tool-call update.
pub fn map_tool_event(
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    ev: &AgentEvent,
) -> Option<SessionNotification> {
    let sid = session_id.into();
    match ev.event_type() {
        AgentEventType::ToolCallStart => {
            let data = ev.data();
            let id = data.fields.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = data.fields.get("tool_name").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
            Some(tool_call_start(sid.clone(), id, name, ToolKind::Edit))
        }
        AgentEventType::ToolCallEnd => {
            let data = ev.data();
            let id = data.fields.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Some(tool_call_update(sid, id, ToolKind::Edit, ToolCallStatus::Completed))
        }
        _ => None,
    }
}

/// Maps the final agent result into an ACP message chunk so that answers which
/// are *not* streamed as incremental `TextBlockDelta`s are still surfaced to the
/// client (otherwise the turn ends with `end_turn` and no visible output).
///
/// Returns `None` when the reply carries no text.
pub fn map_result_event(
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    ev: &AgentEvent,
) -> Option<SessionNotification> {
    let sid = session_id.into();
    let reply = ev.data().fields.get("reply")?;
    let msg: agentscope_core::message::Msg = serde_json::from_value(reply.clone()).ok()?;
    let text = msg.text_content();
    if text.trim().is_empty() {
        return None;
    }
    Some(message_chunk(sid, text))
}

/// Maps a core text/thinking delta event into an ACP message/thought chunk.
pub fn map_delta_event(
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    ev: &AgentEvent,
) -> Option<SessionNotification> {
    let sid = session_id.into();
    let delta = ev.data().fields.get("delta").and_then(|v| v.as_str()).unwrap_or("");
    if delta.is_empty() {
        return None;
    }
    match ev.event_type() {
        AgentEventType::TextBlockDelta => Some(message_chunk(sid, delta.to_string())),
        AgentEventType::ThinkingBlockDelta => Some(thought_chunk(sid, delta.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{SessionId, SessionUpdate};
    use agentscope_core::event::{AgentEvent, EventData};

    fn sid() -> SessionId {
        SessionId::new("s1")
    }

    #[test]
    fn text_delta_maps_to_message_chunk() {
        let mut fields = serde_json::Map::new();
        fields.insert("delta".to_string(), serde_json::json!("hello"));
        let ev = AgentEvent::with_fields(AgentEventType::TextBlockDelta, fields);
        let notif = map_delta_event(sid(), &ev).expect("maps to notification");
        assert!(matches!(
            notif.update,
            SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn thinking_delta_maps_to_thought_chunk() {
        let mut fields = serde_json::Map::new();
        fields.insert("delta".to_string(), serde_json::json!("reasoning"));
        let ev = AgentEvent::with_fields(AgentEventType::ThinkingBlockDelta, fields);
        let notif = map_delta_event(sid(), &ev).expect("maps to notification");
        assert!(matches!(
            notif.update,
            SessionUpdate::AgentThoughtChunk(_)
        ));
    }

    #[test]
    fn empty_delta_maps_to_none() {
        let ev = AgentEvent::AgentEnd(EventData::default());
        assert!(map_delta_event(sid(), &ev).is_none());
    }

    #[test]
    fn tool_call_start_maps_to_tool_call() {
        let mut fields = serde_json::Map::new();
        fields.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
        fields.insert("tool_name".to_string(), serde_json::json!("read_file"));
        let ev = AgentEvent::with_fields(AgentEventType::ToolCallStart, fields);
        let notif = map_tool_event(sid(), &ev).expect("maps to tool call");
        assert!(matches!(notif.update, SessionUpdate::ToolCall(_)));
    }
}
