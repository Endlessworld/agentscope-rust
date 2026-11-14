//! Durable execution host: applies committed input, resumes interrupted finalization and
//! fences its own writes.
//!
//! Core owns the loop; this host only acts at the boundaries [`ReActAgent`] reports, using the
//! facts a durable commit already published. Two gaps are deliberate and documented in
//! `docs/DURABLE_MIGRATION_PLAN.md`:
//!
//! * `input/steer` | `input/inject` signals are folded into the context, but they are **not**
//!   consumed durably: that needs a commit carrying `consumed_signals` together with the native
//!   checkpoint, which the journal bridge does not expose yet. Because the injected message id is
//!   derived from the signal id, re-applying a boundary never duplicates input.
//! * A runtime restart is only resumed up to the committed iteration; the acting continuation
//!   (`acting_iteration`) has no consumer because core does not implement the pending-tool resume
//!   branch.

use std::collections::HashSet;
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
    DurableError, ErrorCode, Lease, SessionBackend, Signal, ThreadKey, TurnStatus,
};
use async_trait::async_trait;
use serde_json::{json, Value};

/// A reply was produced and persisted; finalization has not completed.
pub const FINALIZATION_STARTED: &str = "durable/finalization_started";
/// Finalization of that reply finished, so it must not be replayed.
pub const FINALIZATION_COMPLETED: &str = "durable/finalization_completed";

/// The committed finalization frame of one run.
#[derive(Debug, Clone)]
struct Frame {
    id: String,
    result: Msg,
    position: AgentExecutionPosition,
}

/// [`AgentExecutionHost`] over a [`SessionBackend`] lease.
pub struct AgentScopeExecutionHost {
    backend: Arc<dyn SessionBackend>,
    thread: ThreadKey,
    run_id: String,
    lease: Mutex<Option<Lease>>,
    /// Iteration to continue from, read from the committed facts at construction.
    position: AgentExecutionPosition,
    /// A committed reply whose finalization must be resumed.
    resume: Option<Msg>,
    completed: AtomicBool,
    frame: Mutex<Option<Frame>>,
}

impl AgentScopeExecutionHost {
    /// Reads the thread's committed facts: the last reasoning iteration and whether a produced
    /// reply still awaits finalization.
    pub async fn open(
        backend: Arc<dyn SessionBackend>,
        thread: ThreadKey,
        lease: Lease,
        run_id: impl Into<String>,
    ) -> Result<Self> {
        ensure_event_codecs();
        let turn_id = lease.turn_id.clone();
        let mut position = AgentExecutionPosition::default();
        let mut frame: Option<Frame> = None;
        let mut completed = false;
        for event in read_events(backend.as_ref(), &thread).await? {
            if event.turn_id.as_deref() != Some(turn_id.as_str()) {
                continue;
            }
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
                    frame = Some(Frame {
                        id,
                        result,
                        position: frame_position,
                    });
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
            thread,
            run_id: run_id.into(),
            lease: Mutex::new(Some(lease)),
            position,
            resume,
            completed: AtomicBool::new(completed),
            frame: Mutex::new(frame),
        })
    }

    /// The iteration this invocation continues from.
    pub fn position(&self) -> AgentExecutionPosition {
        self.position
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

    /// Folds undelivered `input/steer` | `input/inject` signals into the conversation.
    async fn apply_inputs(&self, state: &mut AgentState) -> Result<()> {
        let lease = self.lease()?;
        let frame = self.backend.frame(&lease).await.map_err(convert)?;
        if frame.signals.is_empty() {
            return Ok(());
        }
        let mut known: HashSet<String> = state
            .context()
            .iter()
            .map(|message| message.id().to_string())
            .collect();
        for signal in frame.signals.iter().filter(|signal| is_input(signal)) {
            let id = format!("signal:{}", signal.id);
            if !known.insert(id.clone()) {
                continue;
            }
            let Some(text) = signal_text(&signal.value) else {
                continue;
            };
            let message = Msg::builder()
                .role(MsgRole::User)
                .id(id)
                .content(vec![ContentBlock::Text(TextBlock::new(text))])
                .build();
            state.context_mut().push(message);
        }
        Ok(())
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
        _context: &RuntimeContext,
        state: &mut AgentState,
    ) -> Result<()> {
        self.check_control().await?;
        // Input applied here enters the next model request; the other boundaries only need the
        // control check.
        if boundary == ExecutionBoundary::Reasoning {
            self.apply_inputs(state).await?;
        }
        Ok(())
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
                    position,
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

/// Pins the host and its committed position onto a call context, so the agent resumes the
/// invocation instead of starting a fresh iteration budget.
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
) -> Result<Vec<JournalEvent>> {
    let state = backend.thread_state(thread).await.map_err(convert)?;
    if state.journal_position == 0 {
        return Ok(Vec::new());
    }
    let limit = state.journal_position.min(usize::MAX as u64) as usize;
    let entries = backend
        .read_journal(thread, 0, limit)
        .await
        .map_err(convert)?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<JournalEvent>(entry).ok())
        .collect())
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
