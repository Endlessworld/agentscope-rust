//! Run-level execution control, mirroring `io.agentscope.core.agent.AgentRun` and
//! `RunControl`.
//!
//! Java wraps every invocation in a single-use `AgentRun` handle that owns the run's
//! `runId`, its status machine and its own `InterruptControl`. Rust is not reactive,
//! so the handle is the same state machine without the publisher plumbing: the caller
//! creates a run, hands it to a call, and can cancel/interrupt or observe the terminal
//! status from another task.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::interruption::{InterruptControl, InterruptSource};
use crate::message::Msg;

/// Runtime-context key under which a caller's [`CancelSignal`] travels.
///
/// The signal is what lets a caller (a channel adapter, a session runner) stop an execution it
/// started, mirroring Java's `RunControl.CONTEXT_KEY`.
pub const CANCEL_SIGNAL_CONTEXT_KEY: &str = "cancel_signal";

/// Runtime-context key under which the current call's [`RunControl`] travels.
///
/// The run lives in the call's own context, so the interrupt check reads *this* call's signal
/// instead of the agent-level slot, which concurrent calls overwrite.
pub const RUN_CONTROL_CONTEXT_KEY: &str = "run_control";

/// A caller-held signal that requests the cancellation of one execution.
///
/// Mirrors Java's `Sinks.Empty` cancellation: the holder asks a running execution to stop, and
/// the execution observes the request at its next checkpoint. A model call already in flight
/// still finishes; everything scheduled after it is skipped and the run ends as cancelled.
///
/// The signal is also awaitable, so the holder can race it against the work it wants to abandon
/// and hard-stop that work when the graceful path takes too long.
#[derive(Debug, Clone)]
pub struct CancelSignal {
    inner: Arc<CancelInner>,
}

#[derive(Debug)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Default for CancelSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelSignal {
    /// Creates a signal that has not been triggered.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancelInner {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    /// Requests cancellation. Returns `true` for the first request only.
    ///
    /// The permit from [`Notify::notify_one`] covers a waiter that registers *after* this call;
    /// [`Notify::notify_waiters`] wakes the ones already waiting.
    pub fn cancel(&self) -> bool {
        if self.inner.cancelled.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.inner.notify.notify_one();
        self.inner.notify.notify_waiters();
        true
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves once cancellation is requested.
    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.inner.notify.notified().await;
        }
    }
}

/// Execution-handle state, mirroring `AgentRun.Status`.
///
/// `Completed` means the invocation returned normally, including a suspended or
/// unsuccessful semantic result; inspect the reply / durable turn status for the
/// logical outcome. `Queued` covers setup before admission to the core lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunStatus {
    #[default]
    Created,
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    /// Whether no further execution can start through this handle.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// The wire/name form used by events and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Queued => "QUEUED",
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }
}

/// Runtime-only control for one execution; never stored in conversation state.
#[derive(Debug)]
pub struct RunControl {
    agent_id: String,
    run_id: String,
    interruption: InterruptControl,
    status: AtomicU8,
    cancelled: AtomicBool,
}

impl RunControl {
    /// Creates a control for `agent_id`, adopting `run_id` when non-blank.
    pub fn new(agent_id: impl Into<String>, run_id: Option<String>) -> Self {
        let run_id = run_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(generate_run_id);
        Self {
            agent_id: agent_id.into(),
            run_id,
            interruption: InterruptControl::new(),
            status: AtomicU8::new(status_code(RunStatus::Created)),
            cancelled: AtomicBool::new(false),
        }
    }

    /// Same identifier used by [`AgentRun`] and the run's `RuntimeContext`.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The agent this run belongs to.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Whether `id` identifies this run's agent.
    pub fn belongs_to(&self, id: &str) -> bool {
        self.agent_id == id
    }

    /// The run's own interrupt signal (independent of any other run's).
    pub fn interruption(&self) -> &InterruptControl {
        &self.interruption
    }

    /// Current execution state.
    pub fn status(&self) -> RunStatus {
        status_from_code(self.status.load(Ordering::SeqCst))
    }

    /// Moves `Created` to `Queued`. Returns `false` when the run is past creation.
    pub fn queue(&self) -> bool {
        self.status
            .compare_exchange(
                status_code(RunStatus::Created),
                status_code(RunStatus::Queued),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Moves the run to `Running`. Returns `false` once it is terminal.
    pub fn start(&self) -> bool {
        loop {
            let current = self.status();
            if current.is_terminal() {
                return false;
            }
            if self
                .status
                .compare_exchange(
                    status_code(current),
                    status_code(RunStatus::Running),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Cancels this execution immediately, including before it starts.
    pub fn cancel(&self) -> bool {
        if !self.try_finish(RunStatus::Cancelled) {
            return false;
        }
        self.cancelled.store(true, Ordering::SeqCst);
        // Wake a cooperatively-waiting loop rather than only flipping the status.
        self.interruption.trigger(None, None);
        true
    }

    /// Running calls interrupt cooperatively; calls not yet admitted are cancelled.
    pub fn interrupt(&self, source: Option<InterruptSource>, message: Option<Msg>) -> bool {
        if self.status().is_terminal() {
            return false;
        }
        if self.status() == RunStatus::Running {
            self.interruption.trigger(source, message);
            return true;
        }
        self.cancel()
    }

    /// Records the terminal outcome. No-op when already terminal.
    pub fn finish(&self, outcome: RunStatus) {
        self.try_finish(outcome);
    }

    /// Whether this run was cancelled (as opposed to interrupted and finished normally).
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn try_finish(&self, outcome: RunStatus) -> bool {
        loop {
            let current = self.status();
            if current.is_terminal() {
                return false;
            }
            if self
                .status
                .compare_exchange(
                    status_code(current),
                    status_code(outcome),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return true;
            }
        }
    }
}

/// A single-use, lazy execution handle, mirroring `AgentRun`.
///
/// Register it before starting the work: the handle owns the run's cancellation and
/// status, and never registers anything on the agent. Cancelling an in-flight run also
/// cancels this handle. Create another handle to retry.
#[derive(Debug, Clone)]
pub struct AgentRun {
    control: Arc<RunControl>,
}

impl AgentRun {
    /// Creates a handle for `agent_id`, generating a fresh `run_id`.
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self::with_run_id(agent_id, None)
    }

    /// Creates a handle for `agent_id` that adopts `run_id` (typically
    /// `RuntimeContext::run_id`) so both identify the same execution. Blank generates one.
    pub fn with_run_id(agent_id: impl Into<String>, run_id: Option<String>) -> Self {
        Self {
            control: Arc::new(RunControl::new(agent_id, run_id)),
        }
    }

    /// Wraps an existing control (used by the agent when it creates the run itself).
    pub fn from_control(control: Arc<RunControl>) -> Self {
        Self { control }
    }

    /// Stable opaque identifier for this single execution.
    pub fn run_id(&self) -> &str {
        self.control.run_id()
    }

    /// Current execution state.
    pub fn status(&self) -> RunStatus {
        self.control.status()
    }

    /// The underlying control, for code that needs to start/finish the run.
    pub fn control(&self) -> &Arc<RunControl> {
        &self.control
    }

    /// Immediately cancel this run, including before it starts.
    pub fn cancel(&self) -> bool {
        self.control.cancel()
    }

    /// Cooperatively interrupt a running run, or cancel it if it has not started.
    pub fn interrupt_with(&self, message: Option<Msg>) -> bool {
        self.control.interrupt(Some(InterruptSource::User), message)
    }

    /// Cooperatively interrupt without an associated user message.
    pub fn interrupt(&self) -> bool {
        self.interrupt_with(None)
    }
}

/// Generates a fresh opaque run id (32-char hex). The single source for default ids.
pub fn generate_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn status_code(status: RunStatus) -> u8 {
    match status {
        RunStatus::Created => 0,
        RunStatus::Queued => 1,
        RunStatus::Running => 2,
        RunStatus::Completed => 3,
        RunStatus::Failed => 4,
        RunStatus::Cancelled => 5,
    }
}

fn status_from_code(code: u8) -> RunStatus {
    match code {
        1 => RunStatus::Queued,
        2 => RunStatus::Running,
        3 => RunStatus::Completed,
        4 => RunStatus::Failed,
        5 => RunStatus::Cancelled,
        _ => RunStatus::Created,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_transitions_follow_the_java_state_machine() {
        let control = RunControl::new("agent-1", None);
        assert_eq!(control.status(), RunStatus::Created);
        assert!(control.queue());
        assert!(!control.queue(), "queueing twice must fail");
        assert!(control.start());
        assert_eq!(control.status(), RunStatus::Running);
        assert!(
            control.start(),
            "restarting a running run reports the move as a no-op success"
        );
        control.finish(RunStatus::Completed);
        assert_eq!(control.status(), RunStatus::Completed);
        assert!(!control.start(), "a terminal run cannot restart");
        control.finish(RunStatus::Failed);
        assert_eq!(
            control.status(),
            RunStatus::Completed,
            "terminal status is sticky"
        );
    }

    #[test]
    fn running_runs_interrupt_but_queued_runs_cancel() {
        let running = RunControl::new("a", None);
        assert!(running.queue());
        assert!(running.start());
        assert!(running.interrupt(Some(InterruptSource::User), Some(Msg::user("stop"))));
        assert!(running.interruption().is_interrupted());
        assert_eq!(
            running.status(),
            RunStatus::Running,
            "an interrupt is cooperative"
        );

        let queued = RunControl::new("a", None);
        assert!(queued.queue());
        assert!(queued.interrupt(None, None));
        assert_eq!(queued.status(), RunStatus::Cancelled);
        assert!(queued.is_cancelled());
        assert!(
            !queued.interrupt(None, None),
            "a cancelled run ignores further interrupts"
        );
    }

    #[test]
    fn cancel_wakes_a_cooperatively_waiting_loop() {
        let control = Arc::new(RunControl::new("a", None));
        assert!(control.queue());
        assert!(control.start());
        let waiter = Arc::clone(&control);
        let handle = std::thread::spawn(move || {
            while !waiter.interruption().is_interrupted() {
                std::thread::yield_now();
            }
        });
        assert!(control.cancel());
        handle.join().unwrap();
        assert_eq!(control.status(), RunStatus::Cancelled);
    }

    #[test]
    fn run_id_is_adopted_or_generated() {
        let adopted = AgentRun::with_run_id("a", Some("run-42".to_string()));
        assert_eq!(adopted.run_id(), "run-42");
        let blank = AgentRun::with_run_id("a", Some("   ".to_string()));
        assert_eq!(
            blank.run_id().len(),
            32,
            "blank ids fall back to a generated 32-hex id"
        );
        assert!(blank.run_id().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn a_cancel_signal_is_observed_by_its_waiter() {
        let signal = CancelSignal::new();
        assert!(!signal.is_cancelled());
        let waiter = signal.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        // The waiter is registered before the request, so it must observe it.
        tokio::task::yield_now().await;
        assert!(signal.cancel());
        assert!(
            !signal.cancel(),
            "a second request must not report a new cancellation"
        );
        assert!(signal.is_cancelled());
        handle.await.expect("the waiter must be released");
    }

    #[tokio::test]
    async fn a_cancelled_signal_releases_a_late_waiter() {
        let signal = CancelSignal::new();
        signal.cancel();
        // A waiter that arrives after the request must not block forever.
        signal.cancelled().await;
    }
}
