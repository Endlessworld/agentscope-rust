//! Turn admission, control, ownership and failure reporting.

use serde_json::json;

use super::*;

pub(super) fn submit(
    store: &dyn RecordStore,
    thread_key: &ThreadKey,
    request_id: &str,
    input: Value,
) -> Result<TurnReceipt> {
    json::text(request_id, "requestId")?;
    with_session(store, &thread_key.session, |txn| {
        let mut state = require_state(txn)?;
        let mut thread = require_thread(txn, thread_key)?;
        if state.session.status != Lifecycle::Open {
            return Err(DurableError::conflict("Session is closed"));
        }
        if thread.status != Lifecycle::Open {
            return Err(DurableError::conflict("Thread is closed"));
        }
        let digest = digest_of(&(thread_key, &input))?;
        if let Some(admission) = load_json::<AdmissionRecord>(txn, &admission_part(request_id))? {
            if admission.digest != digest {
                return Err(DurableError::conflict("Input changed for this request"));
            }
            return Ok(admission.receipt);
        }
        if load_json::<CommandRecord>(txn, &command_part(&format!("control:{request_id}")))?
            .is_some()
        {
            return Err(DurableError::conflict(
                "Request identity belongs to a control operation",
            ));
        }
        let now = txn.now();
        let turn_id = format!("turn_{}", digest_of(&(thread_key, request_id))?);
        let mut turn = TurnData::new(turn_id.clone(), thread_key.thread_id.clone(), input, now);
        turn.next_attempt_at = now;
        state.admitted += 1;
        thread.admitted += 1;
        store_json(
            txn,
            &queue_part(&thread_key.thread_id, thread.admitted),
            &turn_id,
        )?;
        store_json(txn, &turn_order_part(state.admitted), &turn_id)?;
        let receipt = TurnReceipt {
            thread: thread_key.clone(),
            request_id: request_id.to_string(),
            turn_id: turn_id.clone(),
        };
        store_json(
            txn,
            &admission_part(request_id),
            &AdmissionRecord {
                digest,
                receipt: receipt.clone(),
            },
        )?;
        append_event(
            txn,
            &mut state,
            thread_key,
            &turn_id,
            None,
            None,
            &format!("turn.queued.{turn_id}"),
            "turn.queued",
            json!({ "requestId": request_id }),
        )?;
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        save_thread(txn, &thread)?;
        save_state(txn, &state)?;
        Ok(receipt)
    })
}

pub(super) fn admission(
    store: &dyn RecordStore,
    session: &SessionKey,
    request_id: &str,
) -> Result<Option<TurnReceipt>> {
    with_session(store, session, |txn| {
        require_state(txn)?;
        Ok(load_json::<AdmissionRecord>(txn, &admission_part(request_id))?.map(|it| it.receipt))
    })
}

pub(super) fn signal(
    store: &dyn RecordStore,
    thread_key: &ThreadKey,
    turn_id: &str,
    signal: Signal,
) -> Result<()> {
    with_session(store, &thread_key.session, |txn| {
        let mut state = require_state(txn)?;
        let thread = require_thread(txn, thread_key)?;
        let mut turn = require_turn(txn, turn_id)?;
        if turn.thread_id != thread_key.thread_id {
            return Err(DurableError::not_found(
                "Turn does not belong to this thread",
            ));
        }
        if turn.status.terminal() {
            return Err(DurableError::conflict("Turn is terminal"));
        }
        let now = txn.now();
        accept_signal(txn, &mut state, thread_key, &mut turn, signal)?;
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        save_state(txn, &state)?;
        Ok(())
    })
}

/// Records one accepted signal on the turn it was addressed to.
///
/// Acceptance is idempotent by signal id: the same signal twice is one record, different content
/// under a used id is a conflict, and a withdrawn id cannot come back. Delivery is a separate
/// fact: a commit listing the id in `consumed_signals` marks it applied (see `apply_consumed`).
fn accept_signal(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    thread_key: &ThreadKey,
    turn: &mut TurnData,
    signal: Signal,
) -> Result<()> {
    if let Some(existing) = turn.signals.get(&signal.id) {
        if *existing == signal {
            return Ok(());
        }
        return Err(DurableError::conflict("Signal identity changed"));
    }
    if turn.withdrawn.contains(&signal.id) {
        return Err(DurableError::conflict("Signal was withdrawn"));
    }
    turn.signals.insert(signal.id.clone(), signal.clone());
    // A waiting turn becomes due as soon as its key is satisfied.
    turn.next_attempt_at = 0;
    append_event(
        txn,
        state,
        thread_key,
        &turn.turn_id,
        None,
        None,
        &format!("signal.{}", signal.id),
        "signal.accepted",
        json!({ "signalId": signal.id, "key": signal.key }),
    )
}

pub(super) fn control(
    store: &dyn RecordStore,
    thread_key: &ThreadKey,
    command: ControlCommand,
) -> Result<CommandReceipt> {
    json::text(&command.request_id, "requestId")?;
    with_session(store, &thread_key.session, |txn| {
        let mut state = require_state(txn)?;
        let thread = require_thread(txn, thread_key)?;
        let identity = format!("control:{}", command.request_id);
        let digest = digest_of(&(thread_key, &command))?;
        if let Some(existing) = load_json::<CommandRecord>(txn, &command_part(&identity))? {
            if existing.digest != digest {
                return Err(DurableError::conflict("Control identity changed"));
            }
            return load_json::<CommandReceipt>(txn, &control_receipt_part(&command.request_id))?
                .ok_or_else(|| DurableError::invalid("Control receipt is missing"));
        }
        if load_json::<AdmissionRecord>(txn, &admission_part(&command.request_id))?.is_some() {
            return Err(DurableError::conflict(
                "Request identity belongs to an admitted turn",
            ));
        }
        let mut turn = require_turn(txn, &command.turn_id)?;
        if turn.thread_id != thread_key.thread_id {
            return Err(DurableError::not_found(
                "Turn does not belong to this thread",
            ));
        }
        let mut status = "accepted";
        let now = txn.now();
        match command.kind {
            // Input adjustment is only meaningful for the turn a worker is executing: it is
            // folded in before the next reasoning boundary of *that* turn.
            ControlKind::Steer | ControlKind::Inject => {
                if turn.status.terminal() {
                    return Err(DurableError::conflict("Turn is terminal"));
                }
                let active = turn.epoch != 0
                    && current_turn(txn, &thread_key.thread_id)?
                        .is_some_and(|current| current.turn_id == turn.turn_id);
                if !active {
                    return Err(DurableError::conflict(
                        "Input adjustment requires the active Turn",
                    ));
                }
                let key = match command.kind {
                    ControlKind::Steer => "input:steer",
                    _ => "input:inject",
                };
                let signal = Signal::new(command.request_id.clone(), key, command.value.clone())?;
                accept_signal(txn, &mut state, thread_key, &mut turn, signal)?;
            }
            ControlKind::Signal => {
                if turn.status.terminal() {
                    return Err(DurableError::conflict("Turn is terminal"));
                }
                let signal = Signal::new(
                    command.request_id.clone(),
                    command.target.clone().unwrap_or_default(),
                    command.value.clone(),
                )?;
                accept_signal(txn, &mut state, thread_key, &mut turn, signal)?;
            }
            ControlKind::Cancel => {
                if turn.status.terminal() {
                    status = "settled";
                } else {
                    turn.cancel = true;
                    turn.next_attempt_at = 0;
                    if turn.epoch == 0 {
                        turn.status = TurnStatus::Cancelled;
                    } else {
                        turn.status = TurnStatus::Cancelling;
                    }
                    turn.revision += 1;
                    append_event(
                        txn,
                        &mut state,
                        thread_key,
                        &turn.turn_id,
                        None,
                        None,
                        &format!("turn.cancel.{}", command.request_id),
                        "turn.cancel_requested",
                        json!({}),
                    )?;
                }
            }
            ControlKind::Pause => {
                expect_revision(&turn, command.expected_revision)?;
                if turn.status.terminal() || turn.status == TurnStatus::Paused {
                    status = "settled";
                } else {
                    turn.pause_requested = true;
                    turn.status = if turn.owner.is_some() {
                        TurnStatus::Pausing
                    } else {
                        TurnStatus::Paused
                    };
                    turn.revision += 1;
                    append_event(
                        txn,
                        &mut state,
                        thread_key,
                        &turn.turn_id,
                        None,
                        None,
                        &format!("turn.pause.{}", command.request_id),
                        "turn.pause_requested",
                        json!({}),
                    )?;
                }
            }
            ControlKind::Resume => {
                expect_revision(&turn, command.expected_revision)?;
                if turn.status != TurnStatus::Paused && turn.status != TurnStatus::Pausing {
                    return Err(DurableError::conflict("Turn is not paused"));
                }
                turn.pause_requested = false;
                turn.revision += 1;
                turn.status = if turn.waiting.is_some() || turn.any_unsettled_tool() {
                    TurnStatus::Waiting
                } else if turn.epoch == 0 {
                    TurnStatus::Queued
                } else {
                    TurnStatus::Running
                };
                append_event(
                    txn,
                    &mut state,
                    thread_key,
                    &turn.turn_id,
                    None,
                    None,
                    &format!("turn.resume.{}", command.request_id),
                    "turn.resumed",
                    json!({}),
                )?;
            }
            ControlKind::Withdraw => {
                let signal_id = command.target.clone().unwrap_or_default();
                json::text(&signal_id, "signal id")?;
                if turn.consumed.contains(&signal_id) {
                    return Err(DurableError::conflict("Input already applied"));
                }
                if !turn.withdrawn.contains(&signal_id) {
                    turn.withdrawn.push(signal_id.clone());
                    append_event(
                        txn,
                        &mut state,
                        thread_key,
                        &turn.turn_id,
                        None,
                        None,
                        &format!("signal.withdraw.{}", command.request_id),
                        "signal.withdrawn",
                        json!({ "signalId": signal_id }),
                    )?;
                }
            }
            ControlKind::Respond => {
                let key = command
                    .target
                    .clone()
                    .unwrap_or_else(|| "response".to_string());
                let signal = Signal::new(command.request_id.clone(), key, command.value.clone())?;
                turn.signals.insert(signal.id.clone(), signal);
                turn.next_attempt_at = 0;
                append_event(
                    txn,
                    &mut state,
                    thread_key,
                    &turn.turn_id,
                    None,
                    None,
                    &format!("signal.{}", command.request_id),
                    "signal.accepted",
                    json!({ "requestId": command.request_id }),
                )?;
            }
        }
        let receipt = CommandReceipt {
            request_id: command.request_id.clone(),
            turn_id: turn.turn_id.clone(),
            operation: format!("{:?}", command.kind).to_uppercase(),
            status: status.to_string(),
            cursor: Some(cursor_of(&state, state.sequence)),
        };
        store_json(txn, &command_part(&identity), &CommandRecord { digest })?;
        store_json(txn, &control_receipt_part(&command.request_id), &receipt)?;
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        save_state(txn, &state)?;
        Ok(receipt)
    })
}

fn expect_revision(turn: &TurnData, expected: u64) -> Result<()> {
    if turn.revision != expected {
        return Err(DurableError::conflict("Revision changed"));
    }
    Ok(())
}

pub(super) fn claim(
    store: &dyn RecordStore,
    thread_key: &ThreadKey,
    owner: &str,
    ttl: Duration,
) -> Result<Option<Lease>> {
    json::text(owner, "owner")?;
    let ttl = ttl_millis(ttl)?;
    with_session(store, &thread_key.session, |txn| {
        let mut state = require_state(txn)?;
        let mut thread = require_thread(txn, thread_key)?;
        let now = txn.now();
        let Some(mut turn) = current_turn(txn, &thread_key.thread_id)? else {
            schedule_thread(txn, &thread, None, now)?;
            save_thread(txn, &thread)?;
            save_state(txn, &state)?;
            return Ok(None);
        };
        if turn.owner.as_deref() == Some(owner) && turn.expires_at > now {
            return Ok(Some(Lease {
                thread: thread_key.clone(),
                turn_id: turn.turn_id.clone(),
                owner: owner.to_string(),
                epoch: turn.epoch,
                expires_at: turn.expires_at,
            }));
        }
        let ready = ready_at(&thread, Some(&turn), now);
        if ready < 0 || ready > now {
            schedule_thread(txn, &thread, Some(&turn), now)?;
            save_thread(txn, &thread)?;
            save_state(txn, &state)?;
            return Ok(None);
        }
        abandon_runs(&mut turn, RunStatus::Lost, "ownership_lost", now);
        thread.epoch += 1;
        turn.owner = Some(owner.to_string());
        turn.epoch = thread.epoch;
        turn.expires_at = now + ttl;
        if turn.status == TurnStatus::Queued {
            turn.status = TurnStatus::Running;
            append_event(
                txn,
                &mut state,
                thread_key,
                &turn.turn_id,
                None,
                None,
                &format!("turn.started.{}.{}", turn.turn_id, turn.epoch),
                "turn.started",
                json!({ "owner": owner, "epoch": turn.epoch }),
            )?;
        }
        let lease = Lease {
            thread: thread_key.clone(),
            turn_id: turn.turn_id.clone(),
            owner: owner.to_string(),
            epoch: turn.epoch,
            expires_at: turn.expires_at,
        };
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        save_thread(txn, &thread)?;
        save_state(txn, &state)?;
        Ok(Some(lease))
    })
}

pub(super) fn renew(store: &dyn RecordStore, lease: &Lease, ttl: Duration) -> Result<()> {
    let ttl = ttl_millis(ttl)?;
    with_session(store, &lease.thread.session, |txn| {
        let thread = require_thread(txn, &lease.thread)?;
        let mut turn = require_turn(txn, &lease.turn_id)?;
        let now = txn.now();
        assert_owned(&turn, lease, now)?;
        turn.expires_at = now + ttl;
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)
    })
}

pub(super) fn release(store: &dyn RecordStore, lease: &Lease, delay: Duration) -> Result<()> {
    with_session(store, &lease.thread.session, |txn| {
        let mut state = require_state(txn)?;
        let thread = require_thread(txn, &lease.thread)?;
        let mut turn = require_turn(txn, &lease.turn_id)?;
        let now = txn.now();
        assert_owned(&turn, lease, now)?;
        let delay = delay.as_millis().min(i64::MAX as u128) as i64;
        if turn.status == TurnStatus::Pausing {
            complete_pause(txn, &mut state, &lease.thread, &mut turn, now)?;
        } else {
            abandon_runs(&mut turn, RunStatus::Stopped, "host_released", now);
            turn.owner = None;
            turn.expires_at = 0;
            turn.next_attempt_at = turn.next_attempt_at.max(now + delay);
        }
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        save_state(txn, &state)
    })
}

pub(super) fn frame(store: &dyn RecordStore, lease: &Lease) -> Result<Frame> {
    with_session(store, &lease.thread.session, |txn| {
        let thread = require_thread(txn, &lease.thread)?;
        let turn = require_turn(txn, &lease.turn_id)?;
        let now = txn.now();
        assert_owned(&turn, lease, now)?;
        let checkpoint = txn
            .get(&checkpoint_part(&lease.thread.thread_id))?
            .unwrap_or(Value::Null);
        let tools: Vec<ToolExecution> = turn
            .tool_order
            .iter()
            .filter_map(|id| turn.tools.get(id))
            .map(ToolData::view)
            .collect();
        let signals: Vec<Signal> = turn
            .signals
            .values()
            .filter(|signal| {
                !turn.consumed.contains(&signal.id) && !turn.withdrawn.contains(&signal.id)
            })
            .cloned()
            .collect();
        Ok(Frame {
            definition: thread.definition.clone(),
            turn: turn.view(&lease.thread),
            input: turn.input.clone(),
            checkpoint,
            checkpoint_metadata: thread.checkpoint_metadata.clone(),
            tools,
            signals,
            now,
        })
    })
}

pub(super) fn record_failure(
    store: &dyn RecordStore,
    lease: &Lease,
    failure: Failure,
) -> Result<RecoveryView> {
    with_session(store, &lease.thread.session, |txn| {
        let mut state = require_state(txn)?;
        let thread = require_thread(txn, &lease.thread)?;
        let mut turn = require_turn(txn, &lease.turn_id)?;
        let part = format!(
            "failure:{}",
            model::hash(&format!("{}:{}", lease.turn_id, lease.epoch))
        );
        let digest = digest_of(&(lease, &failure))?;
        if let Some(existing) = load_json::<FailureRecord>(txn, &part)? {
            if existing.digest != digest {
                return Err(DurableError::conflict("Failure report changed"));
            }
            return Ok(existing.recovery);
        }
        let now = txn.now();
        assert_owned(&turn, lease, now)?;
        let previous = turn.recovery.clone();
        let policy = previous
            .as_ref()
            .map(|view| view.policy)
            .unwrap_or(failure.policy);
        let failures = previous.as_ref().map(|view| view.failures + 1).unwrap_or(1);
        let retryable = failure.retryable;
        let retry_allowed = retryable && failures <= policy.max_retries;
        let retry_at = retry_allowed.then(|| now + policy.delay_millis(failures) as i64);
        let view = RecoveryView {
            failures,
            policy,
            retryable,
            retry_at,
            message: failure.message.clone(),
        };
        view.validate()?;
        turn.recovery = Some(view.clone());
        turn.error = Some(failure.message.clone());
        turn.failed_epoch = Some(lease.epoch);
        turn.revision += 1;
        turn.next_attempt_at = if turn.cancel {
            0
        } else if retry_allowed {
            retry_at.unwrap_or(now)
        } else if failures > policy.max_retries {
            now + policy.max_delay_millis as i64
        } else {
            now
        };
        if turn.status == TurnStatus::Pausing {
            complete_pause(txn, &mut state, &lease.thread, &mut turn, now)?;
        } else if !retry_allowed && !turn.cancel && !turn.status.terminal() {
            turn.status = TurnStatus::Waiting;
        }
        turn.owner = None;
        turn.expires_at = 0;
        abandon_runs(&mut turn, RunStatus::Stopped, "failure", now);
        append_event(
            txn,
            &mut state,
            &lease.thread,
            &turn.turn_id,
            None,
            None,
            &format!("turn.recovery.{}.{}", turn.turn_id, lease.epoch),
            "turn.recovery",
            json!({ "failures": view.failures, "retryable": view.retryable, "message": view.message }),
        )?;
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        store_json(
            txn,
            &part,
            &FailureRecord {
                digest,
                recovery: view.clone(),
            },
        )?;
        save_state(txn, &state)?;
        Ok(view)
    })
}

fn complete_pause(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    thread_key: &ThreadKey,
    turn: &mut TurnData,
    now: i64,
) -> Result<()> {
    turn.status = TurnStatus::Paused;
    turn.revision += 1;
    turn.owner = None;
    turn.expires_at = 0;
    abandon_runs(turn, RunStatus::Stopped, "paused", now);
    append_event(
        txn,
        state,
        thread_key,
        &turn.turn_id,
        None,
        None,
        &format!("turn.paused.{}.{}", turn.turn_id, turn.revision),
        "turn.paused",
        json!({}),
    )
}

pub(super) fn abandon_runs(turn: &mut TurnData, status: RunStatus, reason: &str, now: i64) {
    for run in turn.runs.values_mut() {
        if !run.status.terminal() {
            run.status = status;
            run.reason = Some(reason.to_string());
            run.ended_at = Some(now);
        }
    }
    for step in turn.steps.values_mut() {
        if !step.status.terminal() {
            step.status = match status {
                RunStatus::Lost => StepStatus::Lost,
                RunStatus::Stopped => StepStatus::Stopped,
                _ => StepStatus::Stopped,
            };
            step.reason = Some(reason.to_string());
            step.ended_at = Some(now);
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FailureRecord {
    digest: String,
    recovery: RecoveryView,
}
