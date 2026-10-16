//! Conversion of core [`AgentEvent`] into ACP `SessionUpdate` notifications.
//!
//! Mirrors `emitOutput` / `recursiveAgentFlux` streaming logic from the Kotlin
//! `AgiHarnessAgent`: maps text/thinking deltas and tool-call progress into ACP
//! session updates.

use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, ContentChunk, Diff, MessageId, Notice, NoticeSeverity, Plan, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, SessionId, SessionNotification, SessionUpdate, TextContent,
    ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use agentscope_core::event::{AgentEvent, AgentEventType};

/// Infers an ACP [`ToolKind`] from a tool name so clients can pick the right
/// icon / UI treatment for a tool call.
pub(crate) fn tool_kind_for(name: &str) -> ToolKind {
    let n = name.to_ascii_lowercase();
    if n.contains("fetch") || n.contains("web") || n.contains("http") {
        ToolKind::Fetch
    } else if n.contains("read") || n.contains("list") || n.contains("ls") || n.contains("cat")
        || n.contains("glob") || n.contains("grep") || n.contains("search")
    {
        ToolKind::Read
    } else if n.contains("write") || n.contains("edit") || n.contains("patch")
        || n.contains("insert") || n.contains("replace")
    {
        ToolKind::Edit
    } else if n.contains("delete") || n.contains("rm") || n.contains("remove") {
        ToolKind::Delete
    } else if n.contains("move") || n.contains("mv") || n.contains("rename") {
        ToolKind::Move
    } else if n.contains("execute") || n.contains("run") || n.contains("shell")
        || n.contains("bash") || n.contains("command")
    {
        ToolKind::Execute
    } else if n.contains("think") || n.contains("reason") || n.contains("plan") {
        ToolKind::Think
    } else if n.contains("mode") {
        ToolKind::SwitchMode
    } else {
        ToolKind::Edit
    }
}

/// Builds a human-readable tool-call title from the tool name, mirroring how
/// reference agents label tool calls (e.g. `read_file` → "Reading file").
pub(crate) fn human_title(name: &str) -> String {
    let words: Vec<String> = name
        .split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        name.to_string()
    } else {
        words.join(" ")
    }
}

/// Builds the ID-addressed `SessionUpdate::CompactionUpdate` for a completed
/// context compaction (capability-gated on `session.compaction`).
///
/// The summary is sent as a single text `CompactionSummaryChunk` alongside an
/// `in_progress` → `completed` upsert pair, so clients that render the retained
/// summary see it flow in exactly once.
pub fn compaction_notifications(
    session_id: impl Into<SessionId>,
    compaction_id: &str,
    summary: &str,
) -> Vec<SessionNotification> {
    use agent_client_protocol::schema::v1::{
        CompactionStatus, CompactionSummaryChunk, CompactionUpdate,
    };
    let sid = session_id.into();
    let mut updates = vec![
        SessionNotification::new(
            sid.clone(),
            SessionUpdate::CompactionUpdate(CompactionUpdate::new(
                compaction_id.to_string(),
                CompactionStatus::InProgress,
            )),
        ),
    ];
    if !summary.trim().is_empty() {
        updates.push(SessionNotification::new(
            sid.clone(),
            SessionUpdate::CompactionSummaryChunk(CompactionSummaryChunk::new(
                compaction_id.to_string(),
                ContentBlock::Text(TextContent::new(summary.to_string())),
            )),
        ));
    }
    updates.push(SessionNotification::new(
        sid,
        SessionUpdate::CompactionUpdate(CompactionUpdate::new(
            compaction_id.to_string(),
            CompactionStatus::Completed,
        )),
    ));
    updates
}

/// Recognizes the in-band compaction marker emitted by
/// `ConversationCompactionMiddleware` and returns `(agent_id, summary)`.
pub(crate) fn compaction_marker(ev: &AgentEvent) -> Option<(String, String)> {
    if ev.event_type() != AgentEventType::Custom {
        return None;
    }
    let fields = &ev.data().fields;
    if fields.get("kind").and_then(|v| v.as_str()) != Some("compaction") {
        return None;
    }
    let agent_id = fields
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let summary = fields
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Some((agent_id, summary))
}

/// Emits a plain-text agent message chunk update.
///
/// The chunk is tagged with `message_id` so clients can group the streamed
/// deltas of one assistant message; without it many clients refuse to render
/// thoughts / tool calls that belong to the same message.
pub fn message_chunk(
    session_id: impl Into<SessionId>,
    text: String,
    message_id: Option<MessageId>,
) -> SessionNotification {
    let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    if let Some(id) = message_id {
        chunk = chunk.message_id(id);
    }
    SessionNotification::new(session_id, SessionUpdate::AgentMessageChunk(chunk))
}

/// Emits a thinking/reasoning chunk update.
pub fn thought_chunk(
    session_id: impl Into<SessionId>,
    text: String,
    message_id: Option<MessageId>,
) -> SessionNotification {
    let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    if let Some(id) = message_id {
        chunk = chunk.message_id(id);
    }
    SessionNotification::new(session_id, SessionUpdate::AgentThoughtChunk(chunk))
}

/// Builds a capability-gated `SessionUpdate::Notice` (advisory, non-history
/// information such as a failed MCP server or a model-switch failure).
///
/// The protocol only allows notices when the client advertised
/// `session.notices`; when it did not, the caller must fall back to a normal
/// agent message so the information still reaches the user.
pub fn notice(
    session_id: impl Into<SessionId>,
    severity: NoticeSeverity,
    title: impl Into<String>,
    description: Option<String>,
) -> SessionNotification {
    let mut n = Notice::new(severity, title.into());
    if let Some(d) = description {
        n = n.description(d);
    }
    SessionNotification::new(session_id, SessionUpdate::Notice(n))
}

/// Argument keys whose value is a file path. Used to derive `locations` so
/// clients can offer "follow-along" navigation to the files the agent touches.
const PATH_ARG_KEYS: &[&str] = &[
    "path", "file", "file_path", "filePath", "filepath", "filename", "dir", "directory",
    "target", "source",
];

/// Derives ACP `locations` from the tool-call arguments.
pub(crate) fn locations_from(raw_input: &str) -> Vec<ToolCallLocation> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_input) else {
        return Vec::new();
    };
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let line = object
        .get("line")
        .or_else(|| object.get("line_number"))
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let mut locations = Vec::new();
    for key in PATH_ARG_KEYS {
        if let Some(p) = object.get(*key).and_then(|v| v.as_str()) {
            if !p.trim().is_empty() {
                locations.push(ToolCallLocation::new(PathBuf::from(p)).line(line));
            }
        }
    }
    locations
}

/// Wraps a tool's text output as ACP `content` so clients can render it in the
/// tool-call body, not only as a raw JSON blob.
fn content_from(text: &str) -> Vec<ToolCallContent> {
    if text.is_empty() {
        return Vec::new();
    }
    vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
        TextContent::new(text),
    )))]
}

/// Metadata key under which filesystem tools report ACP diffs.
const METADATA_ACP_DIFFS: &str = "acp_diffs";

/// Builds ACP `ToolCallContent::Diff` entries from a tool result's `metadata`
/// (`acp_diffs: [{path, old_text?, new_text}]`), so clients render inline file
/// diffs instead of only the plain-text summary.
fn diffs_from(metadata: Option<&serde_json::Value>) -> Vec<ToolCallContent> {
    let Some(entries) = metadata
        .and_then(|m| m.get(METADATA_ACP_DIFFS))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let path = e.get("path").and_then(|v| v.as_str())?;
            let new_text = e.get("new_text").and_then(|v| v.as_str())?;
            let mut diff = Diff::new(PathBuf::from(path), new_text);
            if let Some(old) = e.get("old_text").and_then(|v| v.as_str()) {
                diff = diff.old_text(old);
            }
            Some(ToolCallContent::Diff(diff))
        })
        .collect()
}

/// Builds an ACP `Plan` session update from a `todo_write` call's arguments.
///
/// Returns `None` when the arguments are not a valid `todo_write` payload, so a
/// malformed call never erases the client's current plan.
fn plan_from_todos(raw_input: Option<&serde_json::Value>) -> Option<Plan> {
    let todos = raw_input?.get("todos")?.as_array()?;
    let entries = todos
        .iter()
        .filter_map(|t| {
            let content = t.get("content")?.as_str()?;
            let status = match t.get("status").and_then(|v| v.as_str()).unwrap_or("pending") {
                "completed" => PlanEntryStatus::Completed,
                "in_progress" => PlanEntryStatus::InProgress,
                _ => PlanEntryStatus::Pending,
            };
            let priority = match t.get("priority").and_then(|v| v.as_str()) {
                Some("high") => PlanEntryPriority::High,
                Some("low") => PlanEntryPriority::Low,
                _ => PlanEntryPriority::Medium,
            };
            Some(PlanEntry::new(content, priority, status))
        })
        .collect::<Vec<_>>();
    Some(Plan::new(entries))
}

/// Builds the `SessionUpdate::Plan` notification for a completed `todo_write`
/// call, when the tool arguments carry a todo list.
fn plan_notification(
    session_id: impl Into<SessionId>,
    name: &str,
    raw_input: Option<&serde_json::Value>,
) -> Option<SessionNotification> {
    if name != "todo_write" {
        return None;
    }
    let plan = plan_from_todos(raw_input)?;
    Some(SessionNotification::new(
        session_id,
        SessionUpdate::Plan(plan),
    ))
}

/// Stable plan id used for the `todo_write`-derived plan. The legacy
/// `SessionUpdate::Plan` there is only ever one plan per session, so a constant
/// id is sufficient to address it through the ID-addressed plan operations.
pub(crate) const TODO_PLAN_ID: &str = "todos";

/// Builds the ID-addressed `SessionUpdate::PlanUpdate` for a completed
/// `todo_write` call (used when the client advertises `plan` capabilities, i.e.
/// it can consume `plan_update` / `plan_removed`).
fn plan_update_notification(
    session_id: impl Into<SessionId>,
    name: &str,
    raw_input: Option<&serde_json::Value>,
) -> Option<SessionNotification> {
    if name != "todo_write" {
        return None;
    }
    let plan = plan_from_todos(raw_input)?;
    use agent_client_protocol::schema::v1::{PlanRemoved, PlanUpdate, PlanUpdateContent};
    // An empty entry list means the todo list was cleared: the ID-addressed
    // protocol removes the plan instead of rendering an empty one.
    if plan.entries.is_empty() {
        return Some(SessionNotification::new(
            session_id,
            SessionUpdate::PlanRemoved(PlanRemoved::new(TODO_PLAN_ID)),
        ));
    }
    Some(SessionNotification::new(
        session_id,
        SessionUpdate::PlanUpdate(PlanUpdate::new(PlanUpdateContent::items(
            TODO_PLAN_ID,
            plan.entries,
        ))),
    ))
}

/// Parses an accumulated string as JSON if possible, else wraps it as a string
/// value. Returns `None` for empty payloads so clients never render `""`.
fn json_value_from(s: String) -> Option<serde_json::Value> {
    if s.is_empty() {
        return None;
    }
    Some(serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s)))
}

/// `Some` only for non-empty collections: ACP updates *overwrite* collections,
/// so sending an empty one would erase values already reported to the client.
pub(crate) fn non_empty<T>(v: Vec<T>) -> Option<Vec<T>> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Builds a `ToolCall` (creation) update.
///
/// Carries every field a client needs to render the call up front: the programmatic
/// `name`, the inferred `kind`, the arguments known so far as `raw_input` and the
/// `locations` derived from them.
pub fn tool_call_start(
    session_id: impl Into<SessionId>,
    id: String,
    name: String,
    kind: ToolKind,
    raw_input: Option<serde_json::Value>,
) -> SessionNotification {
    let mut tc = ToolCall::new(id, human_title(&name))
        .name(name)
        .kind(kind)
        .status(ToolCallStatus::Pending);
    if let Some(v) = raw_input {
        let locations = locations_from(&v.to_string());
        if !locations.is_empty() {
            tc = tc.locations(locations);
        }
        tc = tc.raw_input(v);
    }
    SessionNotification::new(session_id, SessionUpdate::ToolCall(tc))
}

/// Builds a `ToolCallUpdate` (progress/completion) update.
pub fn tool_call_update(
    session_id: impl Into<SessionId>,
    id: String,
    fields: ToolCallUpdateFields,
) -> SessionNotification {
    SessionNotification::new(
        session_id,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields)),
    )
}

/// Builds the `ToolCallUpdate` payload embedded in an ACP
/// `session/request_permission` request, describing the tool call whose
/// execution needs human approval.
pub(crate) fn tool_call_update_for_permission(
    id: String,
    name: &str,
    raw_input: Option<serde_json::Value>,
) -> ToolCallUpdate {
    let mut fields = ToolCallUpdateFields::new()
        .title(human_title(name))
        .kind(tool_kind_for(name))
        .status(ToolCallStatus::Pending)
        .raw_input(raw_input.clone());
    if let Some(v) = &raw_input {
        let locations = locations_from(&v.to_string());
        if !locations.is_empty() {
            fields = fields.locations(locations);
        }
    }
    ToolCallUpdate::new(id, fields)
}

/// Update fields for a tool call whose arguments are (at least partially) known:
/// streams `raw_input` plus the `locations` derived from it.
fn input_fields(name: &str, raw_input: &str) -> ToolCallUpdateFields {
    ToolCallUpdateFields::new()
        .title(human_title(name))
        .kind(tool_kind_for(name))
        .raw_input(json_value_from(raw_input.to_string()))
        .locations(non_empty(locations_from(raw_input)))
}

/// Update fields for the terminal state of a tool call: final `status` plus the
/// arguments and the output (both as `raw_output` and as structured `content`).
///
/// When `metadata` carries ACP diffs (`acp_diffs`), matching `Diff` content is
/// appended so clients render the file change inline.
fn result_fields(
    name: &str,
    status: ToolCallStatus,
    raw_input: &str,
    raw_output: &str,
    metadata: Option<&serde_json::Value>,
) -> ToolCallUpdateFields {
    let mut content = content_from(raw_output);
    content.extend(diffs_from(metadata));
    ToolCallUpdateFields::new()
        .title(human_title(name))
        .kind(tool_kind_for(name))
        .status(status)
        .raw_input(json_value_from(raw_input.to_string()))
        .locations(non_empty(locations_from(raw_input)))
        .raw_output(json_value_from(raw_output.to_string()))
        .content(non_empty(content))
}

/// Accumulated state of a single tool call.
#[derive(Default)]
struct ToolCallAcc {
    name: String,
    input: String,
    output: String,
    /// `false` until the ACP `ToolCall` creation notification has been sent.
    created: bool,
}

/// Stateful mapper that accumulates streamed tool-call inputs (`ToolCallDelta`)
/// and outputs (`ToolResultTextDelta`) per tool call and pushes them as ACP
/// `ToolCall` / `ToolCallUpdate` notifications.
pub struct ToolEventMapper {
    calls: HashMap<String, ToolCallAcc>,
    message_id: Option<MessageId>,
    /// Whether the client advertised plan operations. When true the
    /// `todo_write`-derived plan is sent as `plan_update` (ID-addressed);
    /// otherwise the legacy `SessionUpdate::Plan` is used.
    plan_operations: bool,
}

impl Default for ToolEventMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolEventMapper {
    pub fn new() -> Self {
        Self {
            calls: HashMap::new(),
            message_id: None,
            plan_operations: false,
        }
    }

    /// Selects the ID-addressed plan operations (`plan_update`) instead of the
    /// legacy `SessionUpdate::Plan` for `todo_write`-derived plans.
    pub fn with_plan_operations(mut self, enabled: bool) -> Self {
        self.plan_operations = enabled;
        self
    }

    /// Sets the `message_id` shared by the tool calls of the current turn, so
    /// clients can associate them with the assistant message that requested them.
    pub fn with_message_id(mut self, message_id: Option<MessageId>) -> Self {
        self.message_id = message_id;
        self
    }

    /// Map one core event; returns the notifications to send, or an empty vector
    /// for pure bookkeeping events.
    ///
    /// The lifecycle sent to the client is:
    /// - `ToolCall` creation (`Pending`) as soon as the arguments are known
    /// - `ToolCallUpdate`s streaming `raw_input` / `locations` while the model
    ///   emits arguments
    /// - `ToolCallUpdate` (`InProgress`) when the tool actually starts running
    /// - `ToolCallUpdate` (`Completed` / `Failed`) with `raw_output` + `content`
    ///   (plus `Diff` content for filesystem writes, and a `Plan` update for
    ///   `todo_write`).
    pub fn map(
        &mut self,
        session_id: impl Into<SessionId> + Clone,
        ev: &AgentEvent,
    ) -> Vec<SessionNotification> {
        let sid = session_id.into();
        let data = ev.data();
        let id = data
            .fields
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match ev.event_type() {
            // Creation is deferred until the arguments are known (normally the
            // first `ToolCallDelta`), so clients can render the parameters as
            // soon as the tool call appears instead of waiting for an update.
            AgentEventType::ToolCallStart => {
                let name = data
                    .fields
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                self.calls.entry(id).or_insert_with(|| ToolCallAcc {
                    name,
                    ..Default::default()
                });
                Vec::new()
            }
            AgentEventType::ToolCallDelta => {
                let delta = data.fields.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                let Some(acc) = self.calls.get_mut(&id) else {
                    return Vec::new();
                };
                if delta.is_empty() {
                    return Vec::new();
                }
                acc.input.push_str(delta);
                if !acc.created {
                    acc.created = true;
                    let name = acc.name.clone();
                    let raw_input = json_value_from(acc.input.clone());
                    return vec![tool_call_start(
                        sid,
                        id,
                        name.clone(),
                        tool_kind_for(&name),
                        raw_input,
                    )];
                }
                let (name, input) = (acc.name.clone(), acc.input.clone());
                vec![tool_call_update(sid, id, input_fields(&name, &input))]
            }
            AgentEventType::ToolCallEnd => {
                let Some(acc) = self.calls.get_mut(&id) else {
                    return Vec::new();
                };
                if !acc.created {
                    // A parameterless tool call: no delta ever arrives, so the
                    // creation notification must be sent here.
                    acc.created = true;
                    let name = acc.name.clone();
                    let kind = tool_kind_for(&name);
                    return vec![tool_call_start(sid, id, name, kind, None)];
                }
                let (name, input) = (acc.name.clone(), acc.input.clone());
                vec![tool_call_update(sid, id, input_fields(&name, &input))]
            }
            AgentEventType::ToolResultStart => {
                // The tool is actually executing now.
                let Some(acc) = self.calls.get_mut(&id) else {
                    return Vec::new();
                };
                if !acc.created {
                    acc.created = true;
                    let name = acc.name.clone();
                    let kind = tool_kind_for(&name);
                    return vec![tool_call_start(sid, id, name, kind, None)];
                }
                vec![tool_call_update(
                    sid,
                    id,
                    ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
                )]
            }
            AgentEventType::ToolResultTextDelta => {
                if let Some(d) = data.fields.get("delta").and_then(|v| v.as_str()) {
                    if let Some(acc) = self.calls.get_mut(&id) {
                        acc.output.push_str(d);
                    }
                }
                Vec::new()
            }
            AgentEventType::ToolResultEnd => {
                let state = data
                    .fields
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("SUCCESS");
                let status = match state {
                    "ERROR" | "DENIED" => ToolCallStatus::Failed,
                    _ => ToolCallStatus::Completed,
                };
                let metadata = data.fields.get("metadata");
                let Some(acc) = self.calls.get_mut(&id) else {
                    return Vec::new();
                };
                let (name, input, output) = (
                    acc.name.clone(),
                    acc.input.clone(),
                    acc.output.clone(),
                );
                let raw_input = json_value_from(input.clone());
                self.calls.remove(&id);
                let mut notifications = vec![tool_call_update(
                    sid.clone(),
                    id,
                    result_fields(&name, status, &input, &output, metadata),
                )];
                // `todo_write` also drives the client's structured todo list: emit
                // a `Plan` update built from the tool's own arguments so clients
                // render a real todo widget rather than only the text summary.
                // Clients that advertised plan operations get the ID-addressed
                // `plan_update` form instead.
                let plan = if self.plan_operations {
                    plan_update_notification(sid, &name, raw_input.as_ref())
                } else {
                    plan_notification(sid, &name, raw_input.as_ref())
                };
                if let Some(plan) = plan {
                    notifications.push(plan);
                }
                notifications
            }
            _ => Vec::new(),
        }
    }
}

/// Maps the final agent result into an ACP message chunk so that answers which
/// are *not* streamed as incremental `TextBlockDelta`s are still surfaced to the
/// client (otherwise the turn ends with `end_turn` and no visible output).
///
/// Returns `None` when the reply carries no text.
pub fn map_result_event(
    session_id: impl Into<SessionId>,
    ev: &AgentEvent,
    message_id: Option<MessageId>,
) -> Option<SessionNotification> {
    let sid = session_id.into();
    let reply = ev.data().fields.get("reply")?;
    let msg: agentscope_core::message::Msg = serde_json::from_value(reply.clone()).ok()?;
    let text = msg.text_content();
    if text.trim().is_empty() {
        return None;
    }
    Some(message_chunk(sid, text, message_id))
}

/// Maps a core text/thinking delta event into an ACP message/thought chunk.
pub fn map_delta_event(
    session_id: impl Into<SessionId>,
    ev: &AgentEvent,
    message_id: Option<MessageId>,
) -> Option<SessionNotification> {
    let sid = session_id.into();
    let delta = ev.data().fields.get("delta").and_then(|v| v.as_str()).unwrap_or("");
    if delta.is_empty() {
        return None;
    }
    match ev.event_type() {
        AgentEventType::TextBlockDelta => Some(message_chunk(sid, delta.to_string(), message_id)),
        AgentEventType::ThinkingBlockDelta => Some(thought_chunk(sid, delta.to_string(), message_id)),
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

    fn mid() -> Option<MessageId> {
        Some(MessageId::new("msg-1"))
    }

    #[test]
    fn text_delta_maps_to_message_chunk() {
        let mut fields = serde_json::Map::new();
        fields.insert("delta".to_string(), serde_json::json!("hello"));
        let ev = AgentEvent::with_fields(AgentEventType::TextBlockDelta, fields);
        let notif = map_delta_event(sid(), &ev, mid()).expect("maps to notification");
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
        let notif = map_delta_event(sid(), &ev, mid()).expect("maps to notification");
        assert!(matches!(
            notif.update,
            SessionUpdate::AgentThoughtChunk(_)
        ));
    }

    #[test]
    fn empty_delta_maps_to_none() {
        let ev = AgentEvent::AgentEnd(EventData::default());
        assert!(map_delta_event(sid(), &ev, mid()).is_none());
    }

    #[test]
    fn tool_call_creation_waits_for_arguments_then_carries_them() {
        let mut mapper = ToolEventMapper::new();
        let mut start = serde_json::Map::new();
        start.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
        start.insert("tool_name".to_string(), serde_json::json!("read_file"));
        // Creation is deferred: nothing is sent until the arguments are known,
        // so the client can render the parameters with the tool call itself.
        assert!(mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallStart, start))
            .is_empty());

        let mut delta = serde_json::Map::new();
        delta.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
        delta.insert("delta".to_string(), serde_json::json!("{\"path\":\"a.txt\"}"));
        let notifs = mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallDelta, delta));
        let notif = notifs.first().expect("creation emitted once arguments are known");
        match &notif.update {
            SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.title, "Read File");
                assert_eq!(tc.name.as_deref(), Some("read_file"));
                assert_eq!(tc.status, ToolCallStatus::Pending);
                assert_eq!(tc.raw_input, Some(serde_json::json!({"path": "a.txt"})));
                assert_eq!(tc.locations.len(), 1, "path argument must become a location");
                assert_eq!(tc.locations[0].path, std::path::PathBuf::from("a.txt"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_lifecycle_streams_input_then_output() {
        let mut mapper = ToolEventMapper::new();
        let mut fields = serde_json::Map::new();
        fields.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
        fields.insert("tool_name".to_string(), serde_json::json!("read_file"));
        mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallStart, fields));

        // Arguments arrive as deltas; later ones are streamed as updates.
        for chunk in [r#"{"path":"a.t"#, r#"xt","line":3}"#] {
            let mut delta = serde_json::Map::new();
            delta.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
            delta.insert("delta".to_string(), serde_json::json!(chunk));
            mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallDelta, delta));
        }

        // The model finished emitting arguments: the tool has not run yet.
        let mut end = serde_json::Map::new();
        end.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
        let notifs = mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallEnd, end));
        let notif = notifs.first().expect("maps to tool call update");
        match &notif.update {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.fields.title.as_deref(), Some("Read File"));
                assert_eq!(
                    u.fields.raw_input,
                    Some(serde_json::json!({"path": "a.txt", "line": 3}))
                );
                assert_eq!(u.fields.locations.as_ref().map(|l| l.len()), Some(1));
                assert!(u.fields.status.is_none(), "the tool has not started yet");
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }

        // Execution starts.
        let mut started = serde_json::Map::new();
        started.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
        let notifs = mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolResultStart, started));
        let notif = notifs.first().expect("maps to in-progress update");
        match &notif.update {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.fields.status, Some(ToolCallStatus::InProgress));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }

        // Output is accumulated and attached on completion, together with the
        // arguments (so the final update fully describes the call).
        let mut out = serde_json::Map::new();
        out.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
        out.insert("delta".to_string(), serde_json::json!("file contents"));
        assert!(mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolResultTextDelta, out))
            .is_empty());

        let mut done = serde_json::Map::new();
        done.insert("tool_call_id".to_string(), serde_json::json!("call_1"));
        done.insert("state".to_string(), serde_json::json!("SUCCESS"));
        let notifs = mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolResultEnd, done));
        let notif = notifs.first().expect("maps to completion update");
        match &notif.update {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.fields.status, Some(ToolCallStatus::Completed));
                assert_eq!(
                    u.fields.raw_input,
                    Some(serde_json::json!({"path": "a.txt", "line": 3}))
                );
                assert_eq!(u.fields.raw_output, Some(serde_json::json!("file contents")));
                assert_eq!(u.fields.content.as_ref().map(|c| c.len()), Some(1));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tool_call_id_produces_no_notification() {
        // Tools hidden from the UI (`__` prefix) never emit `ToolCallStart`, so
        // their deltas / results must not create a tool call out of thin air.
        let mut mapper = ToolEventMapper::new();
        let mut delta = serde_json::Map::new();
        delta.insert("tool_call_id".to_string(), serde_json::json!("call_x"));
        delta.insert("delta".to_string(), serde_json::json!("{}"));
        assert!(mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallDelta, delta))
            .is_empty());
    }

    #[test]
    fn edit_result_metadata_becomes_diff_content() {
        let mut mapper = ToolEventMapper::new();
        let mut start = serde_json::Map::new();
        start.insert("tool_call_id".to_string(), serde_json::json!("call_e"));
        start.insert("tool_name".to_string(), serde_json::json!("edit_file"));
        mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallStart, start));
        let mut delta = serde_json::Map::new();
        delta.insert("tool_call_id".to_string(), serde_json::json!("call_e"));
        delta.insert(
            "delta".to_string(),
            serde_json::json!("{\"operations\":[{\"path\":\"a.txt\"}]}"),
        );
        mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallDelta, delta));

        let mut done = serde_json::Map::new();
        done.insert("tool_call_id".to_string(), serde_json::json!("call_e"));
        done.insert("state".to_string(), serde_json::json!("SUCCESS"));
        done.insert(
            "metadata".to_string(),
            serde_json::json!({
                "acp_diffs": [{"path": "a.txt", "old_text": "old\n", "new_text": "new\n"}]
            }),
        );
        let notifs = mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolResultEnd, done));
        let u = match &notifs[0].update {
            SessionUpdate::ToolCallUpdate(u) => u,
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        };
        let diffs: Vec<_> = u
            .fields
            .content
            .as_ref()
            .expect("content present")
            .iter()
            .filter_map(|c| match c {
                ToolCallContent::Diff(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(diffs.len(), 1, "one diff expected");
        assert_eq!(diffs[0].path, std::path::PathBuf::from("a.txt"));
        assert_eq!(diffs[0].old_text.as_deref(), Some("old\n"));
        assert_eq!(diffs[0].new_text, "new\n");
    }

    #[test]
    fn todo_write_emits_plan_update() {
        let mut mapper = ToolEventMapper::new();
        let mut start = serde_json::Map::new();
        start.insert("tool_call_id".to_string(), serde_json::json!("call_t"));
        start.insert("tool_name".to_string(), serde_json::json!("todo_write"));
        mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallStart, start));
        let mut delta = serde_json::Map::new();
        delta.insert("tool_call_id".to_string(), serde_json::json!("call_t"));
        delta.insert(
            "delta".to_string(),
            serde_json::json!(
                "{\"todos\":[{\"content\":\"step one\",\"status\":\"completed\",\"priority\":\"high\"},{\"content\":\"step two\",\"status\":\"in_progress\"}]}"
            ),
        );
        mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallDelta, delta));

        let mut done = serde_json::Map::new();
        done.insert("tool_call_id".to_string(), serde_json::json!("call_t"));
        done.insert("state".to_string(), serde_json::json!("SUCCESS"));
        let notifs = mapper
            .map(sid(), &AgentEvent::with_fields(AgentEventType::ToolResultEnd, done));
        assert_eq!(notifs.len(), 2, "tool update + plan update");
        let plan = match &notifs[1].update {
            SessionUpdate::Plan(plan) => plan,
            other => panic!("expected Plan, got {other:?}"),
        };
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].content, "step one");
        assert_eq!(plan.entries[0].status, PlanEntryStatus::Completed);
        assert_eq!(plan.entries[0].priority, PlanEntryPriority::High);
        assert_eq!(plan.entries[1].status, PlanEntryStatus::InProgress);
    }

    #[test]
    fn tool_kind_inferred_from_name() {
        assert_eq!(tool_kind_for("read_file"), ToolKind::Read);
        assert_eq!(tool_kind_for("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind_for("bash"), ToolKind::Execute);
        assert_eq!(tool_kind_for("web_search"), ToolKind::Fetch);
    }

    #[test]
    fn plan_operations_use_id_addressed_update_when_enabled() {
        let mut mapper = ToolEventMapper::new().with_plan_operations(true);
        let notifs = todo_write_result(&mut mapper, "[{\"content\":\"a\",\"status\":\"pending\"}]");
        assert_eq!(notifs.len(), 2, "tool update + plan update");
        let plan = match &notifs[1].update {
            SessionUpdate::PlanUpdate(update) => update,
            other => panic!("expected PlanUpdate, got {other:?}"),
        };
        match &plan.plan {
            agent_client_protocol::schema::v1::PlanUpdateContent::Items(items) => {
                assert_eq!(items.plan_id.to_string(), TODO_PLAN_ID);
                assert_eq!(items.entries.len(), 1);
            }
            other => panic!("expected itemized plan, got {other:?}"),
        }
    }

    #[test]
    fn empty_todo_list_removes_the_id_addressed_plan() {
        let mut mapper = ToolEventMapper::new().with_plan_operations(true);
        let notifs = todo_write_result(&mut mapper, "[]");
        assert_eq!(notifs.len(), 2);
        match &notifs[1].update {
            SessionUpdate::PlanRemoved(removed) => {
                assert_eq!(removed.plan_id.to_string(), TODO_PLAN_ID);
            }
            other => panic!("expected PlanRemoved, got {other:?}"),
        }
    }

    #[test]
    fn compaction_notifications_go_in_progress_then_completed() {
        let notifs = compaction_notifications(sid(), "compaction-1", "retained summary");
        assert_eq!(notifs.len(), 3, "in_progress + summary + completed");
        use agent_client_protocol::schema::v1::CompactionStatus;
        match &notifs[0].update {
            SessionUpdate::CompactionUpdate(u) => {
                assert_eq!(u.compaction_id.to_string(), "compaction-1");
                assert_eq!(u.status, CompactionStatus::InProgress);
            }
            other => panic!("expected CompactionUpdate, got {other:?}"),
        }
        assert!(matches!(
            &notifs[1].update,
            SessionUpdate::CompactionSummaryChunk(_)
        ));
        match &notifs[2].update {
            SessionUpdate::CompactionUpdate(u) => {
                assert_eq!(u.status, CompactionStatus::Completed);
            }
            other => panic!("expected CompactionUpdate, got {other:?}"),
        }
    }

    #[test]
    fn compaction_marker_is_recognized_and_other_custom_events_are_not() {
        let mut fields = serde_json::Map::new();
        fields.insert("kind".to_string(), serde_json::json!("compaction"));
        fields.insert("agent_id".to_string(), serde_json::json!("a1"));
        fields.insert("summary".to_string(), serde_json::json!("s"));
        let ev = AgentEvent::with_fields(AgentEventType::Custom, fields);
        assert_eq!(compaction_marker(&ev), Some(("a1".to_string(), "s".to_string())));

        let other = AgentEvent::with_fields(AgentEventType::Custom, serde_json::Map::new());
        assert!(compaction_marker(&other).is_none());
        assert!(compaction_marker(&AgentEvent::AgentEnd(EventData::default())).is_none());
    }

    /// Drives a `todo_write` call through the mapper and returns the completion
    /// notifications (tool update first, plan update second).
    fn todo_write_result(
        mapper: &mut ToolEventMapper,
        todos_json: &str,
    ) -> Vec<SessionNotification> {
        let mut start = serde_json::Map::new();
        start.insert("tool_call_id".to_string(), serde_json::json!("call_t"));
        start.insert("tool_name".to_string(), serde_json::json!("todo_write"));
        mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallStart, start));
        let mut delta = serde_json::Map::new();
        delta.insert("tool_call_id".to_string(), serde_json::json!("call_t"));
        delta.insert(
            "delta".to_string(),
            serde_json::json!(format!("{{\"todos\":{todos_json}}}")),
        );
        mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolCallDelta, delta));
        let mut done = serde_json::Map::new();
        done.insert("tool_call_id".to_string(), serde_json::json!("call_t"));
        done.insert("state".to_string(), serde_json::json!("SUCCESS"));
        mapper.map(sid(), &AgentEvent::with_fields(AgentEventType::ToolResultEnd, done))
    }

    #[test]
    fn thought_chunk_carries_message_id() {
        let mut fields = serde_json::Map::new();
        fields.insert("delta".to_string(), serde_json::json!("reasoning"));
        let ev = AgentEvent::with_fields(AgentEventType::ThinkingBlockDelta, fields);
        let notif = map_delta_event(sid(), &ev, mid()).expect("maps to notification");
        match notif.update {
            SessionUpdate::AgentThoughtChunk(chunk) => {
                assert_eq!(chunk.message_id, mid());
            }
            _ => panic!("expected AgentThoughtChunk"),
        }
    }
}
