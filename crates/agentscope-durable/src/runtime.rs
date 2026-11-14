//! The orchestration layer: one tick, one unit of progress, no background loops.
//!
//! A caller (a scheduler, an HTTP handler, a test) drives the runtime by calling
//! [`DurableRuntime::tick`]. A tick claims the thread's single-writer lease, advances the turn
//! as far as one adapter call allows, and always releases the lease with the delay the backend
//! asked for. Nothing here waits on work that is not already due: the tick model is what lets a
//! long-running local host and an edge host that may sleep share one implementation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, warn};

use crate::error::{DurableError, ErrorCode, Result};
use crate::executor::Executor;
use crate::harness_adapter::{HarnessAdapter, HarnessContext};
use crate::now_millis;
use crate::session_backend::SessionBackend;
use crate::types::*;

/// Shortest lease the runtime will ask for; a lease below this cannot survive one round trip.
const MIN_LEASE_TTL: Duration = Duration::from_millis(100);

/// Default lease duration when the host does not configure one.
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);

/// What one [`DurableRuntime::tick`] achieved. One tick never advances more than one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// Nothing was claimable: the thread is owned by another writer or is not ready yet.
    Idle,
    /// The tick did its unit of work and the turn needs no host input. This covers a terminal
    /// turn as well as one that can simply run again on the next tick.
    Settled,
    /// The turn is blocked on a signal, interaction or deadline; only new input can move it.
    Waiting,
    /// The turn is paused (or pausing) and needs an explicit [`DurableRuntime::resume`].
    Paused,
    /// The tick recorded a failure. `retryable` is the backend's verdict on retrying now, so a
    /// caller can stop rescanning a thread whose retry budget is spent.
    Failed { retryable: bool },
    /// The turn settled as cancelled.
    Cancelled,
}

impl TickOutcome {
    fn for_status(status: TurnStatus) -> Self {
        match status {
            TurnStatus::Succeeded | TurnStatus::Failed => Self::Settled,
            TurnStatus::Cancelled => Self::Cancelled,
            TurnStatus::Waiting => Self::Waiting,
            TurnStatus::Paused | TurnStatus::Pausing => Self::Paused,
            // Still runnable (or a cancellation is in flight); the next tick picks it up.
            TurnStatus::Queued | TurnStatus::Running | TurnStatus::Cancelling => Self::Settled,
        }
    }
}

/// The result of one adapter call inside a tick, plus the release delay it implies.
struct Step {
    outcome: TickOutcome,
    delay: Duration,
}

impl Step {
    fn settled() -> Self {
        Self {
            outcome: TickOutcome::Settled,
            delay: Duration::ZERO,
        }
    }
}

/// How a failed advance was handled; `Fenced` is never recorded as a turn failure.
enum Recovery {
    Recorded {
        outcome: TickOutcome,
        delay: Duration,
    },
    Fenced(DurableError),
}

/// Drives durable turns one step at a time against a [`SessionBackend`].
pub struct DurableRuntime {
    backend: Arc<dyn SessionBackend>,
    owner: String,
    lease_ttl: Duration,
    executor: Arc<dyn Executor>,
    adapters: HashMap<String, Arc<dyn HarnessAdapter>>,
}

impl DurableRuntime {
    pub fn new(backend: Arc<dyn SessionBackend>, owner: impl Into<String>) -> Self {
        Self {
            backend,
            owner: owner.into(),
            lease_ttl: DEFAULT_LEASE_TTL,
            executor: Arc::new(UnconfiguredExecutor),
            adapters: HashMap::new(),
        }
    }

    /// The executor every [`HarnessContext`] carries. Without one, tool dispatch fails as
    /// `UNAVAILABLE` instead of silently producing an effect nobody can recover.
    pub fn with_executor(mut self, executor: Arc<dyn Executor>) -> Self {
        self.executor = executor;
        self
    }

    /// Registers the adapter for one harness name; a later registration under the same name wins.
    pub fn with_adapter(
        mut self,
        harness: impl Into<String>,
        adapter: Arc<dyn HarnessAdapter>,
    ) -> Self {
        self.adapters.insert(harness.into(), adapter);
        self
    }

    pub fn with_lease_ttl(mut self, ttl: Duration) -> Self {
        self.lease_ttl = ttl.max(MIN_LEASE_TTL);
        self
    }

    fn adapter(&self, harness: &str) -> Option<Arc<dyn HarnessAdapter>> {
        self.adapters.get(harness).cloned()
    }

    // ---- identity ----

    /// Idempotent session creation: an existing session is read back instead of failing.
    pub async fn ensure_session(&self, spec: SessionSpec) -> Result<SessionView> {
        let key = spec.key.clone();
        match self.backend.create(spec).await {
            Ok(view) => Ok(view),
            Err(error) if error.code() == ErrorCode::Conflict => {
                debug!(session = %key.session_id, "session exists; reading it back");
                self.backend.session(&key).await
            }
            Err(error) => Err(error),
        }
    }

    pub async fn create_thread(&self, spec: ThreadSpec) -> Result<ThreadView> {
        self.backend.create_thread(spec).await
    }

    pub async fn close_session(&self, key: &SessionKey) -> Result<SessionView> {
        self.backend.close_session(key).await
    }

    pub async fn close_thread(&self, key: &ThreadKey) -> Result<ThreadView> {
        self.backend.close_thread(key).await
    }

    pub async fn submit(
        &self,
        thread: &ThreadKey,
        request_id: &str,
        input: Value,
    ) -> Result<TurnReceipt> {
        self.backend.submit(thread, request_id, input).await
    }

    // ---- the tick ----

    /// Advances one thread by at most one step and never waits for work that is not due.
    ///
    /// The lease is released on every path, with the delay the backend asked for; a lost lease
    /// (`FENCED`) is propagated without writing any turn state, because another writer owns the
    /// turn now and recording a failure would corrupt its recovery budget.
    pub async fn tick(&self, thread: &ThreadKey) -> Result<TickOutcome> {
        let lease = match self
            .backend
            .claim(thread, &self.owner, self.lease_ttl)
            .await?
        {
            Some(lease) => lease,
            None => return Ok(TickOutcome::Idle),
        };
        let (result, delay) = match self.advance(&lease).await {
            Ok(step) => (Ok(step.outcome), step.delay),
            Err(error) => match self.recover(&lease, &error).await {
                Recovery::Recorded { outcome, delay } => (Ok(outcome), delay),
                Recovery::Fenced(error) => (Err(error), Duration::ZERO),
            },
        };
        self.release(&lease, delay).await;
        result
    }

    /// Runs one tick per ready thread, serially. A thread that cannot even be claimed or framed
    /// is reported as a failure and does not abort the rest of the scan.
    pub async fn scan(
        &self,
        selectors: &[HarnessSelector],
        limit: usize,
    ) -> Result<Vec<TickOutcome>> {
        let ready = self.backend.ready(selectors, limit).await?;
        let mut outcomes = Vec::with_capacity(ready.len());
        for spec in ready {
            match self.tick(&spec.key).await {
                Ok(outcome) => outcomes.push(outcome),
                Err(error) => {
                    if error.code() == ErrorCode::Fenced {
                        debug!(error = %error, "durable scan lost a lease");
                    } else {
                        warn!(error = %error, thread = %spec.key.thread_id, "durable scan step failed");
                    }
                    outcomes.push(TickOutcome::Failed {
                        retryable: retryable(error.code()),
                    });
                }
            }
        }
        Ok(outcomes)
    }

    /// One adapter call, fenced by `lease`. No status is ever forced here: the adapter commits
    /// the transitions it observes and the backend decides whether the turn is settled.
    async fn advance(&self, lease: &Lease) -> Result<Step> {
        let frame = self.backend.frame(lease).await?;
        if frame.turn.status.terminal() {
            return Ok(Step::settled());
        }
        let run = self
            .backend
            .prepare_run(lease, &run_request_id(lease))
            .await?;
        debug!(run = %run.run_id, turn = %lease.turn_id, "durable invocation prepared");
        let Some(adapter) = self.adapter(frame.definition.harness()) else {
            let message = format!(
                "no adapter is registered for harness `{}`",
                frame.definition.harness()
            );
            warn!(%message, thread = %lease.thread.thread_id);
            let failure = Failure::new(false, message, RecoveryPolicy::defaults());
            if let Err(error) = self.backend.record_failure(lease, failure).await {
                warn!(error = %error, "cannot record the missing-adapter failure");
            }
            return Ok(Step {
                outcome: TickOutcome::Failed { retryable: false },
                delay: Duration::ZERO,
            });
        };
        let context = HarnessContext {
            definition: frame.definition.clone(),
            frame,
            backend: Arc::clone(&self.backend),
            executor: Arc::clone(&self.executor),
            lease: lease.clone(),
        };
        adapter.execute(&context).await?;
        let turn = self.backend.turn(&lease.thread, &lease.turn_id).await?;
        Ok(Step {
            outcome: TickOutcome::for_status(turn.status),
            delay: Duration::ZERO,
        })
    }

    /// Turns an advance failure into a recorded failure plus the release delay it implies.
    async fn recover(&self, lease: &Lease, error: &DurableError) -> Recovery {
        if error.code() == ErrorCode::Fenced {
            debug!(error = %error, "durable tick lost the lease; not recording a failure");
            return Recovery::Fenced(error.clone());
        }
        let retryable = retryable(error.code());
        let failure = Failure::new(retryable, error.message(), RecoveryPolicy::defaults());
        match self.backend.record_failure(lease, failure).await {
            Ok(view) => Recovery::Recorded {
                outcome: TickOutcome::Failed {
                    retryable: view.retry_allowed(),
                },
                delay: backoff(&view),
            },
            Err(record_error) if record_error.code() == ErrorCode::Fenced => {
                Recovery::Fenced(record_error)
            }
            Err(record_error) => {
                warn!(error = %record_error, cause = %error, "cannot record the failure");
                Recovery::Recorded {
                    outcome: TickOutcome::Failed { retryable },
                    delay: Duration::ZERO,
                }
            }
        }
    }

    /// Every tick path releases through here, so no branch can leak the writer slot.
    async fn release(&self, lease: &Lease, delay: Duration) {
        if let Err(error) = self.backend.release(lease, delay).await {
            if error.code() == ErrorCode::Fenced {
                debug!(error = %error, turn = %lease.turn_id, "stale lease released");
            } else {
                warn!(error = %error, turn = %lease.turn_id, "lease release failed");
            }
        }
    }

    /// Convenience for a host that only wants to nudge one thread.
    pub async fn resume_turn(&self, thread: &ThreadKey) -> Result<TickOutcome> {
        self.tick(thread).await
    }

    // ---- control ----

    /// Requests cancellation durably, then asks the executor to stop outstanding effects.
    pub async fn cancel(&self, thread: &ThreadKey, turn_id: &str, request_id: &str) -> Result<()> {
        let turn = self.backend.turn(thread, turn_id).await?;
        self.backend
            .control(
                thread,
                ControlCommand {
                    turn_id: turn_id.to_string(),
                    request_id: request_id.to_string(),
                    kind: ControlKind::Cancel,
                    target: None,
                    value: Value::Null,
                    expected_revision: turn.revision,
                },
            )
            .await?;
        self.cancel_effects(thread, turn_id).await;
        Ok(())
    }

    /// Best-effort external cancellation: the durable intent is already recorded, so nothing
    /// here may fail the command. An executor that cannot cancel answers `UNKNOWN`.
    async fn cancel_effects(&self, thread: &ThreadKey, turn_id: &str) {
        let lease = match self
            .backend
            .claim(thread, &self.owner, self.lease_ttl)
            .await
        {
            Ok(Some(lease)) => lease,
            Ok(None) => return,
            Err(error) => {
                debug!(error = %error, "cannot observe outstanding effects for cancellation");
                return;
            }
        };
        if lease.turn_id == turn_id {
            match self.backend.frame(&lease).await {
                Ok(frame) => {
                    for tool in frame.tools.iter().filter(|tool| !settled_effect(tool)) {
                        if let Err(error) = self.executor.cancel(&tool.request).await {
                            debug!(error = %error, "executor refused the cancellation");
                        }
                    }
                }
                Err(error) => debug!(error = %error, "cannot read effects to cancel"),
            }
        }
        self.release(&lease, Duration::ZERO).await;
    }

    /// Requests a durable pause; observe `Paused` before assuming the invocation has stopped.
    pub async fn pause(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        request_id: &str,
    ) -> Result<TurnView> {
        let turn = self.backend.turn(thread, turn_id).await?;
        self.backend
            .control(
                thread,
                ControlCommand {
                    turn_id: turn_id.to_string(),
                    request_id: request_id.to_string(),
                    kind: ControlKind::Pause,
                    target: None,
                    value: Value::Null,
                    expected_revision: turn.revision,
                },
            )
            .await?;
        self.backend.turn(thread, turn_id).await
    }

    /// Continues an observed `Paused` turn; a changed pause revision is a `CONFLICT`.
    pub async fn resume(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        request_id: &str,
    ) -> Result<TurnView> {
        let turn = self.backend.turn(thread, turn_id).await?;
        self.backend
            .control(
                thread,
                ControlCommand {
                    turn_id: turn_id.to_string(),
                    request_id: request_id.to_string(),
                    kind: ControlKind::Resume,
                    target: None,
                    value: Value::Null,
                    expected_revision: turn.revision,
                },
            )
            .await?;
        self.backend.turn(thread, turn_id).await
    }

    pub async fn signal(&self, thread: &ThreadKey, turn_id: &str, signal: Signal) -> Result<()> {
        self.backend.signal(thread, turn_id, signal).await
    }

    pub async fn withdraw(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        signal_id: &str,
        request_id: &str,
    ) -> Result<()> {
        self.backend
            .control(
                thread,
                ControlCommand {
                    turn_id: turn_id.to_string(),
                    request_id: request_id.to_string(),
                    kind: ControlKind::Withdraw,
                    target: Some(signal_id.to_string()),
                    value: Value::Null,
                    expected_revision: 0,
                },
            )
            .await?;
        Ok(())
    }

    // ---- observation ----

    pub async fn turn(&self, thread: &ThreadKey, turn_id: &str) -> Result<TurnView> {
        self.backend.turn(thread, turn_id).await
    }

    pub async fn turns(
        &self,
        thread: &ThreadKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<TurnPage> {
        self.backend.turns(thread, cursor, limit).await
    }

    pub async fn history(
        &self,
        thread: &ThreadKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage> {
        self.backend.history(thread, cursor, limit).await
    }

    pub async fn events(
        &self,
        session: &SessionKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<EventPage> {
        self.backend.events(session, cursor, limit).await
    }

    pub async fn snapshot(&self, session: &SessionKey) -> Result<Snapshot> {
        self.backend.snapshot(session).await
    }
}

/// Invocation identity of one lease: stable across a re-entered tick, distinct per epoch.
fn run_request_id(lease: &Lease) -> String {
    format!("invoke:{}:{}", lease.turn_id, lease.epoch)
}

/// Transient codes may be retried; everything else is a decision the caller must see.
fn retryable(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::Unavailable | ErrorCode::Conflict)
}

/// Release delay the backend asked for, measured from now so the lease is not held meanwhile.
fn backoff(view: &RecoveryView) -> Duration {
    match view.retry_at {
        Some(retry_at) if view.retry_allowed() => {
            let wait = retry_at.saturating_sub(now_millis()).max(0);
            Duration::from_millis(wait as u64)
        }
        _ => Duration::ZERO,
    }
}

fn settled_effect(tool: &ToolExecution) -> bool {
    tool.result
        .as_ref()
        .is_some_and(|result| result.state.terminal())
}

/// The executor a runtime carries when the host configured none.
struct UnconfiguredExecutor;

#[async_trait]
impl Executor for UnconfiguredExecutor {
    async fn start(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        Err(DurableError::unavailable("no executor is configured"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::harness_adapter::AdapterCapabilities;

    #[test]
    fn transient_codes_are_the_only_retryable_ones() {
        assert!(retryable(ErrorCode::Unavailable));
        assert!(retryable(ErrorCode::Conflict));
        for code in [
            ErrorCode::Invalid,
            ErrorCode::Incompatible,
            ErrorCode::Fenced,
            ErrorCode::NotFound,
            ErrorCode::CursorExpired,
        ] {
            assert!(!retryable(code), "{code:?} must not be retried");
        }
    }

    #[test]
    fn invocation_identity_is_stable_per_lease() -> Result<()> {
        let thread = thread_key()?;
        let mut lease = lease(&thread);
        let first = run_request_id(&lease);
        assert_eq!(first, run_request_id(&lease));
        assert!(first.contains(&lease.turn_id));
        lease.epoch += 1;
        assert_ne!(first, run_request_id(&lease));
        Ok(())
    }

    #[test]
    fn status_maps_to_the_caller_facing_outcome() {
        assert_eq!(
            TickOutcome::for_status(TurnStatus::Succeeded),
            TickOutcome::Settled
        );
        assert_eq!(
            TickOutcome::for_status(TurnStatus::Failed),
            TickOutcome::Settled
        );
        assert_eq!(
            TickOutcome::for_status(TurnStatus::Cancelled),
            TickOutcome::Cancelled
        );
        assert_eq!(
            TickOutcome::for_status(TurnStatus::Waiting),
            TickOutcome::Waiting
        );
        assert_eq!(
            TickOutcome::for_status(TurnStatus::Paused),
            TickOutcome::Paused
        );
        assert_eq!(
            TickOutcome::for_status(TurnStatus::Pausing),
            TickOutcome::Paused
        );
        assert_eq!(
            TickOutcome::for_status(TurnStatus::Running),
            TickOutcome::Settled
        );
    }

    #[tokio::test]
    async fn an_unowned_thread_is_idle() -> Result<()> {
        let thread = thread_key()?;
        let backend = Arc::new(RecordingBackend::new(
            None,
            frame(&thread, "stub", TurnStatus::Running)?,
        ));
        let runtime = runtime(&backend).with_adapter("stub", Arc::new(StubAdapter::ok()));
        assert_eq!(runtime.tick(&thread).await?, TickOutcome::Idle);
        assert!(backend.snapshot().releases.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_adapter_fails_without_retrying() -> Result<()> {
        let thread = thread_key()?;
        let backend = Arc::new(RecordingBackend::new(
            Some(lease(&thread)),
            frame(&thread, "ghost", TurnStatus::Running)?,
        ));
        let runtime = runtime(&backend).with_adapter("stub", Arc::new(StubAdapter::ok()));
        assert_eq!(
            runtime.tick(&thread).await?,
            TickOutcome::Failed { retryable: false }
        );
        let recorded = backend.snapshot();
        assert_eq!(recorded.failures.len(), 1);
        assert!(!recorded.failures[0].retryable);
        assert_eq!(recorded.releases.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn losing_the_lease_is_not_a_turn_failure() -> Result<()> {
        let thread = thread_key()?;
        let backend = Arc::new(RecordingBackend::new(
            Some(lease(&thread)),
            frame(&thread, "stub", TurnStatus::Running)?,
        ));
        let runtime = runtime(&backend)
            .with_adapter("stub", Arc::new(StubAdapter::failing(ErrorCode::Fenced)));
        let error = match runtime.tick(&thread).await {
            Err(error) => error,
            Ok(outcome) => return Err(DurableError::invalid(format!("unexpected {outcome:?}"))),
        };
        assert_eq!(error.code(), ErrorCode::Fenced);
        let recorded = backend.snapshot();
        assert!(recorded.failures.is_empty());
        assert_eq!(recorded.releases.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn a_normal_return_settles_and_prepares_one_invocation() -> Result<()> {
        let thread = thread_key()?;
        let lease = lease(&thread);
        let backend = Arc::new(RecordingBackend::new(
            Some(lease.clone()),
            frame(&thread, "stub", TurnStatus::Running)?,
        ));
        let runtime = runtime(&backend).with_adapter("stub", Arc::new(StubAdapter::ok()));
        assert_eq!(runtime.tick(&thread).await?, TickOutcome::Settled);
        let recorded = backend.snapshot();
        assert_eq!(recorded.prepared, vec![run_request_id(&lease)]);
        assert_eq!(recorded.releases, vec![(lease.epoch, 0)]);
        assert!(recorded.failures.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn the_last_registration_of_a_harness_wins() -> Result<()> {
        let thread = thread_key()?;
        let backend = Arc::new(RecordingBackend::new(
            Some(lease(&thread)),
            frame(&thread, "stub", TurnStatus::Running)?,
        ));
        let first = Arc::new(StubAdapter::ok());
        let second = Arc::new(StubAdapter::ok());
        let first_adapter = as_adapter(Arc::clone(&first));
        let second_adapter = as_adapter(Arc::clone(&second));
        let runtime = runtime(&backend)
            .with_adapter("stub", first_adapter)
            .with_adapter("stub", second_adapter);
        runtime.tick(&thread).await?;
        assert_eq!(first.calls.load(Ordering::SeqCst), 0);
        assert_eq!(second.calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    fn runtime(backend: &Arc<RecordingBackend>) -> DurableRuntime {
        DurableRuntime::new(as_backend(Arc::clone(backend)), "test-owner")
    }

    fn as_backend(backend: Arc<RecordingBackend>) -> Arc<dyn SessionBackend> {
        backend
    }

    fn as_adapter(adapter: Arc<StubAdapter>) -> Arc<dyn HarnessAdapter> {
        adapter
    }

    fn thread_key() -> Result<ThreadKey> {
        ThreadKey::new(SessionKey::new("ns", "session")?, "thread")
    }

    fn lease(thread: &ThreadKey) -> Lease {
        Lease {
            thread: thread.clone(),
            turn_id: "turn-1".to_string(),
            owner: "test-owner".to_string(),
            epoch: 7,
            expires_at: 0,
        }
    }

    fn frame(thread: &ThreadKey, harness: &str, status: TurnStatus) -> Result<Frame> {
        Ok(Frame {
            definition: ThreadSpec {
                key: thread.clone(),
                binding: ThreadBinding::new(harness, "definition-1", "state-1")?,
            },
            turn: TurnView {
                thread: thread.clone(),
                turn_id: "turn-1".to_string(),
                status,
                revision: 3,
                cancellation_requested: false,
                output: Value::Null,
                error: None,
                waiting: None,
                recovery: None,
                parent: None,
            },
            input: Value::Null,
            checkpoint: Value::Null,
            checkpoint_metadata: None,
            tools: Vec::new(),
            signals: Vec::new(),
            now: 0,
        })
    }

    struct StubAdapter {
        fail: Option<ErrorCode>,
        calls: AtomicUsize,
    }

    impl StubAdapter {
        fn ok() -> Self {
            Self {
                fail: None,
                calls: AtomicUsize::new(0),
            }
        }

        fn failing(code: ErrorCode) -> Self {
            Self {
                fail: Some(code),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl HarnessAdapter for StubAdapter {
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities {
                state_version: "state-1".to_string(),
                recovery_boundary: true,
                event_delivery: false,
            }
        }

        async fn execute(&self, _ctx: &HarnessContext) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.fail {
                Some(code) => Err(DurableError::new(code, "adapter failure")),
                None => Ok(()),
            }
        }
    }

    /// Only the tick path is real; every other call asserts it was not used.
    struct RecordingBackend {
        lease: Option<Lease>,
        frame: Frame,
        recorded: Mutex<Recorded>,
    }

    #[derive(Default)]
    struct Recorded {
        failures: Vec<Failure>,
        prepared: Vec<String>,
        releases: Vec<(u64, u64)>,
    }

    impl RecordingBackend {
        fn new(lease: Option<Lease>, frame: Frame) -> Self {
            Self {
                lease,
                frame,
                recorded: Mutex::new(Recorded::default()),
            }
        }

        fn snapshot(&self) -> Recorded {
            match self.recorded.lock() {
                Ok(recorded) => Recorded {
                    failures: recorded.failures.clone(),
                    prepared: recorded.prepared.clone(),
                    releases: recorded.releases.clone(),
                },
                Err(_) => Recorded::default(),
            }
        }
    }

    #[async_trait]
    impl SessionBackend for RecordingBackend {
        fn capabilities(&self) -> crate::session_backend::BackendCapabilities {
            crate::session_backend::BackendCapabilities::default()
        }

        async fn create(&self, _spec: SessionSpec) -> Result<SessionView> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn session(&self, _key: &SessionKey) -> Result<SessionView> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn close_session(&self, _key: &SessionKey) -> Result<SessionView> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn create_thread(&self, _spec: ThreadSpec) -> Result<ThreadView> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn thread(&self, _key: &ThreadKey) -> Result<ThreadView> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn close_thread(&self, _key: &ThreadKey) -> Result<ThreadView> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn threads(
            &self,
            _session: &SessionKey,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<ThreadPage> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn submit(
            &self,
            _thread: &ThreadKey,
            _request_id: &str,
            _input: Value,
        ) -> Result<TurnReceipt> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn admission(
            &self,
            _session: &SessionKey,
            _request_id: &str,
        ) -> Result<Option<TurnReceipt>> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn turn(&self, _thread: &ThreadKey, _turn_id: &str) -> Result<TurnView> {
            Ok(self.frame.turn.clone())
        }

        async fn turns(
            &self,
            _thread: &ThreadKey,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<TurnPage> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn history(
            &self,
            _thread: &ThreadKey,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<HistoryPage> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn control(
            &self,
            _thread: &ThreadKey,
            _command: ControlCommand,
        ) -> Result<CommandReceipt> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn signal(&self, _thread: &ThreadKey, _turn_id: &str, _signal: Signal) -> Result<()> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn claim(
            &self,
            _thread: &ThreadKey,
            _owner: &str,
            _ttl: Duration,
        ) -> Result<Option<Lease>> {
            Ok(self.lease.clone())
        }

        async fn renew(&self, _lease: &Lease, _ttl: Duration) -> Result<()> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn release(&self, lease: &Lease, delay: Duration) -> Result<()> {
            if let Ok(mut recorded) = self.recorded.lock() {
                recorded
                    .releases
                    .push((lease.epoch, delay.as_millis() as u64));
            }
            Ok(())
        }

        async fn frame(&self, _lease: &Lease) -> Result<Frame> {
            Ok(self.frame.clone())
        }

        async fn record_failure(&self, _lease: &Lease, failure: Failure) -> Result<RecoveryView> {
            let retryable = failure.retryable;
            if let Ok(mut recorded) = self.recorded.lock() {
                recorded.failures.push(failure);
            }
            Ok(RecoveryView {
                failures: 1,
                policy: RecoveryPolicy::defaults(),
                retryable,
                retry_at: None,
                message: "recorded".to_string(),
            })
        }

        async fn commit(&self, _lease: &Lease, _commit: Commit) -> Result<CommitReceipt> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn receipt(
            &self,
            _thread: &ThreadKey,
            _turn_id: &str,
            _commit_id: &str,
        ) -> Result<Option<CommitReceipt>> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn prepare_run(&self, lease: &Lease, request_id: &str) -> Result<RunView> {
            if let Ok(mut recorded) = self.recorded.lock() {
                recorded.prepared.push(request_id.to_string());
            }
            Ok(RunView {
                thread: lease.thread.clone(),
                turn_id: lease.turn_id.clone(),
                run_id: "run-1".to_string(),
                request_id: request_id.to_string(),
                owner: lease.owner.clone(),
                epoch: lease.epoch,
                previous_run_id: None,
                status: RunStatus::Prepared,
                prepared_at: 0,
                started_at: None,
                ended_at: None,
                outcome: None,
                reason: None,
            })
        }

        async fn run(&self, _thread: &ThreadKey, _turn_id: &str, _run_id: &str) -> Result<RunView> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn runs(
            &self,
            _thread: &ThreadKey,
            _turn_id: &str,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<RunPage> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn steps(
            &self,
            _thread: &ThreadKey,
            _turn_id: &str,
            _run_id: &str,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<StepPage> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn journal(&self, _lease: &Lease, _after: u64, _limit: usize) -> Result<Vec<Value>> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn execution(
            &self,
            _thread: &ThreadKey,
            _execution_id: &str,
        ) -> Result<ToolExecution> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn begin_dispatch(
            &self,
            _lease: &Lease,
            _execution_id: &str,
        ) -> Result<ToolExecution> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn accept_result(
            &self,
            _thread: &ThreadKey,
            _execution_id: &str,
            _result: ExecutionResult,
        ) -> Result<ToolExecution> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn ready(
            &self,
            _selectors: &[HarnessSelector],
            _limit: usize,
        ) -> Result<Vec<ThreadSpec>> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn checkpoints(
            &self,
            _thread: &ThreadKey,
            _after: u64,
            _limit: usize,
        ) -> Result<Vec<u64>> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn checkpoint(&self, _thread: &ThreadKey, _position: u64) -> Result<CheckpointView> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn events(
            &self,
            _session: &SessionKey,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<EventPage> {
            Err(DurableError::unavailable("not used in this test"))
        }

        async fn snapshot(&self, _session: &SessionKey) -> Result<Snapshot> {
            Err(DurableError::unavailable("not used in this test"))
        }
    }
}
