//! The fenced commit: checkpoint, effects, events and turn state in one transaction.

use serde_json::json;

use super::tools::{apply_dispatch, apply_result};
use super::turn::abandon_runs;
use super::*;
use crate::backend::compact;

pub(super) fn commit(
    store: &dyn RecordStore,
    lease: &Lease,
    command: Commit,
) -> Result<CommitReceipt> {
    command.validate()?;
    let digest = digest_of(&command)?;
    // Measured before the transaction: the store answers "how big is this scope", the backend
    // decides what may be dropped. A store that cannot measure (in memory, remote KV) never
    // triggers this and keeps every fact.
    let scope_bytes = store.scope_bytes(&lease.thread.session.storage_key())?;
    with_session(store, &lease.thread.session, |txn| {
        let mut state = require_state(txn)?;
        let mut thread = require_thread(txn, &lease.thread)?;
        let mut turn = require_turn(txn, &lease.turn_id)?;
        if turn.thread_id != lease.thread.thread_id {
            return Err(DurableError::not_found(
                "Turn does not belong to this thread",
            ));
        }
        // A committed replay must answer even for a writer that already lost the lease.
        if let Some(existing) = turn.receipts.get(&command.commit_id) {
            if !digest_matches(&existing.digest, &command)? {
                return Err(DurableError::conflict("Commit content changed"));
            }
            return Ok(existing.clone());
        }
        let now = txn.now();
        assert_owned(&turn, lease, now)?;
        // A compaction cannot see how big the document it trimmed will be, so the commit after it
        // records the size it wrote — this one, when the last commit did not trim. A concurrent
        // writer may have grown the scope in between, which only makes the recorded size larger
        // and the next trim later: conservative, never wrong about what is safe to drop.
        if state.compact_pending {
            if let Some(bytes) = scope_bytes {
                state.compacted_bytes = bytes;
                state.compact_pending = false;
            }
        }
        let trim = scope_bytes
            .is_some_and(|bytes| compact::worth_compacting(bytes, state.compacted_bytes));
        apply_journal(txn, &mut thread, &command)?;
        if command.expected_revision != turn.revision {
            return Err(DurableError::conflict("Revision changed"));
        }
        if let Some(run) = &command.run {
            apply_run(&mut turn, run, lease, now)?;
        }
        for step in &command.steps {
            apply_step(&mut turn, step, now)?;
        }
        apply_model_calls(&mut turn, &command.model_calls, now)?;
        if !command.applied_results.is_empty() && command.checkpoint.is_null() {
            return Err(DurableError::invalid(
                "Result application requires a native checkpoint",
            ));
        }
        apply_tools(txn, &mut state, &mut turn, lease, &command, now)?;
        for execution_id in &command.dispatches {
            apply_dispatch(txn, &mut state, &mut turn, lease, execution_id)?;
        }
        for record in &command.results {
            // A result for a call this turn keeps no execution record of: when it is the
            // `interrupted` settlement of a call the journal dispatched but this turn never
            // recorded (an explicit reconciliation of an unknown outcome), the effect may or may
            // not have happened and is never replayed, so the journal's settlement stands and
            // the commit proceeds — refusing it here is what made such a session refuse to open
            // at all. Any other state for an unknown execution is an inconsistency between the
            // journal and the durable facts, and still fails the batch.
            if record.result.state == ExecutionState::Unknown
                && !turn.tools.contains_key(&record.execution_id)
            {
                continue;
            }
            apply_result(
                txn,
                &mut state,
                &lease.thread,
                &mut turn,
                &record.execution_id,
                &record.result,
            )?;
        }
        apply_results(txn, &mut state, &mut turn, lease, &command)?;
        apply_consumed(txn, &mut state, &mut turn, lease, &command)?;
        let previous = turn.status;
        let next = derive_status(&turn, &command)?;
        write_checkpoint(txn, &mut thread, &command, next)?;
        turn.revision += 1;
        turn.status = next;
        turn.waiting = command.waiting.clone();
        turn.output = command.output.clone();
        turn.error = command.error.clone();
        for event in &command.events {
            reject_reserved(&event.event_type)?;
            append_event(
                txn,
                &mut state,
                &lease.thread,
                &turn.turn_id,
                event.run_id.as_deref(),
                event.step_id.as_deref(),
                &event.event_id,
                &event.event_type,
                event.data.clone(),
            )?;
        }
        if previous == TurnStatus::Waiting && next == TurnStatus::Running {
            append_event(
                txn,
                &mut state,
                &lease.thread,
                &turn.turn_id,
                None,
                None,
                &format!("turn.resumed.{}.{}", turn.turn_id, turn.revision),
                "turn.resumed",
                json!({}),
            )?;
        }
        if next.terminal() || matches!(next, TurnStatus::Waiting | TurnStatus::Paused) {
            abandon_runs(&mut turn, RunStatus::Stopped, "turn_settled", now);
            turn.owner = None;
            turn.expires_at = 0;
        }
        append_event(
            txn,
            &mut state,
            &lease.thread,
            &turn.turn_id,
            None,
            None,
            &format!("turn.status.{}.{}", turn.turn_id, turn.revision),
            status_event(next),
            json!({ "status": next, "revision": turn.revision }),
        )?;
        let receipt = CommitReceipt {
            commit_id: command.commit_id.clone(),
            digest,
            revision: turn.revision,
            cursor: cursor_of(&state, state.sequence),
        };
        turn.receipts
            .insert(command.commit_id.clone(), receipt.clone());
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        // A terminal turn advances the materialized prefix. The thread record is written by this
        // same commit, so materializing it costs no extra transaction and no extra document.
        if next.terminal() {
            advance_settled(txn, &mut thread)?;
        }
        // The scope is one document that this commit is rewriting anyway, so trimming it here
        // costs no extra write: the bytes it drops are bytes this write would otherwise carry.
        if trim {
            // Whatever the trim frees (nothing, when the scope cannot shrink), the resulting size
            // is measured by the next commit: `compact_pending` says the recorded size is stale.
            state.compact_pending = true;
            match compact::run(txn, &mut state, &thread, compact::EVENT_RETENTION) {
                Ok(outcome) if !outcome.is_empty() => tracing::debug!(
                    session = %lease.thread.session.storage_key(),
                    payloads = outcome.payloads,
                    checkpoints = outcome.checkpoints,
                    events = outcome.events,
                    "compacted the session scope"
                ),
                // A compaction that fails is not a failed commit: every fact it drops is
                // independently safe to drop, and the next commit tries again.
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    session = %lease.thread.session.storage_key(),
                    %error,
                    "session scope left uncompacted"
                ),
            }
        }
        save_thread(txn, &thread)?;
        save_state(txn, &state)?;
        Ok(receipt)
    })
}

fn status_event(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Queued => "turn.queued",
        TurnStatus::Running => "turn.running",
        TurnStatus::Waiting => "turn.waiting",
        TurnStatus::Pausing | TurnStatus::Paused => "turn.paused",
        TurnStatus::Cancelling | TurnStatus::Cancelled => "turn.cancelled",
        TurnStatus::Succeeded => "turn.succeeded",
        TurnStatus::Failed => "turn.failed",
    }
}

fn reject_reserved(event_type: &str) -> Result<()> {
    const RESERVED: [&str; 4] = ["session.", "thread.", "run.", "step."];
    if RESERVED.iter().any(|prefix| event_type.starts_with(prefix)) {
        return Err(DurableError::invalid(
            "Structured facts cannot be written as committed events",
        ));
    }
    Ok(())
}

fn apply_journal(txn: &mut dyn Txn, thread: &mut ThreadData, command: &Commit) -> Result<()> {
    let Some(journal) = &command.journal else {
        return Ok(());
    };
    if journal.expected_position != thread.journal_position {
        return Err(DurableError::conflict("Journal position changed"));
    }
    for entry in &journal.entries {
        thread.journal_position += 1;
        store_json(
            txn,
            &journal_part(
                &thread.definition.key.thread_id,
                &thread.journal_branch,
                thread.journal_position,
            ),
            entry,
        )?;
    }
    Ok(())
}

fn apply_run(turn: &mut TurnData, change: &RunChange, lease: &Lease, now: i64) -> Result<()> {
    let Some(run) = turn.runs.get_mut(&change.run_id) else {
        return Err(DurableError::invalid("Unknown run"));
    };
    if run.status.terminal() {
        return Err(DurableError::conflict("Run already settled"));
    }
    if run.owner != lease.owner || run.epoch != lease.epoch {
        return Err(DurableError::fenced("Run is owned by another writer"));
    }
    if run.started_at.is_none() {
        run.started_at = Some(now);
    }
    run.status = change.status;
    run.outcome = change.outcome.clone();
    run.reason = change.reason.clone();
    if change.status.terminal() {
        run.ended_at = Some(now);
    }
    Ok(())
}

fn apply_step(turn: &mut TurnData, change: &StepChange, now: i64) -> Result<()> {
    if !turn.runs.contains_key(&change.run_id) {
        return Err(DurableError::invalid("Step requires a known run"));
    }
    match turn.steps.get_mut(&change.step_id) {
        Some(step) => {
            if step.status.terminal() {
                return Err(DurableError::conflict("Step already settled"));
            }
            step.status = change.status;
            step.phase = change.phase;
            step.reason = change.reason.clone();
            if change.status.terminal() {
                step.ended_at = Some(now);
            }
        }
        None => {
            let index = turn.steps.len() as i32;
            turn.steps.insert(
                change.step_id.clone(),
                StepView {
                    thread: ThreadKey::new(
                        turn.runs[&change.run_id].thread.session.clone(),
                        turn.thread_id.clone(),
                    )?,
                    turn_id: turn.turn_id.clone(),
                    run_id: change.run_id.clone(),
                    step_id: change.step_id.clone(),
                    index,
                    status: change.status,
                    phase: change.phase,
                    resumed_from_step_id: change.resumed_from_step_id.clone(),
                    started_at: now,
                    ended_at: change.status.terminal().then_some(now),
                    reason: change.reason.clone(),
                },
            );
            turn.step_order.push(change.step_id.clone());
        }
    }
    Ok(())
}

/// Applies the model facts one batch observed.
///
/// The native journal writes a dispatch and its ending in two batches, so this is an upsert by
/// `modelCallId`: a change only overwrites what it carries, and a call whose ending was already
/// recorded is left alone (a replayed batch must not reopen it).
fn apply_model_calls(turn: &mut TurnData, changes: &[ModelCallChange], now: i64) -> Result<()> {
    for change in changes {
        if !turn.model_calls.contains_key(&change.model_call_id) {
            turn.model_calls.insert(
                change.model_call_id.clone(),
                ModelCallData::new(change.model_call_id.clone(), now),
            );
            turn.model_call_order.push(change.model_call_id.clone());
        }
        let call = turn
            .model_calls
            .get_mut(&change.model_call_id)
            .ok_or_else(|| DurableError::invalid("Unknown model call"))?;
        if let Some(model) = &change.model {
            call.model = Some(model.clone());
        }
        if let Some(purpose) = &change.purpose {
            call.purpose = Some(purpose.clone());
        }
        if let Some(iteration) = change.iteration {
            call.iteration = Some(iteration);
        }
        if let Some(step) = &change.step_id {
            call.step_id = Some(step.clone());
        }
        if call.status.terminal() {
            continue;
        }
        call.status = change.status;
        if change.status.terminal() {
            call.ended_at = Some(now);
            call.duration_millis = Some(now.saturating_sub(call.started_at));
            call.usage = change.usage.clone();
            call.usage_complete = change.usage_complete;
            call.error_type = change.error_type.clone();
        }
        // A change that names a provider opened one attempt; its ending settles the same entry
        // instead of adding a second one, because the native journal reports one attempt per
        // dispatch and this implementation never retries inside a dispatch.
        if let Some(provider) = &change.provider {
            let attempt = call.requests.len() as i32 + 1;
            let ended = change.status.terminal().then_some(now);
            call.requests.push(ModelRequestView {
                request_id: format!("{}:{attempt}", change.model_call_id),
                attempt,
                provider: provider.clone(),
                status: change.status,
                started_at: call.started_at,
                ended_at: ended,
                duration_millis: ended.map(|ended| ended.saturating_sub(call.started_at)),
                usage: change.usage.clone(),
                usage_complete: change.usage_complete,
                error_type: change.error_type.clone(),
            });
        }
    }
    Ok(())
}

fn apply_tools(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    turn: &mut TurnData,
    lease: &Lease,
    command: &Commit,
    now: i64,
) -> Result<()> {
    for call in &command.tools {
        if turn.cancel {
            return Err(DurableError::conflict("Turn is stopping"));
        }
        let execution_id = call.execution_id(&turn.turn_id);
        if let Some(existing) = turn.tools.get(&execution_id) {
            if existing.request.call != *call {
                return Err(DurableError::conflict("Tool intent changed"));
            }
            continue;
        }
        let request = ExecutionRequest::new(
            execution_id.clone(),
            lease.thread.clone(),
            turn.turn_id.clone(),
            call.clone(),
        )?;
        turn.tools.insert(
            execution_id.clone(),
            ToolData {
                request,
                attempted: false,
                result: None,
                applied: false,
                applied_at: None,
                delegation: None,
            },
        );
        turn.tool_order.push(execution_id.clone());
        store_json(txn, &execution_part(&execution_id), &turn.turn_id)?;
        append_event(
            txn,
            state,
            &lease.thread,
            &turn.turn_id,
            call.origin.as_ref().map(|origin| origin.run_id.as_str()),
            call.origin
                .as_ref()
                .and_then(|origin| origin.step_id.as_deref()),
            &format!("tool.requested.{execution_id}"),
            "tool.requested",
            json!({ "executionId": execution_id, "name": call.name, "callId": call.call_id }),
        )?;
    }
    let _ = now;
    Ok(())
}

fn apply_results(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    turn: &mut TurnData,
    lease: &Lease,
    command: &Commit,
) -> Result<()> {
    for applied in &command.applied_results {
        let Some(tool) = turn.tools.get_mut(&applied.execution_id) else {
            return Err(DurableError::invalid("Unknown execution"));
        };
        let terminal = tool
            .result
            .as_ref()
            .is_some_and(|result| result.state.terminal());
        if !terminal {
            return Err(DurableError::invalid("Result is not terminal"));
        }
        if tool.applied {
            if tool.applied_at.as_ref() != Some(&applied.applied_at) {
                return Err(DurableError::conflict("Applied result changed"));
            }
            continue;
        }
        tool.applied = true;
        tool.applied_at = Some(applied.applied_at.clone());
        append_event(
            txn,
            state,
            &lease.thread,
            &turn.turn_id,
            Some(&applied.applied_at.run_id),
            applied.applied_at.step_id.as_deref(),
            &format!("tool.applied.{}", applied.execution_id),
            "tool.result.applied",
            json!({ "executionId": applied.execution_id }),
        )?;
    }
    Ok(())
}

fn apply_consumed(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    turn: &mut TurnData,
    lease: &Lease,
    command: &Commit,
) -> Result<()> {
    for signal_id in &command.consumed_signals {
        if turn.consumed.contains(signal_id) {
            continue;
        }
        if turn.withdrawn.contains(signal_id) {
            return Err(DurableError::conflict("Signal was withdrawn"));
        }
        if !turn.signals.contains_key(signal_id) {
            return Err(DurableError::invalid("Unknown signal"));
        }
        turn.consumed.push(signal_id.clone());
        append_event(
            txn,
            state,
            &lease.thread,
            &turn.turn_id,
            None,
            None,
            &format!("signal.applied.{signal_id}"),
            "signal.applied",
            json!({ "signalId": signal_id }),
        )?;
    }
    Ok(())
}

/// Status guards, in the order the invariants must be checked.
fn derive_status(turn: &TurnData, command: &Commit) -> Result<TurnStatus> {
    let mut next = command.status;
    if turn.failed_epoch == Some(turn.epoch)
        && (!command.tools.is_empty() || next == TurnStatus::Succeeded)
    {
        return Err(DurableError::conflict(
            "Failure ownership must be released before continuing",
        ));
    }
    if next == TurnStatus::Succeeded {
        if let Some(recovery) = &turn.recovery {
            if !recovery.retry_allowed() && !turn.cancel {
                return Err(DurableError::conflict("Recovery has stopped"));
            }
        }
    }
    if next == TurnStatus::Paused && turn.status != TurnStatus::Pausing && !turn.cancel {
        return Err(DurableError::conflict("Pause was not requested"));
    }
    if turn.status == TurnStatus::Pausing && !turn.cancel {
        if next.terminal() {
            return Err(DurableError::conflict("Pause must be acknowledged"));
        }
        next = if next == TurnStatus::Running {
            TurnStatus::Pausing
        } else {
            TurnStatus::Paused
        };
    }
    if let Some(recovery) = &turn.recovery {
        if !recovery.retry_allowed()
            && !turn.cancel
            && turn.status != TurnStatus::Pausing
            && !next.terminal()
        {
            next = TurnStatus::Waiting;
        }
    }
    if next.terminal() && turn.any_unsettled_tool() {
        return Err(DurableError::invalid("External operations are not settled"));
    }
    if next == TurnStatus::Succeeded && turn.unknown_tool() {
        return Err(DurableError::invalid(
            "A tool outcome is unknown; the turn cannot report success",
        ));
    }
    if next == TurnStatus::Succeeded && turn.unapplied_tool() {
        return Err(DurableError::invalid("Results have not been applied"));
    }
    if next == TurnStatus::Waiting
        && command.waiting.is_none()
        && turn.recovery.is_none()
        && !turn.unapplied_tool()
    {
        return Err(DurableError::invalid(
            "A waiting turn needs a wake condition",
        ));
    }
    if turn.cancel {
        next = if next.terminal() {
            TurnStatus::Cancelled
        } else {
            TurnStatus::Cancelling
        };
    }
    Ok(next)
}

fn write_checkpoint(
    txn: &mut dyn Txn,
    thread: &mut ThreadData,
    command: &Commit,
    next: TurnStatus,
) -> Result<()> {
    let previous = match thread.checkpoint_position {
        0 => None,
        position => load_json::<CheckpointRecord>(
            txn,
            &checkpoint_history_part(&thread.definition.key.thread_id, position),
        )?,
    };
    // A null checkpoint keeps the committed state: a journal batch that carries no native
    // state must not re-send (and re-hash) the whole conversation. A terminal turn still
    // records that the kept checkpoint is settled.
    if command.checkpoint.is_null() {
        if next.terminal() {
            mark_checkpoint_settled(txn, thread, previous)?;
        }
        return Ok(());
    }
    let digest = json::hash(&command.checkpoint)?;
    let unchanged = thread
        .checkpoint_metadata
        .as_ref()
        .is_some_and(|metadata| json::matches(&metadata.digest, &command.checkpoint))
        && previous
            .as_ref()
            .is_some_and(|record| record.resource_position == thread.resource_position);
    if !unchanged {
        thread.checkpoint_position += 1;
        let metadata = CheckpointMetadata {
            schema_version: CHECKPOINT_SCHEMA_VERSION.to_string(),
            harness: thread.definition.harness().to_string(),
            definition_version: thread.definition.definition_version().to_string(),
            state_version: thread.definition.state_version().to_string(),
            position: thread.checkpoint_position.to_string(),
            digest,
        };
        let record = CheckpointRecord {
            metadata: metadata.clone(),
            checkpoint: command.checkpoint.clone(),
            branch: thread.journal_branch.clone(),
            journal_position: thread.journal_position,
            resource_position: thread.resource_position,
            settled: next.terminal(),
        };
        txn.put(
            &checkpoint_part(&thread.definition.key.thread_id),
            Some(command.checkpoint.clone()),
        )?;
        store_json(
            txn,
            &checkpoint_history_part(&thread.definition.key.thread_id, thread.checkpoint_position),
            &record,
        )?;
        thread.checkpoint_metadata = Some(metadata);
    } else if next.terminal() {
        mark_checkpoint_settled(txn, thread, previous)?;
    }
    Ok(())
}

/// Marks the checkpoint a terminal turn keeps as settled, without rewriting its payload.
fn mark_checkpoint_settled(
    txn: &mut dyn Txn,
    thread: &ThreadData,
    previous: Option<CheckpointRecord>,
) -> Result<()> {
    if thread.checkpoint_position == 0 {
        return Ok(());
    }
    if let Some(mut record) = previous {
        if !record.settled {
            record.settled = true;
            store_json(
                txn,
                &checkpoint_history_part(
                    &thread.definition.key.thread_id,
                    thread.checkpoint_position,
                ),
                &record,
            )?;
        }
    }
    Ok(())
}

pub(super) fn receipt(
    store: &dyn RecordStore,
    thread: &ThreadKey,
    turn_id: &str,
    commit_id: &str,
) -> Result<Option<CommitReceipt>> {
    with_session(store, &thread.session, |txn| {
        let turn = require_turn(txn, turn_id)?;
        if turn.thread_id != thread.thread_id {
            return Err(DurableError::not_found(
                "Turn does not belong to this thread",
            ));
        }
        Ok(turn.receipts.get(commit_id).cloned())
    })
}

pub(super) fn journal(
    store: &dyn RecordStore,
    lease: &Lease,
    after: u64,
    limit: usize,
) -> Result<Vec<Value>> {
    let limit = clamp_limit(limit)?;
    with_session(store, &lease.thread.session, |txn| {
        let thread = require_thread(txn, &lease.thread)?;
        let turn = require_turn(txn, &lease.turn_id)?;
        assert_owned(&turn, lease, txn.now())?;
        if after > thread.journal_position {
            return Err(DurableError::invalid("Journal position is out of range"));
        }
        let prefix = journal_prefix(&thread.definition.key.thread_id, &thread.journal_branch);
        // The cursor is the key of the last entry already read: journal parts end in their
        // zero-padded position, so lexical order is sequence order and the store can start the
        // page where the caller stopped instead of at the beginning of the thread.
        let cursor = journal_part(
            &thread.definition.key.thread_id,
            &thread.journal_branch,
            after,
        );
        let mut entries = Vec::new();
        for part in txn.keys(&prefix, Some(&cursor), limit)? {
            if let Some(entry) = txn.get(&part)? {
                entries.push(entry);
            }
        }
        crate::traffic::count_journal_read(entries.len());
        Ok(entries)
    })
}
