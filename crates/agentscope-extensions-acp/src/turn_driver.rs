//! The seam a session's turn execution sits behind.
//!
//! An ACP session is protocol state (id, cwd, model, config, client) plus a way to run turns.
//! This module is the second half: a [`TurnDriver`] owns everything a turn needs and answers the
//! six questions the protocol layer asks about it. Two implementations are planned — the durable
//! runtime that drives turns today, and a native session driver that does not depend on
//! `agentscope-durable` — and the protocol layer must not know which one it holds.
//!
//! [`Guarantees`] is deliberately narrow: it carries the *physical* properties a persistent store
//! can offer (cross-process leases, crash atomicity, multi-host writes) and never a feature flag.
//! A driver that cannot offer one of them still implements every capability; the difference is
//! stated where it matters (a second writer on the same session) rather than by refusing `load`,
//! `resume` or `status`.

use std::fmt;
use std::sync::Arc;

use agentscope_core::event::AgentEvent;
use agentscope_core::message::{ChatUsage, Msg};
use async_trait::async_trait;

/// Where a driver publishes the events of a turn.
///
/// The driver calls it synchronously; the session's queue bounds the backlog, so a slow client
/// never grows it without limit.
pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// One prompt, as the protocol layer sees it.
#[derive(Clone, Debug)]
pub struct DriverInput {
    pub turn_id: String,
    pub message: Msg,
    pub accepted_at_ms: i64,
    pub model: String,
    pub thought_level: String,
    /// The generation the turn started under; a cancel that advances it stops only that turn.
    pub cancel_epoch: u64,
}

/// What a finished turn reports back to the protocol layer.
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    pub stop_reason: agent_client_protocol::v1::schema::StopReason,
    pub usage: Option<ChatUsage>,
}

/// A turn's lifecycle, independent of the store that records it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnStatusView {
    #[default]
    Idle,
    Queued,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    /// Stopped without finishing: a later tick (or `session/resume`) may continue it.
    Interrupted,
}

impl TurnStatusView {
    /// Terminal turns never advance again; a waiting or interrupted one may be resumed.
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// The turn as `session/status` and the prompt response see it.
#[derive(Clone, Debug, Default)]
pub struct TurnView {
    pub status: TurnStatusView,
    pub waiting_for: Option<String>,
    pub usage: Option<ChatUsage>,
    pub stop_reason: Option<agent_client_protocol::v1::schema::StopReason>,
}

/// One item of `session/load`'s replay.
#[derive(Clone, Debug)]
pub enum ReplayItem {
    Message(Msg),
    TurnBoundary {
        turn_id: String,
        status: TurnStatusView,
    },
}

/// What `session/resume` did.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResumeOutcome {
    pub resumed_turns: usize,
    pub reconciled_tools: usize,
}

/// The physical properties a driver can promise.
#[derive(Clone, Copy, Debug, Default)]
pub struct Guarantees {
    /// Another process can drive the same session (leases and fences).
    pub cross_process: bool,
    /// A crash mid-write leaves a state a reader can replay, not a torn one.
    pub crash_atomic: bool,
    /// Two hosts may share one session's storage.
    pub multi_host: bool,
}

/// Why a driver could not do what it was asked.
#[derive(Clone, Debug)]
pub enum DriverError {
    /// The driver has no runtime for this session yet.
    Unavailable(String),
    /// The turn failed for a reason the client should see.
    Failed(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for DriverError {}

/// Everything the protocol layer needs from whatever runs turns.
///
/// `run` is the only execution entry: the protocol layer renders what `sink` publishes and never
/// learns whether the events came from a journal, an in-memory session or an agent stream.
#[async_trait]
pub trait TurnDriver: Send + Sync {
    /// Runs one turn to a terminal or yielded state.
    async fn run(&self, input: DriverInput, sink: EventSink) -> Result<TurnOutcome, DriverError>;

    /// Asks the turn to stop; returns once the request is recorded.
    async fn cancel(&self, turn_id: &str) -> Result<(), DriverError>;

    /// The current view of the session's newest turn.
    async fn view(&self) -> Result<TurnView, DriverError>;

    /// Replays the session's history for `session/load`.
    async fn replay(&self) -> Result<Vec<ReplayItem>, DriverError>;

    /// Marks a turn a dead process left unfinished as interrupted and settles its tool calls.
    async fn resume(&self) -> Result<ResumeOutcome, DriverError>;

    /// The physical properties this driver offers; never a feature gate.
    fn guarantees(&self) -> Guarantees;
}
