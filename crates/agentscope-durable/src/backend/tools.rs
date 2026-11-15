//! Run/step facts and tool execution records.

use serde_json::json;

use super::*;

pub(super) fn prepare_run(
    store: &dyn RecordStore,
    lease: &Lease,
    request_id: &str,
) -> Result<RunView> {
    json::text(request_id, "requestId")?;
    with_session(store, &lease.thread.session, |txn| {
        let thread = require_thread(txn, &lease.thread)?;
        let mut turn = require_turn(txn, &lease.turn_id)?;
        let now = txn.now();
        assert_owned(&turn, lease, now)?;
        let run_id = format!(
            "run_{}",
            digest_of(&(&lease.thread, &turn.turn_id, request_id, lease.epoch))?
        );
        // One invocation has exactly one run, and its id is derived from the lease's epoch: a
        // second prepare for the same invocation answers what the first one recorded. Re-deriving
        // the record instead used to overwrite a run that already ended and push its id onto
        // `run_order` a second time, at the cost of rewriting the whole turn.
        if let Some(existing) = turn.runs.get(&run_id) {
            return Ok(existing.clone());
        }
        if let Some(current) = turn.current_run() {
            if current.status == RunStatus::Prepared || current.status == RunStatus::Running {
                return Err(DurableError::conflict("Previous run is still open"));
            }
        }
        let view = RunView {
            thread: lease.thread.clone(),
            turn_id: turn.turn_id.clone(),
            run_id: run_id.clone(),
            request_id: request_id.to_string(),
            owner: lease.owner.clone(),
            epoch: lease.epoch,
            previous_run_id: turn.last_run_id.clone(),
            status: RunStatus::Prepared,
            prepared_at: now,
            started_at: None,
            ended_at: None,
            outcome: None,
            reason: None,
        };
        turn.runs.insert(run_id.clone(), view.clone());
        turn.run_order.push(run_id.clone());
        turn.last_run_id = Some(run_id);
        turn.invocation_count += 1;
        save_turn(txn, &turn)?;
        let _ = thread;
        Ok(view)
    })
}

pub(super) fn run(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    turn_id: &str,
    run_id: &str,
) -> Result<RunView> {
    with_session(store, &thread.session, |txn| {
        require_thread(txn, thread)?;
        let turn = require_turn(txn, turn_id)?;
        turn.runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| DurableError::not_found("Run does not exist"))
    })
}

pub(super) fn runs(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    turn_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<RunPage> {
    let limit = clamp_limit(limit)?;
    with_session(store, &thread.session, |txn| {
        require_thread(txn, thread)?;
        let turn = require_turn(txn, turn_id)?;
        let start = cursor_offset(cursor)?;
        let mut items = Vec::new();
        let mut next = None;
        for (index, run_id) in turn.run_order.iter().enumerate().skip(start) {
            if items.len() == limit {
                next = Some((index - 1).to_string());
                break;
            }
            if let Some(view) = turn.runs.get(run_id) {
                items.push(view.clone());
            }
        }
        Ok(RunPage {
            items,
            cursor: next,
        })
    })
}

pub(super) fn steps(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    turn_id: &str,
    run_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<StepPage> {
    let limit = clamp_limit(limit)?;
    with_session(store, &thread.session, |txn| {
        require_thread(txn, thread)?;
        let turn = require_turn(txn, turn_id)?;
        let start = cursor_offset(cursor)?;
        let mut items = Vec::new();
        let mut next = None;
        let matching: Vec<&StepView> = turn
            .step_order
            .iter()
            .filter_map(|id| turn.steps.get(id))
            .filter(|step| step.run_id == run_id)
            .collect();
        for (index, step) in matching.iter().enumerate().skip(start) {
            if items.len() == limit {
                next = Some((index - 1).to_string());
                break;
            }
            items.push((*step).clone());
        }
        Ok(StepPage {
            items,
            cursor: next,
        })
    })
}

fn cursor_offset(cursor: Option<&str>) -> Result<usize> {
    match cursor {
        None => Ok(0),
        Some(cursor) => cursor
            .parse::<usize>()
            .map(|index| index + 1)
            .map_err(|_| DurableError::invalid("Invalid pagination cursor")),
    }
}

/// Finds a tool execution by identity through the session-wide index.
fn find_tool(txn: &dyn Txn, execution_id: &str) -> Result<(ThreadData, TurnData, String)> {
    let turn_id = load_json::<String>(txn, &execution_part(execution_id))?
        .ok_or_else(|| DurableError::not_found("Execution does not exist"))?;
    let turn = require_turn(txn, &turn_id)?;
    let thread = load_thread(txn, &turn.thread_id)?
        .ok_or_else(|| DurableError::not_found("Thread does not exist"))?;
    Ok((thread, turn, turn_id))
}

pub(super) fn execution(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    execution_id: &str,
) -> Result<ToolExecution> {
    with_session(store, &thread.session, |txn| {
        let (owner, turn, _) = find_tool(txn, execution_id)?;
        if owner.definition.key != *thread {
            return Err(DurableError::not_found(
                "Execution belongs to another thread",
            ));
        }
        turn.tools
            .get(execution_id)
            .map(ToolData::view)
            .ok_or_else(|| DurableError::not_found("Execution does not exist"))
    })
}

pub(super) fn begin_dispatch(
    store: &dyn RecordStore,
    lease: &Lease,
    execution_id: &str,
) -> Result<ToolExecution> {
    with_session(store, &lease.thread.session, |txn| {
        let mut state = require_state(txn)?;
        let thread = require_thread(txn, &lease.thread)?;
        let mut turn = require_turn(txn, &lease.turn_id)?;
        let now = txn.now();
        let before = apply_dispatch(txn, &mut state, &mut turn, lease, execution_id)?;
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        save_state(txn, &state)?;
        Ok(before)
    })
}

/// Marks one registered intent as dispatched inside an open transaction.
///
/// Returns the record as it was before the mark, mirroring the public call.
pub(super) fn apply_dispatch(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    turn: &mut TurnData,
    lease: &Lease,
    execution_id: &str,
) -> Result<ToolExecution> {
    let now = txn.now();
    assert_owned(turn, lease, now)?;
    if turn.cancel || turn.status == TurnStatus::Pausing {
        return Err(DurableError::conflict("Turn is stopping"));
    }
    if turn.failed_epoch == Some(lease.epoch) {
        return Err(DurableError::conflict("Turn ownership already failed"));
    }
    if let Some(recovery) = &turn.recovery {
        if !recovery.retry_allowed() {
            return Err(DurableError::conflict("Recovery has stopped"));
        }
    }
    let Some(tool) = turn.tools.get(execution_id) else {
        return Err(DurableError::not_found("Execution does not exist"));
    };
    if tool.request.turn_id != turn.turn_id {
        return Err(DurableError::not_found(
            "Execution does not belong to this turn",
        ));
    }
    let before = tool.view();
    if let Some(tool) = turn.tools.get_mut(execution_id) {
        if !tool.attempted {
            tool.attempted = true;
            append_event(
                txn,
                state,
                &lease.thread,
                &turn.turn_id,
                tool.request.call.origin.as_ref().map(|o| o.run_id.as_str()),
                tool.request
                    .call
                    .origin
                    .as_ref()
                    .and_then(|o| o.step_id.as_deref()),
                &format!("tool.dispatching.{execution_id}"),
                "tool.dispatching",
                json!({ "executionId": execution_id }),
            )?;
        }
    }
    Ok(before)
}

pub(super) fn accept_result(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    execution_id: &str,
    result: ExecutionResult,
) -> Result<ToolExecution> {
    if result.state == ExecutionState::NotFound {
        return Err(DurableError::invalid(
            "NOT_FOUND is not a recordable execution result",
        ));
    }
    with_session(store, &thread.session, |txn| {
        let mut state = require_state(txn)?;
        let (owner, mut turn, _) = find_tool(txn, execution_id)?;
        if owner.definition.key != *thread {
            return Err(DurableError::not_found(
                "Execution belongs to another thread",
            ));
        }
        let now = txn.now();
        let view = apply_result(txn, &mut state, thread, &mut turn, execution_id, &result)?;
        schedule_thread(txn, &owner, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        save_state(txn, &state)?;
        Ok(view)
    })
}

/// Records one tool result inside an open transaction.
pub(super) fn apply_result(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    thread: &ThreadKey,
    turn: &mut TurnData,
    execution_id: &str,
    result: &ExecutionResult,
) -> Result<ToolExecution> {
    if result.state == ExecutionState::NotFound {
        return Err(DurableError::invalid(
            "NOT_FOUND is not a recordable execution result",
        ));
    }
    let Some(tool) = turn.tools.get_mut(execution_id) else {
        return Err(DurableError::not_found("Execution does not exist"));
    };
    if !tool.attempted && result.state != ExecutionState::Cancelled {
        return Err(DurableError::conflict("Execution has not been dispatched"));
    }
    if let Some(existing) = &tool.result {
        if existing.state.terminal() {
            if json::hash(&serde_json::to_value(existing)?)
                == json::hash(&serde_json::to_value(result)?)
            {
                return Ok(tool.view());
            }
            return Err(DurableError::conflict("Frozen result changed"));
        }
    }
    let terminal = result.state.terminal();
    // The event is keyed by the result it records, not by the clock: two results for one
    // execution inside the same millisecond used to share an event id and the second one was
    // rejected as `Event identity changed`.
    let fingerprint = json::hash(&serde_json::to_value(result)?);
    tool.result = Some(result.clone());
    turn.next_attempt_at = 0;
    let origin = turn
        .tools
        .get(execution_id)
        .and_then(|tool| tool.request.call.origin.clone());
    append_event(
        txn,
        state,
        thread,
        &turn.turn_id,
        origin.as_ref().map(|origin| origin.run_id.as_str()),
        origin.as_ref().and_then(|origin| origin.step_id.as_deref()),
        &format!("tool.result.{execution_id}.{fingerprint}"),
        if terminal {
            "tool.result.accepted"
        } else {
            "tool.progress"
        },
        json!({ "executionId": execution_id, "state": result.state }),
    )?;
    Ok(turn
        .tools
        .get(execution_id)
        .map(ToolData::view)
        .ok_or_else(|| DurableError::not_found("Execution does not exist"))?)
}
