//! One step: one tick, at most one decision, at most one effect — and never a wait.

use std::sync::Arc;

use agentscope_durable::types::{
    ExecutionRequest, ExecutionResult, ExecutionState, Signal, ThreadKey,
};
use agentscope_durable::{DurableRuntime, Result, TickOutcome};
use serde_json::json;

use crate::brain::{Brain, BrainView};
use crate::hands::Hands;
use crate::intent::{Intent, IntentKind};

/// The signal key an effect result is written back under.
///
/// One key for every effect keeps the brain's read side simple ("the results are the signals
/// with this key") while the signal id carries the execution identity, so the write-back is
/// idempotent by the same rule every other durable write is.
pub const HANDS_SIGNAL_KEY: &str = "hands.effect";

/// What one [`Worker::step`] did. Every field is a fact about that step only; nothing here is
/// remembered between steps (R1).
#[derive(Debug, Clone)]
pub struct StepReport {
    /// What the tick at the start of the step achieved.
    pub tick: TickOutcome,
    /// The decision that followed, when the tick left something to decide.
    pub intent: Option<Intent>,
    /// The effect the hands reported, when the decision was to act.
    pub effect: Option<ExecutionResult>,
    /// The durable signal id the effect was written back under.
    pub written_back: Option<String>,
    /// Whether the same host can make more progress right now without new input.
    ///
    /// Computed here, not in a driver: this is the one verdict both a resident loop and an
    /// edge wakeup have to agree on.
    pub more_work: bool,
}

impl StepReport {
    pub fn kind(&self) -> Option<IntentKind> {
        self.intent.as_ref().map(Intent::kind)
    }
}

/// Ties the durable tick to a [`Brain`] and a [`Hands`].
///
/// The worker is the only place the three meet, and it always in the same order: tick, decide,
/// act, write back. It owns no state of its own, so two hosts sharing a backend produce the same
/// behaviour as one.
pub struct Worker {
    runtime: Arc<DurableRuntime>,
    brain: Arc<dyn Brain>,
    hands: Arc<dyn Hands>,
}

impl Worker {
    pub fn new(runtime: Arc<DurableRuntime>, brain: Arc<dyn Brain>, hands: Arc<dyn Hands>) -> Self {
        Self {
            runtime,
            brain,
            hands,
        }
    }

    pub fn runtime(&self) -> &Arc<DurableRuntime> {
        &self.runtime
    }

    /// Advances one thread by one unit of work.
    ///
    /// Exactly one tick runs, whatever the outcome; a tick that did not settle (idle, waiting,
    /// paused, failed, cancelled) leaves nothing to decide, so the brain is not asked. A settled
    /// tick on a live turn asks for at most one intent, applies at most that one effect, and
    /// writes its result back durably before returning.
    pub async fn step(&self, thread: &ThreadKey) -> Result<StepReport> {
        let tick = self.runtime.tick(thread).await?;
        let mut report = StepReport {
            tick,
            intent: None,
            effect: None,
            written_back: None,
            more_work: false,
        };
        if tick != TickOutcome::Settled {
            return Ok(report);
        }

        // The turn is read, never remembered: `thread_state` is the cheap pointer, `turn` the
        // revision the decision is made against.
        let state = self.runtime.backend().thread_state(thread).await?;
        let Some(turn_id) = state.current_turn_id else {
            return Ok(report);
        };
        let turn = self.runtime.turn(thread, &turn_id).await?;
        if turn.status.terminal() {
            // A terminal turn has nothing for a brain to decide; the host reads its ending.
            return Ok(report);
        }

        let intent = self
            .brain
            .think(&BrainView {
                thread,
                turn: Some(&turn),
                tick,
            })
            .await?;
        if let Intent::Act(request) = &intent {
            let effect = self.apply(request).await;
            // An effect nobody can account for is not permission to keep going: the turn stops
            // here until a host reconciles it (never a retry, which would risk a second effect).
            report.more_work = effect.state != ExecutionState::Unknown;
            report.written_back = Some(self.write_back(thread, &turn_id, request, &effect).await?);
            report.effect = Some(effect);
        }
        report.intent = Some(intent);
        Ok(report)
    }

    /// Applies one intent. A failed apply is `UNKNOWN`, never "did not happen".
    async fn apply(&self, request: &ExecutionRequest) -> ExecutionResult {
        match self.hands.apply(request).await {
            Ok(result) => result,
            Err(error) => ExecutionResult::unknown(error.message().to_string()),
        }
    }

    /// Records the effect durably, so the next step (in this process or another one) can see it.
    ///
    /// A signal is used instead of a checkpoint commit because the worker does not own the
    /// native conversation: the adapter that does commits the result into its state, and until
    /// then the durable answer to "what did the hands say" is this signal.
    async fn write_back(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        request: &ExecutionRequest,
        effect: &ExecutionResult,
    ) -> Result<String> {
        let signal = Signal::new(
            format!("{HANDS_SIGNAL_KEY}:{}", request.execution_id),
            HANDS_SIGNAL_KEY,
            json!({
                "executionId": request.execution_id,
                "callId": request.call.call_id,
                "state": effect.state,
                "handle": effect.handle,
                "result": effect.result,
                "error": effect.error,
            }),
        )?;
        self.runtime.signal(thread, turn_id, signal.clone()).await?;
        Ok(signal.id)
    }
}
