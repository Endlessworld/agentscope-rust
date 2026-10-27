//! Offline migration and fork primitives, mirroring `SessionMigration`.
//!
//! A destination may be seeded with one baseline checkpoint only. Checkpoints never invent
//! past facts: an imported baseline declares `baseline_only` coverage, so the destination does
//! not pretend to replay history the source actually had.

use std::sync::Arc;
use std::time::Duration;

use super::{session_error, SessionEvent, SessionKey, SessionLog, SessionProjection};
use crate::state::AgentState;

/// The migration writer lease.
const LEASE: Duration = Duration::from_secs(120);

/// A state exported from a source journal together with its committed prefix.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportedState {
    pub state: AgentState,
    pub as_of_seq: u64,
}

/// Seeds an empty destination with one baseline checkpoint.
///
/// Re-importing the identical baseline is a no-op; importing into a non-empty destination is
/// rejected instead of merging unrelated histories.
pub async fn import_baseline(
    target: Arc<dyn SessionLog>,
    state: &AgentState,
    source: &str,
    source_version: u64,
) -> crate::error::Result<()> {
    let json = state.to_json()?;
    let hash = super::JournalSessionLog::hash(json.as_bytes());
    let batch = format!("baseline-{hash}");
    let writer = target
        .acquire(&uuid::Uuid::new_v4().to_string(), LEASE)
        .await?;
    let result = async {
        let head = target.head().await?;
        if head.seq > 0 {
            let first = target.read_after(0, 1).await?;
            let is_identical = first.first().is_some_and(|event| {
                event.event_type == "migration/baseline"
                    && event
                        .data()
                        .ok()
                        .and_then(|data| {
                            data.get("sourceHash")
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                        })
                        .as_deref()
                        == Some(hash.as_str())
            });
            if is_identical {
                return Ok(());
            }
            return Err(session_error("Migration destination is not empty"));
        }
        let payload = serde_json::json!({
            "stateJson": json,
            "coverage": "baseline_only",
            "source": source,
            "sourceVersion": source_version,
            "sourceHash": hash,
        });
        let event = SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            1,
            chrono::Utc::now().timestamp_millis(),
            "migration/baseline",
            Some(writer.owner.clone()),
            None,
            true,
            payload.to_string(),
        )?;
        target.commit(&writer, &batch, 0, &[event]).await?;
        Ok(())
    }
    .await;
    let _ = target.release(&writer).await;
    result
}

/// Exports the recoverable state of a paused source journal.
pub async fn export_state(source: &dyn SessionLog) -> crate::error::Result<AgentState> {
    Ok(export_snapshot(source).await?.state)
}

/// Exports the recoverable state and its committed prefix.
///
/// Refuses a source with an active writer, an uncertain tool outcome, a live run or a pending
/// interaction: exporting such a session would fork an execution that never finished.
pub async fn export_snapshot(source: &dyn SessionLog) -> crate::error::Result<ExportedState> {
    if source.head().await?.owner.is_some() {
        return Err(session_error("Pause the source session before migration"));
    }
    let writer = source
        .acquire(&uuid::Uuid::new_v4().to_string(), LEASE)
        .await?;
    let result = async {
        let projection = SessionProjection::read(source).await?;
        if !projection.uncertain_tool_calls.is_empty() || !projection.active_runs.is_empty() {
            return Err(session_error(
                "Reconcile interrupted execution before exporting",
            ));
        }
        if !super::pending_interactions(source).await?.is_empty() {
            return Err(session_error(
                "Resolve pending interactions before exporting or forking",
            ));
        }
        let state = projection
            .restore()?
            .ok_or_else(|| session_error("No recoverable baseline"))?;
        Ok(ExportedState {
            state,
            as_of_seq: projection.as_of_seq,
        })
    }
    .await;
    let _ = source.release(&writer).await;
    result
}

/// Forks a paused source into a destination key with a new logical identity.
pub async fn fork(
    source: &dyn SessionLog,
    destination: Arc<dyn SessionLog>,
    destination_key: &SessionKey,
    source_reference: &str,
) -> crate::error::Result<ExportedState> {
    let snapshot = export_snapshot(source).await?;
    let mut value: serde_json::Value = serde_json::from_str(&snapshot.state.to_json()?)?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "session_id".into(),
            serde_json::json!(destination_key.session_id),
        );
        object.insert("user_id".into(), serde_json::json!(destination_key.user_id));
    }
    let forked = AgentState::from_json_str(&serde_json::to_string(&value)?)?;
    import_baseline(
        destination,
        &forked,
        &format!("fork:{source_reference}"),
        snapshot.as_of_seq,
    )
    .await?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Msg;
    use crate::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionRecorder,
    };

    fn journal() -> Arc<dyn SessionLog> {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        Arc::new(JournalSessionLog::new(storage, "s1"))
    }

    fn state() -> AgentState {
        AgentState::builder()
            .session_id("s1")
            .user_id("alice")
            .add_message(Msg::user("hello"))
            .build()
    }

    async fn paused_source() -> Arc<dyn SessionLog> {
        let log = journal();
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
            .await
            .unwrap();
        recorder.start(&[], &state()).await.unwrap();
        recorder
            .checkpoint_now(&state(), "before_model")
            .await
            .unwrap();
        recorder.finish(&state(), "completed").await.unwrap();
        log
    }

    #[tokio::test]
    async fn import_seeds_one_baseline_and_is_idempotent() {
        let target = journal();
        // Reuse one instance: `AgentState` carries a generated reply id, so two separate
        // instances serialize to different JSON and would not be the same baseline.
        let base = state();
        import_baseline(Arc::clone(&target), &base, "legacy_store", 7)
            .await
            .unwrap();
        assert_eq!(target.head().await.unwrap().seq, 1);

        // Importing the same baseline again is a no-op.
        import_baseline(Arc::clone(&target), &base, "legacy_store", 7)
            .await
            .unwrap();
        assert_eq!(target.head().await.unwrap().seq, 1);

        let projection = SessionProjection::read(&*target).await.unwrap();
        let restored = projection.restore().unwrap().unwrap();
        assert_eq!(restored.user_id.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn import_into_a_non_empty_destination_is_rejected() {
        let target = journal();
        import_baseline(Arc::clone(&target), &state(), "a", 1)
            .await
            .unwrap();
        let mut different = state();
        different.summary = "other baseline".into();
        let err = import_baseline(Arc::clone(&target), &different, "b", 2)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
    }

    #[tokio::test]
    async fn export_returns_the_recovered_state() {
        let source = paused_source().await;
        let snapshot = export_snapshot(&*source).await.unwrap();
        assert_eq!(snapshot.state.user_id.as_deref(), Some("alice"));
        assert!(snapshot.as_of_seq > 0);
        assert_eq!(export_state(&*source).await.unwrap().session_id, "s1");
    }

    #[tokio::test]
    async fn export_refuses_a_source_with_an_active_writer() {
        let source = paused_source().await;
        let writer = source
            .acquire("busy", Duration::from_secs(60))
            .await
            .unwrap();
        let err = export_snapshot(&*source).await.unwrap_err();
        assert!(
            err.to_string().contains("Pause the source session"),
            "{err}"
        );
        source.release(&writer).await.unwrap();
    }

    #[tokio::test]
    async fn export_refuses_pending_interactions() {
        let source = journal();
        let recorder = SessionRecorder::open(Arc::clone(&source), Some("t1"))
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
            serde_json::json!([crate::message::ToolUseBlock::new(
                "call-1",
                "create_order",
                Default::default()
            )]),
        );
        recorder
            .observe(&crate::event::AgentEvent::RequireExternalExecution(data))
            .unwrap();
        // End the run (suspended) so the only blocking condition is the pending interaction;
        // export checks live runs before pending interactions, like the Java implementation.
        // `record` commits immediately — `close` never flushes a partially filled buffer.
        recorder
            .record("run/end", &serde_json::json!({ "status": "suspended" }))
            .await
            .unwrap();
        recorder.close().await;

        let err = export_snapshot(&*source).await.unwrap_err();
        assert!(
            err.to_string().contains("Resolve pending interactions"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn fork_rewrites_the_logical_identity() {
        let source = paused_source().await;
        let destination = journal();
        let key = SessionKey::new(Some("bob"), "agent", "forked").unwrap();
        fork(&*source, Arc::clone(&destination), &key, "s1@t1")
            .await
            .unwrap();

        let forked = export_state(&*destination).await.unwrap();
        assert_eq!(forked.session_id, "forked");
        assert_eq!(forked.user_id.as_deref(), Some("bob"));
        // The fork keeps the message content but is a new session.
        assert_eq!(forked.context.len(), 1);

        let head = destination.head().await.unwrap();
        let first = destination.read_after(0, 1).await.unwrap();
        assert_eq!(first[0].event_type, "migration/baseline");
        assert_eq!(head.seq, 1);
        let data = first[0].data().unwrap();
        assert_eq!(data["coverage"], "baseline_only");
        assert!(data["source"].as_str().unwrap().starts_with("fork:"));
    }
}
