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
//! * a run — `tick` needs an adapter that knows how to execute the harness; the binding lives in
//!   `agentscope-durable-adapter-agentscope`, and publishing events is the adapter's call.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{DurableError, Result};
use crate::event_filter::EventStreamOptions;
use crate::harness_adapter::{settle_turn, EventSink};
use crate::runtime::{DurableRuntime, TickOutcome};
use crate::session_backend::SessionBackend;
use crate::types::{
    CommandReceipt, EventPage, HistoryPage, Interaction, Lease, ModelCallPage, Signal, ThreadKey,
    ThreadStateView, TurnPage, TurnReceipt, TurnStatus, TurnTask, TurnView,
};

/// How many turns one page of [`DurableSession::waiting`] asks for.
const WAIT_PAGE: usize = 100;
/// How many invocations one page of [`DurableSession::task`] reads; a turn has one per process.
const RUN_PAGE: usize = 100;

/// The session surface a host drives: admission, one tick, control, ending and views.
pub struct DurableSession {
    runtime: Arc<DurableRuntime>,
    thread: ThreadKey,
}

impl DurableSession {
    pub fn new(runtime: DurableRuntime, thread: ThreadKey) -> Self {
        Self {
            runtime: Arc::new(runtime),
            thread,
        }
    }

    /// Shares one runtime with another session; a runtime is cloneable and shares its sinks.
    pub fn shared(runtime: Arc<DurableRuntime>, thread: ThreadKey) -> Self {
        Self { runtime, thread }
    }

    pub fn runtime(&self) -> &Arc<DurableRuntime> {
        &self.runtime
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

    /// Admits a turn that carries no input: a wakeup.
    ///
    /// The durable record has no separate concept, so a wakeup is an admission whose input is
    /// `null`. What a turn does without a prompt (injected context, due reminders) is the
    /// adapter's business.
    pub async fn wakeup(&self, request_id: &str) -> Result<TurnReceipt> {
        self.submit(request_id, Value::Null).await
    }

    // ---- drive ----

    /// One unit of progress; the caller decides when to call again.
    pub async fn tick(&self) -> Result<TickOutcome> {
        self.runtime.tick(&self.thread).await
    }

    /// Installs the sink a run's events are published to while the run is still going.
    ///
    /// This is the live stream a host renders; the committed `events(cursor)` page is what a
    /// reader catching up after a restart replays. A host that replaces the sink per prompt does
    /// so through this call.
    pub fn set_event_sink(&self, sink: EventSink) {
        self.runtime.set_event_sink(sink);
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

    /// Answers the interaction a waiting turn is blocked on.
    ///
    /// The wait key *is* the interaction id, so an answer needs no second record: it is the
    /// signal the turn folds in at its next boundary. Answering a turn that waits on something
    /// else, or on nothing, is a `CONFLICT` — that is the validation an answer needs.
    pub async fn respond(
        &self,
        turn_id: &str,
        interaction_id: &str,
        request_id: &str,
        answer: Value,
    ) -> Result<()> {
        match self.turn(turn_id).await?.waiting {
            Some(waiting) if waiting.key == interaction_id => {}
            Some(waiting) => {
                return Err(DurableError::conflict(format!(
                    "Turn waits on `{}`, not on `{interaction_id}`",
                    waiting.key
                )))
            }
            None => return Err(DurableError::conflict("Turn is not waiting for an answer")),
        }
        self.signal(turn_id, Signal::new(request_id, interaction_id, answer)?)
            .await
    }

    /// The turns of this thread that are blocked on an interaction, in queue order.
    pub async fn waiting(&self) -> Result<Vec<TurnView>> {
        let mut waiting = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.turns(cursor.as_deref(), WAIT_PAGE).await?;
            waiting.extend(page.items.into_iter().filter(|turn| turn.waiting.is_some()));
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(waiting),
            }
        }
    }

    /// The interactions the thread's waiting turns are blocked on.
    ///
    /// The wait key *is* the interaction id, so this is the `waiting()` projection a host needs
    /// for a permission prompt: list these, then answer one with [`Self::respond`].
    pub async fn pending_interactions(&self) -> Result<Vec<Interaction>> {
        let waiting = self.waiting().await?;
        Ok(waiting
            .into_iter()
            .filter_map(|turn| {
                let wait = turn.waiting?;
                Some(Interaction {
                    thread_id: turn.thread.thread_id.clone(),
                    run_id: None,
                    step_id: None,
                    id: wait.key.clone(),
                    request_id: wait.key.clone(),
                    turn_id: turn.turn_id.clone(),
                    key: wait.key.clone(),
                    kind: "signal".to_string(),
                    status: "waiting".to_string(),
                    automatic: false,
                    call: Value::Null,
                })
            })
            .collect())
    }

    // ---- ending ----

    /// Writes the ending the lease holder observed; the lease is the caller's, not the façade's,
    /// because a tick never leaves one behind.
    pub async fn settle(
        &self,
        lease: &Lease,
        status: TurnStatus,
        output: Value,
        error: Option<String>,
    ) -> Result<()> {
        settle_turn(
            self.runtime.backend().as_ref(),
            lease,
            status,
            output,
            error,
        )
        .await
    }

    // ---- views ----

    /// A handle to one turn that survives the call that admitted it.
    pub fn turn_handle(&self, turn_id: impl Into<String>) -> TurnHandle {
        TurnHandle {
            runtime: self.runtime.clone(),
            thread: self.thread.clone(),
            turn_id: turn_id.into(),
        }
    }

    pub async fn turn(&self, turn_id: &str) -> Result<TurnView> {
        self.runtime.turn(&self.thread, turn_id).await
    }

    pub async fn turns(&self, cursor: Option<&str>, limit: usize) -> Result<TurnPage> {
        self.runtime.turns(&self.thread, cursor, limit).await
    }

    /// The committed transcript of this thread, rebuilt from the journal's message facts.
    pub async fn history(&self, cursor: Option<&str>, limit: usize) -> Result<HistoryPage> {
        self.runtime.history(&self.thread, cursor, limit).await
    }

    /// The model calls one turn observed, in dispatch order.
    pub async fn model_calls(
        &self,
        turn_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ModelCallPage> {
        self.runtime
            .backend()
            .model_calls(&self.thread, turn_id, cursor, limit)
            .await
    }

    /// The queue-facing projection of one turn: status, ending, the run that last worked on it.
    pub async fn task(&self, turn_id: &str) -> Result<TurnTask> {
        let turn = self.turn(turn_id).await?;
        let runs = self
            .runtime
            .backend()
            .runs(&self.thread, turn_id, None, RUN_PAGE)
            .await?;
        Ok(TurnTask {
            thread: turn.thread.clone(),
            turn_id: turn.turn_id.clone(),
            status: turn.status,
            settled: turn.status.terminal(),
            run_id: runs.items.last().map(|run| run.run_id.clone()),
            error: turn.error.clone(),
            output: turn.output.clone(),
        })
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

    /// One page of the committed event stream, narrowed to what the reader asked for.
    pub async fn events_with(
        &self,
        cursor: Option<&str>,
        limit: usize,
        options: EventStreamOptions,
    ) -> Result<EventPage> {
        self.runtime
            .events_with(&self.thread.session, cursor, limit, options)
            .await
    }
}

/// A handle to one turn: polling, waiting and the committed ending.
///
/// The handle holds the runtime, not the session, so a host can keep it across a request or
/// hand it to a renderer. `wait_until_terminal` polls the committed turn and sleeps with
/// `std::thread::sleep` — the durable crate keeps no timer dependency; a host that needs a
/// non-blocking wait polls [`view`](Self::view) from its own scheduler instead.
#[derive(Clone)]
pub struct TurnHandle {
    runtime: Arc<DurableRuntime>,
    thread: ThreadKey,
    turn_id: String,
}

impl TurnHandle {
    pub fn thread(&self) -> &ThreadKey {
        &self.thread
    }

    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The turn as committed right now.
    pub async fn view(&self) -> Result<TurnView> {
        self.runtime.turn(&self.thread, &self.turn_id).await
    }

    /// Polls until the turn is terminal, or fails once `timeout` is spent.
    ///
    /// A terminal turn returns without sleeping, so a caller that arrives after the ending does
    /// not pay one poll interval.
    pub async fn wait_until_terminal(
        &self,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<TurnView> {
        let deadline = Instant::now() + timeout;
        loop {
            let view = self.view().await?;
            if view.status.terminal() {
                return Ok(view);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(DurableError::unavailable(format!(
                    "Turn `{}` did not reach a terminal status before the timeout",
                    self.turn_id
                )));
            }
            std::thread::sleep(poll_interval.min(deadline - now));
        }
    }

    /// The ending the turn committed, or `None` while it is still running.
    pub async fn result(&self) -> Result<Option<Value>> {
        let view = self.view().await?;
        Ok(view.status.terminal().then_some(view.output))
    }
}
