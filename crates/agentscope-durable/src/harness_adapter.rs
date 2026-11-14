//! Binding to a concrete agent implementation.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::executor::Executor;
use crate::session_backend::SessionBackend;
use crate::types::{Frame, Lease, ThreadSpec, TurnStatus, TurnView};
use crate::Result;

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
    pub lease: Lease,
}

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
    async fn settle(
        &self,
        _ctx: &HarnessContext,
        _status: TurnStatus,
        _error: Option<String>,
    ) -> Result<()> {
        Ok(())
    }
}
