//! The backend contract: one session-scoped transaction per commit, fenced by a lease.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::types::*;
use crate::Result;

/// What a backend promises about atomicity and replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub protocol_version: String,
    /// Checkpoint, tool intent and events land in one transaction.
    pub atomic_checkpoint_and_intent: bool,
    pub fenced_ownership: bool,
    pub durable_waits: bool,
    pub event_replay: bool,
    pub shared_across_hosts: bool,
}

impl Default for BackendCapabilities {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_string(),
            atomic_checkpoint_and_intent: true,
            fenced_ownership: true,
            durable_waits: true,
            event_replay: true,
            shared_across_hosts: false,
        }
    }
}

/// Durable state for one session.
///
/// Every mutating call that carries a [`Lease`] is fenced by it; everything else is
/// authenticated by identity (request id, commit id, execution id).
#[async_trait]
pub trait SessionBackend: Send + Sync {
    fn capabilities(&self) -> BackendCapabilities;

    // ---- identity ----
    async fn create(&self, spec: SessionSpec) -> Result<SessionView>;
    async fn session(&self, key: &SessionKey) -> Result<SessionView>;
    async fn close_session(&self, key: &SessionKey) -> Result<SessionView>;
    async fn create_thread(&self, spec: ThreadSpec) -> Result<ThreadView>;
    async fn thread(&self, key: &ThreadKey) -> Result<ThreadView>;
    async fn close_thread(&self, key: &ThreadKey) -> Result<ThreadView>;
    async fn thread_state(&self, key: &ThreadKey) -> Result<ThreadStateView>;
    async fn threads(
        &self,
        session: &SessionKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ThreadPage>;

    // ---- turn queue ----
    async fn submit(
        &self,
        thread: &ThreadKey,
        request_id: &str,
        input: Value,
    ) -> Result<TurnReceipt>;
    async fn admission(
        &self,
        session: &SessionKey,
        request_id: &str,
    ) -> Result<Option<TurnReceipt>>;
    async fn turn(&self, thread: &ThreadKey, turn_id: &str) -> Result<TurnView>;
    async fn turns(
        &self,
        thread: &ThreadKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<TurnPage>;
    async fn history(
        &self,
        thread: &ThreadKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage>;
    async fn control(&self, thread: &ThreadKey, command: ControlCommand) -> Result<CommandReceipt>;
    async fn signal(&self, thread: &ThreadKey, turn_id: &str, signal: Signal) -> Result<()>;

    // ---- ownership ----
    async fn claim(&self, thread: &ThreadKey, owner: &str, ttl: Duration) -> Result<Option<Lease>>;
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<()>;
    async fn release(&self, lease: &Lease, delay: Duration) -> Result<()>;
    async fn frame(&self, lease: &Lease) -> Result<Frame>;
    async fn record_failure(&self, lease: &Lease, failure: Failure) -> Result<RecoveryView>;
    async fn commit(&self, lease: &Lease, commit: Commit) -> Result<CommitReceipt>;
    async fn receipt(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        commit_id: &str,
    ) -> Result<Option<CommitReceipt>>;

    // ---- invocation facts ----
    async fn prepare_run(&self, lease: &Lease, request_id: &str) -> Result<RunView>;
    async fn run(&self, thread: &ThreadKey, turn_id: &str, run_id: &str) -> Result<RunView>;
    async fn runs(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<RunPage>;
    async fn steps(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        run_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<StepPage>;
    async fn journal(&self, lease: &Lease, after: u64, limit: usize) -> Result<Vec<Value>>;
    /// Lease-free journal read for readers catching up outside the writer lease.
    async fn read_journal(
        &self,
        thread: &ThreadKey,
        after: u64,
        limit: usize,
    ) -> Result<Vec<Value>>;

    // ---- tools ----
    async fn execution(&self, thread: &ThreadKey, execution_id: &str) -> Result<ToolExecution>;
    async fn begin_dispatch(&self, lease: &Lease, execution_id: &str) -> Result<ToolExecution>;
    async fn accept_result(
        &self,
        thread: &ThreadKey,
        execution_id: &str,
        result: ExecutionResult,
    ) -> Result<ToolExecution>;

    // ---- discovery, checkpoints, events ----
    async fn ready(&self, selectors: &[HarnessSelector], limit: usize) -> Result<Vec<ThreadSpec>>;
    async fn checkpoints(&self, thread: &ThreadKey, after: u64, limit: usize) -> Result<Vec<u64>>;
    async fn checkpoint(&self, thread: &ThreadKey, position: u64) -> Result<CheckpointView>;
    async fn events(
        &self,
        session: &SessionKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<EventPage>;
    async fn snapshot(&self, session: &SessionKey) -> Result<Snapshot>;
}
