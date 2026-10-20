//! Explicit operator reconciliation, mirroring `SessionRecovery`.
//!
//! An action whose outcome is unknown must never be re-executed automatically. The operator
//! records what was actually found; only then can the session continue.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use super::{
    pending_interactions, session_error, SessionEvent, SessionInbox, SessionLog, SessionProjection,
    SessionTurns,
};
use crate::message::{
    ContentBlock, MsgBuilder, MsgRole, TextBlock, ToolCallState, ToolResultBlock, ToolResultState,
    ToolUseBlock,
};

/// The reconciliation writer lease.
const LEASE: Duration = Duration::from_secs(120);

/// Message recorded when a caller cancels a pending interaction.
pub const CANCELLED_PENDING_MESSAGE: &str = "Caller cancelled this pending interaction; external \
     side effects, if any, were not verified.";

/// Cancels an idle pending interaction without inventing success or replaying external work.
///
/// Each pending request for `turn_id` becomes an `interrupted` tool result, the corresponding
/// tool calls are marked finished, a checkpoint is saved and the turn is cancelled.
pub async fn cancel_pending(
    log: Arc<dyn SessionLog>,
    turn_id: &str,
) -> crate::error::Result<()> {
    let writer = log
        .acquire(&uuid::Uuid::new_v4().to_string(), LEASE)
        .await?;
    let result = cancel_pending_locked(&log, &writer, turn_id).await;
    let _ = log.release(&writer).await;
    result
}

async fn cancel_pending_locked(
    log: &Arc<dyn SessionLog>,
    writer: &super::Writer,
    turn_id: &str,
) -> crate::error::Result<()> {
    let projection = SessionProjection::read(&**log).await?;
    if !projection.uncertain_tool_calls.is_empty() {
        return Err(session_error(
            "Reconcile uncertain tool outcomes before cancelling this interrupted turn",
        ));
    }
    let Some(mut state) = projection.restore()? else {
        return Ok(());
    };

    let head = log.head().await?.seq;
    for event in log.scan(0, head).await? {
        if event.event_type == "turn/cancelled" && event.turn_id.as_deref() == Some(turn_id) {
            reject_steering(log, turn_id).await?;
            return Ok(());
        }
    }

    let pending = pending_interactions(&**log).await?;
    let mut cancelled_ids = BTreeSet::new();
    let mut facts = Vec::new();
    let mut seq = projection.as_of_seq;
    let run = writer.owner.clone();
    let mut push = |event_type: &str, payload: serde_json::Value,
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
            vec![ContentBlock::Text(TextBlock::new(CANCELLED_PENDING_MESSAGE))],
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
    if !projection.active_runs.is_empty() {
        push(
            "recovery/applied",
            serde_json::json!({
                "interruptedRuns": projection.active_runs,
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
    reject_steering(&log, turn_id).await
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
    let writer = log
        .acquire(&uuid::Uuid::new_v4().to_string(), LEASE)
        .await?;
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
        return Err(session_error(
            "Supply exactly the unresolved tool outcomes",
        ));
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
    let mut push = |event_type: &str, payload: serde_json::Value,
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
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionRecorder, SessionTurns,
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
        let recorder = SessionRecorder::open(Arc::clone(log), Some("t1")).await.unwrap();
        recorder.start(&[], &state()).await.unwrap();
        recorder.checkpoint_now(&state(), "before_model").await.unwrap();
        let mut data = crate::event::EventData::default();
        data.fields.insert(
            "toolCalls".into(),
            serde_json::json!([ToolUseBlock::new("call-1", "create_order", Default::default())]),
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
            .record("tool/dispatch", &serde_json::json!({ "toolCallId": "call-1" }))
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
                vec![ContentBlock::Text(TextBlock::new("Order ORD-42 confirmed created"))],
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
        assert!(reconcile(Arc::clone(&log), suspended, "checked").await.is_err());

        let mut mismatched = BTreeMap::new();
        mismatched.insert(
            "call-1".to_string(),
            ToolResultBlock::new("different", "create_order", vec![]),
        );
        assert!(reconcile(Arc::clone(&log), mismatched, "checked").await.is_err());

        let mut blank = BTreeMap::new();
        blank.insert(
            "call-1".to_string(),
            ToolResultBlock::new("call-1", "create_order", vec![]),
        );
        let err = reconcile(Arc::clone(&log), blank, "  ").await.unwrap_err();
        assert!(err.to_string().contains("reason is required"), "{err}");
    }
}
