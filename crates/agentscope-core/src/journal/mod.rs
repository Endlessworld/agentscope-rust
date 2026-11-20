//! Native journal, mirroring `io.agentscope.core.journal`.
//!
//! A thread is the continuing conversation; a turn is one logical request; a run is one
//! execution of that request. The native journal records immutable facts (events) as a
//! fenced, checksummed commit chain, so recovery never depends on listing storage and a
//! lost acknowledge can be re-delivered instead of re-executed.

pub mod atomic;
pub mod event;
pub mod execution;
pub mod export;
pub mod header;
pub mod in_memory;
pub mod inbox;
pub mod log;
pub mod migration;
pub mod models;
pub mod recorder;
pub mod recovery;
pub mod snapshot;
pub mod state_access;
pub mod storage;
pub mod views;

pub use atomic::AtomicThreadJournal;
pub use event::{JournalEvent, JournalEventCodec, JournalEventCodecRegistry, JournalEventTypes};
pub use execution::{ExecutionBaseline, SessionExecution, EXECUTION_CONTEXT_KEY};
pub use export::{
    JournalExportSink, JournalExportSinkHandle, JournalExporter, JOURNAL_EXPORT_SINK_CONTEXT_KEY,
};
pub use header::{SessionHeader, SessionHistoryMode};
pub use in_memory::{InMemoryThreadJournalStore, MemoryJournalStorage};
pub use inbox::{Command, InboxSnapshot, SessionInbox};
pub use log::{Head, ThreadJournal, ThreadJournalStore, Writer};
pub use migration::{export_snapshot, export_state, fork, import_baseline, ExportedState};
pub use models::{
    interrupted_model_output, policy_from_context, wrap as wrap_model, InterruptedOutput,
    ModelCallPolicy, ModelCallPolicyHandle, RecordedModels, MODEL_POLICY_CONTEXT_KEY,
};
pub use recorder::{
    outcome, InvocationRecorder, RecorderBaseline, RecordingPolicy, SessionMetrics,
    PARENT_CONTEXT_KEY, RECORDER_CONTEXT_KEY, TURN_ID_CONTEXT_KEY,
};
pub use recovery::{abandon_turn, cancel_pending, reconcile};
pub use snapshot::{
    JournalRetention, JournalSnapshot, SnapshotPointer, SNAPSHOT_EVERY, SNAPSHOT_EVERY_EVENTS,
    SNAPSHOT_FORMAT, SNAPSHOT_TRACKED_RUNS,
};
pub use state_access::{
    read as read_snapshot, replace as replace_state, update as update_state, Snapshot,
};
pub use storage::{AtomicJournalStorage, StorageValue};
pub use views::{
    admission_views, apply_pending_event, committed_tool_results, pending_interactions,
    realign_tool_results, recovery_views, transcript, transcript_entries, transcript_jsonl,
    JournalProjection, Transcript, TranscriptEntry, Turn, TurnViews, UncertaintyTracker,
};

/// Error message prefix used by session persistence failures.
///
/// Persistence failures must propagate and must never be converted into a successful
/// model/tool result. Mirrors Java `SessionLogException`.
pub(crate) const JOURNAL_ERROR_PREFIX: &str = "journal: ";

/// Builds a session-persistence error.
pub fn journal_error(message: impl Into<String>) -> crate::error::Error {
    crate::error::Error::Journal(format!("{JOURNAL_ERROR_PREFIX}{}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_error_carries_prefix() {
        let err = journal_error("boom");
        assert!(matches!(err, crate::error::Error::Journal(_)));
        assert!(err.to_string().contains("journal: boom"));
    }
}
