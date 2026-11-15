//! Who decides what happens next, and nothing else.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use agentscope_durable::types::{ExecutionRequest, ThreadKey, ToolCall, TurnView};
use agentscope_durable::{DurableError, Result, TickOutcome};
use async_trait::async_trait;
use serde_json::Value;

use crate::intent::Intent;

/// Everything a decision may read: durable facts as of the tick that just ran.
///
/// Deliberately a snapshot of values rather than a handle to the runtime. A brain that could
/// reach the runtime could execute, and then "the brain only decides" would be a convention
/// instead of a boundary.
#[derive(Debug, Clone, Copy)]
pub struct BrainView<'a> {
    /// The thread the step is about.
    pub thread: &'a ThreadKey,
    /// The turn the tick just advanced; `None` when the thread has no active turn.
    pub turn: Option<&'a TurnView>,
    /// What the tick immediately before this decision achieved.
    pub tick: TickOutcome,
}

impl BrainView<'_> {
    /// Builds the durable request for `call` inside the turn this view is about.
    ///
    /// Every brain goes through here, so the execution identity of a call has one derivation
    /// ([`ToolCall::execution_id`]) and two brains can never disagree about what "the same
    /// effect" is.
    pub fn request(&self, call: ToolCall) -> Result<ExecutionRequest> {
        let turn = self.turn.ok_or_else(|| {
            DurableError::unavailable("the thread has no active turn to plan against")
        })?;
        ExecutionRequest::new(
            call.execution_id(&turn.turn_id),
            self.thread.clone(),
            turn.turn_id.clone(),
            call,
        )
    }
}

/// Decides the next unit of work. Produces intent only: no tool, no model call of its own.
#[async_trait]
pub trait Brain: Send + Sync {
    /// Exactly one decision per step.
    async fn think(&self, view: &BrainView<'_>) -> Result<Intent>;
}

/// A scripted decision for tests and for a host that has no model wired yet.
///
/// `Act` carries a [`ToolCall`] instead of a ready-made [`ExecutionRequest`] because the durable
/// identity needs the turn id, which only exists once a turn does.
#[derive(Debug, Clone, PartialEq)]
pub enum StubReply {
    Act(ToolCall),
    Wait(String),
    Done(Value),
}

impl StubReply {
    pub fn act(
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Result<Self> {
        Ok(Self::Act(ToolCall::new(call_id, "stub", name, arguments)?))
    }

    pub fn wait(key: impl Into<String>) -> Self {
        Self::Wait(key.into())
    }

    pub fn done(output: Value) -> Self {
        Self::Done(output)
    }
}

/// A brain that answers from a fixed script and counts how often it was asked.
///
/// The counter is the point of the stub: "one tick per step" and "one decision per step" are
/// assertions about how often this is called, not about how the runtime feels.
pub struct StubBrain {
    script: Mutex<VecDeque<StubReply>>,
    thinks: AtomicUsize,
    fallback: StubReply,
}

impl StubBrain {
    pub fn new(script: impl IntoIterator<Item = StubReply>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            thinks: AtomicUsize::new(0),
            fallback: StubReply::Done(Value::Null),
        }
    }

    /// The answer once the script is exhausted; `Done(null)` by default.
    pub fn with_fallback(mut self, fallback: StubReply) -> Self {
        self.fallback = fallback;
        self
    }

    /// How many times [`Brain::think`] was called.
    pub fn thinks(&self) -> usize {
        self.thinks.load(Ordering::SeqCst)
    }

    pub fn remaining(&self) -> usize {
        self.script.lock().expect("script").len()
    }
}

#[async_trait]
impl Brain for StubBrain {
    async fn think(&self, view: &BrainView<'_>) -> Result<Intent> {
        self.thinks.fetch_add(1, Ordering::SeqCst);
        let reply = self
            .script
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone());
        match reply {
            StubReply::Act(call) => Ok(Intent::Act(view.request(call)?)),
            StubReply::Wait(key) => Ok(Intent::wait(key)),
            StubReply::Done(output) => Ok(Intent::done(output)),
        }
    }
}
