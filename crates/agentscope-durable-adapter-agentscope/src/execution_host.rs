//! Durable execution host: applies committed input, resumes interrupted finalization and
//! fences its own writes.
//!
//! Core owns the loop; this host only acts at the boundaries [`ReActAgent`] reports, using the
//! facts a durable commit already published. Every boundary commits the native `AgentState` as
//! the thread's checkpoint (see [`crate::checkpoint`]), and [`AgentScopeExecutionHost::open`]
//! rebuilds it, together with the iteration it was taken in, instead of starting from zero.
//! Two gaps are deliberate and documented in `docs/DURABLE_MIGRATION_PLAN.md`:
//!
//! * `input/steer` | `input/inject` signals are folded into the context and marked delivered:
//!   the `consumed_signals` marker rides the same commit as the state that carries the messages,
//!   so a signal is folded exactly once and never re-enters the conversation.
//! * A runtime restart is only resumed up to the committed iteration; the acting continuation
//!   (`acting_iteration`) has no consumer because core does not implement the pending-tool resume
//!   branch.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};

use agentscope_core::agent::{
    AgentExecutionHost, AgentExecutionPosition, ExecutionBoundary, FinishContinuation, RunOutcome,
    RunOutcomeStatus, RuntimeContext, EXECUTION_HOST_CONTEXT_KEY, EXECUTION_POSITION_CONTEXT_KEY,
};
use agentscope_core::error::{Error, Result};
use agentscope_core::journal::{
    journal_error, InvocationRecorder, JournalEvent, JournalEventCodec, JournalEventCodecRegistry,
};
use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock};
use agentscope_core::state::AgentState;
use agentscope_durable::{
    Commit, DurableError, ErrorCode, Lease, SessionBackend, Signal, ThreadKey, TurnStatus,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{checkpoint, tool_facts};

/// A reply was produced and persisted; finalization has not completed.
pub const FINALIZATION_STARTED: &str = "durable/finalization_started";
/// Finalization of that reply finished, so it must not be replayed.
pub const FINALIZATION_COMPLETED: &str = "durable/finalization_completed";

/// The committed finalization frame of one run.
#[derive(Debug, Clone)]
struct Frame {
    id: String,
    result: Msg,
}

/// [`AgentExecutionHost`] over a [`SessionBackend`] lease.
pub struct AgentScopeExecutionHost {
    backend: Arc<dyn SessionBackend>,
    run_id: String,
    lease: Mutex<Option<Lease>>,
    /// Iteration to continue from, read from the committed checkpoint and facts.
    position: AgentExecutionPosition,
    /// Native state rebuilt from the committed checkpoint; `None` when the thread has none.
    state: Option<AgentState>,
    /// A committed reply whose finalization must be resumed.
    resume: Option<Msg>,
    /// Whether the turn already committed facts, i.e. a previous tick ran it.
    started: bool,
    completed: AtomicBool,
    frame: Mutex<Option<Frame>>,
}

impl AgentScopeExecutionHost {
    /// Reads the thread's committed facts: the native state, the iteration to continue from and
    /// whether a produced reply still awaits finalization.
    pub async fn open(
        backend: Arc<dyn SessionBackend>,
        thread: ThreadKey,
        lease: Lease,
        run_id: impl Into<String>,
    ) -> Result<Self> {
        ensure_event_codecs();
        let turn_id = lease.turn_id.clone();
        let thread_state = backend.thread_state(&thread).await.map_err(convert)?;
        // The committed checkpoint is the only source of the previous invocation's state, and
        // so of the iteration it died in: a restart is not a fresh iteration budget.
        let (state, mut position) = match &thread_state.checkpoint_metadata {
            Some(metadata) => {
                let committed = metadata.position.parse::<u64>().map_err(|error| {
                    journal_error(format!("Invalid checkpoint position: {error}"))
                })?;
                let view = backend
                    .checkpoint(&thread, committed)
                    .await
                    .map_err(convert)?;
                let state = checkpoint::decode(&view.checkpoint)?;
                let iteration = state.cur_iter().max(0) as u32;
                (
                    Some(state),
                    AgentExecutionPosition::new(iteration, iteration.saturating_sub(1)),
                )
            }
            None => (None, AgentExecutionPosition::default()),
        };
        let mut frame: Option<Frame> = None;
        let mut completed = false;
        let mut started = false;
        for event in read_events(backend.as_ref(), &thread, thread_state.journal_position).await? {
            if event.turn_id.as_deref() != Some(turn_id.as_str()) {
                continue;
            }
            started = true;
            let data = event.data().unwrap_or(Value::Null);
            match event.event_type.as_str() {
                "step/start" => {
                    if let Some(iteration) = data.get("iteration").and_then(Value::as_u64) {
                        let iteration = iteration.min(u32::MAX as u64) as u32;
                        position =
                            AgentExecutionPosition::new(iteration, iteration.saturating_sub(1));
                    }
                }
                FINALIZATION_STARTED => {
                    let Some(result) = data.get("result") else {
                        continue;
                    };
                    let Ok(result) = serde_json::from_value::<Msg>(result.clone()) else {
                        continue;
                    };
                    let id = data
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let frame_position = data
                        .get("position")
                        .and_then(|value| serde_json::from_value(value.clone()).ok())
                        .unwrap_or(position);
                    position = frame_position;
                    frame = Some(Frame { id, result });
                    completed = false;
                }
                FINALIZATION_COMPLETED => {
                    let id = data.get("id").and_then(Value::as_str);
                    if frame.as_ref().map(|frame| frame.id.as_str()) == id {
                        completed = true;
                    }
                }
                // A settled turn must not replay its finished reply for the next input.
                "run/end" if completed => {
                    frame = None;
                    completed = false;
                }
                _ => {}
            }
        }
        let resume = frame.as_ref().map(|frame| frame.result.clone());
        Ok(Self {
            backend,
            run_id: run_id.into(),
            lease: Mutex::new(Some(lease)),
            position,
            state,
            resume,
            started,
            completed: AtomicBool::new(completed),
            frame: Mutex::new(frame),
        })
    }

    /// The iteration this invocation continues from.
    pub fn position(&self) -> AgentExecutionPosition {
        self.position
    }

    /// The native state rebuilt from the committed checkpoint, if the thread has one.
    pub fn state(&self) -> Option<&AgentState> {
        self.state.as_ref()
    }

    /// Whether this turn already ran, so its input is part of the committed state.
    ///
    /// A turn with no committed fact of its own has never been invoked: that is the invocation
    /// that must deliver the turn's input. A resumed one must not repeat it.
    pub fn turn_started(&self) -> bool {
        self.started
    }

    fn lease(&self) -> Result<Lease> {
        self.lease
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| journal_error("Durable execution host has no writer lease"))
    }

    /// Verifies this writer still owns the turn and nobody asked it to stop.
    ///
    /// A lost fence means another writer owns the run: that is a pause, not a failure.
    async fn check_control(&self) -> Result<()> {
        let lease = self.lease()?;
        let frame = match self.backend.frame(&lease).await {
            Ok(frame) => frame,
            Err(error) if error.code() == ErrorCode::Fenced => return Err(Error::HostPaused),
            Err(error) => return Err(convert(error)),
        };
        if frame.turn.cancellation_requested || frame.turn.status == TurnStatus::Pausing {
            return Err(Error::HostPaused);
        }
        Ok(())
    }

    /// Folds undelivered `input/steer` | `input/inject` signals into the conversation and marks
    /// them delivered.
    ///
    /// The state that carries the folded messages and the `consumed_signals` marker land in one
    /// commit, so a crash either loses both (the signal is folded again) or neither (it never
    /// re-enters the conversation). The durable marker is what makes delivery a fact; the message
    /// id only keeps the folded message stable for the journal.
    async fn apply_inputs(&self, state: &mut AgentState) -> Result<()> {
        let lease = self.lease()?;
        let frame = self.backend.frame(&lease).await.map_err(convert)?;
        let mut delivered = Vec::new();
        for signal in frame.signals.iter().filter(|signal| is_input(signal)) {
            if let Some(text) = signal_text(&signal.value) {
                let message = Msg::builder()
                    .role(MsgRole::User)
                    .id(format!("signal:{}", signal.id))
                    .content(vec![ContentBlock::Text(TextBlock::new(text))])
                    .build();
                state.context_mut().push(message);
            }
            delivered.push(signal.id.clone());
        }
        if delivered.is_empty() {
            return Ok(());
        }
        let checkpoint = serde_json::to_value(&*state)
            .map_err(|error| journal_error(format!("Cannot encode agent state: {error}")))?;
        let mut commit = Commit::new(
            format!("input:{}:{}", lease.turn_id, frame.turn.revision),
            frame.turn.revision,
            TurnStatus::Running,
        );
        commit.checkpoint = checkpoint;
        commit.consumed_signals = delivered;
        self.backend.commit(&lease, commit).await.map_err(convert)?;
        Ok(())
    }

    /// Log-before-do: records every call the next acting phase will run, before any of them
    /// leaves the runtime.
    ///
    /// Without it a process that dies mid-tool leaves no trace at all, and the next run sees
    /// a plain pending call. With it the durable record says `dispatched` with no result, so
    /// the effect is recovered as unknown instead of being applied twice.
    async fn dispatch_calls(&self, state: &AgentState) -> Result<()> {
        let pending = tool_facts::pending_calls(state);
        if pending.is_empty() {
            return Ok(());
        }
        let lease = self.lease()?;
        let mut calls = Vec::with_capacity(pending.len());
        for block in &pending {
            calls.push(tool_facts::tool_call(block)?);
        }
        let dispatches = calls
            .iter()
            .map(|call| call.execution_id(&lease.turn_id))
            .collect();
        let frame = self.backend.frame(&lease).await.map_err(convert)?;
        // Deterministic per revision: a replay of the same boundary writes nothing new.
        let commit_id = format!("dispatch:{}:{}", lease.turn_id, frame.turn.revision);
        let mut commit = Commit::new(commit_id, frame.turn.revision, TurnStatus::Running);
        // The backend refuses a commit that discards committed native state; the identical
        // value is deduplicated, so carrying it costs no checkpoint position.
        commit.checkpoint = frame.checkpoint.clone();
        commit.tools = calls;
        commit.dispatches = dispatches;
        self.backend.commit(&lease, commit).await.map_err(convert)?;
        Ok(())
    }

    /// Commits the boundary's state as the turn's native checkpoint.
    ///
    /// The snapshot rides the recorder's next journal batch, so the checkpoint and the turn
    /// revision advance in one transaction; an unchanged state is deduplicated by the backend
    /// (and skipped outright by the recorder), so a repeated boundary adds no position.
    async fn persist_state(&self, context: &RuntimeContext, state: &AgentState) -> Result<()> {
        let Some(recorder) = InvocationRecorder::from_context(context) else {
            return Ok(());
        };
        recorder.checkpoint_now(state, "durable_boundary").await
    }

    fn frame_id(&self) -> String {
        if let Ok(guard) = self.frame.lock() {
            if let Some(frame) = guard.as_ref() {
                return frame.id.clone();
            }
        }
        format!("finalization:{}", self.run_id)
    }
}

#[async_trait]
impl AgentExecutionHost for AgentScopeExecutionHost {
    async fn before_boundary(
        &self,
        boundary: ExecutionBoundary,
        context: &RuntimeContext,
        state: &mut AgentState,
    ) -> Result<()> {
        self.check_control().await?;
        match boundary {
            // Input applied here enters the next model request.
            ExecutionBoundary::Reasoning => self.apply_inputs(state).await?,
            // Every tool call this iteration will run is logged before any of them runs.
            ExecutionBoundary::Acting => self.dispatch_calls(state).await?,
            // `Model` runs with the recorder's own `before_model` snapshot and `Finalization`
            // persists the reply together with the state it belongs to.
            _ => return Ok(()),
        }
        self.persist_state(context, state).await
    }

    fn resume_result(&self) -> Option<Msg> {
        self.resume.clone()
    }

    async fn finish<'a>(
        &'a self,
        context: &RuntimeContext,
        result: &Msg,
        continuation: FinishContinuation<'a>,
    ) -> Result<()> {
        // Only a completed reply is persisted; a suspended/interrupted outcome keeps its own
        // recovery facts and finalization is a no-op.
        if RunOutcome::from_result(result).status() != RunOutcomeStatus::Completed {
            return continuation.await;
        }
        if self.completed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.check_control().await?;
        let recorder = InvocationRecorder::from_context(context)
            .ok_or_else(|| journal_error("Durable finalization requires a native recorder"))?;
        let state = |context: &RuntimeContext| {
            context
                .state
                .as_ref()
                .map(|state| state.lock().unwrap().clone())
        };
        let before = state(context)
            .ok_or_else(|| journal_error("Durable finalization requires an agent state"))?;
        let frame_id = self.frame_id();
        let position = self.position;
        let has_frame = self
            .frame
            .lock()
            .map(|frame| frame.is_some())
            .unwrap_or(false);
        if !has_frame {
            // Persist the reply together with the checkpoint it belongs to: both facts land in
            // one durable commit, so a replay never re-invokes the primary model.
            recorder.append(
                FINALIZATION_STARTED,
                &json!({ "id": frame_id, "result": result, "position": position }),
            )?;
            recorder
                .checkpoint_now(&before, "durable_finalization_start")
                .await?;
            if let Ok(mut guard) = self.frame.lock() {
                *guard = Some(Frame {
                    id: frame_id.clone(),
                    result: result.clone(),
                });
            }
        }
        continuation.await?;
        recorder.append(FINALIZATION_COMPLETED, &json!({ "id": frame_id }))?;
        if let Some(after) = state(context) {
            recorder
                .checkpoint_now(&after, "durable_finalization_completed")
                .await?;
        }
        self.completed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Pins the host and the position it committed onto a call context, so the agent resumes the
/// invocation instead of starting a fresh iteration budget.
///
/// The position is the host's committed one (rebuilt from the checkpoint), never a constant
/// supplied by the caller: a restart after a crash must not reset the budget to zero.
pub fn pin(context: &mut RuntimeContext, host: Arc<AgentScopeExecutionHost>) {
    let position = host.position();
    let host: Arc<dyn AgentExecutionHost> = host;
    context.set_extension(EXECUTION_HOST_CONTEXT_KEY, host);
    context.set_extension(EXECUTION_POSITION_CONTEXT_KEY, position);
}

fn is_input(signal: &Signal) -> bool {
    matches!(signal.key.as_str(), "input:steer" | "input:inject")
}

fn signal_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

fn convert(error: DurableError) -> Error {
    journal_error(format!("{error}"))
}

/// Committed native journal entries of `thread`, in order.
async fn read_events(
    backend: &dyn SessionBackend,
    thread: &ThreadKey,
    journal_position: u64,
) -> Result<Vec<JournalEvent>> {
    // The backend reads at most one page per call (`Page limit must be 1..=1000`). A thread whose
    // committed history is longer than a page therefore has to be read page by page: asking for
    // `journal_position` entries in one call asked for a page size the backend refuses as soon as
    // the thread passed 1,000 entries, which failed every `open` of that thread.
    const PAGE: usize = 1000;
    let mut events = Vec::new();
    let mut after = 0u64;
    while after < journal_position {
        let entries = backend
            .read_journal(thread, after, PAGE)
            .await
            .map_err(convert)?;
        if entries.is_empty() {
            break;
        }
        let full_page = entries.len() == PAGE;
        let mut progress = false;
        for entry in entries {
            // `seq` is the entry's journal position, and it is what the next page starts after;
            // it is read from the envelope so an entry the codec cannot decode still advances.
            if let Some(seq) = entry.get("seq").and_then(Value::as_u64) {
                if seq > after {
                    after = seq;
                    progress = true;
                }
            }
            if let Ok(event) = serde_json::from_value::<JournalEvent>(entry) {
                events.push(event);
            }
        }
        if !full_page || !progress {
            break;
        }
    }
    Ok(events)
}

/// Codecs for the two host facts, so a required event type is never unknown to recovery.
struct FinalizationCodec;

impl JournalEventCodec for FinalizationCodec {
    fn validate(&self, data: &Value) -> Result<()> {
        if !data.is_object() {
            return Err(journal_error("Invalid durable finalization fact"));
        }
        Ok(())
    }

    fn apply(&self, _state: &mut AgentState, _data: &Value) -> Result<()> {
        Ok(())
    }
}

fn ensure_event_codecs() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        let registry = JournalEventCodecRegistry::default_registry();
        for event_type in [FINALIZATION_STARTED, FINALIZATION_COMPLETED] {
            let _ = registry.register(event_type, Arc::new(FinalizationCodec));
        }
    });
}
