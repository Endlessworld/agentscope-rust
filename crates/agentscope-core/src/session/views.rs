//! Read-only projections over one committed session prefix, mirroring
//! `SessionTurns`, `SessionViews`, `SessionProjection`, `SessionInteractions` and
//! `SessionTranscriptExport`.
//!
//! Inspection never repairs records, never acquires a writer and never invokes a model or a
//! tool: it only reads committed facts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::message::{ContentBlock, Msg, MsgRole, ToolResultBlock};
use crate::state::TaskVerification;

use super::{session_error, SessionEvent, SessionLog};

/// Read-only task and input projection over one committed execution prefix.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionTurns {
    pub turns: Vec<Turn>,
    pub inputs: HashMap<String, Vec<Msg>>,
    pub started: BTreeSet<String>,
    pub applied: BTreeSet<String>,
}

/// One logical request and its execution status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub turn_id: String,
    pub run_id: Option<String>,
    pub status: String,
}

impl Turn {
    /// Whether the turn can no longer be continued.
    ///
    /// A failed run is terminal: there is nothing to resume, so the next submit must be
    /// allowed to start instead of waiting for the failed turn forever.
    ///
    /// An `interrupted` turn is deliberately **not** terminal: its reconciled context can
    /// still be continued with the session's `resume` path (Java `handleInterrupt` parity),
    /// and the journal keeps the partial assistant message for exactly that. The cost is that
    /// an interrupted turn blocks new submissions until someone closes it — a host that gave
    /// up on it must call [`abandon_turn`](super::abandon_turn) (which records unknown tool
    /// outcomes as `interrupted` and writes `turn/cancelled`) rather than wait.
    pub fn ended(&self) -> bool {
        self.status == "completed" || self.status == "cancelled" || self.status == "failed"
    }
}

impl SessionTurns {
    /// The most recently recorded turn, if any.
    pub fn latest(&self) -> Option<&Turn> {
        self.turns.last()
    }

    /// The original input messages of a turn (confirmation answers filtered out on resume).
    pub fn inputs_of(&self, turn_id: &str) -> &[Msg] {
        self.inputs.get(turn_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Projects turns, inputs and inbox state from a committed prefix.
    pub async fn read(log: &dyn SessionLog) -> crate::error::Result<Self> {
        let mut ordered: Vec<String> = Vec::new();
        let mut turns: BTreeMap<String, Turn> = BTreeMap::new();
        let mut inputs: HashMap<String, Vec<Msg>> = HashMap::new();
        let mut started = BTreeSet::new();
        let mut applied = BTreeSet::new();

        let upper = log.head().await?.seq;
        let mut visit = |event: &SessionEvent| -> crate::error::Result<()> {
            let data = event.data()?;
            if event.event_type == "state/restored" {
                ordered.clear();
                turns.clear();
                inputs.clear();
            }
            let turn_id = event.turn_id.clone();
            match event.event_type.as_str() {
                "turn/cancelled" => {
                    if let Some(id) = &turn_id {
                        Self::record_turn(
                            &mut ordered,
                            &mut turns,
                            id,
                            event.execution_run_id.clone(),
                            "cancelled",
                        );
                    }
                }
                "run/start" => {
                    if let Some(id) = &turn_id {
                        Self::record_turn(
                            &mut ordered,
                            &mut turns,
                            id,
                            event.execution_run_id.clone(),
                            "running",
                        );
                    }
                }
                "run/end" => {
                    if let Some(id) = &turn_id {
                        let status = json_str(&data, "status").unwrap_or_else(|| "unknown".into());
                        Self::record_turn(
                            &mut ordered,
                            &mut turns,
                            id,
                            event.execution_run_id.clone(),
                            &status,
                        );
                    }
                }
                "input/received" => {
                    if let Some(id) = &turn_id {
                        let bucket = inputs.entry(id.clone()).or_default();
                        for value in data
                            .get("messages")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default()
                        {
                            let msg: Msg = serde_json::from_value(value)?;
                            if !bucket.iter().any(|existing| existing.id == msg.id) {
                                bucket.push(msg);
                            }
                        }
                    }
                }
                "inbox/started" => {
                    if let Some(command_id) = json_str(&data, "commandId") {
                        started.insert(command_id);
                    }
                }
                "inbox/applied" => {
                    for value in data
                        .get("commandIds")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default()
                    {
                        if let Some(id) = value.as_str() {
                            applied.insert(id.to_string());
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        };
        log.visit(0, upper, &mut visit).await?;

        let turns = ordered
            .into_iter()
            .filter_map(|id| turns.remove(&id))
            .collect();
        Ok(Self {
            turns,
            inputs,
            started,
            applied,
        })
    }

    fn record_turn(
        ordered: &mut Vec<String>,
        turns: &mut BTreeMap<String, Turn>,
        turn_id: &str,
        run_id: Option<String>,
        status: &str,
    ) {
        if !turns.contains_key(turn_id) {
            ordered.push(turn_id.to_string());
        }
        turns.insert(
            turn_id.to_string(),
            Turn {
                turn_id: turn_id.to_string(),
                run_id,
                status: status.to_string(),
            },
        );
    }

    /// Folds one committed event into this projection.
    ///
    /// A caller that already holds the projection at an earlier head can advance it by
    /// visiting only the events committed since then, instead of replaying the whole
    /// journal.
    pub fn apply_event(&mut self, event: &SessionEvent) -> crate::error::Result<()> {
        let data = event.data()?;
        if event.event_type == "state/restored" {
            self.turns.clear();
            self.inputs.clear();
        }
        let turn_id = event.turn_id.clone();
        match event.event_type.as_str() {
            "turn/cancelled" => {
                if let Some(id) = &turn_id {
                    self.apply_turn(id, event.execution_run_id.clone(), "cancelled");
                }
            }
            "run/start" => {
                if let Some(id) = &turn_id {
                    self.apply_turn(id, event.execution_run_id.clone(), "running");
                }
            }
            "run/end" => {
                if let Some(id) = &turn_id {
                    let status = json_str(&data, "status").unwrap_or_else(|| "unknown".into());
                    self.apply_turn(id, event.execution_run_id.clone(), &status);
                }
            }
            "input/received" => {
                if let Some(id) = &turn_id {
                    let bucket = self.inputs.entry(id.clone()).or_default();
                    for value in data
                        .get("messages")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default()
                    {
                        let msg: Msg = serde_json::from_value(value)?;
                        if !bucket.iter().any(|existing| existing.id == msg.id) {
                            bucket.push(msg);
                        }
                    }
                }
            }
            "inbox/started" => {
                if let Some(command_id) = json_str(&data, "commandId") {
                    self.started.insert(command_id);
                }
            }
            "inbox/applied" => {
                for value in data
                    .get("commandIds")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    if let Some(id) = value.as_str() {
                        self.applied.insert(id.to_string());
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_turn(&mut self, turn_id: &str, run_id: Option<String>, status: &str) {
        if let Some(existing) = self.turns.iter_mut().find(|turn| turn.turn_id == turn_id) {
            existing.run_id = run_id;
            existing.status = status.to_string();
            return;
        }
        self.turns.push(Turn {
            turn_id: turn_id.to_string(),
            run_id,
            status: status.to_string(),
        });
    }
}

/// Rebuildable human/history views. Context replacement never erases earlier messages.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub as_of_seq: u64,
    pub messages: Vec<Msg>,
}

/// Rebuilds the committed transcript up to the current head.
pub async fn transcript(log: &dyn SessionLog) -> crate::error::Result<Transcript> {
    let target = log.head().await?.seq;
    let mut ordered: Vec<String> = Vec::new();
    let mut messages: HashMap<String, Msg> = HashMap::new();

    for event in log.scan(0, target).await? {
        match event.event_type.as_str() {
            "migration/baseline" => {
                if let Some(state) = event.data()?.get("stateJson").and_then(|v| v.as_str()) {
                    let restored = crate::state::AgentState::from_json_str(state)?;
                    for msg in restored.context {
                        if !messages.contains_key(&msg.id) {
                            ordered.push(msg.id.clone());
                            messages.insert(msg.id.clone(), msg);
                        }
                    }
                }
            }
            "message/user" | "message/assistant" | "tool/result" | "turn/output" => {
                let data = event.data()?;
                let Some(value) = data.get("message") else {
                    continue;
                };
                let msg: Msg = serde_json::from_value(value.clone())?;
                let is_output = event.event_type == "turn/output";
                if is_output || !messages.contains_key(&msg.id) {
                    if !messages.contains_key(&msg.id) {
                        ordered.push(msg.id.clone());
                    }
                    messages.insert(msg.id.clone(), msg);
                }
            }
            _ => {}
        }
    }

    Ok(Transcript {
        as_of_seq: target,
        messages: ordered
            .into_iter()
            .filter_map(|id| messages.remove(&id))
            .collect(),
    })
}

/// Read-only recovery view. Inspection never repairs records or invokes models/tools.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionProjection {
    pub as_of_seq: u64,
    pub state_json: Option<String>,
    pub uncertain_tool_calls: BTreeSet<String>,
    pub active_runs: BTreeSet<String>,
    /// Committed verification reports, in commit order and deduplicated by [`TaskVerification::id`].
    ///
    /// Java keeps a `Set` here; a `Vec` preserves replay order, which is what recovery needs.
    pub verifications: Vec<TaskVerification>,
}

impl SessionProjection {
    /// Walks a committed prefix and folds state, tool outcomes and live runs.
    pub async fn read(log: &dyn SessionLog) -> crate::error::Result<Self> {
        let upper = log.head().await?.seq;
        let mut cursor = 0;
        let mut state: Option<String> = None;
        let mut uncertain = BTreeSet::new();
        let mut runs = BTreeSet::new();
        let mut results: BTreeMap<String, ToolResultBlock> = BTreeMap::new();
        let mut verifications: Vec<TaskVerification> = Vec::new();
        let mut extensions: Vec<SessionEvent> = Vec::new();

        let mut visit = |event: &SessionEvent| -> crate::error::Result<()> {
            super::SessionEventTypes::validate(event)?;
            let data = event.data()?;
            match event.event_type.as_str() {
                "state/checkpoint" | "migration/baseline" | "state/restored" => {
                    state = data
                        .get("stateJson")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    results.clear();
                    verifications.clear();
                    extensions.clear();
                }
                // A verification report is durable state, not optional telemetry: a payload that
                // cannot be decoded fails the projection instead of being skipped. A missing
                // `verification` key is the one tolerated case (Java guards with `!= null`).
                "verification/result" => {
                    if let Some(value) = data.get("verification") {
                        let report: TaskVerification = serde_json::from_value(value.clone())?;
                        match verifications.iter_mut().find(|known| known.id == report.id) {
                            // Same id committed again: the later fact wins, in place.
                            Some(known) => *known = report,
                            None => verifications.push(report),
                        }
                    }
                }
                "tool/dispatch" => {
                    if let Some(id) = json_str(&data, "toolCallId") {
                        uncertain.insert(id);
                    }
                }
                "tool/result" => {
                    // A persisted tool result is the authoritative outcome, regardless of
                    // whether the action observer managed to write `action/end` (a denied or
                    // plan-rejected call never runs one). It must clear dispatch uncertainty;
                    // otherwise every later open demands reconciliation for a settled call.
                    if let Some(value) = data.get("message") {
                        if let Ok(message) =
                            serde_json::from_value::<crate::message::Msg>(value.clone())
                        {
                            for result in message.tool_result_blocks() {
                                if let Some(id) = result.id.as_deref() {
                                    uncertain.remove(id);
                                }
                            }
                        }
                    }
                }
                // `action/start` is the first durable fact of a tool execution. Treating it as
                // uncertain means a crash between "tool started" and "action/end" is recoverable
                // even when the writer never got to record `tool/dispatch`.
                "action/start" => {
                    if let Some(id) = json_str(&data, "toolCallId") {
                        uncertain.insert(id);
                    }
                }
                "action/end" => {
                    // The attempt ended, so it is no longer *uncertain* — even when the tool
                    // failed before producing a result (`result: null`). Leaving it uncertain
                    // would make every later open of the session demand a reconciliation for a
                    // failure the run already recorded.
                    let id = json_str(&data, "toolCallId").unwrap_or_default();
                    uncertain.remove(&id);
                    // An explicit null is a missing result, not a result to decode: earlier
                    // writers recorded the field unconditionally, and trying to parse it fails
                    // the whole projection (and with it every resume of that session).
                    if let Some(value) = data.get("result").filter(|value| !value.is_null()) {
                        // Early native action payloads omitted the polymorphic discriminator.
                        let mut value = value.clone();
                        if let Some(object) = value.as_object_mut() {
                            object
                                .entry("type")
                                .or_insert_with(|| serde_json::json!("tool_result"));
                        }
                        results.insert(id, serde_json::from_value(value)?);
                    }
                }
                "recovery/applied" => {
                    if let Some(ids) = data.get("interruptedRuns").and_then(|v| v.as_array()) {
                        for id in ids.iter().filter_map(|v| v.as_str()) {
                            runs.remove(id);
                        }
                    }
                    if let Some(ids) = data.get("resolvedToolCallIds").and_then(|v| v.as_array()) {
                        for id in ids.iter().filter_map(|v| v.as_str()) {
                            uncertain.remove(id);
                        }
                    }
                }
                "run/start" => {
                    if let Some(id) = &event.execution_run_id {
                        runs.insert(id.clone());
                    }
                }
                "run/end" => {
                    if let Some(id) = &event.execution_run_id {
                        runs.remove(id);
                    }
                }
                other => {
                    if !super::SessionEventTypes::is_builtin(other) {
                        extensions.push(event.clone());
                    }
                }
            }
            cursor = event.seq;
            Ok(())
        };
        log.visit(0, upper, &mut visit).await?;

        // Realign a recovered state with facts whose effect may be lost in an older
        // checkpoint: committed tool results, verification reports and extension events.
        if let Some(state_json) = &state {
            if !results.is_empty() || !extensions.is_empty() {
                let mut restored = crate::state::AgentState::from_json_str(state_json)?;
                let mut existing: HashSet<String> = HashSet::new();
                for message in &restored.context {
                    for result in message.tool_result_blocks() {
                        if let Some(id) = &result.id {
                            existing.insert(id.clone());
                        }
                    }
                }
                for (id, result) in &results {
                    if existing.contains(id) || result.is_suspended() {
                        continue;
                    }
                    let mut block = result.clone();
                    block.id = Some(id.clone());
                    restored.context.push(
                        crate::message::MsgBuilder::new()
                            .id(format!("recovered_{id}"))
                            .role(MsgRole::Tool)
                            .content(vec![ContentBlock::ToolResult(block)])
                            .build(),
                    );
                }
                for extension in &extensions {
                    super::SessionEventCodecRegistry::default_registry()
                        .apply_extension(&mut restored, extension)?;
                }
                state = Some(restored.to_json()?);
            }
        }

        Ok(Self {
            as_of_seq: cursor,
            state_json: state,
            uncertain_tool_calls: uncertain,
            active_runs: runs,
            verifications,
        })
    }

    /// Restores the projected state, if a checkpoint exists.
    pub fn restore(&self) -> crate::error::Result<Option<crate::state::AgentState>> {
        match &self.state_json {
            Some(json) => Ok(Some(crate::state::AgentState::from_json_str(json)?)),
            None => Ok(None),
        }
    }
}

/// Outstanding typed requests at a committed prefix; callers cannot invent tool outcomes.
///
/// Resolves a tool continuation against committed pending requests.
pub async fn continuation_turn(
    log: &dyn SessionLog,
    request_ids: &BTreeSet<String>,
) -> crate::error::Result<String> {
    if request_ids.is_empty() {
        return Err(session_error("Tool result IDs are required"));
    }
    let requests = pending_interactions(log).await?;
    let mut turn: Option<String> = None;
    for id in request_ids {
        let request = requests
            .get(id)
            .ok_or_else(|| session_error(format!("No pending external execution request: {id}")))?;
        let data = request.data()?;
        if data.get("kind").and_then(|v| v.as_str()) != Some("external_execution") {
            return Err(session_error(format!(
                "No pending external execution request: {id}"
            )));
        }
        let request_turn = request
            .turn_id
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| session_error(format!("Pending request has no logical turn: {id}")))?;
        if let Some(existing) = &turn {
            if existing != request_turn {
                return Err(session_error("Tool results belong to different turns"));
            }
        }
        turn = Some(request_turn.to_string());
    }
    turn.ok_or_else(|| session_error("Tool result IDs are required"))
}

/// The unresolved interaction requests, keyed by request id.
pub async fn pending_interactions(
    log: &dyn SessionLog,
) -> crate::error::Result<BTreeMap<String, SessionEvent>> {
    let upper = log.head().await?.seq;
    let mut result = BTreeMap::new();
    let mut visit = |event: &SessionEvent| -> crate::error::Result<()> {
        apply_pending_event(&mut result, event)
    };
    log.visit(0, upper, &mut visit).await?;
    Ok(result)
}

/// Folds one `interaction/*` event into a pending-request map.
///
/// Callers that already hold the map at an earlier head can advance it incrementally.
pub fn apply_pending_event(
    pending: &mut BTreeMap<String, SessionEvent>,
    event: &SessionEvent,
) -> crate::error::Result<()> {
    let data = event.data()?;
    if event.event_type == "interaction/requested" {
        if let Some(id) = json_str(&data, "requestId") {
            pending.insert(id, event.clone());
        }
    }
    if event.event_type == "interaction/resolved" {
        if let Some(id) = json_str(&data, "requestId") {
            pending.remove(&id);
        }
    }
    Ok(())
}

/// A flat, client-facing transcript entry.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEntry {
    pub id: String,
    pub role: String,
    pub content: String,
    pub timestamp_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
}

/// Read-time export for clients that still consume flat transcript entries. Never writes.
pub fn transcript_entries(transcript: &Transcript) -> Vec<TranscriptEntry> {
    let mut entries = Vec::new();
    for message in &transcript.messages {
        let timestamp = timestamp_millis(&message.timestamp);
        let text = message.text_content();
        let has_tools = message
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse(_) | ContentBlock::ToolResult(_)));
        if !text.trim().is_empty() || !has_tools {
            entries.push(TranscriptEntry {
                id: message.id.clone(),
                role: role_name(message.role).to_string(),
                content: text,
                timestamp_ms: timestamp,
                tool_name: None,
                tool_input: None,
                tool_result: None,
            });
        }
        for (index, block) in message.content.iter().enumerate() {
            let id = format!("{}:{index}", message.id);
            match block {
                ContentBlock::ToolUse(call) => entries.push(TranscriptEntry {
                    id,
                    role: "TOOL".into(),
                    content: String::new(),
                    timestamp_ms: timestamp,
                    tool_name: Some(call.name.clone()),
                    tool_input: Some(serde_json::Value::Object(call.input.clone())),
                    tool_result: None,
                }),
                ContentBlock::ToolResult(result) => {
                    let all_text = result
                        .output
                        .iter()
                        .all(|o| matches!(o, ContentBlock::Text(_)));
                    let output = if all_text {
                        result
                            .output
                            .iter()
                            .filter_map(|o| match o {
                                ContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("")
                    } else {
                        serde_json::to_string(&result.output).unwrap_or_default()
                    };
                    entries.push(TranscriptEntry {
                        id,
                        role: "TOOL".into(),
                        content: String::new(),
                        timestamp_ms: timestamp,
                        tool_name: result.name.clone(),
                        tool_input: None,
                        tool_result: Some(output),
                    });
                }
                _ => {}
            }
        }
    }
    entries
}

/// One JSONL row per transcript entry, mirroring `SessionTranscriptExport.jsonl`.
pub fn transcript_jsonl(transcript: &Transcript) -> String {
    let mut out = String::new();
    for entry in transcript_entries(transcript) {
        let mut row = serde_json::Map::new();
        row.insert("type".into(), serde_json::json!("message"));
        row.insert("id".into(), serde_json::json!(entry.id));
        row.insert("role".into(), serde_json::json!(entry.role));
        row.insert("content".into(), serde_json::json!(entry.content));
        row.insert(
            "timestamp".into(),
            serde_json::json!(entry.timestamp_ms as f64 / 1000.0),
        );
        if let Some(tool) = &entry.tool_name {
            row.insert("toolName".into(), serde_json::json!(tool));
        }
        if let Some(input) = &entry.tool_input {
            row.insert("toolInput".into(), input.clone());
        }
        if let Some(result) = &entry.tool_result {
            row.insert("toolResult".into(), serde_json::json!(result));
        }
        out.push_str(&serde_json::Value::Object(row).to_string());
        out.push('\n');
    }
    out
}

fn role_name(role: MsgRole) -> &'static str {
    match role {
        MsgRole::System => "SYSTEM",
        MsgRole::User => "USER",
        MsgRole::Assistant => "ASSISTANT",
        MsgRole::Tool => "TOOL",
    }
}

fn json_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Parses the SDK timestamp format; unparsable timestamps report `0` like the Java export.
fn timestamp_millis(timestamp: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S%.3f")
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::message::TextBlock;
    use crate::message::ToolResultState;
    use crate::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionLog, Writer,
    };

    /// Renders a text-only block list for transcript/result fixtures.
    fn text_blocks(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(TextBlock::new(text))]
    }

    fn event(
        seq: u64,
        kind: &str,
        run: Option<&str>,
        turn: Option<&str>,
        payload: serde_json::Value,
    ) -> SessionEvent {
        SessionEvent::new(
            format!("e{seq}"),
            seq,
            0,
            kind,
            run.map(str::to_string),
            turn.map(str::to_string),
            true,
            payload.to_string(),
        )
        .unwrap()
    }

    async fn journal() -> (Arc<dyn AtomicSessionStorage>, Arc<dyn SessionLog>, Writer) {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        let log: Arc<dyn SessionLog> = Arc::new(JournalSessionLog::new(Arc::clone(&storage), "s1"));
        let writer = log.acquire("run-1", Duration::from_secs(60)).await.unwrap();
        (storage, log, writer)
    }

    async fn commit(log: &dyn SessionLog, writer: &Writer, batch: &str, events: &[SessionEvent]) {
        let head = log.head().await.unwrap();
        log.commit(writer, batch, head.seq, events).await.unwrap();
    }

    #[tokio::test]
    async fn turns_track_running_interrupted_and_completed_statuses() {
        let (_storage, log, writer) = journal().await;
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "run/start",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({}),
                ),
                event(
                    2,
                    "input/received",
                    None,
                    Some("t1"),
                    serde_json::json!({ "messages": [Msg::user("hello")] }),
                ),
            ],
        )
        .await;

        let turns = SessionTurns::read(&*log).await.unwrap();
        assert_eq!(turns.turns.len(), 1);
        let turn = turns.latest().unwrap();
        assert_eq!(turn.turn_id, "t1");
        assert_eq!(turn.run_id.as_deref(), Some("run-1"));
        assert_eq!(turn.status, "running");
        assert!(!turn.ended());
        assert_eq!(turns.inputs_of("t1").len(), 1);

        commit(
            &*log,
            &writer,
            "b2",
            &[event(
                3,
                "run/end",
                Some("run-1"),
                Some("t1"),
                serde_json::json!({ "status": "interrupted" }),
            )],
        )
        .await;
        let turns = SessionTurns::read(&*log).await.unwrap();
        // Interrupted turns are not terminal: they can still be resumed.
        assert_eq!(turns.latest().unwrap().status, "interrupted");
        assert!(!turns.latest().unwrap().ended());
    }

    #[tokio::test]
    async fn cancelled_turns_are_terminal() {
        let (_storage, log, writer) = journal().await;
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "run/start",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({}),
                ),
                event(2, "turn/cancelled", None, Some("t1"), serde_json::json!({})),
            ],
        )
        .await;
        let turns = SessionTurns::read(&*log).await.unwrap();
        assert!(turns.latest().unwrap().ended());
    }

    #[tokio::test]
    async fn failed_turns_are_terminal() {
        let (_storage, log, writer) = journal().await;
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "run/start",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({}),
                ),
                event(
                    2,
                    "run/end",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "status": "failed" }),
                ),
            ],
        )
        .await;
        let turns = SessionTurns::read(&*log).await.unwrap();
        assert!(turns.latest().unwrap().ended());
    }

    #[tokio::test]
    async fn state_restored_resets_earlier_turns() {
        let (_storage, log, writer) = journal().await;
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "run/start",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({}),
                ),
                event(2, "state/restored", None, None, serde_json::json!({})),
                event(
                    3,
                    "run/start",
                    Some("run-2"),
                    Some("t2"),
                    serde_json::json!({}),
                ),
            ],
        )
        .await;
        let turns = SessionTurns::read(&*log).await.unwrap();
        assert_eq!(turns.turns.len(), 1);
        assert_eq!(turns.latest().unwrap().turn_id, "t2");
    }

    #[tokio::test]
    async fn transcript_keeps_first_message_but_lets_output_replace_it() {
        let (_storage, log, writer) = journal().await;
        let mut assistant = Msg::assistant("partial");
        assistant.id = "a1".into();
        let mut final_msg = Msg::assistant("final answer");
        final_msg.id = "a1".into();
        let mut user = Msg::user("hi");
        user.id = "u1".into();
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "message/user",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "message": user }),
                ),
                event(
                    2,
                    "message/assistant",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "message": assistant }),
                ),
                event(
                    3,
                    "turn/output",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "message": final_msg }),
                ),
            ],
        )
        .await;
        let transcript = transcript(&*log).await.unwrap();
        assert_eq!(transcript.as_of_seq, 3);
        assert_eq!(transcript.messages.len(), 2);
        assert_eq!(transcript.messages[0].id, "u1");
        assert_eq!(transcript.messages[1].text_content(), "final answer");
    }

    #[tokio::test]
    async fn projection_recovers_state_and_flags_uncertain_tool_calls() {
        let (_storage, log, writer) = journal().await;
        let mut state = crate::state::AgentState::builder().session_id("s1").build();
        state.summary = "checkpoint".into();
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "state/checkpoint",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "stateJson": state.to_json().unwrap() }),
                ),
                event(
                    2,
                    "tool/dispatch",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "toolCallId": "call-1" }),
                ),
                event(
                    3,
                    "run/start",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({}),
                ),
            ],
        )
        .await;

        let projection = SessionProjection::read(&*log).await.unwrap();
        assert_eq!(projection.as_of_seq, 3);
        assert!(projection.uncertain_tool_calls.contains("call-1"));
        assert!(projection.active_runs.contains("run-1"));
        assert_eq!(projection.restore().unwrap().unwrap().summary, "checkpoint");
    }

    #[tokio::test]
    async fn action_end_clears_uncertainty_and_replays_the_result() {
        let (_storage, log, writer) = journal().await;
        let state = crate::state::AgentState::builder().session_id("s1").build();
        let result = ToolResultBlock::new(
            "call-1",
            "create_order",
            text_blocks("Order ORD-42 confirmed"),
        );
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "state/checkpoint",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "stateJson": state.to_json().unwrap() }),
                ),
                event(
                    2,
                    "tool/dispatch",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "toolCallId": "call-1" }),
                ),
                event(
                    3,
                    "action/end",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "toolCallId": "call-1", "result": serde_json::to_value(&result).unwrap() }),
                ),
                event(4, "run/end", Some("run-1"), Some("t1"), serde_json::json!({ "status": "completed" })),
            ],
        )
        .await;

        let projection = SessionProjection::read(&*log).await.unwrap();
        assert!(projection.uncertain_tool_calls.is_empty());
        assert!(projection.active_runs.is_empty());
        // The recovered state carries a synthetic tool-result message for the committed result.
        let restored = projection.restore().unwrap().unwrap();
        let recovered: Vec<_> = restored
            .context
            .iter()
            .filter(|m| m.id.starts_with("recovered_call-1"))
            .collect();
        assert_eq!(recovered.len(), 1);
    }

    #[tokio::test]
    async fn action_end_with_a_null_result_does_not_break_the_projection() {
        // Earlier writers recorded the field unconditionally. A null result means "no result
        // captured", not a payload to decode: failing here used to make every resume of the
        // session fail with a JSON error.
        let (_storage, log, writer) = journal().await;
        let state = crate::state::AgentState::builder().session_id("s1").build();
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "state/checkpoint",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "stateJson": state.to_json().unwrap() }),
                ),
                event(
                    2,
                    "action/end",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "toolCallId": "call-1", "result": null }),
                ),
            ],
        )
        .await;

        let projection = SessionProjection::read(&*log).await.unwrap();
        assert_eq!(projection.as_of_seq, 2);
        assert!(projection.uncertain_tool_calls.is_empty());
    }

    #[tokio::test]
    async fn an_action_start_alone_marks_the_tool_uncertain() {
        // A crash between "tool started" and its result must be recoverable even when the
        // writer never recorded `tool/dispatch`: the side effect may already have happened.
        let (_storage, log, writer) = journal().await;
        let state = crate::state::AgentState::builder().session_id("s1").build();
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "state/checkpoint",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "stateJson": state.to_json().unwrap() }),
                ),
                event(
                    2,
                    "action/start",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "toolCallId": "call-crash" }),
                ),
            ],
        )
        .await;

        let projection = SessionProjection::read(&*log).await.unwrap();
        assert!(
            projection.uncertain_tool_calls.contains("call-crash"),
            "{:?}",
            projection.uncertain_tool_calls
        );
    }

    #[tokio::test]
    async fn a_persisted_tool_result_clears_dispatch_uncertainty() {
        // A denied or plan-rejected call writes `tool/result` without a matching `action/end`.
        // It is settled, so a later open must not demand a reconciliation.
        let (_storage, log, writer) = journal().await;
        let state = crate::state::AgentState::builder().session_id("s1").build();
        let result = ToolResultBlock::new(
            "call-denied",
            "write_file",
            text_blocks("Permission denied by rules"),
        )
        .with_state(ToolResultState::Denied);
        let msg = Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(result)])
            .build();
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "state/checkpoint",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "stateJson": state.to_json().unwrap() }),
                ),
                event(
                    2,
                    "tool/dispatch",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "toolCallId": "call-denied" }),
                ),
                event(
                    3,
                    "tool/result",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "message": msg }),
                ),
            ],
        )
        .await;

        let projection = SessionProjection::read(&*log).await.unwrap();
        assert!(
            projection.uncertain_tool_calls.is_empty(),
            "{:?}",
            projection.uncertain_tool_calls
        );
    }

    #[tokio::test]
    async fn recovery_fact_resolves_runs_and_tool_calls() {
        let (_storage, log, writer) = journal().await;
        let state = crate::state::AgentState::builder().session_id("s1").build();
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "state/checkpoint",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "stateJson": state.to_json().unwrap() }),
                ),
                event(2, "run/start", Some("run-1"), Some("t1"), serde_json::json!({})),
                event(3, "tool/dispatch", Some("run-1"), Some("t1"), serde_json::json!({ "toolCallId": "call-9" })),
                event(
                    4,
                    "recovery/applied",
                    Some("run-9"),
                    None,
                    serde_json::json!({ "interruptedRuns": ["run-1"], "resolvedToolCallIds": ["call-9"] }),
                ),
            ],
        )
        .await;
        let projection = SessionProjection::read(&*log).await.unwrap();
        assert!(projection.active_runs.is_empty());
        assert!(projection.uncertain_tool_calls.is_empty());
    }

    #[tokio::test]
    async fn pending_interactions_are_added_and_removed() {
        let (_storage, log, writer) = journal().await;
        commit(
            &*log,
            &writer,
            "b1",
            &[
                event(
                    1,
                    "interaction/requested",
                    Some("run-1"),
                    Some("t1"),
                    serde_json::json!({ "requestId": "r1", "kind": "external_execution", "call": { "type": "tool_use", "id": "r1", "name": "create_order", "input": {} } }),
                ),
            ],
        )
        .await;
        let pending = pending_interactions(&*log).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            continuation_turn(&*log, &BTreeSet::from(["r1".to_string()]))
                .await
                .unwrap(),
            "t1"
        );

        commit(
            &*log,
            &writer,
            "b2",
            &[event(
                2,
                "interaction/resolved",
                Some("run-1"),
                Some("t1"),
                serde_json::json!({ "requestId": "r1", "kind": "external_execution" }),
            )],
        )
        .await;
        assert!(pending_interactions(&*log).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn continuation_requires_external_execution_requests() {
        let (_storage, log, writer) = journal().await;
        commit(
            &*log,
            &writer,
            "b1",
            &[event(
                1,
                "interaction/requested",
                Some("run-1"),
                Some("t1"),
                serde_json::json!({ "requestId": "c1", "kind": "confirmation" }),
            )],
        )
        .await;
        // A confirmation is not an external-execution continuation.
        let err = continuation_turn(&*log, &BTreeSet::from(["c1".to_string()]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("external execution"), "{err}");
        assert!(continuation_turn(&*log, &BTreeSet::new()).await.is_err());
        assert!(
            continuation_turn(&*log, &BTreeSet::from(["nope".to_string()]))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn transcript_export_splits_tool_blocks_into_entries() {
        let mut assistant = Msg::builder()
            .id("a1")
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Text(TextBlock::new("calling")),
                ContentBlock::ToolUse(crate::message::ToolUseBlock {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({ "path": "a.txt" })
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    content: None,
                    metadata: Default::default(),
                    state: Default::default(),
                }),
            ])
            .build();
        assistant.timestamp = "2026-10-20 10:11:12.345".into();
        let tool = Msg::builder()
            .id("t1")
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(ToolResultBlock::new(
                "call-1",
                "read_file",
                text_blocks("file body"),
            ))])
            .build();
        let transcript = Transcript {
            as_of_seq: 2,
            messages: vec![assistant, tool],
        };
        let entries = transcript_entries(&transcript);
        // Text entry, tool-use entry, then the tool-result entry.
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, "ASSISTANT");
        assert_eq!(entries[1].role, "TOOL");
        assert_eq!(entries[1].tool_name.as_deref(), Some("read_file"));
        assert_eq!(entries[2].tool_result.as_deref(), Some("file body"));
        assert!(entries[0].timestamp_ms > 0);

        let jsonl = transcript_jsonl(&transcript);
        let rows: Vec<serde_json::Value> = jsonl
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["toolName"], "read_file");
        assert!(rows[1]["timestamp"].is_f64());
    }

    #[test]
    fn unparsable_timestamp_reports_zero() {
        assert_eq!(timestamp_millis("not-a-time"), 0);
        assert!(timestamp_millis("2026-10-20 10:11:12.345") > 0);
    }
}
