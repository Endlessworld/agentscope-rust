//! Binding to a concrete agent implementation.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::executor::{Executor, ExecutorRegistry};
use crate::session_backend::SessionBackend;
use crate::types::{
    AppliedResult, Commit, ExecutionState, Frame, Lease, RunStepRef, ThreadSpec, ToolExecution,
    TurnStatus, TurnView,
};
use crate::{DurableError, Result};

/// What an adapter promises about its native state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterCapabilities {
    /// Version of the native state format; a checkpoint is only restorable under it.
    pub state_version: String,
    /// `execute` can resume from a checkpoint produced by a previous process.
    pub recovery_boundary: bool,
    /// Events are published as they happen (best effort), not only at settle time.
    pub event_delivery: bool,
}

/// Everything an adapter needs to run one turn and record what it did.
///
/// The adapter never mutates the backend directly: it commits through `backend` with the
/// `lease`, which is what makes the checkpoint and the tool intent atomic.
pub struct HarnessContext {
    pub definition: ThreadSpec,
    pub frame: Frame,
    pub backend: Arc<dyn SessionBackend>,
    pub executor: Arc<dyn Executor>,
    /// The named executors a call's `executor` field routes to.
    pub executors: Arc<ExecutorRegistry>,
    pub lease: Lease,
    /// Where the adapter publishes a run's events while it is still going.
    pub events: Option<EventSink>,
}

/// A host sink for the events of a run that has not settled yet.
///
/// The payload is the event the adapter serialized for its journal; a host that renders a live
/// stream (ACP, a CLI) decodes it, and one that does not installs no sink at all.
pub type EventSink = Arc<dyn Fn(&Value) + Send + Sync>;

impl HarnessContext {
    pub fn turn(&self) -> &TurnView {
        &self.frame.turn
    }

    pub fn input(&self) -> &Value {
        &self.frame.input
    }

    pub fn checkpoint(&self) -> &Value {
        &self.frame.checkpoint
    }

    /// Hands one event to the host that asked for this tick; a no-op without a host sink.
    pub fn publish(&self, event: &Value) {
        if let Some(sink) = &self.events {
            sink(event);
        }
    }
}

/// Runs turns of one harness/definition/state-version triple.
#[async_trait]
pub trait HarnessAdapter: Send + Sync {
    fn capabilities(&self) -> AdapterCapabilities;

    /// Advances the turn as far as it can go, committing along the way.
    ///
    /// Returning `Ok(())` means the turn reached a settled state (terminal, waiting or
    /// paused); returning an error hands the turn back to the runtime for recovery.
    async fn execute(&self, ctx: &HarnessContext) -> Result<()>;

    /// Records a turn status the adapter cannot derive itself (host cancellation, crash).
    ///
    /// The default applies the effects this run really produced and writes `output`/`error` as
    /// the turn's ending; an adapter that derives its own ending overrides it.
    async fn settle(
        &self,
        ctx: &HarnessContext,
        status: TurnStatus,
        output: Value,
        error: Option<String>,
    ) -> Result<()> {
        settle_turn(ctx.backend.as_ref(), &ctx.lease, status, output, error).await
    }
}

/// Applies the effects a run really produced, then writes the ending the host observed.
///
/// `Succeeded` requires every succeeded tool to be applied, so the markers for the effects of
/// this run are committed first (`appliedAt` is immutable, so this happens once); the ending is
/// committed after them. Both commits are keyed by the turn, the revision and the target status,
/// so repeating `settle` for the same ending is a no-op, and a different ending for a settled
/// turn is refused: a terminal status is never rewritten.
pub async fn settle_turn(
    backend: &dyn SessionBackend,
    lease: &Lease,
    status: TurnStatus,
    output: Value,
    error: Option<String>,
) -> Result<()> {
    let turn = backend.turn(&lease.thread, &lease.turn_id).await?;
    if turn.status.terminal() {
        if turn.status == status {
            return Ok(());
        }
        return Err(DurableError::conflict("Turn is already settled"));
    }
    // A fenced read: only the writer that still owns the turn may end it.
    let frame = backend.frame(lease).await?;
    let succeeded: Vec<ToolExecution> = frame
        .tools
        .iter()
        .filter(|tool| {
            !tool.applied
                && tool.result.as_ref().map(|result| result.state)
                    == Some(ExecutionState::Succeeded)
        })
        .cloned()
        .collect();
    if !succeeded.is_empty() {
        let applied_at = RunStepRef::new(latest_run_id(backend, lease).await?, None)?;
        let mut commit = Commit::new(
            format!("settle:apply:{}:{}", lease.turn_id, frame.turn.revision),
            frame.turn.revision,
            TurnStatus::Running,
        );
        commit.checkpoint = frame.checkpoint.clone();
        commit.applied_results = succeeded
            .iter()
            .map(|tool| AppliedResult {
                execution_id: tool.request.execution_id.clone(),
                applied_at: applied_at.clone(),
            })
            .collect();
        backend.commit(lease, commit).await?;
    }
    let frame = backend.frame(lease).await?;
    let mut commit = Commit::new(
        format!("settle:{}:{:?}", lease.turn_id, status),
        frame.turn.revision,
        status,
    );
    commit.checkpoint = frame.checkpoint.clone();
    commit.output = output;
    commit.error = error;
    backend.commit(lease, commit).await?;
    Ok(())
}

/// The run an applied result is attributed to: the newest run this turn recorded.
async fn latest_run_id(backend: &dyn SessionBackend, lease: &Lease) -> Result<String> {
    let runs = backend
        .runs(&lease.thread, &lease.turn_id, None, 1000)
        .await?;
    runs.items
        .last()
        .map(|run| run.run_id.clone())
        .ok_or_else(|| DurableError::invalid("Result application requires a recorded run"))
}
