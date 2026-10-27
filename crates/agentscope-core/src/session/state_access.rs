//! Read-only snapshots and fenced administrative state changes against native history,
//! mirroring `SessionStateAccess`.
//!
//! Mutations reload the committed prefix under the writer lease, refuse to run while a tool
//! outcome is unresolved, and publish every changed fact together with a checkpoint.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use super::{pending_interactions, session_error, SessionEvent, SessionLog, SessionProjection};
use crate::message::{ContentBlock, Msg, MsgRole};
use crate::state::AgentState;

/// The administrative writer lease.
const LEASE: Duration = Duration::from_secs(120);

/// A detached state and the committed prefix it was recovered from.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub as_of_seq: u64,
    pub state: AgentState,
}

/// Reads one committed prefix without acquiring a writer or changing session history.
pub async fn read<F>(log: &dyn SessionLog, initial_state: F) -> crate::error::Result<Snapshot>
where
    F: FnOnce() -> AgentState,
{
    let projection = SessionProjection::read(log).await?;
    let state = projection.restore()?.unwrap_or_else(initial_state);
    Ok(Snapshot {
        as_of_seq: projection.as_of_seq,
        state,
    })
}

/// Reloads and mutates state while holding the writer lease, then commits a checkpoint.
pub async fn update<F, M>(
    log: Arc<dyn SessionLog>,
    initial_state: F,
    reason: &str,
    mutation: M,
) -> crate::error::Result<Snapshot>
where
    F: Fn() -> AgentState,
    M: FnOnce(&mut AgentState) -> crate::error::Result<()>,
{
    change(&log, &initial_state, reason, None, Some(mutation)).await
}

/// Saves an explicitly edited snapshot only if its committed prefix is still current.
///
/// A stale snapshot is rejected; it never overwrites an execution completed elsewhere.
pub async fn replace<F>(
    log: Arc<dyn SessionLog>,
    initial_state: F,
    reason: &str,
    snapshot: Snapshot,
) -> crate::error::Result<Snapshot>
where
    F: Fn() -> AgentState,
{
    let frozen = AgentState::from_json_str(&snapshot.state.to_json()?)?;
    change(
        &log,
        &initial_state,
        reason,
        Some(Snapshot {
            as_of_seq: snapshot.as_of_seq,
            state: frozen,
        }),
        // `replace` carries no mutation closure; name the closure type so `M` is inferable.
        None::<fn(&mut AgentState) -> crate::error::Result<()>>,
    )
    .await
}

async fn change<F, M>(
    log: &Arc<dyn SessionLog>,
    initial_state: &F,
    reason: &str,
    replacement: Option<Snapshot>,
    mutation: Option<M>,
) -> crate::error::Result<Snapshot>
where
    F: Fn() -> AgentState,
    M: FnOnce(&mut AgentState) -> crate::error::Result<()>,
{
    if reason.trim().is_empty() {
        return Err(session_error("A state change reason is required"));
    }
    let writer = log
        .acquire(&uuid::Uuid::new_v4().to_string(), LEASE)
        .await?;
    let result = change_locked(
        log,
        writer.clone(),
        initial_state,
        reason,
        replacement,
        mutation,
    )
    .await;
    let _ = log.release(&writer).await;
    result
}

async fn change_locked<F, M>(
    log: &Arc<dyn SessionLog>,
    writer: super::Writer,
    initial_state: &F,
    reason: &str,
    replacement: Option<Snapshot>,
    mutation: Option<M>,
) -> crate::error::Result<Snapshot>
where
    F: Fn() -> AgentState,
    M: FnOnce(&mut AgentState) -> crate::error::Result<()>,
{
    let projection = SessionProjection::read(&**log).await?;
    if !projection.uncertain_tool_calls.is_empty() {
        return Err(session_error(
            "Reconcile unresolved tool outcomes before changing session state",
        ));
    }
    if let Some(snapshot) = &replacement {
        if snapshot.as_of_seq != projection.as_of_seq {
            return Err(session_error(
                "Session state changed since inspection; reload or use an atomic update",
            ));
        }
    }
    let mut current = projection.restore()?.unwrap_or_else(initial_state);
    let user_id = current.user_id.clone();
    let session_id = current.session_id.clone();
    let before_json = current.to_json()?;
    let before_value: serde_json::Value = serde_json::from_str(&before_json)?;
    let previous_ids: HashSet<String> = current.context.iter().map(|m| m.id.clone()).collect();

    let mut changed = match replacement {
        Some(snapshot) => snapshot.state,
        None => current.clone(),
    };
    if let Some(mutation) = mutation {
        mutation(&mut changed)?;
    }
    if changed.user_id != user_id || changed.session_id != session_id {
        return Err(session_error(
            "State changes must preserve session identity",
        ));
    }
    let state_json = changed.to_json()?;
    let after_value: serde_json::Value = serde_json::from_str(&state_json)?;

    let context_changed = before_value.get("context") != after_value.get("context");
    if context_changed && !pending_interactions(&**log).await?.is_empty() {
        return Err(session_error(
            "Resolve pending interactions before replacing context",
        ));
    }

    let mut facts: Vec<SessionEvent> = Vec::new();
    let mut next_seq = projection.as_of_seq;
    let mut push = |event_type: &str, payload: serde_json::Value| -> crate::error::Result<()> {
        next_seq += 1;
        facts.push(SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            next_seq,
            chrono::Utc::now().timestamp_millis(),
            event_type,
            None,
            None,
            true,
            payload.to_string(),
        )?);
        Ok(())
    };

    if context_changed {
        push(
            "context/replaced",
            serde_json::json!({
                "reason": reason,
                "messages": changed.context,
                "previousAsOfSeq": projection.as_of_seq,
            }),
        )?;
    }
    for message in &changed.context {
        if previous_ids.contains(&message.id) {
            continue;
        }
        push(
            message_event_type(message),
            serde_json::json!({ "message": message }),
        )?;
    }
    for (field, event_type) in [
        ("tasks_context", "task/changed"),
        ("plan_mode_context", "plan/changed"),
        ("permission_context", "permission/changed"),
    ] {
        let after = after_value.get(field);
        if let Some(value) = after {
            if Some(value) != before_value.get(field) {
                push(event_type, serde_json::json!({ "state": value }))?;
            }
        }
    }
    push(
        "state/checkpoint",
        serde_json::json!({
            "stateJson": state_json,
            "projectorVersion": 1,
            "reason": reason,
        }),
    )?;

    let head = log
        .commit(
            &writer,
            &uuid::Uuid::new_v4().to_string(),
            projection.as_of_seq,
            &facts,
        )
        .await?;
    current = AgentState::from_json_str(&state_json)?;
    Ok(Snapshot {
        as_of_seq: head.seq,
        state: current,
    })
}

fn message_event_type(message: &Msg) -> &'static str {
    if message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult(_)))
    {
        "tool/result"
    } else if message.role == MsgRole::Assistant {
        "message/assistant"
    } else if message.role == MsgRole::System {
        "message/system"
    } else {
        "message/user"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionRecorder,
    };

    fn journal() -> Arc<dyn SessionLog> {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        Arc::new(JournalSessionLog::new(storage, "s1"))
    }

    fn initial() -> AgentState {
        crate::state::AgentState::builder()
            .session_id("s1")
            .user_id("alice")
            .add_message(Msg::user("hello"))
            .build()
    }

    async fn seeded() -> Arc<dyn SessionLog> {
        let log = journal();
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
            .await
            .unwrap();
        let state = initial();
        recorder.start(&[], &state).await.unwrap();
        recorder
            .checkpoint_now(&state, "before_model")
            .await
            .unwrap();
        recorder.close().await;
        log
    }

    #[tokio::test]
    async fn read_returns_the_committed_state_without_a_writer() {
        let log = seeded().await;
        let snapshot = read(&*log, || AgentState::builder().session_id("s1").build())
            .await
            .unwrap();
        assert_eq!(snapshot.state.user_id.as_deref(), Some("alice"));
        assert!(snapshot.as_of_seq > 0);
        // Reading never acquires the writer lease.
        let writer = log.acquire("probe", Duration::from_secs(10)).await.unwrap();
        log.release(&writer).await.unwrap();
    }

    #[tokio::test]
    async fn update_commits_a_checkpoint_and_keeps_identity() {
        let log = seeded().await;
        let snapshot = update(Arc::clone(&log), initial, "operator_note", |state| {
            state.summary = "reviewed".into();
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(snapshot.state.summary, "reviewed");
        assert_eq!(snapshot.state.user_id.as_deref(), Some("alice"));

        let reread = read(&*log, initial).await.unwrap();
        assert_eq!(reread.state.summary, "reviewed");
    }

    #[tokio::test]
    async fn update_rejects_identity_changes_and_blank_reasons() {
        let log = seeded().await;
        let err = update(Arc::clone(&log), initial, "rename", |state| {
            state.session_id = "other".into();
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("preserve session identity"),
            "{err}"
        );

        let err = update(Arc::clone(&log), initial, "   ", |state| {
            state.summary = "x".into();
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("reason is required"), "{err}");
    }

    #[tokio::test]
    async fn replacing_context_records_the_reason() {
        let log = seeded().await;
        let snapshot = update(Arc::clone(&log), initial, "trim_history", |state| {
            state.context.clear();
            Ok(())
        })
        .await
        .unwrap();
        assert!(snapshot.state.context.is_empty());

        let head = log.head().await.unwrap();
        let events = log.scan(0, head.seq).await.unwrap();
        let replaced = events
            .iter()
            .find(|e| e.event_type == "context/replaced")
            .expect("context/replaced recorded");
        assert_eq!(replaced.data().unwrap()["reason"], "trim_history");
    }

    #[tokio::test]
    async fn replace_accepts_a_current_snapshot_and_rejects_a_stale_one() {
        let log = seeded().await;
        let snapshot = read(&*log, initial).await.unwrap();
        let mut edited = snapshot.clone();
        edited.state.summary = "edited".into();
        let saved = replace(Arc::clone(&log), initial, "manual_edit", edited)
            .await
            .unwrap();
        assert_eq!(saved.state.summary, "edited");

        // The earlier snapshot is now stale: its committed prefix moved on.
        let mut stale = snapshot.clone();
        stale.state.summary = "stale".into();
        let err = replace(Arc::clone(&log), initial, "manual_edit", stale)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("changed since inspection"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn context_replacement_is_blocked_while_an_interaction_is_pending() {
        let log = seeded().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
            .await
            .unwrap();
        let mut data = crate::event::EventData::default();
        data.fields.insert(
            "toolCalls".into(),
            serde_json::json!([crate::message::ToolUseBlock::new(
                "call-1",
                "create_order",
                Default::default()
            )]),
        );
        recorder
            .observe(&crate::event::AgentEvent::RequireExternalExecution(data))
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.close().await;

        let err = update(Arc::clone(&log), initial, "trim", |state| {
            state.context.clear();
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("Resolve pending interactions"),
            "{err}"
        );
    }
}
