//! Tool facts a native journal batch implies.
//!
//! Core names a tool call by its stable `toolCallId` in three native facts: `tool/requested`
//! (the intent), `tool/dispatch` (the attempt) and a `tool/result` message carrying
//! [`ToolResultBlock`](agentscope_core::message::ToolResultBlock)s. The bridge turns those
//! into durable facts so one commit owns the journal entries and the tool records together.
//!
//! A fact that cannot be derived is an error, never a silent skip: a journal entry without
//! its tool record would let a restart re-dispatch a call that already left the runtime.

use std::collections::HashSet;

use agentscope_core::error::Result;
use agentscope_core::journal::{journal_error, JournalEvent};
use agentscope_core::message::{Msg, MsgRole, ToolResultBlock, ToolResultState, ToolUseBlock};
use agentscope_core::state::AgentState;
use agentscope_durable::{ExecutionResult, ExecutionState, ToolCall, ToolResultRecord};
use serde_json::Value;

/// Native journal events carry no executor: a local tool is run by the core toolkit.
pub(crate) const LOCAL_EXECUTOR: &str = "local";

/// Tool facts implied by one journal batch, in the order they must be applied.
#[derive(Default)]
pub(crate) struct ToolFacts {
    pub intents: Vec<ToolCall>,
    pub dispatches: Vec<String>,
    pub results: Vec<ToolResultRecord>,
}

/// The durable identity of a call inside a turn.
pub(crate) fn execution_id(turn_id: &str, call_id: &str) -> String {
    format!("{turn_id}:{call_id}")
}

/// Derives the tool facts of one batch. `turn_id` is the turn the batch is committed into.
pub(crate) fn from_batch(turn_id: &str, events: &[JournalEvent]) -> Result<ToolFacts> {
    let mut facts = ToolFacts::default();
    let mut seen: HashSet<String> = HashSet::new();
    // Calls a provider executed: they never got an intent, so they must never get a result
    // either (see the `tool/result` arm).
    let mut server_calls: HashSet<String> = HashSet::new();
    for event in events {
        let data = event.data().unwrap_or(Value::Null);
        match event.event_type.as_str() {
            // A server tool runs at the provider and is answered inline; core never dispatches
            // it locally, so recording an intent would leave the turn unsettled forever.
            "tool/requested" => {
                let call = decode_call(&data)?;
                if call.is_server_tool() {
                    server_calls.insert(call.id.clone());
                    continue;
                }
                let intent = tool_call(&call)?;
                if seen.insert(intent.call_id.clone()) {
                    facts.intents.push(intent);
                }
            }
            // `action/start` is deliberately ignored: core already records `tool/dispatch`
            // for the same call, and a dispatch mark before its intent would fail the commit.
            "tool/dispatch" => {
                let call_id = call_id(&data)?;
                let id = execution_id(turn_id, &call_id);
                if seen.insert(id.clone()) {
                    facts.dispatches.push(id);
                }
            }
            "tool/result" => {
                let message = decode_message(&data)?;
                for block in message.tool_result_blocks() {
                    let call_id = block.id.as_deref().ok_or_else(|| {
                        journal_error("A tool result without a call id cannot be recorded")
                    })?;
                    // Skip results, not only intents, of a provider-side call: the local turn
                    // never dispatched it, so the backend would reject the whole batch with
                    // `Execution has not been dispatched`. The skip is decided by the call
                    // itself as well as by what this batch saw, because the result block can
                    // arrive in a later batch than the request that named the call.
                    if block.is_server_tool() || server_calls.contains(call_id) {
                        continue;
                    }
                    let id = execution_id(turn_id, call_id);
                    let result = result_of(block);
                    match facts.results.iter_mut().find(|r| r.execution_id == id) {
                        Some(existing) => existing.result = result,
                        None => facts.results.push(ToolResultRecord::new(id, result)),
                    }
                }
            }
            _ => {}
        }
    }
    Ok(facts)
}

/// The durable intent of a native tool call.
pub(crate) fn tool_call(call: &ToolUseBlock) -> Result<ToolCall> {
    let arguments = serde_json::to_value(&call.input)
        .map_err(|error| journal_error(format!("Cannot encode tool arguments: {error}")))?;
    ToolCall::new(
        call.id.clone(),
        LOCAL_EXECUTOR,
        call.name.clone(),
        arguments,
    )
    .map_err(|error| journal_error(format!("Invalid tool call: {error}")))
}

/// The calls the next acting phase would run: the unanswered tool uses of the last assistant
/// message. Mirrors core's private `extract_pending_tool_calls`, including the server-tool
/// filter, so the host logs exactly what core is about to execute.
pub(crate) fn pending_calls(state: &AgentState) -> Vec<ToolUseBlock> {
    let context = state.context();
    let Some(owner) = context.iter().rposition(|m| m.role() == MsgRole::Assistant) else {
        return Vec::new();
    };
    let answered: HashSet<&str> = context[owner + 1..]
        .iter()
        .filter(|m| m.role() == MsgRole::Tool)
        .flat_map(|m| m.tool_result_blocks())
        .filter_map(|result| result.id.as_deref())
        .collect();
    context[owner]
        .tool_use_blocks()
        .filter(|call| !call.is_server_tool())
        .filter(|call| !answered.contains(call.id.as_str()))
        .cloned()
        .collect()
}

fn call_id(data: &Value) -> Result<String> {
    data.get("toolCallId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| journal_error("A tool fact without a call id cannot be recorded"))
}

fn field<'a>(data: &'a Value, key: &str) -> Result<&'a Value> {
    data.get(key)
        .ok_or_else(|| journal_error(format!("Tool fact is missing `{key}`")))
}

fn decode_call(data: &Value) -> Result<ToolUseBlock> {
    serde_json::from_value(field(data, "call")?.clone())
        .map_err(|error| journal_error(format!("Cannot decode the tool call: {error}")))
}

fn decode_message(data: &Value) -> Result<Msg> {
    serde_json::from_value(field(data, "message")?.clone())
        .map_err(|error| journal_error(format!("Cannot decode the tool result message: {error}")))
}

/// Only terminal states reach the three-state record; `NOT_FOUND` is never derived because a
/// native result block always describes a call that happened.
fn result_of(block: &ToolResultBlock) -> ExecutionResult {
    let state = match block.state {
        ToolResultState::Success => ExecutionState::Succeeded,
        ToolResultState::Error => ExecutionState::Failed,
        ToolResultState::Denied => ExecutionState::Cancelled,
        // The effect may or may not have happened.
        ToolResultState::Interrupted => ExecutionState::Unknown,
        ToolResultState::Suspended => ExecutionState::Pending,
        ToolResultState::Running => ExecutionState::Running,
    };
    ExecutionResult {
        state,
        handle: None,
        result: serde_json::to_value(&block.output).unwrap_or(Value::Null),
        error: None,
    }
}
