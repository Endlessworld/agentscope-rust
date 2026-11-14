//! Host-facing façade over one thread: composition only, no policy of its own.
//!
//! A host that used to hold a full session object (a turn loop, an inbox and a receipt cache)
//! needs a place to call, not a second implementation. Everything here is a delegation: to
//! [`DurableRuntime`] for the steps the runtime owns (admission, one tick, control) and to the
//! backend it drives for pure reads and for the ending a lease holder observed. The invariants
//! (receipt before fence, revision CAS, epoch fencing, terminal immutability) stay where they are
//! enforced — in the backend — and nothing is re-decided here.
//!
//! Deliberately **not** provided, because there is no honest implementation yet:
//!
//! * `wakeup` — a turn without input. Durable admission freezes the input for a request id, so a
//!   host submits an explicit empty input instead of getting a second concept.
//! * `respond` / pending interactions — the durable record has signals, not interaction records;
//!   a host answers a wait by accepting a signal with the waiting key.
//! * an ACP-shaped `Task` — `TurnView` (status/output/error) plus the receipt is the source, and
//!   projecting it is the caller's business.
//! * live event push — `events(cursor)` is a committed read, not a stream.

use std::sync::Arc;

use serde_json::Value;

use crate::error::Result;
use crate::harness_adapter::settle_turn;
use crate::runtime::{DurableRuntime, TickOutcome};
use crate::session_backend::SessionBackend;
use crate::types::{
    CommandReceipt, EventPage, Lease, Signal, ThreadKey, ThreadStateView, TurnPage, TurnReceipt,
    TurnStatus, TurnView,
};

/// The session surface a host drives: admission, one tick, control, ending and views.
pub struct DurableSession {
    runtime: DurableRuntime,
    thread: ThreadKey,
}

impl DurableSession {
    pub fn new(runtime: DurableRuntime, thread: ThreadKey) -> Self {
        Self { runtime, thread }
    }

    pub fn thread(&self) -> &ThreadKey {
        &self.thread
    }

    pub fn backend(&self) -> &Arc<dyn SessionBackend> {
        self.runtime.backend()
    }

    // ---- admission ----

    /// Admits one turn; the same request id and input answers with the same receipt.
    pub async fn submit(&self, request_id: &str, input: Value) -> Result<TurnReceipt> {
        self.runtime.submit(&self.thread, request_id, input).await
    }

    /// What a request id was admitted as, if it was admitted at all.
    pub async fn admission(&self, request_id: &str) -> Result<Option<TurnReceipt>> {
        self.runtime
            .backend()
            .admission(&self.thread.session, request_id)
            .await
    }

    // ---- drive ----

    /// One unit of progress; the caller decides when to call again.
    pub async fn tick(&self) -> Result<TickOutcome> {
        self.runtime.tick(&self.thread).await
    }

    // ---- control ----

    /// Ends the turn as `Cancelled`: terminal, so it can never be resumed.
    pub async fn cancel(&self, turn_id: &str, request_id: &str) -> Result<()> {
        self.runtime.cancel(&self.thread, turn_id, request_id).await
    }

    /// Interrupts the turn and keeps it resumable (`Paused`, position and tools kept).
    pub async fn suspend(&self, turn_id: &str, request_id: &str) -> Result<TurnView> {
        self.runtime
            .suspend(&self.thread, turn_id, request_id)
            .await
    }

    /// Continues a suspended turn; a `Paused` turn is never terminal, so it resumes in place.
    pub async fn resume(&self, turn_id: &str, request_id: &str) -> Result<TurnView> {
        self.runtime.resume(&self.thread, turn_id, request_id).await
    }

    /// Folds a message into the turn a writer is executing.
    pub async fn steer(
        &self,
        turn_id: &str,
        request_id: &str,
        text: impl Into<String>,
    ) -> Result<CommandReceipt> {
        self.runtime
            .steer(&self.thread, turn_id, request_id, text)
            .await
    }

    /// Same delivery as [`Self::steer`]; the key tells the two apart.
    pub async fn inject(
        &self,
        turn_id: &str,
        request_id: &str,
        text: impl Into<String>,
    ) -> Result<CommandReceipt> {
        self.runtime
            .inject(&self.thread, turn_id, request_id, text)
            .await
    }

    /// Accepts a signal for a turn (a wait key, a host answer, an extension key).
    pub async fn signal(&self, turn_id: &str, signal: Signal) -> Result<()> {
        self.runtime.signal(&self.thread, turn_id, signal).await
    }

    // ---- ending ----

    /// Writes the ending the lease holder observed; the lease is the caller's, not the façade's,
    /// because a tick never leaves one behind.
    pub async fn settle(&self, lease: &Lease, status: TurnStatus, output: Value) -> Result<()> {
        settle_turn(self.runtime.backend().as_ref(), lease, status, output, None).await
    }

    // ---- views ----

    pub async fn turn(&self, turn_id: &str) -> Result<TurnView> {
        self.runtime.turn(&self.thread, turn_id).await
    }

    pub async fn turns(&self, cursor: Option<&str>, limit: usize) -> Result<TurnPage> {
        self.runtime.turns(&self.thread, cursor, limit).await
    }

    /// The turn a writer would work on, if any: the first non-terminal turn of the thread.
    pub async fn active(&self) -> Result<Option<TurnView>> {
        let state = self.state().await?;
        let Some(turn_id) = state.current_turn_id else {
            return Ok(None);
        };
        self.turn(&turn_id).await.map(Some)
    }

    pub async fn state(&self) -> Result<ThreadStateView> {
        self.runtime.backend().thread_state(&self.thread).await
    }

    pub async fn events(&self, cursor: Option<&str>, limit: usize) -> Result<EventPage> {
        self.runtime
            .events(&self.thread.session, cursor, limit)
            .await
    }
}
