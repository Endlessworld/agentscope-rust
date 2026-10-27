//! Native session log, mirroring `io.agentscope.core.session`.
//!
//! A session is the continuing conversation; a turn is one logical request; a run is one
//! execution of that request. The native journal records immutable facts (events) as a
//! fenced, checksummed commit chain, so recovery never depends on listing storage and a
//! lost acknowledge can be re-delivered instead of re-executed.

pub mod event;
pub mod execution;
pub mod export;
pub mod header;
pub mod inbox;
pub mod in_memory;
pub mod journal;
pub mod key;
pub mod log;
pub mod migration;
pub mod models;
pub mod recorder;
pub mod recovery;
pub mod state_access;
pub mod storage;
pub mod views;

pub use event::{
    SessionEvent, SessionEventCodec, SessionEventCodecRegistry, SessionEventTypes,
};
pub use execution::{ExecutionBaseline, SessionExecution, EXECUTION_CONTEXT_KEY};
pub use export::{
    SessionExportSink, SessionExportSinkHandle, SessionLogExporter,
    SESSION_EXPORT_SINK_CONTEXT_KEY,
};
pub use header::{SessionHeader, SessionHistoryMode};
pub use inbox::{Command, InboxSnapshot, SessionInbox};
pub use recorder::{
    outcome, RecorderBaseline, SessionRecorder, PARENT_CONTEXT_KEY, RECORDER_CONTEXT_KEY,
    TURN_ID_CONTEXT_KEY,
};
pub use models::{
    policy_from_context, wrap as wrap_model, SessionModelPolicy, SessionModelPolicyHandle,
    SessionModels, MODEL_POLICY_CONTEXT_KEY,
};
pub use migration::{
    export_snapshot, export_state, fork, import_baseline, ExportedState,
};
pub use recovery::{abandon_turn, cancel_pending, reconcile};
pub use state_access::{read as read_snapshot, replace as replace_state, update as update_state, Snapshot};
pub use in_memory::{InMemorySessionLogStore, MemorySessionStorage};
pub use journal::JournalSessionLog;
pub use key::SessionKey;
pub use log::{Head, SessionLog, SessionLogStore, Writer};
pub use storage::{AtomicSessionStorage, StorageValue};
pub use views::{
    apply_pending_event, pending_interactions, transcript, transcript_entries, transcript_jsonl,
    SessionProjection, SessionTurns, Transcript, TranscriptEntry, Turn,
};

/// Error message prefix used by session persistence failures.
///
/// Persistence failures must propagate and must never be converted into a successful
/// model/tool result. Mirrors Java `SessionLogException`.
pub(crate) const SESSION_ERROR_PREFIX: &str = "session log: ";

/// Builds a session-persistence error.
pub fn session_error(message: impl Into<String>) -> crate::error::Error {
    crate::error::Error::Session(format!("{SESSION_ERROR_PREFIX}{}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_error_carries_prefix() {
        let err = session_error("boom");
        assert!(matches!(err, crate::error::Error::Session(_)));
        assert!(err.to_string().contains("session log: boom"));
    }
}
