//! A durable adapter that runs one `HarnessAgent` invocation per tick.
//!
//! The tick owns the lease, so everything happens inside it: the execution host is opened on
//! the committed facts, the native journal writes under the *same* lease (borrowed, never
//! claimed), the core loop runs, and the adapter settles the turn from what the invocation
//! observed. One tick is one model decision, not one whole turn: `ReActAgent` runs its
//! reasoning/acting loop to completion inside a single invocation, so the adapter stops that
//! loop before the second model call ([`TickBudget`]) and leaves the rest to the next tick.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_core::agent::{
    AgentExecutionHost, ExecutionBoundary, Extensions, FinishContinuation, RuntimeContext,
    StreamOptions, EXECUTION_HOST_CONTEXT_KEY, EXECUTION_POSITION_CONTEXT_KEY,
};
use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::journal::{InvocationRecorder, ThreadJournal, RECORDER_CONTEXT_KEY};
use agentscope_core::message::Msg;
use agentscope_core::state::AgentState;
use agentscope_durable::harness_adapter::{AdapterCapabilities, HarnessAdapter, HarnessContext};
use agentscope_durable::{DurableError, Lease, Result, TurnStatus};
use agentscope_harness::HarnessAgent;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use crate::{AgentScopeExecutionHost, BackendThreadJournal};

/// Version of the native state a checkpoint carries (a `core::state::AgentState` snapshot).
const STATE_VERSION: &str = "agentscope/state/1";

/// Model calls one tick may start.
const MODEL_CALLS_PER_TICK: usize = 1;

/// Runs turns of one harness definition by building the agent that executes them.
///
/// The factory is called once per tick: an invocation is a fresh agent over the committed
/// state, never a long-lived one that could carry a previous turn's services.
pub struct AgentScopeHarnessAdapter {
    build: Arc<dyn Fn() -> HarnessAgent + Send + Sync>,
}

impl AgentScopeHarnessAdapter {
    pub fn new(build: impl Fn() -> HarnessAgent + Send + Sync + 'static) -> Self {
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

        // The journal writes under the lease this tick already holds; it must not claim.
        let journal = Arc::new(
            BackendThreadJournal::new(Arc::clone(&backend), lease.thread.clone())
                .attach_lease(lease.clone()),
        );
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
        while let Some(item) = stream.next().await {
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
                // A pause is the tick's yield, not a failure: the turn stays runnable and the
                // next tick continues from the step the loop had already opened.
                Err(Error::HostPaused) => {
                    yielded = true;
                    break;
                }
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }
        drop(stream);
        // Commit what the invocation recorded but did not flush — the step it opened just before
        // the pause: the next tick resumes from the committed position, not a buffered one.
        recorder.flush().await.map_err(convert)?;
        // The recorder owns the lease heartbeat; stopping it also leaves the borrowed lease in
        // place for the runtime that handed it over.
        recorder.close().await;
        journal.detach_lease();

        if yielded {
            return Ok(());
        }
        if let Some(message) = failure {
            return self
                .settle(ctx, TurnStatus::Failed, Value::Null, Some(message))
                .await;
        }
        let reply =
            reply.ok_or_else(|| DurableError::invalid("The invocation produced no reply"))?;
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
