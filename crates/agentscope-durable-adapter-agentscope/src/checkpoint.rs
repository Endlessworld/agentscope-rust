//! Native `AgentState` ↔ durable checkpoint mapping.
//!
//! The durable checkpoint is opaque to the backend: this module owns its shape. A snapshot is
//! encoded as the state's own JSON, so an unchanged state always hashes to the same digest and
//! the backend can tell "same content" from "new content" without knowing what changed
//! ("unchanged" keeps the existing checkpoint position, see `write_checkpoint`).

use agentscope_core::error::Result;
use agentscope_core::journal::{journal_error, JournalEvent};
use agentscope_core::state::AgentState;
use serde_json::Value;

/// The journal event types whose payload carries a native state, matching the journal
/// projection (`state/checkpoint`, a baseline, an explicit restore).
const STATE_EVENTS: [&str; 3] = ["state/checkpoint", "migration/baseline", "state/restored"];

/// Version of the native state a checkpoint carries (a `core::state::AgentState` snapshot).
///
/// Advertised through [`AdapterCapabilities::state_version`](agentscope_durable::harness_adapter::AdapterCapabilities)
/// and frozen into the thread binding, so a checkpoint can be told apart from one written
/// under a different state shape before it is decoded into this one.
pub const STATE_VERSION: &str = "agentscope/state/1";

/// Encodes a state snapshot as the checkpoint value of a commit.
pub(crate) fn encode(state: &AgentState) -> Result<Value> {
    serde_json::to_value(state)
        .map_err(|error| journal_error(format!("Cannot encode agent state: {error}")))
}

/// Decodes a committed checkpoint back into a state.
///
/// `state_version` is the version the checkpoint was written under
/// ([`CheckpointMetadata::state_version`](agentscope_durable::CheckpointMetadata)). A snapshot
/// from another version describes another shape, so decoding it as this one would silently
/// misread it: it is refused instead, naming both versions.
pub(crate) fn decode(value: &Value, state_version: &str) -> Result<AgentState> {
    if state_version != STATE_VERSION {
        return Err(journal_error(format!(
            "Committed checkpoint was written under state version `{state_version}`, \
             which this adapter cannot restore (expected `{STATE_VERSION}`)"
        )));
    }
    let mut state: AgentState = serde_json::from_value(value.clone())
        .map_err(|error| journal_error(format!("Committed checkpoint is not a state: {error}")))?;
    // A snapshot written before the ids were serialized with a default decodes with empty
    // values. Every consumer assumes an id names something (the thread key falls back to it),
    // so mint the missing ones instead of carrying an unnamed state forward.
    if state.session_id.trim().is_empty() || state.reply_id.trim().is_empty() {
        let fresh = AgentState::default();
        if state.session_id.trim().is_empty() {
            state.session_id = fresh.session_id;
        }
        if state.reply_id.trim().is_empty() {
            state.reply_id = fresh.reply_id;
        }
    }
    Ok(state)
}

/// The snapshot a journal batch commits, when the batch carries one.
///
/// The last state-carrying entry wins: it is the newest native state in the batch.
pub(crate) fn from_batch(events: &[JournalEvent]) -> Result<Option<Value>> {
    let mut snapshot = None;
    for event in events {
        if !STATE_EVENTS.contains(&event.event_type.as_str()) {
            continue;
        }
        let Ok(data) = event.data() else {
            continue;
        };
        let Some(json) = data.get("stateJson").and_then(Value::as_str) else {
            continue;
        };
        let state = AgentState::from_json_str(json)
            .map_err(|error| journal_error(format!("Cannot decode journaled state: {error}")))?;
        snapshot = Some(encode(&state)?);
    }
    Ok(snapshot)
}
