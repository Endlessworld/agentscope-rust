//! Turn admission, control, ownership and failure reporting.

use serde_json::json;

use super::*;

/// How much time a lease extension has to leave on the clock before it is worth writing down.
///
/// A lease is extended only when what is left is at most half its ttl (see [`renew`]); this
/// floor keeps that rule meaningful for a lease shorter than ten seconds, where half the ttl is
/// no headroom at all. Five seconds is longer than any scope transaction this store performs, so
/// a caller that renews on schedule always gets its extension written before the lease lapses.
pub(super) fn renew_headroom(ttl_millis: i64) -> i64 {
    (ttl_millis / 2).max(LEASE_RENEW_FLOOR_MILLIS)
}

const LEASE_RENEW_FLOOR_MILLIS: i64 = 5_000;

pub(super) fn submit(
    store: &dyn RecordStore,
    thread_key: &ThreadKey,
    request_id: &str,
    input: Value,
) -> Result<TurnReceipt> {
    submit_with_parent(store, thread_key, request_id, input, None)
}

/// [`submit`] for a turn a parent tool call started.
///
/// The parent identity is written once, with the turn: a recovered parent finds the child it
/// started instead of starting a second one. The admission digest deliberately does not cover it
/// — the parent is derivable from the request identity, and covering it would change the digest
/// of every turn already admitted.
pub(super) fn submit_with_parent(
    store: &dyn RecordStore,
    thread_key: &ThreadKey,
    request_id: &str,
    input: Value,
    parent: Option<DelegationParent>,
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
            if !digest_matches(&admission.digest, &(thread_key, &input))? {
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
        turn.parent = parent;
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
        let mut thread = require_thread(txn, thread_key)?;
        let identity = format!("control:{}", command.request_id);
        let digest = digest_of(&(thread_key, &command))?;
        if let Some(existing) = load_json::<CommandRecord>(txn, &command_part(&identity))? {
            if !digest_matches(&existing.digest, &(thread_key, &command))? {
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
        // Set when this control makes the turn terminal: the settled prefix then has one more
        // candidate at its head, and the record is written by this same transaction.
        let mut settles = false;
        let now = txn.now();
        match command.kind {
            // Input adjustment is only meaningful for the turn a worker is executing: it is
            // folded in before the next reasoning boundary of *that* turn.
            ControlKind::Steer | ControlKind::Inject => {
                if turn.status.terminal() {
                    return Err(DurableError::conflict("Turn is terminal"));
                }
                let active = turn.epoch != 0
                    && current_turn(txn, &thread)?
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
                        settles = true;
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
                if turn.status.terminal() {
                    return Err(DurableError::conflict("Turn is terminal"));
                }
                let key = command
                    .target
                    .clone()
                    .unwrap_or_else(|| "response".to_string());
                let signal = Signal::new(command.request_id.clone(), key, command.value.clone())?;
                // Through the same door as every other signal: an answer has an identity, and a
                // withdrawn id must not come back — answering twice used to append a second
                // event under the same id and be rejected as `Event identity changed` instead.
                accept_signal(txn, &mut state, thread_key, &mut turn, signal)?;
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
        if settles {
            // Cancelling a turn no writer held ends it, so it can move the prefix exactly as a
            // commit does — inside the transaction this control already writes.
            advance_settled(txn, &mut thread)?;
            save_thread(txn, &thread)?;
        }
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
) -> Result<Option<(Lease, i64)>> {
    json::text(owner, "owner")?;
    let ttl = ttl_millis(ttl)?;
    with_session(store, &thread_key.session, |txn| {
        let mut state = require_state(txn)?;
        let mut thread = require_thread(txn, thread_key)?;
        let now = txn.now();
        let Some(mut turn) = current_turn(txn, &thread)? else {
            schedule_thread(txn, &thread, None, now)?;
            save_thread(txn, &thread)?;
            save_state(txn, &state)?;
            return Ok(None);
        };
        if turn.owner.as_deref() == Some(owner) && turn.expires_at > now {
            return Ok(Some((
                Lease {
                    thread: thread_key.clone(),
                    turn_id: turn.turn_id.clone(),
                    owner: owner.to_string(),
                    epoch: turn.epoch,
                    expires_at: turn.expires_at,
                },
                turn.expires_at - now,
            )));
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
        turn.expires_at = now.saturating_add(ttl);
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
        // The caller keeps how much time the store granted, not the absolute expiry: that is the
        // number its memo can compare against a monotonic clock, and the store's clock is the
        // only one that knows when the lease actually runs out.
        Ok(Some((lease, ttl)))
    })
}

/// Extends one lease, answering how much of it is left on the store's clock.
pub(super) fn renew(store: &dyn RecordStore, lease: &Lease, ttl: Duration) -> Result<i64> {
    let ttl = ttl_millis(ttl)?;
    with_session(store, &lease.thread.session, |txn| {
        let thread = require_thread(txn, &lease.thread)?;
        let mut turn = require_turn(txn, &lease.turn_id)?;
        let now = txn.now();
        assert_owned(&turn, lease, now)?;
        // An extension only has to be *written* when the lease is actually running out.
        //
        // The whole session is one document, so restating an expiry that already has more than
        // half its ttl to run costs a rewrite of everything — and every journal flush renews
        // before it commits, which made the plain record-and-write form cost two full rewrites
        // per commit for one integer. The fence is enforced by `assert_owned` on every mutating
        // call anyway, so a later extension is what the caller needs, not an earlier one.
        //
        // The floor below `ttl / 2` keeps a short lease extendable: with a millisecond ttl the
        // half-way rule alone would never fire, so a lease would be allowed to lapse without a
        // single write trying to save it.
        let headroom = renew_headroom(ttl);
        if turn.expires_at - now > headroom {
            return Ok(turn.expires_at - now);
        }
        turn.expires_at = now.saturating_add(ttl);
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        Ok(ttl)
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
            turn.next_attempt_at = turn.next_attempt_at.max(now.saturating_add(delay));
        }
        schedule_thread(txn, &thread, Some(&turn), now)?;
        save_turn(txn, &turn)?;
        save_state(txn, &state)
    })
}

/// The control facts a running invocation polls, read without materializing the frame.
///
/// One record instead of the conversation, and no scope transaction: the file store answers it
/// from the document without a lock (a document is replaced by an atomic rename), so a poll costs
/// a scan of the bytes rather than a lock file plus a full parse of the whole session.
pub(super) fn control_state(store: &dyn RecordStore, lease: &Lease) -> Result<TurnControl> {
    crate::traffic::count_control_read();
    let part = turn_part(&lease.turn_id);
    let mut values = store.read_parts(&lease.thread.session.storage_key(), &[part.as_str()])?;
    let value = values
        .pop()
        .flatten()
        .ok_or_else(|| DurableError::not_found("Turn does not exist"))?;
    let turn: TurnData = serde_json::from_value(value)?;
    // The same ownership rule as `frame`, answered on the reader's clock: a poll is where a
    // writer that lost its lease gets to notice, and waiting for its next commit to be fenced
    // would spend the whole stall before the turn could be handed on.
    assert_owned(&turn, lease, crate::now_millis())?;
    Ok(TurnControl {
        status: turn.status,
        cancellation_requested: turn.cancel,
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
        let mut thread = require_thread(txn, &lease.thread)?;
        let mut turn = require_turn(txn, &lease.turn_id)?;
        let part = format!(
            "failure:{}",
            model::hash(&format!("{}:{}", lease.turn_id, lease.epoch))
        );
        let digest = digest_of(&(lease, &failure))?;
        let now = txn.now();
        if let Some(existing) = load_json::<FailureRecord>(txn, &part)? {
            if !digest_matches(&existing.digest, &(lease, &failure))? {
                return Err(DurableError::conflict("Failure report changed"));
            }
            // A replay of a failure that can never be retried must still end the turn: the
            // record above is idempotent, so without this an unrecoverable turn written by an
            // older build stays `waiting` and no writer ever moves it again.
            if !existing.recovery.retry_allowed()
                && turn.status == TurnStatus::Waiting
                && turn.waiting.is_none()
                && !turn.cancel
            {
                // The record is keyed by the epoch that reported it, but the turn can still
                // belong to someone else: a claim that bumped the epoch and then failed to
                // record its own report leaves this writer's stale record readable. Settling a
                // turn another writer owns is not a repair, so the epoch has to still be ours.
                // (`assert_owned` cannot be used here: a turn with a failure recorded has no
                // owner by construction, which is exactly the state this branch repairs.)
                if turn.epoch != lease.epoch {
                    return Err(DurableError::fenced(
                        "Execution ownership expired or changed",
                    ));
                }
                turn.status = TurnStatus::Failed;
                turn.owner = None;
                turn.expires_at = 0;
                turn.revision += 1;
                abandon_runs(&mut turn, RunStatus::Stopped, "failure", now);
                // The repair ends the turn right here, so the prefix is advanced in the same
                // transaction — otherwise a session whose only writer died never materializes it.
                advance_settled(txn, &mut thread)?;
                save_thread(txn, &thread)?;
                schedule_thread(txn, &thread, Some(&turn), now)?;
                save_turn(txn, &turn)?;
            }
            return Ok(existing.recovery);
        }
        assert_owned(&turn, lease, now)?;
        let previous = turn.recovery.clone();
        let policy = previous
            .as_ref()
            .map(|view| view.policy)
            .unwrap_or(failure.policy);
        let failures = previous.as_ref().map(|view| view.failures + 1).unwrap_or(1);
        let retryable = failure.retryable;
        let retry_allowed = retryable && failures <= policy.max_retries;
        let retry_at =
            retry_allowed.then(|| now.saturating_add(policy.delay_millis(failures) as i64));
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
            now.saturating_add(policy.max_delay_millis as i64)
        } else {
            now
        };
        if turn.status == TurnStatus::Pausing {
            complete_pause(txn, &mut state, &lease.thread, &mut turn, now)?;
        } else if !retry_allowed && !turn.cancel && !turn.status.terminal() {
            // Nothing will retry this turn, so waiting would park it forever: a `waiting` turn
            // is re-tried as soon as its `next_attempt_at` passes, which for a non-retryable
            // failure means failing again on every pass. End it and keep the message.
            turn.status = TurnStatus::Failed;
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
        if turn.status.terminal() {
            // A failure with nothing left to retry ends the turn without a commit; the prefix
            // rides the thread record this same transaction writes.
            advance_settled(txn, &mut thread)?;
            save_thread(txn, &thread)?;
        }
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
