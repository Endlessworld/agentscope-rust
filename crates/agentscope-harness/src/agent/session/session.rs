//! A session's durable task operations and read views, mirroring `AgentSession`.
//!
//! Commands are accepted synchronously into the inbox; this runtime owns execution
//! subscriptions, independently of observers. Obtaining a handle and reading views never
//! starts inference.
//!
//! Scope note: pending `steer`/`inject` input is applied once before the run starts. Rust's
//! `ReActAgent` exposes no "between reasoning steps" hook today, so mid-run delivery is
//! documented as unsupported rather than silently claimed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::{Extensions, RuntimeContext, StreamOptions};
use agentscope_core::event::AgentEvent;
use agentscope_core::message::{
    ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock,
};
use agentscope_core::session::{
    apply_pending_event, outcome, pending_interactions, session_error, transcript, Command,
    ExecutionBaseline, Head, InboxSnapshot, RecorderBaseline, SessionEvent, SessionExecution,
    SessionExportSinkHandle, SessionInbox, SessionKey, SessionLog, SessionProjection,
    SessionRecorder, SessionTurns, Transcript, EXECUTION_CONTEXT_KEY, RECORDER_CONTEXT_KEY,
    SESSION_EXPORT_SINK_CONTEXT_KEY,
};
use agentscope_core::state::AgentState;
use futures::StreamExt;

use super::answer::SessionAnswer;
use crate::agent::HarnessAgent;

/// Metadata key marking an assistant message that was re-attached from a journaled partial
/// model output, so a resumed run continues it instead of answering again.
pub const PREFILL_METADATA: &str = "agentscope/prefill";

/// Whether `message` is the internal recovery message a reconciled interrupt leaves behind.
///
/// It is never shown to the user (the transport replaces it), so it must not stay at the end
/// of the context when the run is resumed: the model would answer it instead of continuing
/// the text it had already streamed.
fn is_interrupt_recovery_message(message: &Msg) -> bool {
    message.role == MsgRole::Assistant
        && message.generate_reason() == agentscope_core::message::GenerateReason::Interrupted
}

/// How often the dispatcher looks for accepted work.
const DISPATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// The longest the dispatcher sleeps while a session has nothing to do.
///
/// An idle session used to re-read the journal twenty times a second forever. Accepted
/// commands now wake the loop, and the timeout only covers facts written by another handle
/// or process, so it can back off to this ceiling.
const IDLE_POLL_MAX: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a host waits for an in-flight run to stop before it closes the turn itself.
const ABANDON_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a continuation waits for the interrupted run to finish unwinding.
///
/// A run's task status flips to `interrupted` while its future is still tearing down
/// (and therefore still owns `active_run`). A host that resumes as soon as it observes
/// that status must let the old run release the slot instead of failing spuriously.
const RESUME_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A command receipt. A queued task may not yet have a run id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub command_id: String,
    pub turn_id: String,
    pub run_id: Option<String>,
    pub status: String,
    /// The failure detail persisted with `run/end`, when the task failed.
    pub error: Option<String>,
}

impl Task {
    /// Whether the task can no longer change without another command.
    pub fn settled(&self) -> bool {
        !matches!(self.status.as_str(), "queued" | "running" | "starting")
    }
}

/// The receipt for a `steer`/`inject` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputReceipt {
    pub input_id: String,
    pub turn_id: Option<String>,
}

/// Mutable runtime state of a session handle.
#[derive(Default)]
struct SessionState {
    stop: Option<Arc<AtomicBool>>,
    active_command: Option<String>,
    active_run: Option<String>,
    error: Option<String>,
    closed: bool,
    /// Receives every event of dispatched executions. A host that renders streaming
    /// output (ACP, a CLI) installs one instead of driving `stream_events` itself, so
    /// the journal writer and the task bookkeeping stay in one place.
    sink: Option<Arc<dyn Fn(&AgentEvent) + Send + Sync>>,
    /// The last dispatch projection and the committed head it was derived from.
    ///
    /// The dispatcher ticks every 50ms even while a session is idle. Re-reading
    /// `SessionTurns` from the whole journal on every tick re-deserialized every state
    /// checkpoint; a long session then burned CPU and churned memory without any new work.
    dispatch_view: Option<(u64, Option<String>, DispatchView)>,
    /// The committed head the live agent state corresponds to.
    ///
    /// A matching head means the agent's in-memory `AgentState` *is* the committed
    /// history, so the next turn can skip `SessionProjection::read`. That projection
    /// walks every event and hydrates every checkpoint/request blob; after the page cache
    /// goes cold this made the first prompt after an idle period take minutes and churn
    /// the disk. A mismatch (another writer, a restart) falls back to the full replay.
    restored_head: Option<(u64, Option<String>)>,
    /// Cached admission view (turns, inputs, applied ids, pending interactions) and the
    /// head it was derived from. Advanced incrementally per turn instead of re-reading
    /// the whole journal for `SessionTurns`/`pending_interactions`.
    execution_view: Option<(
        u64,
        Option<String>,
        SessionTurns,
        BTreeMap<String, SessionEvent>,
    )>,
}

impl SessionState {
    /// Whether the live agent state is the committed history at `head`.
    fn restored_head_matches(&self, head: &agentscope_core::session::Head) -> bool {
        self.restored_head.as_ref().is_some_and(|(seq, commit)| {
            *seq == head.seq && commit.as_deref() == head.commit_id.as_deref()
        })
    }
}

/// The small part of [`SessionTurns`] the dispatcher needs, without the input messages.
#[derive(Debug, Clone, Default)]
struct DispatchView {
    started: BTreeSet<String>,
    /// The latest turn id while it is still unfinished.
    open_turn: Option<String>,
}

/// Incremental receipt state for one command.
#[derive(Debug, Clone, Default)]
struct ReceiptFold {
    run: Option<String>,
    /// `None` means no `inbox/started` has been observed yet (`queued`).
    status: Option<String>,
    error: Option<String>,
}

/// Folds the event-derived part of task receipts once and then advances it by only the
/// events committed since the previous poll.
///
/// A receipt needs three facts: which run belongs to a command, how that run ended, and
/// whether its turn was cancelled. They are append-only, so the whole journal prefix does
/// not need to be re-read on every 50ms tick. Re-reading it loaded every committed state
/// checkpoint (the full conversation state) again; on a long turn that starved the event
/// loop, made output arrive in bursts, and grew memory with the undrained delta backlog.
#[derive(Debug, Default)]
struct ReceiptCache {
    through: u64,
    commit: Option<String>,
    /// Whether the fold covers the journal from seq 0. A seeded cache starts at the
    /// submit-time head and only covers events after it.
    complete: bool,
    /// The command whose first receipt was seeded at submit time.
    seeded_for: Option<String>,
    commands: HashMap<String, ReceiptFold>,
    cancelled_turns: HashSet<String>,
}

impl ReceiptCache {
    /// Starts the fold at the current head for a command that is about to be accepted.
    ///
    /// The dispatcher writes `inbox/started` only after acceptance, so starting here avoids
    /// re-reading the whole (potentially very large) history for the first receipt.
    fn seed(&mut self, head: &Head, command_id: &str) {
        *self = Self {
            through: head.seq,
            commit: head.commit_id.clone(),
            complete: false,
            seeded_for: Some(command_id.to_string()),
            ..Self::default()
        };
    }

    async fn rebuild(&mut self, log: &dyn SessionLog, head: &Head) -> crate::Result<()> {
        *self = Self::default();
        self.sync(log, head).await?;
        self.complete = true;
        Ok(())
    }

    /// Applies the events committed since `through`, up to `head`.
    async fn sync(&mut self, log: &dyn SessionLog, head: &Head) -> crate::Result<()> {
        // A rewritten or rolled-back head invalidates the fold: rebuild from the start.
        if head.seq < self.through
            || (head.seq == self.through && self.commit.as_deref() != head.commit_id.as_deref())
        {
            *self = Self::default();
        }
        if head.seq == self.through {
            self.commit = head.commit_id.clone();
            return Ok(());
        }
        let events = match log.scan(self.through, head.seq).await {
            Ok(events) => events,
            Err(_) => {
                // A segment was rewritten (migration/compaction). The prefix cannot be
                // advanced incrementally; rebuild once rather than reporting a false state.
                *self = Self::default();
                log.scan(0, head.seq).await?
            }
        };
        for event in &events {
            self.apply(event);
        }
        self.through = head.seq;
        self.commit = head.commit_id.clone();
        Ok(())
    }

    fn apply(&mut self, event: &SessionEvent) {
        if event.event_type == "turn/cancelled" {
            if let Some(turn) = &event.turn_id {
                self.cancelled_turns.insert(turn.clone());
            }
        }
        if event.event_type == "inbox/started" {
            let command_id = event.data().ok().and_then(|data| {
                data.get("commandId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
            if let Some(command_id) = command_id {
                let fold = self.commands.entry(command_id).or_default();
                fold.run = event.execution_run_id.clone();
                fold.status = Some("running".into());
                fold.error = None;
            }
        }
        if event.event_type == "run/end" {
            let run = event.execution_run_id.as_deref();
            if let Some(run) = run {
                let data = event.data().ok();
                let status = data
                    .as_ref()
                    .and_then(|d| d.get("status").and_then(|v| v.as_str()))
                    .unwrap_or("completed")
                    .to_string();
                let error = data
                    .as_ref()
                    .and_then(|d| d.get("error"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                for fold in self.commands.values_mut() {
                    if fold.run.as_deref() == Some(run) {
                        fold.status = Some(status.clone());
                        fold.error = error.clone();
                    }
                }
            }
        }
    }

    fn fold(&self, command_id: &str) -> (Option<String>, String, Option<String>) {
        match self.commands.get(command_id) {
            Some(fold) => (
                fold.run.clone(),
                fold.status.clone().unwrap_or_else(|| "queued".into()),
                fold.error.clone(),
            ),
            None => (None, "queued".into(), None),
        }
    }
}

/// A session's durable task operations and read views.
pub struct AgentSession {
    agent: Arc<HarnessAgent>,
    context: RuntimeContext,
    key: SessionKey,
    log: Arc<dyn SessionLog>,
    inbox: SessionInbox,
    state: Arc<Mutex<SessionState>>,
    /// Serializes receipt polls and keeps their incremental fold across ticks.
    receipts: tokio::sync::Mutex<ReceiptCache>,
    /// Wakes the dispatcher as soon as a command is accepted.
    ///
    /// The loop used to sleep a fixed 50ms and re-read the journal on every tick, even when
    /// nothing had been submitted for hours. Accepting a command now wakes it immediately, so
    /// the periodic timeout only has to cover facts written by another handle or process.
    wake: Arc<tokio::sync::Notify>,
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("session_id", &self.key.session_id)
            .finish_non_exhaustive()
    }
}

impl AgentSession {
    /// Opens a handle for a logical session. Opening never writes to the journal.
    pub fn open(
        agent: Arc<HarnessAgent>,
        context: RuntimeContext,
        key: SessionKey,
        log: Arc<dyn SessionLog>,
    ) -> crate::Result<Self> {
        let inbox = SessionInbox::new(Arc::clone(&log))?;
        Ok(Self {
            agent,
            context,
            key,
            log,
            inbox,
            state: Arc::new(Mutex::new(SessionState::default())),
            receipts: tokio::sync::Mutex::new(ReceiptCache::default()),
            wake: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// The logical session id.
    pub fn session_id(&self) -> &str {
        &self.key.session_id
    }

    /// The native journal behind this session.
    pub fn log(&self) -> Arc<dyn SessionLog> {
        Arc::clone(&self.log)
    }

    /// Complete committed messages as of the current head.
    pub async fn transcript(&self) -> crate::Result<Transcript> {
        transcript(&*self.log).await
    }

    /// Read-only recovery view (uncertain tool calls, active runs, state).
    pub async fn inspect(&self) -> crate::Result<SessionProjection> {
        SessionProjection::read(&*self.log).await
    }

    /// Outstanding interaction requests at the current prefix.
    pub async fn pending(&self) -> crate::Result<BTreeMap<String, SessionEvent>> {
        pending_interactions(&*self.log).await
    }

    /// The last dispatch error, if any.
    pub fn execution_error(&self) -> Option<String> {
        lock_state(&self.state).error.clone()
    }

    /// Installs the subscriber that receives every event of dispatched executions.
    ///
    /// The runtime keeps rendering independent of task bookkeeping: a host renders from the
    /// sink while [`await_task`](Self::await_task) tracks settlement. The sink is captured
    /// once per execution, so replacing it never splits a single turn between two renderers.
    pub fn set_event_sink(&self, sink: Arc<dyn Fn(&AgentEvent) + Send + Sync>) {
        lock_state(&self.state).sink = Some(sink);
    }

    /// The committed head of the journal (sequence, writer lease, commit id).
    pub async fn head(&self) -> crate::Result<agentscope_core::session::Head> {
        self.log.head().await
    }

    /// Submits a new task with a fresh idempotency key.
    pub async fn submit(&self, input: impl Into<String>) -> crate::Result<Task> {
        self.submit_msg(Msg::user(input)).await
    }

    /// Submits a pre-built user message (text, images, resources, ...).
    pub async fn submit_msg(&self, message: Msg) -> crate::Result<Task> {
        let key = uuid::Uuid::new_v4().to_string();
        self.submit_with_key_msg(&key, message).await
    }

    /// Starts a turn with **no user input**, so context that middlewares inject on a
    /// reasoning step (pending subagent results, inbox hints) reaches the model.
    ///
    /// Background work only surfaces on a reasoning step, so an idle session has to be given
    /// one; this is the journal-backed equivalent of Java `HarnessGateway#runWakeup`. The
    /// turn is admitted exactly like a `submit` — it is journaled, so its reply stays in the
    /// conversation the next prompt restores — it just carries no input messages.
    pub async fn wakeup(&self) -> crate::Result<Task> {
        self.ensure_open()?;
        // Same admission rule as a submit: settle a turn that can still be continued first,
        // otherwise the wakeup is rejected as `turn_unfinished`.
        self.prepare_open_turn_before_submit().await?;
        let key = uuid::Uuid::new_v4().to_string();
        let command = Command::new(
            &key,
            "submit",
            Some(turn_id_for(&key)),
            Vec::new(),
            Vec::new(),
        )?;
        let accepted = {
            self.seed_receipt_if_new(&command).await?;
            self.inbox.accept(&command).await?
        };
        self.start();
        self.receipt(&accepted).await
    }

    /// Submits a new task; retry with the same key and identical input to get the same receipt.
    pub async fn submit_with_key(
        &self,
        idempotency_key: &str,
        input: impl Into<String>,
    ) -> crate::Result<Task> {
        self.submit_with_key_msg(idempotency_key, Msg::user(input))
            .await
    }

    /// Submits a pre-built message with an explicit idempotency key.
    pub async fn submit_with_key_msg(
        &self,
        idempotency_key: &str,
        message: Msg,
    ) -> crate::Result<Task> {
        self.ensure_open()?;
        if message.content.is_empty() {
            return Err(session_error("A new task requires input"));
        }
        if matches!(
            message.content.as_slice(),
            [ContentBlock::Text(text)] if text.text.is_empty()
        ) {
            return Err(session_error("A new task requires input"));
        }
        if idempotency_key.trim().is_empty() || idempotency_key.len() > 200 {
            return Err(session_error("Invalid idempotency key"));
        }
        // A previous run may have left a non-terminal turn (crash, interrupt, a finish that
        // could not commit). The caller has clearly moved on when it submits new input, so
        // settle that turn before accepting the command instead of rejecting the prompt.
        self.prepare_open_turn_before_submit().await?;
        let turn_id = turn_id_for(idempotency_key);
        let command = Command::new(
            idempotency_key,
            "submit",
            Some(turn_id),
            freeze(idempotency_key, vec![message]),
            Vec::new(),
        )?;
        let accepted = {
            self.seed_receipt_if_new(&command).await?;
            self.inbox.accept(&command).await?
        };
        self.start();
        self.receipt(&accepted).await
    }

    /// Interrupts and waits for a run this handle already owns.
    ///
    /// A host that is about to submit a new prompt must not sit behind a run that may never
    /// return (a stalled provider, a tool that ignores cancellation). Unlike
    /// [`abandon_unfinished`](Self::abandon_unfinished) this leaves an idle, merely interrupted
    /// turn alone: [`submit`](Self::submit) decides whether to continue or close it.
    pub async fn stop_active_run(&self) -> crate::Result<()> {
        let Some(run_id) = lock_state(&self.state).active_run.clone() else {
            return Ok(());
        };
        let _ = self.agent.interrupt_run(&run_id);
        let _ = self.interrupt_run(&run_id).await;
        wait_for_idle(&self.state, ABANDON_TIMEOUT).await
    }

    /// Settles the open turn left by a previous run before a new submission is accepted.
    ///
    /// Routing (mirrors the recovery semantics the journal supports):
    /// - no open turn → nothing to do;
    /// - a live run still owns it → leave it alone, the dispatcher serialises the commands;
    /// - interrupted but resumable (no pending interaction, no uncertain tool outcome) →
    ///   queue a continuation *ahead* of the new submission, so the work that was already
    ///   started is finished instead of discarded;
    /// - suspended or with uncertain tool outcomes → close it, recording unknown tool
    ///   outcomes as interrupted (never success).
    async fn prepare_open_turn_before_submit(&self) -> crate::Result<()> {
        let Some(open_turn) = self.open_turn().await? else {
            return Ok(());
        };
        // A live run still owns the turn: do not race it. The dispatcher will reject/queue
        // until it settles, and the caller can cancel explicitly.
        if lock_state(&self.state).active_run.is_some() {
            return Ok(());
        }
        let resumable = self.pending().await?.is_empty()
            && self.inspect().await?.uncertain_tool_calls.is_empty();
        if resumable {
            match self.resume(&open_turn).await {
                Ok(task) => {
                    tracing::info!(
                        turn = %open_turn,
                        command = %task.command_id,
                        "continuing the interrupted turn before accepting a new submission"
                    );
                    return Ok(());
                }
                Err(error) => {
                    // A continuation is already queued: it will settle the turn itself.
                    if error.to_string().contains("already queued") {
                        return Ok(());
                    }
                    // Otherwise fall through and close the turn: a transient writer conflict
                    // must not lose the caller's prompt.
                    tracing::warn!(
                        turn = %open_turn,
                        %error,
                        "could not continue the open turn; closing it instead"
                    );
                }
            }
        }
        match agentscope_core::session::abandon_turn(Arc::clone(&self.log), &open_turn).await {
            Ok(closed) => {
                if closed {
                    tracing::info!(
                        turn = %open_turn,
                        "closed an orphaned open turn before accepting a new submission"
                    );
                }
                Ok(())
            }
            Err(error) => {
                // The dispatcher retries on its next tick; surfacing this as a rejection would
                // lose the caller's prompt for a transient writer conflict.
                tracing::warn!(
                    turn = %open_turn,
                    %error,
                    "could not close the orphaned turn before accepting a new submission"
                );
                Ok(())
            }
        }
    }

    /// Continues the latest unfinished task with a new run.
    pub async fn resume(&self, turn_id: &str) -> crate::Result<Task> {
        self.ensure_open()?;
        let turns = SessionTurns::read(&*self.log).await?;
        let continue_allowed = turns
            .latest()
            .is_some_and(|turn| turn.turn_id == turn_id && !turn.ended());
        if !continue_allowed {
            return Err(session_error(
                "Only the latest unfinished task can continue",
            ));
        }
        if !self.pending().await?.is_empty() {
            return Err(session_error("Answer pending interactions first"));
        }
        if !self.inspect().await?.uncertain_tool_calls.is_empty() {
            return Err(session_error("Reconcile uncertain tool outcomes first"));
        }
        self.wait_idle().await?;

        let snapshot = self.inbox.snapshot().await?;
        let already_queued = snapshot.commands.iter().any(|existing| {
            existing.kind == "resume"
                && existing.turn_id.as_deref() == Some(turn_id)
                && !turns.started.contains(&existing.id)
                && !snapshot.rejected.contains_key(&existing.id)
        });
        if already_queued {
            return Err(session_error("Continuation already queued"));
        }
        let command = Command::new(
            uuid::Uuid::new_v4().to_string(),
            "resume",
            Some(turn_id.to_string()),
            Vec::new(),
            Vec::new(),
        )?;
        let accepted = {
            self.seed_receipt_if_new(&command).await?;
            self.inbox.accept(&command).await?
        };
        self.start();
        self.receipt(&accepted).await
    }

    /// The latest turn id while it is still open (not `completed`/`failed`/`cancelled`).
    ///
    /// An `interrupted` turn is deliberately *not* terminal (it can still be resumed), so a
    /// host that gave up on it must close it explicitly — see
    /// [`abandon_unfinished`](Self::abandon_unfinished).
    pub async fn open_turn(&self) -> crate::Result<Option<String>> {
        let head = self.log.head().await?;
        let view = dispatch_view(&*self.log, &self.state, &head).await?;
        Ok(view.open_turn)
    }

    /// Closes the latest unfinished turn so a new task can be submitted.
    ///
    /// A run that was interrupted (the process died, the provider stalled, the host stopped
    /// waiting) leaves its turn non-terminal: [`Turn::ended`] only accepts
    /// `completed`/`failed`/`cancelled`. Every later `submit` then waits for a continuation
    /// that never comes, which is exactly the "prompt hangs with no output" symptom. A host
    /// that has given up on the turn calls this instead of waiting.
    ///
    /// Unknown tool outcomes are recorded as `interrupted` (never as success) and their
    /// dispatch uncertainty is cleared, so the next turn starts from a consistent context.
    /// Returns the id of the turn that was closed, or `None` when nothing was open.
    pub async fn abandon_unfinished(&self) -> crate::Result<Option<String>> {
        self.ensure_open()?;
        let deadline = std::time::Instant::now() + ABANDON_TIMEOUT;
        loop {
            // A run in flight still owns the turn. Ask it to stop instead of waiting for a
            // provider/tool that may never return; the loop below then closes whatever terminal
            // facts the run could not write itself.
            let active = lock_state(&self.state).active_run.clone();
            if let Some(run_id) = active {
                let _ = self.agent.interrupt_run(&run_id);
                let _ = self.interrupt_run(&run_id).await;
            }
            while lock_state(&self.state).active_run.is_some() {
                if std::time::Instant::now() >= deadline {
                    return Err(session_error("Wait for the current execution to stop"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            let Some(latest) = self.open_turn().await? else {
                return Ok(None);
            };
            match agentscope_core::session::abandon_turn(Arc::clone(&self.log), &latest).await {
                Ok(closed) => {
                    if closed {
                        return Ok(Some(latest));
                    }
                    // The turn settled concurrently (a finish that raced us). Re-check rather
                    // than returning "nothing closed" while the dispatcher may still see it.
                    if self.open_turn().await?.is_none() {
                        return Ok(None);
                    }
                    continue;
                }
                Err(error) => {
                    // Another handle may still be unwinding the same run; retry until the
                    // deadline instead of reporting a spurious "could not close" to the client.
                    if std::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Answers one pending interaction.
    pub async fn respond(&self, request_id: &str, answer: SessionAnswer) -> crate::Result<Task> {
        let mut answers = BTreeMap::new();
        answers.insert(request_id.to_string(), answer);
        self.respond_all(answers).await
    }

    /// Answers several pending interactions that all belong to one turn.
    pub async fn respond_all(
        &self,
        answers: BTreeMap<String, SessionAnswer>,
    ) -> crate::Result<Task> {
        self.ensure_open()?;
        self.require_idle()?;
        if answers.is_empty() {
            return Err(session_error("An answer is required"));
        }
        let requests = self.pending().await?;
        let mut turn: Option<String> = None;
        let mut results: Vec<ToolResultBlock> = Vec::new();
        let mut confirmations: Vec<agentscope_core::event::ConfirmResult> = Vec::new();

        for (id, answer) in &answers {
            let request = requests
                .get(id)
                .ok_or_else(|| session_error(format!("No pending interaction: {id}")))?;
            let request_turn = request.turn_id.clone().ok_or_else(|| {
                session_error(format!("Pending request has no logical turn: {id}"))
            })?;
            if let Some(existing) = &turn {
                if *existing != request_turn {
                    return Err(session_error("Answers belong to different tasks"));
                }
            }
            let data = request.data()?;
            let kind = data
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let call: ToolUseBlock = match data.get("call") {
                Some(value) => serde_json::from_value(value.clone())?,
                None => {
                    return Err(session_error(format!(
                        "Pending interaction has no tool call: {id}"
                    )))
                }
            };
            match kind.as_str() {
                "external_execution" => {
                    let result = answer.to_tool_result(&call.id, &call.name).ok_or_else(|| {
                        session_error(format!("Answer type does not match interaction: {id}"))
                    })?;
                    results.push(result);
                }
                "confirmation" => {
                    let result = answer.to_confirm_result(&call).ok_or_else(|| {
                        session_error(format!("Answer type does not match interaction: {id}"))
                    })?;
                    if result.tool_call.as_ref().is_some_and(|c| c.id != call.id) {
                        return Err(session_error("Confirmation tool ID mismatch"));
                    }
                    confirmations.push(result);
                }
                _ => {
                    return Err(session_error(format!(
                        "Reply through the interaction's owning service: {id}"
                    )))
                }
            }
            turn = Some(request_turn);
        }

        let mut messages: Vec<Msg> = Vec::new();
        if !results.is_empty() {
            let blocks: Vec<ContentBlock> =
                results.into_iter().map(ContentBlock::ToolResult).collect();
            messages.push(Msg::builder().role(MsgRole::Tool).content(blocks).build());
        }
        if !confirmations.is_empty() {
            let mut msg = Msg::builder()
                .role(MsgRole::User)
                .content(vec![ContentBlock::Text(TextBlock::new(String::new()))])
                .build();
            msg.metadata.insert(
                Msg::METADATA_CONFIRM_RESULTS.to_string(),
                serde_json::to_value(&confirmations)?,
            );
            messages.push(msg);
        }

        let sorted: Vec<String> = answers.keys().cloned().collect();
        let joined = sorted.join("\n");
        let command_id = stable_id("answer", &joined);
        let command = Command::new(
            command_id,
            "respond",
            turn,
            freeze("answer", messages),
            sorted,
        )?;
        let accepted = {
            self.seed_receipt_if_new(&command).await?;
            self.inbox.accept(&command).await?
        };
        self.start();
        self.receipt(&accepted).await
    }

    /// Adds guidance for the running task (or for `expected_turn` only).
    pub async fn steer(
        &self,
        expected_turn: Option<&str>,
        input: impl Into<String>,
    ) -> crate::Result<InputReceipt> {
        self.ensure_open()?;
        let id = uuid::Uuid::new_v4().to_string();
        let snapshot = self.inbox.snapshot().await?;
        let head = self.log.head().await?;
        let running = snapshot
            .run_id
            .as_deref()
            .filter(|run| Some(*run) == head.owner.as_deref() && head.lease_until > now_millis());
        if running.is_none() {
            return Err(session_error(
                "No running task accepts steering; submit or resume explicitly",
            ));
        }
        let turn_id = snapshot.turn_id.clone();
        if let Some(expected) = expected_turn {
            if Some(expected) != turn_id.as_deref() {
                return Err(session_error("Task is no longer running"));
            }
        }
        let command = Command::new(
            &id,
            "steer",
            turn_id.clone(),
            freeze(&id, vec![Msg::user(input)]),
            Vec::new(),
        )?;
        self.inbox.accept(&command).await?;
        Ok(InputReceipt {
            input_id: id,
            turn_id,
        })
    }

    /// Persists reference material without starting execution.
    pub async fn inject(&self, context: impl Into<String>) -> crate::Result<InputReceipt> {
        self.ensure_open()?;
        let id = uuid::Uuid::new_v4().to_string();
        let command = Command::new(
            &id,
            "inject",
            None,
            freeze(&id, vec![Msg::user(context)]),
            Vec::new(),
        )?;
        self.inbox.accept(&command).await?;
        Ok(InputReceipt {
            input_id: id,
            turn_id: None,
        })
    }

    /// Requests interruption of the current execution.
    ///
    /// A cooperative request, not a completion acknowledgement: the task keeps reporting
    /// `running` until the execution actually stops.
    pub async fn interrupt(&self) -> crate::Result<bool> {
        let snapshot = self.inbox.snapshot().await?;
        let Some(run_id) = snapshot.run_id.clone() else {
            return Ok(false);
        };
        self.interrupt_run(&run_id).await
    }

    /// Interrupts one specific run, if it is still the one the inbox is executing.
    ///
    /// A host that is racing a run it started (a cancel retry loop, a shutdown sweep) must use
    /// this instead of [`interrupt`](Self::interrupt): the plain version reads the *current*
    /// run id, so once the next prompt has been admitted it would stop that one instead.
    /// Returns `false` when `run_id` is no longer the active run.
    pub async fn interrupt_run(&self, run_id: &str) -> crate::Result<bool> {
        self.ensure_open()?;
        let snapshot = self.inbox.snapshot().await?;
        // Only signal while the inbox still names this run: a stale id must not land on
        // whatever started in the meantime.
        if snapshot.run_id.as_deref() != Some(run_id) {
            return Ok(false);
        }
        let command = Command::new(
            uuid::Uuid::new_v4().to_string(),
            "interrupt",
            snapshot.turn_id.clone(),
            Vec::new(),
            vec![run_id.to_string()],
        )?;
        // Signal before recording. Accepting input needs the journal writer lease, which the
        // running execution holds until it stops: recording first would make `interrupt` wait
        // for the run it is trying to stop (up to the lease timeout).
        //
        // Target the run by id, not the agent's *current* run: this call may be racing the run
        // it wants to stop (a host retry loop, a shutdown sweep). Without the id the signal
        // would land on whatever started in the meantime — the next prompt — and kill it.
        self.agent.interrupt_run(run_id);
        // Record the command off the caller's path. The interrupt is already delivered, so a
        // busy journal must not turn it into an error, and the host must not block on it.
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            let _ = inbox.accept(&command).await;
        });
        Ok(true)
    }

    /// Starts dispatch; `submit`/`respond`/`resume` already call this.
    pub fn start(&self) {
        let mut guard = lock_state(&self.state);
        if guard.closed {
            return;
        }
        if guard.stop.is_some() {
            // The dispatcher is already running: wake it instead of starting a second loop.
            // It may be sleeping on the idle backoff, and the caller wants this command
            // picked up now rather than at the next tick.
            drop(guard);
            self.wake.notify_one();
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        guard.stop = Some(Arc::clone(&stop));
        drop(guard);

        let agent = Arc::clone(&self.agent);
        let log = Arc::clone(&self.log);
        let inbox = self.inbox.clone();
        let context = self.context.clone();
        let state = Arc::clone(&self.state);
        let wake = Arc::clone(&self.wake);
        tokio::spawn(async move {
            let mut backoff = DISPATCH_INTERVAL;
            while !stop.load(Ordering::SeqCst) {
                // A panic inside one dispatch (a buggy tool, a poisoned dependency lock) must
                // not kill the loop: `stop` would stay set, every later prompt would queue
                // forever and the host would only see a stalled turn.
                let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                    dispatch_once(&agent, &log, &inbox, &context, &state),
                ))
                .await;
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        lock_state(state.as_ref()).error = Some(error.to_string());
                    }
                    Err(panic) => {
                        let detail = panic
                            .downcast_ref::<&str>()
                            .map(|message| message.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        tracing::error!(%detail, "session dispatch panicked; continuing");
                        lock_state(state.as_ref()).error =
                            Some(format!("dispatch panicked: {detail}"));
                    }
                }
                // A live run must be observed at the fast cadence (that is how interrupt
                // commands get consumed); only a genuinely idle session backs off.
                backoff =
                    next_idle_backoff(backoff, lock_state(state.as_ref()).active_run.is_some());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = wake.notified() => backoff = DISPATCH_INTERVAL,
                }
            }
        });
    }

    /// Every accepted execution task, keyed by turn.
    pub async fn tasks(&self) -> crate::Result<Vec<Task>> {
        let snapshot = self.inbox.snapshot_shared().await?;
        let mut seen: BTreeMap<String, Task> = BTreeMap::new();
        for command in &snapshot.commands {
            if command.execution() {
                let task = self.receipt_with_snapshot(command, &snapshot).await?;
                seen.insert(task.turn_id.clone(), task);
            }
        }
        Ok(seen.into_values().collect())
    }

    /// The current receipt of one accepted command.
    pub async fn task(&self, command_id: &str) -> crate::Result<Task> {
        let snapshot = self.inbox.snapshot_shared().await?;
        let command = snapshot
            .command(command_id)
            .filter(|command| command.execution())
            .ok_or_else(|| session_error("Unknown task submission"))?;
        self.receipt_with_snapshot(command, &snapshot).await
    }

    /// Polls one task until it settles. Observing never starts or cancels work.
    pub async fn await_task(&self, command_id: &str) -> crate::Result<Task> {
        loop {
            let task = self.task(command_id).await?;
            if task.settled() {
                return Ok(task);
            }
            tokio::time::sleep(DISPATCH_INTERVAL).await;
        }
    }

    fn ensure_open(&self) -> crate::Result<()> {
        if lock_state(&self.state).closed {
            return Err(session_error("Session runtime is closed"));
        }
        Ok(())
    }

    fn require_idle(&self) -> crate::Result<()> {
        if lock_state(&self.state).active_run.is_some() {
            return Err(session_error("Wait for the current execution to stop"));
        }
        Ok(())
    }

    /// Waits (bounded) for the previous run to release `active_run`.
    ///
    /// `resume` is only reachable for an unfinished turn, and a run that was just
    /// interrupted keeps the slot until its future finishes unwinding. Failing at once
    /// turns that teardown race into a user-visible "could not be continued" error, so
    /// wait for the slot instead; a run that never stops still fails after
    /// [`RESUME_IDLE_TIMEOUT`].
    async fn wait_idle(&self) -> crate::Result<()> {
        wait_for_idle(&self.state, RESUME_IDLE_TIMEOUT).await
    }

    /// Starts the receipt fold at the current head for a command that has not been accepted
    /// before. Retries of an already-started command must keep the full history.
    async fn seed_receipt_if_new(&self, command: &Command) -> crate::Result<()> {
        let known = self
            .inbox
            .snapshot_shared()
            .await?
            .command(&command.id)
            .is_some();
        if !known {
            let head = self.log.head().await?;
            self.receipts.lock().await.seed(&head, &command.id);
        }
        Ok(())
    }

    async fn receipt(&self, command: &Command) -> crate::Result<Task> {
        let snapshot = self.inbox.snapshot_shared().await?;
        self.receipt_with_snapshot(command, &snapshot).await
    }

    async fn receipt_with_snapshot(
        &self,
        command: &Command,
        snapshot: &InboxSnapshot,
    ) -> crate::Result<Task> {
        if let Some(reason) = snapshot.rejected.get(&command.id) {
            return Ok(Task {
                command_id: command.id.clone(),
                turn_id: command.turn_id.clone().unwrap_or_default(),
                run_id: None,
                status: "rejected".into(),
                error: Some(reason.clone()),
            });
        }
        // Keep the fold across polls: a receipt only needs the command's start, its run's
        // end and its turn's cancellation. Re-reading the whole prefix (including every
        // state checkpoint) on each 50ms tick starved the event loop and grew memory with
        // the undrained model-delta backlog.
        let mut cache = self.receipts.lock().await;
        // The fold is re-synced when the journal advanced while we were deciding. A run that
        // committed `run/end` between the scan and the lease check used to be mis-reported as
        // `interrupted`, which made the host try to continue a turn that had already finished.
        for _ in 0..3 {
            // `head` bounds the scan. The ownership check below must NOT reuse this snapshot:
            // the dispatcher may launch the run between this read and the `active_command`
            // check, which would make a live run look like it lost its lease (the snapshot
            // still says `owner: None`).
            let head = self.log.head().await?;
            if !cache.complete && cache.seeded_for.as_deref() != Some(command.id.as_str()) {
                cache.rebuild(&*self.log, &head).await?;
            }
            let was_seeded = cache.seeded_for.as_deref() == Some(command.id.as_str());
            cache.sync(&*self.log, &head).await?;
            if was_seeded && cache.seeded_for.is_none() {
                // The head was rewritten under the seed; fall back to the full prefix.
                cache.rebuild(&*self.log, &head).await?;
            }
            let (mut run, mut status, mut error) = cache.fold(&command.id);
            // A turn closed by the recovery path (`abandon_turn` / `cancel_pending`) never
            // writes a `run/end` for the original run, so the receipt must also honour
            // `turn/cancelled`.
            let turn_cancelled = command
                .turn_id
                .as_deref()
                .is_some_and(|turn| cache.cancelled_turns.contains(turn));
            // Whether *this* process is running the command right now. When it is, the
            // in-memory slot is authoritative: the run is alive by definition, so the lease
            // heuristic below (which exists to detect a run left behind by another/dead
            // process) must not apply.
            let owned_here = {
                let guard = lock_state(&self.state);
                let owned = guard.active_command.as_deref() == Some(command.id.as_str());
                if owned {
                    if let Some(active) = &guard.active_run {
                        run = Some(active.clone());
                    }
                    if status == "queued" {
                        status = "starting".into();
                    }
                }
                owned
            };
            // A live receipt whose writer lease is gone was interrupted, not finished. Only
            // meaningful for a run this process does not own: a locally-owned run holds the
            // lease by construction, and reading it from a stale snapshot would race the
            // launch.
            if !owned_here && matches!(status.as_str(), "running" | "starting") {
                let current = self.log.head().await?;
                let owns = run.as_deref() == current.owner.as_deref()
                    && current.lease_until > now_millis();
                if !owns {
                    // A committed prefix only grows. A head that moved backwards is a torn
                    // read (a value that is briefly absent or mid-replacement), not an
                    // interruption: re-read instead of telling the host its live run stopped.
                    if current.seq != head.seq {
                        // Terminal facts landed after our snapshot (or another writer
                        // committed): re-read before declaring an interruption.
                        continue;
                    }
                    status = "interrupted".into();
                    error = None;
                }
            }
            // An abandoned/cancelled turn is terminal for the caller even though the run
            // itself never wrote `run/end`: report `cancelled` so the prompt stops waiting.
            if turn_cancelled && !matches!(status.as_str(), "completed" | "failed") {
                status = "cancelled".into();
                error = None;
            }
            return Ok(Task {
                command_id: command.id.clone(),
                turn_id: command.turn_id.clone().unwrap_or_default(),
                run_id: run,
                status,
                error,
            });
        }
        // The journal kept advancing while we inspected it. Report the command as still in
        // flight rather than guessing at an interruption: the caller polls again shortly and
        // an unsettled receipt is always safe.
        Ok(Task {
            command_id: command.id.clone(),
            turn_id: command.turn_id.clone().unwrap_or_default(),
            run_id: None,
            status: "running".into(),
            error: None,
        })
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        // Stopping dispatch is the only cleanup a dropped handle owns; the journal keeps the
        // accepted work so another runtime can resume it later.
        let mut guard = lock_state(&self.state);
        guard.closed = true;
        if let Some(stop) = guard.stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

/// Returns the cached dispatch projection for `head`, re-reading the journal only when the
/// committed head changed.
async fn dispatch_view(
    log: &dyn SessionLog,
    state: &Arc<Mutex<SessionState>>,
    head: &Head,
) -> crate::Result<DispatchView> {
    let cached = {
        let guard = lock_state(state.as_ref());
        guard
            .dispatch_view
            .as_ref()
            .filter(|(seq, commit, _)| {
                *seq == head.seq && commit.as_deref() == head.commit_id.as_deref()
            })
            .map(|(_, _, view)| view.clone())
    };
    if let Some(view) = cached {
        return Ok(view);
    }
    let started = std::time::Instant::now();
    // Share the admission projection: it is advanced incrementally, so the dispatcher no
    // longer replays the whole journal after every run.
    let baseline = execution_baseline(log, state, head).await?;
    tracing::debug!(
        head_seq = head.seq,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "derived the dispatch view"
    );
    let turns = baseline.turns;
    let open_turn = turns
        .latest()
        .filter(|turn| !turn.ended())
        .map(|turn| turn.turn_id.clone());
    let view = DispatchView {
        started: turns.started,
        open_turn,
    };
    lock_state(state.as_ref()).dispatch_view =
        Some((head.seq, head.commit_id.clone(), view.clone()));
    Ok(view)
}

/// One dispatch tick: consume interrupts, then launch the next accepted task.
/// Locks the runtime state, recovering from a poisoned mutex.
///
/// A panic while this lock was held used to poison it, so every later `.lock().unwrap()` — the
/// dispatcher, the receipt readers and the host handlers — panicked in turn and left the session
/// permanently wedged. The guarded value is plain bookkeeping, so taking the inner value is safe.
fn lock_state(state: &Mutex<SessionState>) -> std::sync::MutexGuard<'_, SessionState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The next sleep before the dispatcher looks for work again.
///
/// Busy (a live run) keeps the fast cadence; an idle session doubles the interval up to
/// [`IDLE_POLL_MAX`]. Split out so the progression is testable without a wall-clock wait.
fn next_idle_backoff(current: std::time::Duration, busy: bool) -> std::time::Duration {
    if busy {
        DISPATCH_INTERVAL
    } else {
        (current * 2).min(IDLE_POLL_MAX)
    }
}

async fn dispatch_once(
    agent: &Arc<HarnessAgent>,
    log: &Arc<dyn SessionLog>,
    inbox: &SessionInbox,
    context: &RuntimeContext,
    state: &Arc<Mutex<SessionState>>,
) -> crate::Result<()> {
    let active = lock_state(state.as_ref()).active_run.is_some();
    if active {
        // Mark consumed interrupt commands so they are not replayed.
        let snapshot = inbox.snapshot_shared().await?;
        for command in &snapshot.commands {
            if command.kind == "interrupt" && !snapshot.handled.contains(&command.id) {
                inbox.handled(command).await?;
            }
        }
        return Ok(());
    }

    // Never launch while another writer owns the journal.
    let head = log.head().await?;
    if head.owner.is_some() && head.lease_until > now_millis() {
        return Ok(());
    }

    // The dispatcher ticks every 50ms even when idle. Reuse the projection until the
    // committed head changes instead of re-reading every event (and every checkpoint) on
    // each tick.
    let view = dispatch_view(&**log, state, &head).await?;
    let snapshot = inbox.snapshot_shared().await?;
    for command in &snapshot.commands {
        if !command.execution()
            || view.started.contains(&command.id)
            || snapshot.rejected.contains_key(&command.id)
        {
            continue;
        }
        // A submit must not run while an earlier turn is unfinished (only `resume` may
        // continue it). No live writer exists at this point (checked above), so the open turn
        // is orphaned: the host has clearly moved on and there is nobody left to finish it.
        // Close it (unknown tool outcomes become `interrupted`, never success) and let the
        // submit run on the next tick instead of rejecting the user's message outright.
        if view.open_turn.is_some() && command.kind == "submit" {
            if let Some(turn_id) = view.open_turn.as_deref() {
                match agentscope_core::session::abandon_turn(Arc::clone(log), turn_id).await {
                    Ok(_) => return Ok(()),
                    Err(error) => {
                        // A transient busy writer: leave the command queued and retry on a
                        // later tick. The host's admission deadline reports a real stall.
                        tracing::warn!(
                            turn = %turn_id,
                            %error,
                            "could not close the orphaned turn before starting a new submission"
                        );
                        return Ok(());
                    }
                }
            }
            continue;
        }
        launch(agent, log, inbox, context, state, command.clone()).await?;
        return Ok(());
    }
    Ok(())
}

/// Returns the admission view for `head`, advancing the cached projection incrementally.
///
/// A cold process pays one full `SessionTurns`/`pending_interactions` read. Subsequent
/// turns only visit the events committed since the cached head, instead of re-reading
/// every model chunk, checkpoint and request in the session's history.
async fn execution_baseline(
    log: &dyn SessionLog,
    state: &Arc<Mutex<SessionState>>,
    head: &Head,
) -> crate::Result<ExecutionBaseline> {
    let cached = {
        let guard = lock_state(state.as_ref());
        guard.execution_view.clone()
    };
    if let Some((seq, commit, mut turns, mut pending)) = cached {
        if seq == head.seq && commit.as_deref() == head.commit_id.as_deref() {
            return Ok(ExecutionBaseline { turns, pending });
        }
        if seq <= head.seq {
            let started = std::time::Instant::now();
            log.visit(seq, head.seq, &mut |event| {
                turns.apply_event(event)?;
                apply_pending_event(&mut pending, event)
            })
            .await?;
            tracing::debug!(
                from = seq,
                to = head.seq,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "advanced the cached execution view"
            );
            lock_state(state.as_ref()).execution_view = Some((
                head.seq,
                head.commit_id.clone(),
                turns.clone(),
                pending.clone(),
            ));
            return Ok(ExecutionBaseline { turns, pending });
        }
    }
    let started = std::time::Instant::now();
    let turns = SessionTurns::read(log).await?;
    let pending = pending_interactions(log).await?;
    tracing::info!(
        head_seq = head.seq,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "read the execution view from the journal"
    );
    lock_state(state.as_ref()).execution_view = Some((
        head.seq,
        head.commit_id.clone(),
        turns.clone(),
        pending.clone(),
    ));
    Ok(ExecutionBaseline { turns, pending })
}

/// Builds a recorder baseline from the live agent state, when it still matches the committed
/// head.
///
/// Returns `None` when the journal moved under this process or the state cannot be serialized:
/// the caller then opens the recorder without a baseline, which replays the journal. Cheap
/// enough to call again after a failed open -- it costs one `to_json()`, not a replay.
async fn live_baseline(
    log: &Arc<dyn SessionLog>,
    state: &Arc<Mutex<SessionState>>,
    agent: &Arc<HarnessAgent>,
    command: &Command,
    session_id: &str,
) -> Option<RecorderBaseline> {
    let head = log.head().await.ok()?;
    // Serialize under the state lock instead of cloning the whole `AgentState` first: the clone
    // copied every message (megabytes) only to be formatted and dropped right after.
    let state_json = {
        let guard = lock_state(state.as_ref());
        if !guard.restored_head_matches(&head) {
            return None;
        }
        let handle = agent.agent()?.state_handle();
        let live = handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match live.to_json() {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(
                    session = %session_id,
                    %error,
                    "could not serialize the live agent state; falling back to a journal replay"
                );
                return None;
            }
        }
    };
    Some(RecorderBaseline {
        seq: head.seq,
        commit_id: head.commit_id.clone(),
        state_json,
        uncertain_tool_calls: BTreeSet::new(),
        active_runs: BTreeSet::new(),
        // `respond`/`resume` continue an existing turn; a submit opens one.
        resumed_turn: command.kind != "submit",
    })
}

/// Opens a recorder, admits the command and runs the agent to completion.
async fn launch(
    agent: &Arc<HarnessAgent>,
    log: &Arc<dyn SessionLog>,
    inbox: &SessionInbox,
    context: &RuntimeContext,
    state: &Arc<Mutex<SessionState>>,
    command: Command,
) -> crate::Result<()> {
    let turn_id = command
        .turn_id
        .clone()
        .ok_or_else(|| session_error("Execution command requires a turn id"))?;
    // A host opts into committed-event export by installing a sink in the session context
    // (Java `SessionExportSink.CONTEXT_KEY`). Without one the recorder is opened exactly as
    // before, so unrelated harness tests and hosts are unaffected.
    let sink = context.extension::<SessionExportSinkHandle>(SESSION_EXPORT_SINK_CONTEXT_KEY);
    let session_id = agent
        .session_key(context)
        .map(|key| key.session_id)
        .unwrap_or_default();
    let exporter = sink
        .map(|handle| Arc::new(handle.exporter_for_session(Arc::clone(log), session_id.clone())));
    // A long session pays for `SessionProjection::read` on every new turn: it hydrates every
    // state checkpoint and every request/prepared blob. When the live agent state still
    // matches the committed head, hand it to the recorder as a baseline instead. The
    // recorder re-checks the head under the writer lease and falls back to the full replay
    // if a writer raced us in between.
    // Opening the recorder takes the journal writer lease. That can fail transiently (a
    // previous writer still releasing, a storage hiccup); rejecting the user's prompt on the
    // first failure made it look like the session was broken. The command stays accepted while
    // we retry, so a later attempt runs the same turn.
    // The first attempt takes the baseline by value: it carries a full `stateJson` (megabytes),
    // and cloning it per turn was pure copy cost. A retry rebuilds it from the live state
    // instead of replaying the journal: the rebuild is O(state), a replay is O(history).
    let mut pending_baseline = live_baseline(log, state, agent, &command, &session_id).await;
    let mut open_attempts = 0usize;
    let recorder = loop {
        open_attempts += 1;
        let attempt_baseline = match pending_baseline.take() {
            Some(baseline) => Some(baseline),
            // The previous attempt consumed the baseline: rebuild it rather than fall back to a
            // replay of the whole journal.
            None => live_baseline(log, state, agent, &command, &session_id).await,
        };
        let opened = match attempt_baseline {
            Some(baseline) => {
                SessionRecorder::open_with_baseline_and_policy(
                    Arc::clone(log),
                    Some(&turn_id),
                    exporter.clone(),
                    baseline,
                    agent.recording_policy(),
                )
                .await
            }
            None => {
                SessionRecorder::open_with_exporter_and_policy(
                    Arc::clone(log),
                    Some(&turn_id),
                    exporter.clone(),
                    agent.recording_policy(),
                )
                .await
            }
        };
        match opened {
            Ok(recorder) => break Arc::new(recorder),
            Err(error) if open_attempts < 3 => {
                tracing::warn!(%error, turn = %turn_id, "opening the session recorder failed; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(error) => {
                // A busy/expired writer or a transient storage failure is not a verdict on the
                // command. Leave it accepted so the dispatch loop retries; the host admission
                // deadline still reports a real stall.
                tracing::warn!(
                    %error,
                    turn = %turn_id,
                    "could not open the session recorder; leaving the command queued"
                );
                return Ok(());
            }
        }
    };
    let mut execution = SessionExecution::new(Arc::clone(log), command.clone());

    // The recorder already produced the committed projection (from the baseline or a full
    // replay). Reuse it instead of reading the journal a second time in this launcher.
    let mut restored = match recorder.restore_initial()? {
        Some(state) => state,
        None => AgentState::builder()
            .session_id(&turn_id)
            .user_id(context.user_id().unwrap_or_default())
            .build(),
    };
    // Identity is pinned by the agent that owns this session, never by whatever a previous
    // process happened to checkpoint: a restored state with another session id would journal
    // the next facts under the wrong key.
    if let Some(react) = agent.agent() {
        let pinned = react.state_handle();
        let pinned = pinned
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        restored.session_id = pinned.session_id().to_string();
        if restored.user_id.is_none() {
            restored.user_id = pinned.user_id().map(str::to_string);
        }
    }
    // A run that died in the middle of a model call had already streamed text the client saw.
    // Regenerating the answer would produce a different one, so the partial output stays in
    // the context and the model continues where it stopped. Two things are needed for that:
    // the interrupted recovery message ("I noticed that you have interrupted me…") must not
    // sit *after* the partial text — the model would answer it instead of continuing — and
    // the partial text must be the last message. Only a `resume` can carry one: a fresh
    // submit starts a new turn with nothing to continue.
    if command.kind == "resume" {
        match agentscope_core::session::interrupted_model_output(log.as_ref()).await {
            Ok(Some(partial)) if !partial.text.is_empty() => {
                while restored
                    .context
                    .last()
                    .is_some_and(is_interrupt_recovery_message)
                {
                    restored.context.pop();
                }
                if restored.context.last().map(Msg::text_content).as_deref()
                    != Some(partial.text.as_str())
                {
                    let mut message = Msg::assistant(partial.text.clone());
                    message
                        .metadata
                        .insert(PREFILL_METADATA.to_string(), serde_json::Value::Bool(true));
                    restored.context.push(message);
                }
                tracing::info!(
                    turn = %turn_id,
                    call = %partial.call_id,
                    chars = partial.text.chars().count(),
                    "continuing a model call that was interrupted mid-stream"
                );
            }
            Ok(_) => {}
            // Reading the journal is best effort here: failing to find a partial answer must
            // never block the resume itself.
            Err(error) => {
                tracing::warn!(turn = %turn_id, %error, "could not read the interrupted model output")
            }
        }
    }
    // Admission needs turns, inputs, applied ids and pending interactions. Use the cached
    // projection and advance it only over events committed since the cached head; a cold
    // process reads once.
    let admission_head = log.head().await?;
    let admission = execution_baseline(log.as_ref(), state, &admission_head).await?;
    if let Err(error) = execution
        .prepare_with_baseline(&recorder, Some(&admission))
        .await
    {
        // An admission failure is not a durable verdict on the command: a writer race, a
        // transient read error or a just-settled turn must not permanently reject the user's
        // message. Release and let the dispatcher retry the same accepted command.
        recorder.close().await;
        tracing::warn!(
            turn = %turn_id,
            %error,
            "session admission failed; leaving the command queued for a retry"
        );
        return Ok(());
    }

    // Guidance accepted before this run starts is folded in by the agent loop's step boundary
    // (Java `reasoning` is the only caller of `beforeStep`), so the launch path must not apply
    // it a second time.
    //
    // The committed projection already contains everything the turn produced, including a
    // resumed turn's original inputs. Passing those again appended them to the model context a
    // second time (the user prompt and every tool result duplicated), so only messages the
    // restored state does not carry are replayed. A brand-new submit's message is new by id; a
    // `respond` answer is new too; a `resume` replays nothing.
    let inputs: Vec<Msg> = execution
        .inputs()
        .iter()
        .filter(|message| {
            !restored
                .context
                .iter()
                .any(|existing| existing.id == message.id)
        })
        .cloned()
        .collect();
    // Bookkeeping works on a copy that already carries the admitted input: `input/received`
    // is journal metadata, while `message/user` facts come from the state snapshot (Java
    // `SessionRecorder.captureState` does the same). The agent itself gets the state
    // *without* the input, because the loop appends the input when it runs — handing it both
    // would put every admitted message into the context twice.
    let mut working = restored.clone();
    for message in &inputs {
        if !working
            .context
            .iter()
            .any(|existing| existing.id == message.id)
        {
            working.context.push(message.clone());
        }
    }
    if let Some(react) = agent.agent() {
        *react
            .state_handle()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = restored;
    }
    if let Err(error) = recorder.start(&inputs, &working).await {
        recorder.close().await;
        tracing::warn!(
            turn = %turn_id,
            %error,
            "could not record the session start; leaving the command queued for a retry"
        );
        return Ok(());
    }
    if let Err(error) = recorder.checkpoint_now(&working, "session_input").await {
        recorder.close().await;
        tracing::warn!(
            turn = %turn_id,
            %error,
            "could not checkpoint the admitted input; leaving the command queued for a retry"
        );
        return Ok(());
    }

    {
        let mut guard = lock_state(state.as_ref());
        guard.active_command = Some(command.id.clone());
        guard.active_run = Some(recorder.run_id().to_string());
        guard.error = None;
    }
    // Everything below runs the agent. If it panics, the bookkeeping above must still be
    // rolled back: a stale `active_command` makes the receipt report `starting` forever, the
    // dispatcher never launches anything again, and the host waits on a turn that is already
    // dead. A guard is the only way to cover the panic path (`?` and `return` are handled by
    // the explicit cleanup below).
    let active_guard = ActiveRunGuard::new(Arc::clone(state), command.id.clone());

    let mut extensions = Extensions::new();
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));
    extensions.insert(EXECUTION_CONTEXT_KEY, execution.clone());

    // Capture the subscriber once per execution so a host that replaces it mid-run gets a
    // clean cut instead of a half-rendered turn split across two renderers.
    let sink = lock_state(state.as_ref()).sink.clone();

    let react = agent.agent().cloned();
    let result: crate::Result<Option<Msg>> = match react {
        Some(react) => {
            let mut stream =
                react.stream_events_with_extensions(inputs, StreamOptions::defaults(), extensions);
            let mut reply = None;
            let mut failure = None;
            // A run that never emits a terminal event (its task panicked, the stream was cut)
            // must not be reported as completed. Track whether the loop observed one.
            let mut saw_terminal = false;
            while let Some(event) = stream.next().await {
                // Forward before interpreting: the host renders the same events the runtime
                // uses for bookkeeping, including the terminal `AgentResult`.
                if let (Ok(observed), Some(sink)) = (&event, &sink) {
                    sink(observed);
                }
                let terminal = matches!(
                    &event,
                    Ok(AgentEvent::AgentResult(_)) | Ok(AgentEvent::AgentEnd(_))
                );
                if terminal {
                    saw_terminal = true;
                }
                match event {
                    Ok(AgentEvent::AgentResult(data)) => {
                        if let Some(value) = data.fields.get("reply") {
                            reply = serde_json::from_value(value.clone()).ok();
                        }
                    }
                    Ok(_) => {}
                    Err(error) => failure = Some(error),
                }
                if terminal || failure.is_some() {
                    // The merged stream also carries a delta channel that only closes when the
                    // run's own context drops, so waiting for the stream to end would leave a
                    // cancelled or failed run open (and its journal lease held) indefinitely.
                    break;
                }
            }
            match failure {
                Some(error) => Err(error),
                None if !saw_terminal => Err(agentscope_core::Error::Agent(
                    "the run ended without reporting a terminal event".into(),
                )),
                None => Ok(reply),
            }
        }
        None => Err(agentscope_core::Error::Agent(
            "HarnessAgent has no model configured; cannot run".into(),
        )),
    };

    let status = match &result {
        Ok(Some(message)) => outcome(message.generate_reason()),
        Ok(None) => "completed",
        Err(agentscope_core::Error::Interrupted(_)) => "interrupted",
        Err(agentscope_core::Error::Cancelled(_)) => "cancelled",
        Err(_) => "failed",
    };
    // Keep the failure detail with the terminal facts so receipts (ACP, host views)
    // can report the actual cause instead of a bare status.
    let detail = result.as_ref().err().map(|error| error.to_string());
    if let Ok(Some(message)) = &result {
        let _ = recorder.result(Some(message));
    }
    // The terminal checkpoint must be the state the run actually ended with (assistant reply,
    // tool results, tasks, plan mode), not the pre-run copy this launcher built. Java
    // `finishSession` reads the live call state for exactly this reason.
    let final_state = match agent.agent() {
        Some(react) => react
            .state_handle()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        None => working.clone(),
    };
    let finish = recorder
        .finish_with_error(&final_state, status, detail.as_deref())
        .await;
    let _ = execution.close(&recorder).await;

    // Cache the state/head pairing only when the terminal facts committed and the turn
    // actually completed. A failed/interrupted run may carry uncertain tool outcomes or an
    // active run; the next launch must replay the journal so admission can see them.
    let completed_cleanly = finish.is_ok() && status == "completed";
    let committed_head = if completed_cleanly {
        log.head().await.ok()
    } else {
        None
    };
    {
        let mut guard = lock_state(state.as_ref());
        guard.active_command = None;
        guard.active_run = None;
        guard.restored_head = committed_head
            .as_ref()
            .map(|head| (head.seq, head.commit_id.clone()));
        if let Some(error) = &detail {
            guard.error = Some(error.clone());
        }
    }
    active_guard.disarm();

    // A run that never started must not leave a phantom `queued` receipt forever.
    if let Err(error) = &result {
        let started = SessionTurns::read(&**log)
            .await?
            .started
            .contains(&command.id);
        if !started {
            inbox.reject(&command, &error.to_string()).await?;
        }
    }
    // A finish failure must never leave the turn non-terminal: the run is over, so publish the
    // terminal facts through the recovery path (which also settles unknown tool outcomes).
    // Without this the next prompt sees an open turn and is rejected as `turn_unfinished`.
    if finish.is_err() {
        if let Err(error) = agentscope_core::session::abandon_turn(Arc::clone(log), &turn_id).await
        {
            tracing::warn!(
                %error,
                turn = %turn_id,
                "could not close the turn after a failed terminal write"
            );
        }
    }
    if let Err(error) = &finish {
        tracing::warn!(
            %error,
            turn = %turn_id,
            "recording the run's terminal facts failed; the turn was closed as interrupted"
        );
    }
    finish?;
    Ok(())
}

/// Clears a run's `active_command`/`active_run` bookkeeping on every exit path, including a
/// panic.
///
/// `launch` sets these before the agent runs and clears them afterwards. If the run panics in
/// between (a provider SDK unwrap, an OOM in a tool, …) the state stays `starting` forever:
/// the receipt never settles, `dispatch_once` sees an active run and refuses to start anything
/// else, and the host waits on a turn nobody is running. Dropping the guard restores the
/// invariant that "an active run" always means "something is actually executing".
struct ActiveRunGuard {
    state: Arc<Mutex<SessionState>>,
    command_id: String,
    armed: bool,
}

impl ActiveRunGuard {
    fn new(state: Arc<Mutex<SessionState>>, command_id: String) -> Self {
        Self {
            state,
            command_id,
            armed: true,
        }
    }

    /// Marks the normal cleanup as done, so `Drop` becomes a no-op.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // `unwrap_or_else` on a poisoned mutex: the panic that poisoned it is exactly the case
        // this guard exists for, so it must still run.
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.active_command.as_deref() == Some(self.command_id.as_str()) {
            guard.active_command = None;
            guard.active_run = None;
            if guard.error.is_none() {
                guard.error = Some("the run terminated unexpectedly (panic)".into());
            }
        }
    }
}

/// A deterministic id derived from `seed`.
///
/// Determinism matters: a retried submission must produce the same turn id (and therefore the
/// same idempotency receipt) without depending on a random generator. `uuid` only enables the
/// `v4` feature here, so the digest is built with the `sha2` dependency already in use.
fn stable_id(prefix: &str, seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("{prefix}-{}", &hex[..32])
}

/// Deterministic turn id for an idempotency key.
fn turn_id_for(idempotency_key: &str) -> String {
    stable_id("turn", &format!("turn:{idempotency_key}"))
}

/// Freezes input into durable messages with stable ids.
fn freeze(command_id: &str, input: Vec<Msg>) -> Vec<Msg> {
    input
        .into_iter()
        .enumerate()
        .map(|(index, mut message)| {
            message.id = format!("input-{command_id}-{index}");
            message
        })
        .collect()
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Waits (bounded) for `active_run` to be released.
///
/// A run's receipt flips to `interrupted` while its future is still unwinding, so a
/// continuation requested on the strength of that receipt must wait for the slot instead
/// of failing on a teardown race (see [`SessionRuntime::wait_idle`]). A run that never
/// stops still fails once `timeout` elapses.
async fn wait_for_idle(
    state: &Mutex<SessionState>,
    timeout: std::time::Duration,
) -> crate::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if lock_state(state).active_run.is_none() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(session_error("Wait for the current execution to stop"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, StorageValue,
    };
    use async_trait::async_trait;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    struct CountingStorage {
        inner: MemorySessionStorage,
        reads: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AtomicSessionStorage for CountingStorage {
        async fn read(&self, path: &str) -> crate::Result<Option<StorageValue>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.read(path).await
        }

        async fn compare_and_set(
            &self,
            path: &str,
            expected_version: u64,
            bytes: &[u8],
        ) -> crate::Result<bool> {
            self.inner
                .compare_and_set(path, expected_version, bytes)
                .await
        }

        async fn list_paths(&self, prefix: &str) -> crate::Result<Vec<String>> {
            self.inner.list_paths(prefix).await
        }
    }

    #[tokio::test]
    async fn receipts_advance_incrementally_instead_of_rescanning_the_journal() {
        // The ACP polls a task every 50ms. Re-reading the whole committed prefix on every
        // poll loaded every state checkpoint again, starved the event loop and made memory
        // grow with the undrained delta backlog. Only the events committed since the last
        // poll may be read.
        let reads = Arc::new(AtomicUsize::new(0));
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(CountingStorage {
            inner: MemorySessionStorage::new(),
            reads: Arc::clone(&reads),
        });
        let log = JournalSessionLog::new(storage, "s");
        let writer = log.acquire("run", Duration::from_secs(60)).await.unwrap();

        let mut seq = 0u64;
        for i in 0..40u64 {
            let event = SessionEvent::new(
                format!("e{i}"),
                seq + 1,
                0,
                "model/chunk",
                Some("run-1".into()),
                Some("t1".into()),
                true,
                "{\"text\":\"x\"}",
            )
            .unwrap();
            log.commit(&writer, &format!("b{i}"), seq, &[event])
                .await
                .unwrap();
            seq += 1;
        }
        let started = SessionEvent::new(
            "started",
            seq + 1,
            0,
            "inbox/started",
            Some("run-1".into()),
            Some("t1".into()),
            true,
            "{\"commandId\":\"cmd-1\",\"kind\":\"submit\"}",
        )
        .unwrap();
        log.commit(&writer, "started", seq, &[started])
            .await
            .unwrap();
        seq += 1;

        let head = log.head().await.unwrap();
        let mut cache = ReceiptCache::default();
        let before = reads.load(Ordering::SeqCst);
        cache.sync(&log, &head).await.unwrap();
        let initial = reads.load(Ordering::SeqCst) - before;
        assert!(
            initial > 20,
            "the initial sync must read the committed prefix, read {initial} values"
        );
        assert_eq!(
            cache.fold("cmd-1"),
            (Some("run-1".to_string()), "running".to_string(), None)
        );

        // An unchanged head must not read anything at all.
        let before = reads.load(Ordering::SeqCst);
        cache.sync(&log, &head).await.unwrap();
        assert_eq!(
            reads.load(Ordering::SeqCst),
            before,
            "an unchanged head re-read the journal"
        );

        // A new terminal fact reads only its own commit, not the whole prefix again.
        let ended = SessionEvent::new(
            "ended",
            seq + 1,
            0,
            "run/end",
            Some("run-1".into()),
            Some("t1".into()),
            true,
            "{\"status\":\"completed\"}",
        )
        .unwrap();
        let head = log.commit(&writer, "ended", seq, &[ended]).await.unwrap();
        let before = reads.load(Ordering::SeqCst);
        cache.sync(&log, &head).await.unwrap();
        let incremental = reads.load(Ordering::SeqCst) - before;
        assert!(
            incremental < 10,
            "an incremental sync read {incremental} values; it must only read the new commit"
        );
        assert_eq!(
            cache.fold("cmd-1"),
            (Some("run-1".to_string()), "completed".to_string(), None)
        );
    }

    #[tokio::test]
    async fn a_continuation_waits_for_the_previous_run_to_release_its_slot() {
        // A run publishes `interrupted` before its future has unwound, so `resume` can
        // observe the receipt while `active_run` is still set. Waiting for the slot keeps
        // that teardown race from surfacing as "could not be continued".
        let state = Arc::new(Mutex::new(SessionState {
            active_run: Some("run-1".to_string()),
            ..Default::default()
        }));
        let release = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            release.lock().unwrap().active_run = None;
        });

        wait_for_idle(&state, Duration::from_secs(5))
            .await
            .expect("a run that stops unwinding must not fail the continuation");
        assert!(lock_state(state.as_ref()).active_run.is_none());
    }

    #[tokio::test]
    async fn a_run_that_never_stops_still_fails_the_continuation() {
        let state = Arc::new(Mutex::new(SessionState {
            active_run: Some("run-1".to_string()),
            ..Default::default()
        }));

        let error = wait_for_idle(&state, Duration::from_millis(30))
            .await
            .expect_err("a live run must keep blocking the continuation");
        assert!(
            error
                .to_string()
                .contains("Wait for the current execution to stop"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_live_state_cache_matches_only_the_exact_committed_head() {
        let mut state = SessionState::default();
        state.restored_head = Some((7, Some("commit-a".into())));
        let head = |seq, commit: Option<&str>| agentscope_core::session::Head {
            seq,
            epoch: 1,
            owner: None,
            lease_until: 0,
            commit_id: commit.map(str::to_string),
        };
        assert!(state.restored_head_matches(&head(7, Some("commit-a"))));
        assert!(!state.restored_head_matches(&head(8, Some("commit-a"))));
        assert!(!state.restored_head_matches(&head(7, Some("commit-b"))));
        assert!(!SessionState::default().restored_head_matches(&head(0, None)));
    }

    #[tokio::test]
    async fn the_execution_view_advances_incrementally() {
        let reads = Arc::new(AtomicUsize::new(0));
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(CountingStorage {
            inner: MemorySessionStorage::new(),
            reads: Arc::clone(&reads),
        });
        let log = JournalSessionLog::new(storage, "s");
        let writer = log.acquire("run", Duration::from_secs(60)).await.unwrap();
        let mut seq = 0u64;
        for i in 0..40u64 {
            let event = SessionEvent::new(
                format!("chunk-{i}"),
                seq + 1,
                0,
                "model/chunk",
                Some("run-1".into()),
                Some("turn-1".into()),
                true,
                "{\"text\":\"x\"}",
            )
            .unwrap();
            log.commit(&writer, &format!("cold-{i}"), seq, &[event])
                .await
                .unwrap();
            seq += 1;
        }
        let started = SessionEvent::new(
            "started",
            seq + 1,
            0,
            "inbox/started",
            Some("run-1".into()),
            Some("turn-1".into()),
            true,
            "{\"commandId\":\"cmd-1\",\"kind\":\"submit\"}",
        )
        .unwrap();
        log.commit(&writer, "started", seq, &[started])
            .await
            .unwrap();
        seq += 1;

        let state = Arc::new(Mutex::new(SessionState::default()));
        let head = log.head().await.unwrap();
        let before = reads.load(Ordering::SeqCst);
        let cold = execution_baseline(&log, &state, &head).await.unwrap();
        assert!(
            reads.load(Ordering::SeqCst) - before > 20,
            "the first view must read the committed prefix"
        );
        assert!(cold.turns.started.contains("cmd-1"));

        // One new terminal fact: advancing the cached view must not re-read the prefix.
        let ended = SessionEvent::new(
            "ended",
            seq + 1,
            0,
            "run/end",
            Some("run-1".into()),
            Some("turn-1".into()),
            true,
            "{\"status\":\"completed\"}",
        )
        .unwrap();
        let head = log.commit(&writer, "ended", seq, &[ended]).await.unwrap();
        let before = reads.load(Ordering::SeqCst);
        let warm = execution_baseline(&log, &state, &head).await.unwrap();
        let incremental = reads.load(Ordering::SeqCst) - before;
        assert!(
            incremental < 10,
            "advancing the view read {incremental} values; it must only read the new commit"
        );
        assert_eq!(
            warm.turns.latest().map(|turn| turn.status.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn a_seeded_receipt_does_not_read_the_history_before_submit() {
        let reads = Arc::new(AtomicUsize::new(0));
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(CountingStorage {
            inner: MemorySessionStorage::new(),
            reads: Arc::clone(&reads),
        });
        let log = JournalSessionLog::new(storage, "s");
        let writer = log.acquire("run", Duration::from_secs(60)).await.unwrap();

        let mut seq = 0u64;
        for i in 0..40u64 {
            let event = SessionEvent::new(
                format!("old{i}"),
                seq + 1,
                0,
                "model/chunk",
                Some("run-old".into()),
                Some("t-old".into()),
                true,
                "{\"text\":\"x\"}",
            )
            .unwrap();
            log.commit(&writer, &format!("old{i}"), seq, &[event])
                .await
                .unwrap();
            seq += 1;
        }

        let seeded = log.head().await.unwrap();
        let mut cache = ReceiptCache::default();
        cache.seed(&seeded, "cmd-new");

        let started = SessionEvent::new(
            "started-new",
            seq + 1,
            0,
            "inbox/started",
            Some("run-new".into()),
            Some("t-new".into()),
            true,
            "{\"commandId\":\"cmd-new\",\"kind\":\"submit\"}",
        )
        .unwrap();
        let head = log
            .commit(&writer, "started-new", seq, &[started])
            .await
            .unwrap();

        let before = reads.load(Ordering::SeqCst);
        cache.sync(&log, &head).await.unwrap();
        let delta = reads.load(Ordering::SeqCst) - before;
        assert!(
            delta < 10,
            "a seeded receipt read {delta} values; it must not read pre-submit history"
        );
        assert_eq!(
            cache.fold("cmd-new"),
            (Some("run-new".to_string()), "running".to_string(), None)
        );
    }

    #[test]
    fn an_idle_dispatcher_backs_off_and_a_busy_one_does_not() {
        // The loop used to re-read the journal every 50ms forever, even with nothing
        // submitted for hours. Idle ticks now double up to the ceiling; a live run keeps
        // the fast cadence so interrupt commands are consumed promptly.
        let mut backoff = DISPATCH_INTERVAL;
        for _ in 0..10 {
            backoff = next_idle_backoff(backoff, false);
        }
        assert_eq!(backoff, IDLE_POLL_MAX);
        assert_eq!(next_idle_backoff(IDLE_POLL_MAX, false), IDLE_POLL_MAX);
        assert_eq!(next_idle_backoff(IDLE_POLL_MAX, true), DISPATCH_INTERVAL);
    }
}
