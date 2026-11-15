//! Model facts a native journal batch implies.
//!
//! The recorded model boundary already journals every call it sees: `model/dispatch` when the
//! request leaves the runtime and `model/end` with the model, status, purpose and token usage.
//! Those two facts are the only place a real provider call is observable, so the bridge derives
//! the durable [`ModelCallChange`]s from them — in the **same** transaction that carries the
//! journal entries, so a model call costs no extra commit and is never recorded per tick.
//!
//! Nothing here copies the request body: when the host's `RecordingPolicy` records
//! `request/prepared`, that payload stays the journal's fact and the durable call refers to it
//! by the same `modelCallId`.

use agentscope_core::error::Result;
use agentscope_core::journal::JournalEvent;
use agentscope_core::message::ChatUsage;
use agentscope_durable::{ModelCallChange, ModelStatus, TokenUsage};
use serde_json::Value;

/// Derives the model calls one batch observed, in dispatch order.
///
/// A batch that carries no model fact yields nothing: a tick that only settles a turn writes no
/// model record.
pub(crate) fn from_batch(events: &[JournalEvent]) -> Result<Vec<ModelCallChange>> {
    let mut changes = Vec::new();
    // The step a call belongs to is only named by the batch that opens it: `prepare_model`
    // flushes right after the loop opened its step, so a dispatch normally carries its
    // iteration. A call whose ending arrives in a later batch keeps what its dispatch recorded.
    let mut step: Option<(String, i32)> = None;
    for event in events {
        let data = event.data().unwrap_or(Value::Null);
        match event.event_type.as_str() {
            "step/start" | "step/resumed" => {
                let id = data
                    .get("stepId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                let iteration = data
                    .get("iteration")
                    .and_then(Value::as_i64)
                    .map(|value| value as i32);
                step = id.zip(iteration);
            }
            "model/dispatch" => {
                let Some(call_id) = call_id(&data) else {
                    continue;
                };
                changes.push(ModelCallChange {
                    model_call_id: call_id.to_string(),
                    status: ModelStatus::Running,
                    model: None,
                    purpose: None,
                    iteration: step.as_ref().map(|(_, iteration)| *iteration),
                    step_id: step.as_ref().map(|(id, _)| id.clone()),
                    provider: None,
                    usage: None,
                    usage_complete: false,
                    error_type: None,
                });
            }
            "model/end" => {
                let Some(call_id) = call_id(&data) else {
                    continue;
                };
                let status = match data.get("status").and_then(Value::as_str) {
                    Some("completed") => ModelStatus::Completed,
                    Some("cancelled") => ModelStatus::Cancelled,
                    // A provider failure and anything unrecognised are both a failed call; the
                    // journal never records a call that is still running as ended.
                    _ => ModelStatus::Failed,
                };
                let model = text(&data, "model");
                let usage = usage_of(data.get("usage"));
                changes.push(ModelCallChange {
                    model_call_id: call_id.to_string(),
                    status,
                    // The model name is only journaled by the ending: the dispatch fact carries
                    // the call identity alone.
                    provider: model.clone(),
                    model,
                    purpose: text(&data, "purpose"),
                    iteration: None,
                    step_id: None,
                    usage_complete: status == ModelStatus::Completed && usage.is_some(),
                    usage,
                    error_type: text(&data, "errorType"),
                });
            }
            _ => {}
        }
    }
    Ok(changes)
}

/// The call id, or `None` for a fact that cannot name its call (nothing to record it against).
fn call_id(data: &Value) -> Option<&str> {
    data.get("modelCallId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn text(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The known token counts of one call.
///
/// Only the two top-level counts are kept: the provider's subset details (cached, reasoning,
/// tool-use prompt) are already journaled with `model/end`, and repeating them on every call is
/// write amplification for a number nobody bills against separately.
fn usage_of(value: Option<&Value>) -> Option<TokenUsage> {
    let usage = serde_json::from_value::<ChatUsage>(value?.clone()).ok()?;
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return None;
    }
    Some(TokenUsage {
        input_tokens: Some(usage.input_tokens as i64),
        output_tokens: Some(usage.output_tokens as i64),
        details: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_durable::ModelStatus;

    fn event(seq: u64, event_type: &str, payload: Value) -> JournalEvent {
        JournalEvent::new(
            format!("e{seq}"),
            seq,
            1_700_000_000_000 + seq as i64,
            event_type,
            None,
            Some("turn-1".to_string()),
            false,
            payload.to_string(),
        )
        .expect("event")
    }

    fn dispatch(seq: u64) -> JournalEvent {
        event(
            seq,
            "model/dispatch",
            serde_json::json!({ "modelCallId": "c1", "attemptId": "c1" }),
        )
    }

    fn ending(seq: u64) -> JournalEvent {
        event(
            seq,
            "model/end",
            serde_json::json!({
                "modelCallId": "c1",
                "status": "completed",
                "model": "scripted",
                "purpose": "REASONING",
                "usage": { "inputTokens": 7, "outputTokens": 9 },
            }),
        )
    }

    /// The request body is the host's business (`RecordingPolicy`): whether the journal kept it
    /// or not, the derived facts — and therefore the bytes stored for a call — are the same.
    #[test]
    fn the_request_body_never_reaches_the_derived_facts() {
        let without = from_batch(&[dispatch(1), ending(2)]).expect("facts");
        let with = from_batch(&[
            event(
                1,
                "request/prepared",
                serde_json::json!({
                    "modelCallId": "c1",
                    "purpose": "REASONING",
                    "model": "scripted",
                    "messages": [{ "role": "user", "content": [] }],
                    "tools": [],
                    "options": {},
                }),
            ),
            dispatch(2),
            ending(3),
        ])
        .expect("facts");
        assert_eq!(without, with, "the body is not an input to the facts");
        assert_eq!(with.len(), 2, "a dispatch and its ending: {with:?}");
        assert_eq!(with[1].status, ModelStatus::Completed);
        assert_eq!(with[1].model.as_deref(), Some("scripted"));
        let usage = with[1].usage.as_ref().expect("usage");
        assert_eq!(usage.input_tokens, Some(7));
        assert_eq!(usage.output_tokens, Some(9));
    }
}
