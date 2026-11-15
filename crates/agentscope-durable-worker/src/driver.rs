//! When to run a step: the only thing a deployment shape gets to decide.

use std::sync::Arc;

use agentscope_durable::types::ThreadKey;
use agentscope_durable::{Result, TickOutcome};
use async_trait::async_trait;

use crate::worker::{StepReport, Worker};

/// How many steps one resident drive may take before it hands control back.
///
/// A burst is bounded so a brain that always wants another effect cannot pin a host thread; the
/// host calls [`Driver::drive`] again on its next timer tick.
pub const DEFAULT_MAX_STEPS: usize = 16;

/// What a drive did, in order.
#[derive(Debug, Clone)]
pub struct DriveReport {
    pub steps: Vec<StepReport>,
}

impl DriveReport {
    /// Whether the last step left work that is already due.
    ///
    /// The verdict itself is the worker's ([`StepReport::more_work`]); a driver must never reach
    /// its own conclusion from a turn, a tick outcome or an intent.
    pub fn more_work(&self) -> bool {
        self.steps.last().is_some_and(|step| step.more_work)
    }

    /// The tick outcome of every step, in order. This is the sequence the driver contract
    /// compares, because it is all a deployment shape can legitimately change.
    pub fn ticks(&self) -> Vec<TickOutcome> {
        self.steps.iter().map(|step| step.tick).collect()
    }

    pub fn effects(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.effect.is_some())
            .count()
    }

    pub fn writes(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.written_back.is_some())
            .count()
    }
}

/// A wakeup source. Decides *how often* [`Worker::step`] runs, never *what* it does.
#[async_trait]
pub trait Driver: Send + Sync {
    async fn drive(&self, thread: &ThreadKey) -> Result<DriveReport>;
}

/// A resident host: drains the work that is already due, then returns.
///
/// It spawns nothing and sleeps on nothing (R4): the host owns the clock and calls `drive` from
/// its own thread or timer. Between two calls the worker keeps no state, so a crash here costs
/// nothing beyond the step that was in flight.
pub struct LocalDriver {
    worker: Arc<Worker>,
    max_steps: usize,
}

impl LocalDriver {
    pub fn new(worker: Arc<Worker>) -> Self {
        Self {
            worker,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }
}

#[async_trait]
impl Driver for LocalDriver {
    async fn drive(&self, thread: &ThreadKey) -> Result<DriveReport> {
        let mut steps = Vec::new();
        while steps.len() < self.max_steps {
            let step = self.worker.step(thread).await?;
            let more = step.more_work;
            steps.push(step);
            if !more {
                break;
            }
        }
        Ok(DriveReport { steps })
    }
}

/// An edge host: one wakeup, one step.
///
/// A platform that only runs while it is being called cannot drain a queue, so it takes exactly
/// one unit of work and reports whether another is already due — the platform decides whether to
/// wake it again immediately. That is the whole difference from [`LocalDriver`], and it is a
/// difference in call count, not in behaviour: the sequence of ticks a `LocalDriver` produces is
/// the sequence repeated `EdgeDriver` calls produce.
pub struct EdgeDriver {
    worker: Arc<Worker>,
}

impl EdgeDriver {
    pub fn new(worker: Arc<Worker>) -> Self {
        Self { worker }
    }
}

#[async_trait]
impl Driver for EdgeDriver {
    async fn drive(&self, thread: &ThreadKey) -> Result<DriveReport> {
        Ok(DriveReport {
            steps: vec![self.worker.step(thread).await?],
        })
    }
}
