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
use agentscope_core::message::{
    ContentBlock, GenerateReason, Msg, MsgRole, TextBlock, ToolResultBlock, ToolResultState,
    ToolUseBlock,
};
use agentscope_core::state::AgentState;
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::{
    DurableError, ExecutionResult, ExecutionState, Lease, Result, ToolExecution, TurnControl,
    TurnStatus,
};
use agentscope_harness::HarnessAgent;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use crate::checkpoint::STATE_VERSION;
use crate::{AgentScopeExecutionHost, BackendThreadJournal};

/// Model calls one tick may start.
const MODEL_CALLS_PER_TICK: usize = 1;

/// What a tick reports when its invocation ended without a reply.
const NO_REPLY: &str = "The invocation produced no reply";

/// The first interval at which a running invocation re-reads the committed frame for control.
///
/// A cancel or pause can arrive while the model call is still streaming, and a provider that
/// never returns would otherwise hold the turn until its lease expires. Polling is the only
/// way to observe the request from inside the call that is blocking.
const STREAM_CONTROL_POLL_MIN: std::time::Duration = std::time::Duration::from_millis(50);

/// The interval the poll backs off to while nothing changes.
///
/// A hung provider produces no events at all, and the poll was the only thing still running: at
/// a fixed 50 ms a single stalled model call kept re-reading the session several times a second
/// for as long as it hung. Backing off to a second while the answer stays the same costs a
/// request at most a second of latency to land, and any event from the stream — or any change in
/// the control answer — returns the poll to the fast interval at once.
const STREAM_CONTROL_POLL_MAX: std::time::Duration = std::time::Duration::from_secs(1);

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
            let mut state = committed.clone();
            // A committed tool result whose batch carried no state event leaves the checkpoint
            // showing the call as still unanswered (see the helper); answering it here is what
            // keeps the resumed run from executing the same effect a second time.
            answer_known_tool_results(&mut state, &ctx.frame.tools);
            *react
                .state_handle()
                .lock()
                .map_err(|_| DurableError::invalid("agent state lock poisoned"))? = state;
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
        // The invocation itself can end as cancelled: the host turns a cancel request into
        // `Error::Cancelled` at its next phase boundary. That is an ending this tick observed,
        // not a fact to re-derive from the frame — the turn must not be resumed after it.
        let mut invocation_cancelled = false;
        let mut drain = false;
        // A model call may never return (a provider streaming nothing, a hung socket). The only
        // place a cancel can be noticed then is here: poll the committed control facts while
        // waiting and hand the request to the invocation, so the request stops it instead of
        // waiting out the stream. A pause is different: it is a yield, and the turn stays
        // runnable. The poll backs off while its answer stays the same — an idle stream must not
        // cost a read per interval for as long as the provider hangs — and returns to the fast
        // interval as soon as anything moves.
        let mut poll_delay = STREAM_CONTROL_POLL_MIN;
        let mut poll_at = tokio::time::Instant::now() + poll_delay;
        let mut observed: Option<TurnControl> = None;
        let mut cancel_deadline: Option<tokio::time::Instant> = None;
        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                _ = tokio::time::sleep_until(poll_at) => {
                    let control = ctx.backend.control_state(lease).await?;
                    poll_delay = if observed == Some(control) {
                        (poll_delay * 2).min(STREAM_CONTROL_POLL_MAX)
                    } else {
                        STREAM_CONTROL_POLL_MIN
                    };
                    observed = Some(control);
                    // Waiting out a cancel is not idle time: the grace deadline has to be noticed
                    // on time, so the fast interval stays while one is running.
                    if cancel.is_cancelled() {
                        poll_delay = STREAM_CONTROL_POLL_MIN;
                    }
                    poll_at = tokio::time::Instant::now() + poll_delay;
                    if control.cancellation_requested {
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
                    if control.status == TurnStatus::Pausing {
                        yielded = true;
                        break;
                    }
                    continue;
                }
            };
            let Some(item) = item else { break };
            // Anything the stream produces means it is alive: ask again at the fast interval.
            poll_delay = STREAM_CONTROL_POLL_MIN;
            poll_at = tokio::time::Instant::now() + poll_delay;
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
                // The signal did its job: a cancelled run is an ending, not a failure, and the
                // reason is carried by the error itself instead of being inferred later from
                // the frame.
                Err(Error::Cancelled(_)) => {
                    invocation_cancelled = true;
                    break;
                }
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
        //
        // A call the journal reported as *deferred* (the offload placeholder) left this turn for
        // a background runner: its record says `PENDING`, which blocks every terminal status,
        // and nothing in the turn can confirm the effect. Ask the executor what became of it
        // before reading the frame, so a finished effect can be applied and an unknowable one
        // ends the turn honestly instead of parking it.
        let frame = resolve_offloaded_effects(ctx).await?;
        let cancelled = frame.turn.cancellation_requested || invocation_cancelled;
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
            // The turn is being stopped while effects are still in flight. A cancelled turn can
            // only be written once its effects are settled, and the executor is the one that
            // knows whether one finished before the request landed.
            let frame = ctx.backend.frame(&ctx.lease).await?;
            settle_cancelled_effects(ctx, &frame).await;
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

/// Asks the executor what became of the calls this turn handed off to a background runner.
///
/// The offload placeholder is recorded as a non-terminal result (`SUSPENDED` in the journal,
/// `PENDING` on the turn), so the effect left the turn while its record still says "in flight".
/// Such a record blocks every terminal status, and no part of the turn can confirm the effect
/// itself, so the executor is the only authority on it: its answer is accepted as the result
/// (`SUCCEEDED`/`FAILED` end the turn with what really happened; an answer the executor cannot
/// give is recorded as `UNKNOWN`, which keeps the turn from claiming success it cannot prove).
/// The frame is re-read only when a result was accepted.
///
async fn resolve_offloaded_effects(ctx: &HarnessContext) -> Result<agentscope_durable::Frame> {
    let mut frame = ctx.backend.frame(&ctx.lease).await?;
    if !ctx.executor.capabilities().lookup_by_execution_id {
        return Ok(frame);
    }
    let deferred: Vec<agentscope_durable::ExecutionRequest> = frame
        .tools
        .iter()
        .filter(|tool| {
            tool.result.as_ref().map(|result| result.state) == Some(ExecutionState::Pending)
        })
        .map(|tool| tool.request.clone())
        .collect();
    if deferred.is_empty() {
        return Ok(frame);
    }
    let mut accepted = false;
    for request in deferred {
        let result = match ctx.executor.query(&request).await {
            Ok(result) => result,
            Err(error) => {
                // An executor that cannot answer is the same case as one that answers nothing:
                // the effect stays unknown rather than being claimed either way.
                tracing::warn!(
                    execution = %request.execution_id,
                    %error,
                    "the executor could not report an offloaded effect"
                );
                ExecutionResult::unknown(format!(
                    "The executor did not report the outcome of this call: {error}"
                ))
            }
        };
        if matches!(
            result.state,
            ExecutionState::Pending | ExecutionState::Running
        ) {
            // Still running: the answer is not in yet, and a terminal status cannot be written.
            continue;
        }
        ctx.backend
            .accept_result(&ctx.lease.thread, &request.execution_id, result)
            .await?;
        accepted = true;
    }
    if accepted {
        frame = ctx.backend.frame(&ctx.lease).await?;
    }
    Ok(frame)
}

/// Settles the effects a cancelled turn still holds in flight.
///
/// A terminal status is refused while any effect is unsettled, and a turn stopped by a cancel
/// has no invocation left to finish them. The executor is asked first — an effect that finished
/// before the request landed is a fact worth keeping — and whatever is still in flight is
/// recorded as cancelled: the turn's own record says the host stopped waiting, while the
/// background runner keeps going and delivers its result as a notification.
async fn settle_cancelled_effects(ctx: &HarnessContext, frame: &agentscope_durable::Frame) {
    let lookup = ctx.executor.capabilities().lookup_by_execution_id;
    for tool in frame.tools.iter().filter(|tool| {
        !tool
            .result
            .as_ref()
            .is_some_and(|result| result.state.concluded())
    }) {
        let mut confirmed = None;
        if lookup {
            if let Ok(answered) = ctx.executor.query(&tool.request).await {
                // Only an answer that really happened is worth keeping; "cannot tell" is what
                // the cancel itself resolves.
                if answered.state.terminal() {
                    confirmed = Some(answered);
                }
            }
        }
        let result = confirmed.unwrap_or(ExecutionResult {
            state: ExecutionState::Cancelled,
            handle: tool
                .result
                .as_ref()
                .and_then(|result| result.handle.clone()),
            result: Value::Null,
            error: Some("The turn was cancelled before the effect reported".to_string()),
        });
        if let Err(error) = ctx
            .backend
            .accept_result(&ctx.lease.thread, &tool.request.execution_id, result)
            .await
        {
            // Losing the race to another writer is not a reason to fail the ending.
            tracing::debug!(
                execution = %tool.request.execution_id,
                %error,
                "cannot record the cancelled effect"
            );
        }
    }
}

/// Answers the tool calls the turn already holds a result for.
///
/// The journal and the checkpoint are written by different commits: a batch that records
/// `tool/result` and no state event keeps the previous checkpoint
/// (`BackendThreadJournal`), so the committed context still shows the call as unanswered while
/// its effect is already committed on the turn. Core decides what to act on from the state
/// alone, and a repeated dispatch record is deduplicated without stopping the local side effect
/// — so the resumed run would execute that tool a second time. Folding the known results back
/// in before the invocation starts is what prevents the duplicate.
fn answer_known_tool_results(state: &mut AgentState, tools: &[ToolExecution]) {
    let pending = crate::tool_facts::pending_calls(state);
    if pending.is_empty() {
        return;
    }
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for call in pending {
        let Some(tool) = tools
            .iter()
            .find(|tool| tool.request.call.call_id == call.id)
        else {
            continue;
        };
        let Some(result) = &tool.result else { continue };
        if !result.state.concluded() {
            continue;
        }
        blocks.push(tool_result_block(&call, result));
    }
    if blocks.is_empty() {
        return;
    }
    state
        .context_mut()
        .push(Msg::builder().role(MsgRole::Tool).content(blocks).build());
}

/// Rebuilds the native result block a durable result was derived from.
fn tool_result_block(call: &ToolUseBlock, result: &ExecutionResult) -> ContentBlock {
    let content = serde_json::from_value::<Vec<ContentBlock>>(result.result.clone())
        .ok()
        .filter(|blocks| !blocks.is_empty())
        .unwrap_or_else(|| {
            vec![ContentBlock::Text(TextBlock::new(
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "The tool ran in an earlier attempt.".to_string()),
            ))]
        });
    let state = match result.state {
        ExecutionState::Succeeded => ToolResultState::Success,
        ExecutionState::Failed => ToolResultState::Error,
        ExecutionState::Cancelled => ToolResultState::Denied,
        // Unknown and the in-flight states are never applied; an answer they cannot give is an
        // interruption, which is what keeps core from running the call again.
        _ => ToolResultState::Interrupted,
    };
    ContentBlock::ToolResult(ToolResultBlock::new(&call.id, &call.name, content).with_state(state))
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use agentscope_core::message::{
        ContentBlock, Msg, MsgRole, TextBlock, ToolResultState, ToolUseBlock,
    };
    use agentscope_core::state::AgentState;
    use agentscope_durable::backend::TxnBackend;
    use agentscope_durable::executor::{Executor, ExecutorCapabilities};
    use agentscope_durable::harness_adapter::settle_turn;
    use agentscope_durable::types::*;
    use agentscope_durable::{ExecutionState, Result, SessionBackend};
    use async_trait::async_trait;
    use serde_json::{json, Value};

    use super::{
        answer_known_tool_results, resolve_offloaded_effects, settle_cancelled_effects,
        HarnessContext, STATE_VERSION,
    };

    fn state_with_tool_call(call_id: &str) -> AgentState {
        let mut state = AgentState::default();
        state.context_mut().push(Msg::text(MsgRole::User, "run it"));
        let input = json!({ "cmd": "ls" })
            .as_object()
            .cloned()
            .expect("object input");
        let call = ToolUseBlock::new(call_id, "shell_execute", input);
        state.context_mut().push(
            Msg::builder()
                .role(MsgRole::Assistant)
                .content(vec![ContentBlock::ToolUse(call)])
                .build(),
        );
        state
    }

    fn execution(call_id: &str, state: ExecutionState) -> ToolExecution {
        let thread = ThreadKey::new(
            SessionKey::new("ns", "session").expect("session key"),
            "main",
        )
        .expect("thread key");
        let call =
            ToolCall::new(call_id, "local", "shell_execute", json!({ "cmd": "ls" })).expect("call");
        let request = ExecutionRequest::new(format!("turn-1:{call_id}"), thread, "turn-1", call)
            .expect("execution request");
        ToolExecution {
            request,
            dispatch_attempted: true,
            result: Some(ExecutionResult {
                state,
                handle: None,
                result: json!([{ "type": "text", "text": "listed" }]),
                error: None,
            }),
            applied: false,
            applied_at: None,
            delegation: None,
        }
    }

    /// The stale-checkpoint case: the journal holds a result the checkpoint does not.
    #[test]
    fn a_committed_result_answers_a_call_the_checkpoint_still_shows_as_pending() {
        let mut state = state_with_tool_call("call-1");
        assert_eq!(
            crate::tool_facts::pending_calls(&state).len(),
            1,
            "the fixture starts with the call unanswered"
        );

        answer_known_tool_results(
            &mut state,
            &[execution("call-1", ExecutionState::Succeeded)],
        );

        assert!(
            crate::tool_facts::pending_calls(&state).is_empty(),
            "the call must be answered, or core would run it a second time"
        );
        let answers: Vec<&ContentBlock> = state
            .context()
            .iter()
            .flat_map(|message| message.content().iter())
            .filter(|block| matches!(block, ContentBlock::ToolResult(_)))
            .collect();
        assert_eq!(answers.len(), 1);
        let ContentBlock::ToolResult(result) = answers[0] else {
            unreachable!("filtered above")
        };
        assert_eq!(result.id.as_deref(), Some("call-1"));
        assert_eq!(result.state, ToolResultState::Success);
        assert_eq!(
            result.output,
            vec![ContentBlock::Text(TextBlock::new("listed"))]
        );
    }

    #[test]
    fn a_call_without_a_committed_result_stays_pending() {
        let mut state = state_with_tool_call("call-1");
        let mut pending = execution("call-2", ExecutionState::Succeeded);
        pending.request.call.call_id = "call-2".to_string();
        answer_known_tool_results(&mut state, &[pending]);
        assert_eq!(
            crate::tool_facts::pending_calls(&state).len(),
            1,
            "a result of a different call must not answer this one"
        );
    }

    /// An offloaded call: the placeholder result is `PENDING`, which blocks every terminal
    /// status until the executor says what became of the effect.
    async fn offloaded_turn(state: ExecutionState) -> (Arc<TxnBackend>, Lease) {
        let backend = Arc::new(TxnBackend::in_memory());
        let key = SessionKey::new("ns", "session").expect("session key");
        backend
            .create(SessionSpec {
                key: key.clone(),
                primary_binding: ThreadBinding::new("harness-a", "def-1", STATE_VERSION)
                    .expect("binding"),
                metadata: Value::Null,
            })
            .await
            .expect("create session");
        let thread = ThreadKey::new(key, "main").expect("thread key");
        backend
            .submit(&thread, "req-1", json!({ "text": "go" }))
            .await
            .expect("submit");
        let lease = backend
            .claim(&thread, "owner-a", Duration::from_secs(30))
            .await
            .expect("claim")
            .expect("a claimable turn");
        backend.prepare_run(&lease, "req-1").await.expect("run");
        let frame = backend.frame(&lease).await.expect("frame");
        let call = ToolCall::new("call-1", "local", "shell_execute", json!({ "cmd": "ls" }))
            .expect("call");
        let mut intent = Commit::new("intent", frame.turn.revision, TurnStatus::Running);
        // A result may only be applied together with the native state that observed it.
        intent.checkpoint = crate::checkpoint::encode(&AgentState::default()).expect("checkpoint");
        intent.tools = vec![call];
        backend.commit(&lease, intent).await.expect("tool intent");
        let execution_id = format!("{}:call-1", lease.turn_id);
        backend
            .begin_dispatch(&lease, &execution_id)
            .await
            .expect("dispatch");
        backend
            .accept_result(
                &thread,
                &execution_id,
                ExecutionResult {
                    state,
                    handle: None,
                    result: Value::Null,
                    error: None,
                },
            )
            .await
            .expect("placeholder result");
        (backend, lease)
    }

    /// Answers with a fixed result, like a host whose lookup knows the effect's fate.
    struct FixedExecutor {
        answer: ExecutionResult,
        lookups: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Executor for FixedExecutor {
        fn capabilities(&self) -> ExecutorCapabilities {
            ExecutorCapabilities {
                lookup_by_execution_id: true,
                idempotent_start: false,
                cancellation: false,
            }
        }

        async fn start(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
            Ok(ExecutionResult::unknown("not dispatched here"))
        }

        async fn query(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.answer.clone())
        }
    }

    async fn context(
        backend: &Arc<TxnBackend>,
        lease: &Lease,
        executor: Arc<dyn Executor>,
    ) -> HarnessContext {
        let frame = backend.frame(lease).await.expect("frame");
        HarnessContext {
            definition: frame.definition.clone(),
            frame,
            backend: Arc::clone(backend) as Arc<dyn SessionBackend>,
            executor,
            lease: lease.clone(),
            events: None,
        }
    }

    #[tokio::test]
    async fn a_finished_offload_lets_the_turn_report_success() {
        let (backend, lease) = offloaded_turn(ExecutionState::Pending).await;
        let executor = Arc::new(FixedExecutor {
            answer: ExecutionResult::succeeded(json!({ "background": true })),
            lookups: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = context(&backend, &lease, Arc::clone(&executor) as Arc<dyn Executor>).await;
        let frame = resolve_offloaded_effects(&ctx).await.expect("resolve");
        assert_eq!(
            frame.tools[0].result.as_ref().map(|result| result.state),
            Some(ExecutionState::Succeeded),
            "the executor's answer is the turn's record of the effect"
        );

        settle_turn(
            backend.as_ref(),
            &lease,
            TurnStatus::Succeeded,
            json!({ "reply": "done" }),
            None,
        )
        .await
        .expect("settle");
        let turn = backend
            .turn(&lease.thread, &lease.turn_id)
            .await
            .expect("turn");
        assert_eq!(turn.status, TurnStatus::Succeeded);
    }

    #[tokio::test]
    async fn an_offload_nothing_can_confirm_ends_the_turn_instead_of_parking_it() {
        let (backend, lease) = offloaded_turn(ExecutionState::Pending).await;
        let executor = Arc::new(FixedExecutor {
            answer: ExecutionResult::unknown("the background tool has not reported yet"),
            lookups: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = context(&backend, &lease, Arc::clone(&executor) as Arc<dyn Executor>).await;
        let frame = resolve_offloaded_effects(&ctx).await.expect("resolve");
        assert_eq!(
            frame.tools[0].result.as_ref().map(|result| result.state),
            Some(ExecutionState::Unknown)
        );

        // The turn cannot claim an effect nobody confirmed...
        let refused = settle_turn(
            backend.as_ref(),
            &lease,
            TurnStatus::Succeeded,
            json!({ "reply": "done" }),
            None,
        )
        .await;
        assert_eq!(
            refused.expect_err("unknown outcome").code(),
            agentscope_durable::ErrorCode::Invalid
        );

        // ...and it is not left waiting for an answer: the ending names the unknown call.
        settle_turn(
            backend.as_ref(),
            &lease,
            TurnStatus::Failed,
            Value::Null,
            Some("a tool call has an unknown outcome".to_string()),
        )
        .await
        .expect("failed ending");
        let turn = backend
            .turn(&lease.thread, &lease.turn_id)
            .await
            .expect("turn");
        assert_eq!(turn.status, TurnStatus::Failed);
    }

    #[tokio::test]
    async fn an_executor_that_cannot_look_up_leaves_the_record_alone() {
        let (backend, lease) = offloaded_turn(ExecutionState::Pending).await;
        struct NoLookup;
        #[async_trait]
        impl Executor for NoLookup {
            async fn start(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
                Ok(ExecutionResult::unknown("no executor"))
            }
        }
        let ctx = context(&backend, &lease, Arc::new(NoLookup) as Arc<dyn Executor>).await;
        let frame = resolve_offloaded_effects(&ctx).await.expect("resolve");
        assert_eq!(
            frame.tools[0].result.as_ref().map(|result| result.state),
            Some(ExecutionState::Pending),
            "without a lookup the turn keeps the deferral instead of guessing"
        );
    }

    #[tokio::test]
    async fn a_cancelled_turn_settles_the_effects_it_left_in_flight() {
        let (backend, lease) = offloaded_turn(ExecutionState::Pending).await;
        let executor = Arc::new(FixedExecutor {
            answer: ExecutionResult::unknown("the background tool has not reported yet"),
            lookups: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = context(&backend, &lease, Arc::clone(&executor) as Arc<dyn Executor>).await;
        let frame = ctx.backend.frame(&lease).await.expect("frame");
        settle_cancelled_effects(&ctx, &frame).await;

        let frame = ctx.backend.frame(&lease).await.expect("frame");
        assert_eq!(
            frame.tools[0].result.as_ref().map(|result| result.state),
            Some(ExecutionState::Cancelled),
            "an effect nothing could confirm is recorded as cancelled with the turn"
        );
        // A cancelled turn can only be written once its effects are settled, so this is what
        // makes the cancellation reach the client at all.
        settle_turn(
            backend.as_ref(),
            &lease,
            TurnStatus::Cancelled,
            Value::Null,
            None,
        )
        .await
        .expect("cancelled ending");
        let turn = backend
            .turn(&lease.thread, &lease.turn_id)
            .await
            .expect("turn");
        assert_eq!(turn.status, TurnStatus::Cancelled);
    }

    #[tokio::test]
    async fn an_effect_that_finished_before_the_cancel_keeps_its_real_outcome() {
        let (backend, lease) = offloaded_turn(ExecutionState::Pending).await;
        let executor = Arc::new(FixedExecutor {
            answer: ExecutionResult::succeeded(json!({ "background": true })),
            lookups: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = context(&backend, &lease, Arc::clone(&executor) as Arc<dyn Executor>).await;
        let frame = ctx.backend.frame(&lease).await.expect("frame");
        settle_cancelled_effects(&ctx, &frame).await;

        let frame = ctx.backend.frame(&lease).await.expect("frame");
        assert_eq!(
            frame.tools[0].result.as_ref().map(|result| result.state),
            Some(ExecutionState::Succeeded),
            "a cancel must not rewrite an effect that already happened"
        );
    }
}
