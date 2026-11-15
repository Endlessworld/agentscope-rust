//! A durable adapter that runs one `HarnessAgent` invocation per tick.
//!
//! The tick owns the lease, so everything happens inside it: the execution host is opened on
//! the committed facts, the native journal writes under the *same* lease (borrowed, never
//! claimed), the core loop runs, and the adapter settles the turn from what the invocation
//! observed. One tick is one model decision, not one whole turn: `ReActAgent` runs its
//! reasoning/acting loop to completion inside a single invocation, so the adapter stops that
//! loop before the second model call ([`TickBudget`]) and leaves the rest to the next tick.
//!
//! Control is durable, so it is read from the committed turn and never assumed: a cancel is
//! delivered into the invocation as a core `CancelSignal` (the agent ends the run as cancelled
//! instead of the tick dropping a live stream), a pause is a yield that leaves the turn
//! resumable, and the run's native ending is written from that same status. The boundary is the
//! poll: control lands at the next poll while the invocation yields, and a tool that blocks the
//! runtime thread is only stopped once it returns.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_core::agent::{
    AgentExecutionHost, CancelSignal, ExecutionBoundary, Extensions, FinishContinuation,
    RuntimeContext, StreamOptions, CANCEL_SIGNAL_CONTEXT_KEY, EXECUTION_HOST_CONTEXT_KEY,
    EXECUTION_POSITION_CONTEXT_KEY,
};
use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::journal::{InvocationRecorder, ThreadJournal, RECORDER_CONTEXT_KEY};
use agentscope_core::message::{GenerateReason, Msg};
use agentscope_core::state::AgentState;
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::{DurableError, ExecutionState, Lease, Result, TurnStatus};
use agentscope_harness::HarnessAgent;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use crate::{AgentScopeExecutionHost, BackendThreadJournal};

/// Version of the native state a checkpoint carries (a `core::state::AgentState` snapshot).
const STATE_VERSION: &str = "agentscope/state/1";

/// Model calls one tick may start.
const MODEL_CALLS_PER_TICK: usize = 1;

/// What a tick reports when its invocation ended without a reply.
const NO_REPLY: &str = "The invocation produced no reply";

/// How often a running invocation re-reads the committed frame to look for control.
///
/// A cancel or pause can arrive while the model call is still streaming, and a provider that
/// never returns would otherwise hold the turn until its lease expires. Polling is the only
/// way to observe the request from inside the call that is blocking.
const STREAM_CONTROL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long a cancelled invocation may take to end on its own before the tick drops it.
///
/// The agent observes the signal at its phase boundaries and inside a streaming model call, so
/// a cancel normally ends the invocation within a poll or two. A tool that blocks where no
/// checkpoint runs cannot be interrupted at all: waiting forever would leave the turn
/// un-terminated, so the tick stops waiting and settles the cancellation itself.
const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The synthetic result recorded for a tool call a previous run dispatched but never answered.
const STRANDED_OUTCOME_MESSAGE: &str = "This tool call was dispatched by an earlier attempt of \
     this turn, but that attempt ended before its result was recorded. It was not retried; its \
     outcome is unknown.";

/// Why a retry settles the tool calls the failed attempt left unanswered.
const STRANDED_RECONCILE_REASON: &str = "retry: unsettled tool outcomes of the failed run";

/// Why a turn holding a tool call whose outcome is unknown cannot report success.
const UNKNOWN_OUTCOME: &str = "The turn cannot report success because a tool call of this turn \
     has an unknown outcome: an earlier attempt was interrupted before its result was recorded \
     and it was not re-executed, so its effect may or may not have happened.";

/// Runs turns of one harness definition by building the agent that executes them.
///
/// The factory is called once per tick: an invocation runs over the committed state and never
/// carries a previous tick's in-flight state. A host that owns one long-lived agent (ACP) hands
/// the same `Arc` back every time; a host that wants a fresh agent builds one per tick.
pub struct AgentScopeHarnessAdapter {
    build: Arc<dyn Fn() -> Arc<HarnessAgent> + Send + Sync>,
}

impl AgentScopeHarnessAdapter {
    pub fn new(build: impl Fn() -> Arc<HarnessAgent> + Send + Sync + 'static) -> Self {
        Self {
            build: Arc::new(build),
        }
    }
}

#[async_trait]
impl HarnessAdapter for AgentScopeHarnessAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            state_version: STATE_VERSION.to_string(),
            recovery_boundary: true,
            event_delivery: true,
        }
    }

    async fn execute(&self, ctx: &HarnessContext) -> Result<()> {
        let lease = &ctx.lease;
        let backend = Arc::clone(&ctx.backend);
        // The journal writes under the lease this tick already holds; it must not claim.
        let journal = Arc::new(
            BackendThreadJournal::new(Arc::clone(&backend), lease.thread.clone())
                .attach_lease(lease.clone()),
        );
        // A retry runs over the journal the attempt that just failed left behind, and the
        // recorder refuses to open one that holds a dispatched call without a result. Nothing is
        // dispatched by this invocation yet, so such a call belonged to a run that already
        // ended: its outcome can never arrive, and recording it as interrupted (never as
        // success, never re-executed) is what lets the retry run at all.
        let stranded = agentscope_core::journal::recovery::settle_uncertain_tool_outcomes(
            Arc::clone(&journal) as Arc<dyn ThreadJournal>,
            STRANDED_OUTCOME_MESSAGE,
            STRANDED_RECONCILE_REASON,
        )
        .await
        .map_err(|error| {
            DurableError::invalid(format!(
                "The tool calls the failed attempt left unanswered cannot be settled: {error}"
            ))
        })?;
        if !stranded.is_empty() {
            tracing::warn!(
                calls = ?stranded,
                "recorded the tool calls of the failed attempt as interrupted"
            );
        }

        // Opened after the settlement, so the committed state this tick continues already carries
        // the interrupted results instead of the pending calls they replace.
        let host = Arc::new(
            AgentScopeExecutionHost::open(
                Arc::clone(&backend),
                lease.thread.clone(),
                lease.clone(),
                invocation_id(lease),
            )
            .await
            .map_err(convert)?,
        );

        // The committed state *is* the conversation this tick continues. Without seeding it the
        // resumed invocation would ask the model about an empty history and repeat work the
        // previous tick already committed.
        let agent = (self.build)();
        if let (Some(committed), Some(react)) = (host.state(), agent.agent()) {
            *react
                .state_handle()
                .lock()
                .map_err(|_| DurableError::invalid("agent state lock poisoned"))? =
                committed.clone();
        }

        let recorder = Arc::new(
            InvocationRecorder::open_with(
                Arc::clone(&journal) as Arc<dyn ThreadJournal>,
                Some(&lease.turn_id),
                &invocation_id(lease),
                None,
            )
            .await
            .map_err(convert)?,
        );

        let budget = Arc::new(TickBudget::new(Arc::clone(&host)));
        let mut extensions = Extensions::new();
        extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));
        extensions.insert(
            EXECUTION_HOST_CONTEXT_KEY,
            Arc::clone(&budget) as Arc<dyn AgentExecutionHost>,
        );
        extensions.insert(EXECUTION_POSITION_CONTEXT_KEY, host.position());
        // The durable turn is the authority on control; the agent only knows this signal. It is
        // installed before the invocation starts, and a cancel observed below is delivered
        // through it, so the agent ends the run as cancelled itself instead of the tick dropping
        // a stream mid-run.
        let cancel = CancelSignal::new();
        extensions.insert(CANCEL_SIGNAL_CONTEXT_KEY, cancel.clone());

        // Only the first invocation of a turn carries the input; a turn that already ran has it
        // in the committed state, and re-feeding it would duplicate the user message.
        let messages = if host.turn_started() {
            Vec::new()
        } else {
            input_messages(ctx.input())?
        };

        let mut stream =
            agent.stream_events_with_extensions(messages, StreamOptions::default(), extensions);
        let mut reply: Option<Msg> = None;
        let mut failure: Option<String> = None;
        let mut yielded = false;
        let mut drain = false;
        // A model call may never return (a provider streaming nothing, a hung socket). The only
        // place a cancel can be noticed then is here: poll the committed frame while waiting and
        // hand the request to the invocation, so the request stops it instead of waiting out the
        // stream. A pause is different: it is a yield, and the turn stays runnable.
        let mut control = tokio::time::interval(STREAM_CONTROL_POLL);
        control.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut cancel_deadline: Option<tokio::time::Instant> = None;
        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                _ = control.tick() => {
                    let frame = ctx.backend.frame(lease).await?;
                    if frame.turn.cancellation_requested {
                        // Deliver the request into the invocation: the agent checks the signal at
                        // its phase boundaries and while a model call streams, so it ends the run
                        // as cancelled on its own and everything it committed stays consistent.
                        if !cancel.is_cancelled() {
                            cancel.cancel();
                            cancel_deadline = Some(tokio::time::Instant::now() + CANCEL_GRACE);
                        } else if cancel_deadline
                            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                        {
                            // The invocation is stuck where no checkpoint runs (a tool blocking
                            // on I/O). Stop waiting and let the ending below terminate the turn;
                            // the stuck effect itself is the executor's to cancel.
                            break;
                        }
                        continue;
                    }
                    if frame.turn.status == TurnStatus::Pausing {
                        yielded = true;
                        break;
                    }
                    continue;
                }
            };
            let Some(item) = item else { break };
            match item {
                Ok(event) => {
                    if let agentscope_core::event::AgentEvent::AgentResult(data) = &event {
                        reply = data
                            .fields
                            .get("reply")
                            .and_then(|value| serde_json::from_value(value.clone()).ok());
                    }
                    if let Ok(value) = serde_json::to_value(&event) {
                        ctx.publish(&value);
                    }
                }
                // The signal did its job: a cancelled run is an ending, not a failure. The turn's
                // own status is settled below, from the frame, so a cancel that raced a
                // successful reply still wins.
                Err(Error::Cancelled(_)) => break,
                // A pause is the tick's yield, not a failure: the turn stays runnable and the
                // next tick continues from the step the loop had already opened.
                Err(Error::HostPaused) => {
                    yielded = true;
                    drain = true;
                    break;
                }
                // An interruption is a yield too, not a failure: the run stopped where a
                // resume can continue it, so the turn keeps its position and the next tick
                // carries on. Settling it here would turn a host's own interrupt into a
                // refusal on the client.
                Err(Error::Interrupted(_)) => {
                    yielded = true;
                    drain = true;
                    break;
                }
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }
        if drain {
            // A pause is reported through the same stream that carries the events, so whatever
            // the invocation produced just before it (a tool result that finished, a block end)
            // may still be buffered. Drain it before handing the turn back: a host rendering a
            // live stream must not see a tool call stuck at `InProgress` because the tick ended
            // a moment too early.
            while let Some(item) = stream.next().await {
                if let Ok(event) = item {
                    if let Ok(value) = serde_json::to_value(&event) {
                        ctx.publish(&value);
                    }
                }
            }
        }
        drop(stream);
        // A reconciled interruption is a *normal* reply carrying the interrupted reason: the run
        // stopped without failing, and the state already has the recovery message appended. The
        // turn keeps its position so the next tick continues it; settling here would answer the
        // prompt with the recovery text.
        let interrupted = reply
            .as_ref()
            .is_some_and(|reply| reply.generate_reason() == GenerateReason::Interrupted);
        // Control asked while this invocation ran is only *observed* here: a terminal status can
        // be written by the writer that holds the turn and by nobody else, so leaving it to the
        // next claimant would keep a cancelled turn `Cancelling` forever.
        let frame = ctx.backend.frame(lease).await?;
        let cancelled = frame.turn.cancellation_requested;
        let paused = frame.turn.status == TurnStatus::Pausing || yielded || interrupted;

        // An effect whose outcome is unknown can never be applied, so this turn must not report
        // success: a reply is not evidence that the effect happened. Answering honestly with a
        // failure that names the calls that stayed unknown is what keeps a retry from looping
        // and from claiming work nobody can verify.
        if !cancelled && !paused {
            let unknown: Vec<&str> = frame
                .tools
                .iter()
                .filter(|tool| {
                    tool.result.as_ref().map(|result| result.state) == Some(ExecutionState::Unknown)
                })
                .map(|tool| tool.request.execution_id.as_str())
                .collect();
            if failure.is_none() && !unknown.is_empty() {
                failure = Some(format!("{UNKNOWN_OUTCOME} Calls: {}.", unknown.join(", ")));
            }
        }

        // Close the native run with the status this invocation really reached: a host renders
        // the journal, and a run it never sees ending is indistinguishable from one that hung.
        // The recorder does not write these facts on its own (only a call that owns its recorder
        // does), so the tick that supplies it has to.
        let (status, detail) = if cancelled {
            ("cancelled", None)
        } else if paused {
            ("suspended", None)
        } else if let Some(message) = failure.as_deref() {
            ("failed", Some(message))
        } else if let Some(reply) = reply.as_ref() {
            recorder.result(Some(reply)).map_err(convert)?;
            ("completed", None)
        } else {
            ("failed", Some(NO_REPLY))
        };
        // Commit what the invocation recorded but did not flush — the step it opened just before
        // the pause — and seal the run. The next tick resumes from the committed position, not a
        // buffered one.
        let state = agent
            .agent()
            .and_then(|react| react.state_handle().lock().ok().map(|state| state.clone()));
        match state {
            Some(state) => recorder
                .finish_with_error(&state, status, detail)
                .await
                .map_err(convert)?,
            // Without a native state there is nothing to snapshot; still flush what the run
            // recorded so the next tick's position is committed.
            None => {
                recorder.flush().await.map_err(convert)?;
                recorder.close().await;
            }
        }
        // The recorder owns the lease heartbeat; stopping it also leaves the borrowed lease in
        // place for the runtime that handed it over.
        journal.detach_lease();

        if cancelled {
            return self
                .settle(ctx, TurnStatus::Cancelled, Value::Null, None)
                .await;
        }
        // A pause is the runtime's to complete when the lease is handed back; settling the reply
        // here would overwrite the pause the host asked for.
        if paused {
            return Ok(());
        }
        if let Some(message) = failure {
            return self
                .settle(ctx, TurnStatus::Failed, Value::Null, Some(message))
                .await;
        }
        let reply = reply.ok_or_else(|| DurableError::invalid(NO_REPLY))?;
        let output = serde_json::to_value(&reply)?;
        self.settle(ctx, TurnStatus::Succeeded, output, None).await
    }
}

/// The identity of the invocation this tick owns.
///
/// It is deterministic per lease (one run per claim), which is what the execution host needs to
/// name its finalization fact: a replay of the same lease deduplicates instead of writing twice.
fn invocation_id(lease: &Lease) -> String {
    format!("invoke:{}:{}", lease.turn_id, lease.epoch)
}

/// Stops the core loop once the tick's one model call is spent.
///
/// The loop reports a `Model` boundary immediately before every model call, so the second one is
/// where the tick yields. Pausing *there* (rather than at the next reasoning boundary) is what
/// keeps the iteration budget advancing: the loop has already opened the new step, so the
/// committed position names the iteration the next tick resumes at instead of repeating one.
struct TickBudget {
    inner: Arc<AgentScopeExecutionHost>,
    model_calls: AtomicUsize,
}

impl TickBudget {
    fn new(inner: Arc<AgentScopeExecutionHost>) -> Self {
        Self {
            inner,
            model_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl AgentExecutionHost for TickBudget {
    async fn before_boundary(
        &self,
        boundary: ExecutionBoundary,
        context: &RuntimeContext,
        state: &mut AgentState,
    ) -> CoreResult<()> {
        // The host still runs every boundary: a cancelled or re-fenced turn pauses here anyway,
        // and the acting boundary is what logs a call before it runs.
        self.inner.before_boundary(boundary, context, state).await?;
        if boundary == ExecutionBoundary::Model
            && self.model_calls.fetch_add(1, Ordering::SeqCst) >= MODEL_CALLS_PER_TICK
        {
            // A pause, not a failure: the turn stays runnable and the next tick continues.
            return Err(Error::HostPaused);
        }
        Ok(())
    }

    fn resume_result(&self) -> Option<Msg> {
        self.inner.resume_result()
    }

    async fn finish<'a>(
        &'a self,
        context: &RuntimeContext,
        result: &Msg,
        continuation: FinishContinuation<'a>,
    ) -> CoreResult<()> {
        self.inner.finish(context, result, continuation).await
    }
}

/// Maps the turn's admitted input to the messages this invocation runs with.
///
/// `null` is a wakeup (no prompt), a string is user text, and an object is either a serialized
/// `Msg` (it has a `role`) or `{ "text": ... }` — the shape `DurableSession::submit` callers
/// already use.
fn input_messages(input: &Value) -> Result<Vec<Msg>> {
    match input {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![Msg::user(text.clone())]),
        Value::Object(map) => {
            if map.contains_key("role") {
                Ok(vec![serde_json::from_value::<Msg>(input.clone())?])
            } else if let Some(text) = map.get("text").and_then(Value::as_str) {
                Ok(vec![Msg::user(text)])
            } else {
                Err(DurableError::invalid(
                    "The turn input carries neither a message nor text",
                ))
            }
        }
        other => Err(DurableError::invalid(format!(
            "The turn input is not a message: {other}"
        ))),
    }
}

fn convert(error: Error) -> DurableError {
    DurableError::invalid(format!("{error}"))
}
