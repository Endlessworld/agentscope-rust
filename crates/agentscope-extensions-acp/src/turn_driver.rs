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

use agentscope_core::message::{ChatUsage, Msg};
use async_trait::async_trait;

/// Where a driver publishes the events of a turn.
///
/// The payload is the ACP notification already rendered, not a core `AgentEvent`: what a turn
/// reports (a tool call's state, the text of a failure, the stop reason) is read off the frame
/// the driver produces, so rendering is the driver's job. Making the session render core events
/// instead would mean rewriting every `super::event::*` call in the prompt loop, which is a
/// rewrite rather than the move this seam is meant to be.
///
/// The driver calls it synchronously; the session's queue bounds the backlog, so a slow client
/// never grows it without limit.
pub type EventSink =
    Arc<dyn Fn(agent_client_protocol::v1::schema::SessionNotification) + Send + Sync>;

/// The client capabilities a turn needs to render itself.
///
/// They are read once per prompt (a client's advertised capabilities do not change within a
/// session) and carried as plain bits, so the driver never reaches back into the session.
#[derive(Clone)]
pub struct PromptCaps {
    pub supports_notices: bool,
    pub supports_plan_operations: bool,
    pub supports_compaction: bool,
    pub last_input_tokens: Arc<std::sync::atomic::AtomicU64>,
}

/// One prompt, as the protocol layer sees it.
///
/// The turn's own id is not here: the driver derives it when it admits the turn (a retry of the
/// same prompt must reuse the entry the first attempt wrote).
#[derive(Clone)]
pub struct DriverInput {
    pub message: Option<Msg>,
    pub wakeup: bool,
    /// The shared generation a cancel advances.
    pub cancel_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Its value when this turn was admitted; a cancel that advances the generation stops only
    /// this turn.
    pub admitted_epoch: u64,
    pub closed: Arc<std::sync::atomic::AtomicBool>,
    pub cancellation: Option<agent_client_protocol::RequestCancellation>,
    pub caps: PromptCaps,
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
    /// A stable name for a store that has to remember the status across restarts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

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
