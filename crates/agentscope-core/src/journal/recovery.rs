//! Explicit operator reconciliation, mirroring `SessionRecovery`.
//!
//! An action whose outcome is unknown must never be re-executed automatically. The operator
//! records what was actually found; only then can the session continue.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use super::{
    session_error, SessionEvent, SessionInbox, SessionLog, SessionProjection, SessionTurns,
};
use crate::message::{
    ContentBlock, MsgBuilder, MsgRole, TextBlock, ToolCallState, ToolResultBlock, ToolResultState,
    ToolUseBlock,
};

/// The reconciliation writer lease.
///
/// Closing a turn is a short operation. A long lease would keep a crashed close blocking the
/// next writer for two minutes even though its heartbeat is already gone.
const LEASE: Duration = Duration::from_secs(15);

/// How many times a reconciliation waits for the execution writer to go away.
///
/// A run holds the writer lease for its whole lifetime (renewed by its heartbeat), so a
/// cancellation that races the run's own shutdown used to fail on the first attempt and
/// leave the turn open forever. Retrying turns that race into a bounded wait.
const ACQUIRE_ATTEMPTS: usize = 1_200;

/// Delay between writer-acquisition attempts.
const ACQUIRE_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Message recorded when a caller cancels a pending interaction.
pub const CANCELLED_PENDING_MESSAGE: &str = "Caller cancelled this pending interaction; external \
     side effects, if any, were not verified.";

/// Message recorded for a tool call whose outcome was never observed when a host abandons a
/// turn. It is deliberately distinct from a success: the side effects were not verified.
pub const ABANDONED_TOOL_MESSAGE: &str = "This tool call was abandoned when the interrupted turn \
     was closed; external side effects, if any, were not verified.";

/// Acquires the reconciliation writer, retrying while a run still holds the lease.
async fn acquire_writer(log: &Arc<dyn SessionLog>) -> crate::error::Result<super::Writer> {
    for _ in 0..ACQUIRE_ATTEMPTS {
        match log.acquire(&uuid::Uuid::new_v4().to_string(), LEASE).await {
            Ok(writer) => return Ok(writer),
            Err(error) => {
                // A busy journal is the expected case here; anything else is fatal.
                if !error.to_string().contains("active writer") {
                    return Err(error);
                }
                tokio::time::sleep(ACQUIRE_RETRY_DELAY).await;
            }
        }
    }
    Err(session_error(
        "Session writer is still busy; the interrupted turn could not be closed",
    ))
}

/// Cancels an idle pending interaction without inventing success or replaying external work.
///
/// Each pending request for `turn_id` becomes an `interrupted` tool result, the corresponding
/// tool calls are marked finished, a checkpoint is saved and the turn is cancelled.
///
/// Refuses to run while a tool outcome is unknown: use [`reconcile`] first, or
/// [`abandon_turn`] when the host explicitly accepts the unknown outcome.
pub async fn cancel_pending(log: Arc<dyn SessionLog>, turn_id: &str) -> crate::error::Result<()> {
    close_turn(log, turn_id, false).await.map(|_| ())
}

/// Closes an unfinished turn so the session can accept a new one.
///
/// Unlike [`cancel_pending`] this also settles *unknown* tool outcomes, recording them as
/// `interrupted` (never as success) and clearing the `tool/dispatch` uncertainty. It never
/// re-executes a tool. Returns `true` when a turn was actually closed.
///
/// This is what a host calls before submitting a new prompt: an interrupted turn is not
/// terminal, so leaving it open would make every later submission wait forever.
pub async fn abandon_turn(log: Arc<dyn SessionLog>, turn_id: &str) -> crate::error::Result<bool> {
    close_turn(log, turn_id, true).await
}

async fn close_turn(
    log: Arc<dyn SessionLog>,
    turn_id: &str,
    resolve_uncertain: bool,
) -> crate::error::Result<bool> {
    let writer = acquire_writer(&log).await?;
    let result = close_turn_locked(&log, &writer, turn_id, resolve_uncertain).await;
    let _ = log.release(&writer).await;
    result
}

async fn close_turn_locked(
    log: &Arc<dyn SessionLog>,
    writer: &super::Writer,
    turn_id: &str,
    resolve_uncertain: bool,
) -> crate::error::Result<bool> {
    // Only the latest turn can be closed: an older turn has already been superseded, and
    // writing terminal facts for it would corrupt the projection's ordering assumptions.
    // Turns, projection and pending interactions come from one replay: closing a turn used to
    // walk the whole prefix three times, on exactly the path a crashed session takes.
    let (turns, projection, pending) = super::views::recovery_views(&**log).await?;
    let Some(latest) = turns.latest() else {
        // Nothing was ever admitted: there is no turn to close.
        return Ok(false);
    };
    if latest.turn_id != turn_id {
        return Ok(false);
    }
    if latest.ended() {
        // `completed` / `failed` / `cancelled`: the turn settled on its own.
        if latest.status == "cancelled" {
            reject_steering(log, turn_id).await?;
        }
        return Ok(false);
    }

    if !projection.uncertain_tool_calls.is_empty() && !resolve_uncertain {
        return Err(session_error(
            "Reconcile uncertain tool outcomes before cancelling this interrupted turn",
        ));
    }
    let Some(mut state) = projection.restore()? else {
        return Ok(false);
    };

    let mut cancelled_ids = BTreeSet::new();
    let mut facts = Vec::new();
    let mut seq = projection.as_of_seq;
    let run = writer.owner.clone();
    let mut push = |event_type: &str,
                    payload: serde_json::Value,
                    facts: &mut Vec<SessionEvent>|
     -> crate::error::Result<()> {
        seq += 1;
        facts.push(SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            seq,
            chrono::Utc::now().timestamp_millis(),
            event_type,
            Some(run.clone()),
            Some(turn_id.to_string()),
            true,
            payload.to_string(),
        )?);
        Ok(())
    };

    for (request_id, request) in &pending {
        if request.turn_id.as_deref() != Some(turn_id) {
            continue;
        }
        let data = request.data()?;
        let call: ToolUseBlock = match data.get("call") {
            Some(value) => serde_json::from_value(value.clone())?,
            None => continue,
        };
        cancelled_ids.insert(call.id.clone());
        let block = ToolResultBlock::new(
            &call.id,
            &call.name,
            vec![ContentBlock::Text(TextBlock::new(
                CANCELLED_PENDING_MESSAGE,
            ))],
        )
        .with_state(ToolResultState::Interrupted);
        let message = MsgBuilder::new()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(block)])
            .build();
        state.context.push(message.clone());
        push(
            "interaction/resolved",
            serde_json::json!({
                "requestId": request_id,
                "kind": data.get("kind"),
                "status": "cancelled",
            }),
            &mut facts,
        )?;
        push(
            "tool/result",
            serde_json::json!({ "message": message, "synthetic": true }),
            &mut facts,
        )?;
    }

    // Tool calls that were dispatched but never produced a result: the host abandoned the
    // turn without knowing the outcome. Record them as `interrupted` (never success) and
    // clear their dispatch uncertainty, so the next turn can start.
    let mut abandoned_ids: BTreeSet<String> = BTreeSet::new();
    for id in &projection.uncertain_tool_calls {
        abandoned_ids.insert(id.clone());
        if cancelled_ids.contains(id) {
            // Already answered by the pending-interaction path above.
            continue;
        }
        let name = state
            .context
            .iter()
            .flat_map(|message| message.tool_use_blocks())
            .find(|call| &call.id == id)
            .map(|call| call.name.clone())
            .unwrap_or_default();
        cancelled_ids.insert(id.clone());
        let block = ToolResultBlock::new(
            id,
            &name,
            vec![ContentBlock::Text(TextBlock::new(ABANDONED_TOOL_MESSAGE))],
        )
        .with_state(ToolResultState::Interrupted);
        let message = MsgBuilder::new()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(block)])
            .build();
        state.context.push(message.clone());
        push(
            "tool/result",
            serde_json::json!({ "message": message, "synthetic": true, "abandoned": true }),
            &mut facts,
        )?;
    }

    for message in &mut state.context {
        let updated: Vec<ContentBlock> = message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolUse(call) if cancelled_ids.contains(&call.id) => {
                    let mut call = call.clone();
                    call.state = ToolCallState::Finished;
                    ContentBlock::ToolUse(call)
                }
                other => other.clone(),
            })
            .collect();
        message.content = updated;
    }

    push(
        "state/checkpoint",
        serde_json::json!({
            "stateJson": state.to_json()?,
            "projectorVersion": 1,
            "reason": "pending_cancelled",
        }),
        &mut facts,
    )?;
    if !projection.active_runs.is_empty() || !abandoned_ids.is_empty() {
        push(
            "recovery/applied",
            serde_json::json!({
                "interruptedRuns": projection.active_runs,
                "resolvedToolCallIds": abandoned_ids,
                "reason": "caller_cancelled",
            }),
            &mut facts,
        )?;
    }
    push(
        "turn/cancelled",
        serde_json::json!({ "status": "cancelled" }),
        &mut facts,
    )?;

    log.commit(
        writer,
        &uuid::Uuid::new_v4().to_string(),
        projection.as_of_seq,
        &facts,
    )
    .await?;
    reject_steering(&log, turn_id).await?;
    Ok(true)
}

/// Rejects unapplied steering bound to a cancelled turn, so it is never silently lost.
async fn reject_steering(log: &Arc<dyn SessionLog>, turn_id: &str) -> crate::error::Result<()> {
    let applied = SessionTurns::read(&**log).await?.applied;
    let inbox = SessionInbox::new(Arc::clone(log))?;
    let snapshot = inbox.snapshot().await?;
    for command in &snapshot.commands {
        if command.kind == "steer"
            && command.turn_id.as_deref() == Some(turn_id)
            && !applied.contains(&command.id)
            && !snapshot.rejected.contains_key(&command.id)
        {
            inbox.reject(command, "turn_cancelled").await?;
        }
    }
    Ok(())
}

/// Records verified outcomes for exactly the uncertain tool calls, then checkpoints.
///
/// Reconciliation never executes the tools again; continuation stays explicit.
pub async fn reconcile(
    log: Arc<dyn SessionLog>,
    outcomes: BTreeMap<String, ToolResultBlock>,
    reason: &str,
) -> crate::error::Result<()> {
    if reason.trim().is_empty() {
        return Err(session_error("A reconciliation reason is required"));
    }
    let writer = acquire_writer(&log).await?;
    let result = reconcile_locked(&log, &writer, outcomes, reason).await;
    let _ = log.release(&writer).await;
    result
}

async fn reconcile_locked(
    log: &Arc<dyn SessionLog>,
    writer: &super::Writer,
    outcomes: BTreeMap<String, ToolResultBlock>,
    reason: &str,
) -> crate::error::Result<()> {
    let projection = SessionProjection::read(&**log).await?;
    let expected: BTreeSet<String> = outcomes.keys().cloned().collect();
    if projection.uncertain_tool_calls != expected {
        return Err(session_error("Supply exactly the unresolved tool outcomes"));
    }
    let Some(mut state) = projection.restore()? else {
        return Err(session_error("No recoverable state baseline"));
    };

    for (id, outcome) in &outcomes {
        if outcome.id.as_deref() != Some(id.as_str()) || outcome.is_suspended() {
            return Err(session_error(
                "Mismatched or suspended reconciliation result",
            ));
        }
    }

    let run = uuid::Uuid::new_v4().to_string();
    let mut seq = projection.as_of_seq;
    let mut facts = Vec::new();
    let mut push = |event_type: &str,
                    payload: serde_json::Value,
                    facts: &mut Vec<SessionEvent>|
     -> crate::error::Result<()> {
        seq += 1;
        facts.push(SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            seq,
            chrono::Utc::now().timestamp_millis(),
            event_type,
            Some(run.clone()),
            None,
            true,
            payload.to_string(),
        )?);
        Ok(())
    };

    for (_, outcome) in &outcomes {
        let message = MsgBuilder::new()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(outcome.clone())])
            .build();
        state.context.push(message.clone());
        push(
            "tool/result",
            serde_json::json!({ "message": message, "reconciled": true }),
            &mut facts,
        )?;
    }
    push(
        "recovery/applied",
        serde_json::json!({
            "resolvedToolCallIds": outcomes.keys().collect::<Vec<_>>(),
            "reason": reason,
            "outcomes": outcomes,
            "interruptedRuns": projection.active_runs,
        }),
        &mut facts,
    )?;
    push(
        "state/checkpoint",
        serde_json::json!({
            "stateJson": state.to_json()?,
            "projectorVersion": 1,
        }),
        &mut facts,
    )?;

    log.commit(
        writer,
        &uuid::Uuid::new_v4().to_string(),
        projection.as_of_seq,
        &facts,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::message::{Msg, ToolResultBlock};
    use crate::session::{
        pending_interactions, AtomicSessionStorage, JournalSessionLog, MemorySessionStorage,
        SessionRecorder, SessionTurns,
    };

    fn journal() -> Arc<dyn SessionLog> {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        Arc::new(JournalSessionLog::new(storage, "s1"))
    }

    fn state() -> crate::state::AgentState {
        crate::state::AgentState::builder()
            .session_id("s1")
            .add_message(Msg::user("place order"))
            .build()
    }

    /// Seeds a suspended turn with one pending external-execution request.
    ///
    /// No `tool/dispatch` is recorded: the call was suspended, not started, so its outcome is
    /// known (suspended) rather than uncertain.
    async fn suspended(log: &Arc<dyn SessionLog>) -> SessionRecorder {
        let recorder = SessionRecorder::open(Arc::clone(log), Some("t1"))
            .await
            .unwrap();
        recorder.start(&[], &state()).await.unwrap();
        recorder
            .checkpoint_now(&state(), "before_model")
            .await
            .unwrap();
        let mut data = crate::event::EventData::default();
        data.fields.insert(
            "toolCalls".into(),
            serde_json::json!([ToolUseBlock::new(
                "call-1",
                "create_order",
                Default::default()
            )]),
        );
        recorder
            .observe(&crate::event::AgentEvent::RequireExternalExecution(data))
            .unwrap();
        // `observe` only buffers; commit the request so `pending_interactions` can see it.
        recorder.flush().await.unwrap();
        recorder
    }

    /// Seeds a turn whose tool call was dispatched but never produced a result.
    async fn dispatched(log: &Arc<dyn SessionLog>) -> SessionRecorder {
        let recorder = suspended(log).await;
        recorder
            .record(
                "tool/dispatch",
                &serde_json::json!({ "toolCallId": "call-1" }),
            )
            .await
            .unwrap();
        recorder
    }

    #[tokio::test]
    async fn cancelling_a_pending_interaction_interrupts_the_call() {
        let log = journal();
        let recorder = suspended(&log).await;
        recorder.close().await;

        cancel_pending(Arc::clone(&log), "t1").await.unwrap();

        assert!(pending_interactions(&*log).await.unwrap().is_empty());
        let turns = SessionTurns::read(&*log).await.unwrap();
        assert_eq!(turns.latest().unwrap().status, "cancelled");

        let projection = SessionProjection::read(&*log).await.unwrap();
        assert!(projection.active_runs.is_empty());
        let restored = projection.restore().unwrap().unwrap();
        let interrupted: Vec<_> = restored
            .context
            .iter()
            .flat_map(|m| m.tool_result_blocks())
            .filter(|r| r.state == ToolResultState::Interrupted)
            .collect();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].id.as_deref(), Some("call-1"));
    }

    #[tokio::test]
    async fn cancel_is_idempotent_and_rejects_unresolved_tool_outcomes() {
        let log = journal();
        let recorder = suspended(&log).await;
        recorder.close().await;
        cancel_pending(Arc::clone(&log), "t1").await.unwrap();
        // A second cancellation is a no-op.
        cancel_pending(Arc::clone(&log), "t1").await.unwrap();

        // A turn with an unresolved dispatch cannot be cancelled.
        let fresh = journal();
        let recorder = dispatched(&fresh).await;
        recorder.close().await;
        let err = cancel_pending(Arc::clone(&fresh), "t1").await.unwrap_err();
        assert!(err.to_string().contains("Reconcile"), "{err}");
    }

    #[tokio::test]
    async fn abandoning_settles_unknown_tool_outcomes_without_replaying_them() {
        // Regression: a run interrupted mid-tool left `tool/dispatch` unresolved, so
        // `cancel_pending` refused and the turn could never be closed. The next prompt then
        // waited forever. `abandon_turn` records the unknown outcome as `interrupted` (never
        // as success) and clears the uncertainty.
        let log = journal();
        let recorder = dispatched(&log).await;
        recorder.close().await;

        let closed = abandon_turn(Arc::clone(&log), "t1").await.unwrap();
        assert!(closed, "an open turn must report that it was closed");

        let projection = SessionProjection::read(&*log).await.unwrap();
        assert!(
            projection.uncertain_tool_calls.is_empty(),
            "{:?}",
            projection.uncertain_tool_calls
        );
        let turns = SessionTurns::read(&*log).await.unwrap();
        assert_eq!(turns.latest().unwrap().status, "cancelled");
        assert!(turns.latest().unwrap().ended());

        let restored = projection.restore().unwrap().unwrap();
        let abandoned: Vec<_> = restored
            .context
            .iter()
            .flat_map(|m| m.tool_result_blocks())
            .filter(|r| r.state == ToolResultState::Interrupted)
            .collect();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].id.as_deref(), Some("call-1"));
    }

    #[tokio::test]
    async fn abandoning_a_settled_turn_is_a_no_op() {
        let log = journal();
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
            .await
            .unwrap();
        recorder.start(&[], &state()).await.unwrap();
        recorder.finish(&state(), "completed").await.unwrap();

        let closed = abandon_turn(Arc::clone(&log), "t1").await.unwrap();
        assert!(!closed, "a completed turn must not be cancelled");
        let turns = SessionTurns::read(&*log).await.unwrap();
        assert_eq!(turns.latest().unwrap().status, "completed");
    }

    #[tokio::test]
    async fn abandoning_waits_for_a_busy_writer_instead_of_failing() {
        // The run holds the writer lease while it unwinds; a single failed acquire used to
        // leave the turn open forever.
        let log = journal();
        let recorder = dispatched(&log).await;
        recorder.close().await;

        let held = Arc::clone(&log);
        let holder = tokio::spawn(async move {
            let writer = held
                .acquire("run-busy", Duration::from_secs(60))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            held.release(&writer).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let closed = abandon_turn(Arc::clone(&log), "t1").await.unwrap();
        assert!(closed, "the retry must succeed once the writer is released");
        holder.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_rejects_unapplied_steering_for_the_turn() {
        let log = journal();
        let recorder = suspended(&log).await;
        recorder.close().await;
        let inbox = SessionInbox::new(Arc::clone(&log)).unwrap();
        inbox
            .accept(
                &super::super::Command::new(
                    "s1",
                    "steer",
                    Some("t1".into()),
                    vec![Msg::user("guidance")],
                    vec![],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        cancel_pending(Arc::clone(&log), "t1").await.unwrap();
        let snapshot = inbox.snapshot().await.unwrap();
        assert!(snapshot.rejected.contains_key("s1"));
        assert_eq!(snapshot.rejected["s1"], "turn_cancelled");
    }

    #[tokio::test]
    async fn reconciliation_requires_exactly_the_uncertain_calls() {
        let log = journal();
        let recorder = dispatched(&log).await;
        recorder.close().await;

        let mut wrong = BTreeMap::new();
        wrong.insert(
            "other".to_string(),
            ToolResultBlock::new("other", "create_order", vec![]),
        );
        let err = reconcile(Arc::clone(&log), wrong, "checked")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exactly the unresolved"), "{err}");
    }

    #[tokio::test]
    async fn reconciliation_records_the_verified_outcome_and_clears_uncertainty() {
        let log = journal();
        // Reconciliation applies to a call that was dispatched but whose outcome is unknown.
        let recorder = dispatched(&log).await;
        recorder.close().await;

        let mut outcomes = BTreeMap::new();
        outcomes.insert(
            "call-1".to_string(),
            ToolResultBlock::new(
                "call-1",
                "create_order",
                vec![ContentBlock::Text(TextBlock::new(
                    "Order ORD-42 confirmed created",
                ))],
            )
            .with_state(ToolResultState::Success),
        );
        reconcile(Arc::clone(&log), outcomes, "Verified order-system records")
            .await
            .unwrap();

        let projection = SessionProjection::read(&*log).await.unwrap();
        assert!(projection.uncertain_tool_calls.is_empty());
        let restored = projection.restore().unwrap().unwrap();
        assert!(restored
            .context
            .iter()
            .flat_map(|m| m.tool_result_blocks())
            .any(|r| r.id.as_deref() == Some("call-1")));
    }

    #[tokio::test]
    async fn reconciliation_rejects_suspended_or_mismatched_results() {
        let log = journal();
        let recorder = dispatched(&log).await;
        recorder.close().await;

        let mut suspended = BTreeMap::new();
        suspended.insert(
            "call-1".to_string(),
            ToolResultBlock::new("call-1", "create_order", vec![])
                .with_state(ToolResultState::Suspended),
        );
        assert!(reconcile(Arc::clone(&log), suspended, "checked")
            .await
            .is_err());

        let mut mismatched = BTreeMap::new();
        mismatched.insert(
            "call-1".to_string(),
            ToolResultBlock::new("different", "create_order", vec![]),
        );
        assert!(reconcile(Arc::clone(&log), mismatched, "checked")
            .await
            .is_err());

        let mut blank = BTreeMap::new();
        blank.insert(
            "call-1".to_string(),
            ToolResultBlock::new("call-1", "create_order", vec![]),
        );
        let err = reconcile(Arc::clone(&log), blank, "  ").await.unwrap_err();
        assert!(err.to_string().contains("reason is required"), "{err}");
    }
}
