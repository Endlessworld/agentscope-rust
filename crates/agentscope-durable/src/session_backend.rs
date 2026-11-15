//! The backend contract: one session-scoped transaction per commit, fenced by a lease.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::types::*;
use crate::{DurableError, Result};

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

    /// The control facts a running invocation polls: its status and whether a stop was requested.
    ///
    /// Split from [`frame`](SessionBackend::frame) because of *when* it is asked: while a model
    /// call is streaming, several times a second. A backend that can answer it without building
    /// the frame should (the transactional backend reads one record); the default answers from
    /// the frame, which is always correct and only expensive.
    async fn control_state(&self, lease: &Lease) -> Result<TurnControl> {
        let frame = self.frame(lease).await?;
        Ok(TurnControl {
            status: frame.turn.status,
            cancellation_requested: frame.turn.cancellation_requested,
        })
    }

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
    /// Model calls one turn observed, in the order they were dispatched.
    ///
    /// Facts only: a call that never reached its ending is reported as `RUNNING`, and a turn that
    /// never dispatched one answers an empty page.
    async fn model_calls(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ModelCallPage>;
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

    /// Drops every public event outside the newest `keep_newest` of one session.
    ///
    /// Public events are a client-facing log, not durable state: the turn, the tool records and
    /// the native checkpoint keep everything recovery needs. What a client must lose is bounded
    /// and reported — a cursor below the new watermark answers `cursor_expired`, and
    /// [`snapshot`](SessionBackend::snapshot) is how it catches up — instead of the log growing
    /// with the square of the conversation.
    ///
    /// Returns how many events were dropped. A backend that keeps no event log answers `0`.
    async fn prune_events(&self, _session: &SessionKey, _keep_newest: u64) -> Result<u64> {
        Ok(0)
    }

    // ---- resources, delegation, restoration ----

    /// Applies one private-resource request under the writer lease and answers what it touched.
    ///
    /// `GET` answers zero or one value, `SEARCH` a page of the session's values whose namespace
    /// starts with the request's, `PUT`/`CAS` the value written and `DELETE` nothing. A mutation
    /// advances the thread's `resource_position`, the generation a checkpoint records: a baseline
    /// whose resource generation moved is a different baseline, which is why the counter exists.
    /// `request_id` makes a mutation idempotent — replaying it answers the first application
    /// instead of applying it twice — and reads never write.
    ///
    /// The default answers `UNAVAILABLE`: a backend that keeps no resources must not pretend a
    /// `GET` found nothing, because that is indistinguishable from a deleted value.
    async fn resources(
        &self,
        _lease: &Lease,
        _request: ResourceRequest,
    ) -> Result<Vec<ResourceValue>> {
        Err(DurableError::unavailable(
            "backend keeps no session resources",
        ))
    }

    /// Registers a delegation from the leased turn to a child thread and answers its lineage.
    ///
    /// The child thread, its first turn and the link back to the parent tool call are recorded
    /// once per `request_id`: replaying the registration answers the same child instead of
    /// creating a second one. `execution_id` names the parent tool call that delegates, so a
    /// recovered parent finds the child it already started.
    ///
    /// The default answers `UNAVAILABLE`: delegating without recording the lineage would leave a
    /// child nothing can attribute to its parent.
    async fn delegate(
        &self,
        _lease: &Lease,
        _execution_id: &str,
        _request_id: &str,
        _target: DelegationTarget,
    ) -> Result<DelegationView> {
        Err(DurableError::unavailable(
            "backend cannot record delegations",
        ))
    }

    /// Installs a settled baseline prepared by a native adapter into an existing idle thread.
    ///
    /// `expected_position` is the compare-and-set: installation only applies to a thread whose
    /// journal is still at the position the baseline was taken at. `request_id` makes the
    /// installation idempotent. A backend that cannot install a baseline answers `UNAVAILABLE`
    /// rather than silently starting a thread from nothing.
    async fn restore(&self, _restoration: Restoration) -> Result<ThreadView> {
        Err(DurableError::unavailable(
            "backend cannot install restorations",
        ))
    }
}

#[cfg(test)]
mod default_contract_tests {
    //! The three branch-level methods must be addable without touching other backends.

    use async_trait::async_trait;

    use super::*;
    use crate::ErrorCode;

    struct BareBackend;

    fn deny<T>() -> Result<T> {
        Err(DurableError::unavailable("bare backend stores nothing"))
    }

    // Written out one method per line on purpose: `#[async_trait]` rewrites `async fn` items, and
    // a macro that produced them would expand after the attribute already ran. The point of the
    // stub is exactly that a backend written before `resources`/`delegate`/`restore` existed
    // still implements the trait.
    #[async_trait]
    impl SessionBackend for BareBackend {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::default()
        }

        async fn create(&self, _spec: SessionSpec) -> Result<SessionView> {
            deny()
        }
        async fn session(&self, _key: &SessionKey) -> Result<SessionView> {
            deny()
        }
        async fn close_session(&self, _key: &SessionKey) -> Result<SessionView> {
            deny()
        }
        async fn create_thread(&self, _spec: ThreadSpec) -> Result<ThreadView> {
            deny()
        }
        async fn thread(&self, _key: &ThreadKey) -> Result<ThreadView> {
            deny()
        }
        async fn close_thread(&self, _key: &ThreadKey) -> Result<ThreadView> {
            deny()
        }
        async fn thread_state(&self, _key: &ThreadKey) -> Result<ThreadStateView> {
            deny()
        }
        async fn threads(
            &self,
            _session: &SessionKey,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<ThreadPage> {
            deny()
        }
        async fn submit(
            &self,
            _thread: &ThreadKey,
            _request_id: &str,
            _input: Value,
        ) -> Result<TurnReceipt> {
            deny()
        }
        async fn admission(
            &self,
            _session: &SessionKey,
            _request_id: &str,
        ) -> Result<Option<TurnReceipt>> {
            deny()
        }
        async fn turn(&self, _thread: &ThreadKey, _turn_id: &str) -> Result<TurnView> {
            deny()
        }
        async fn turns(
            &self,
            _thread: &ThreadKey,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<TurnPage> {
            deny()
        }
        async fn history(
            &self,
            _thread: &ThreadKey,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<HistoryPage> {
            deny()
        }
        async fn control(
            &self,
            _thread: &ThreadKey,
            _command: ControlCommand,
        ) -> Result<CommandReceipt> {
            deny()
        }
        async fn signal(&self, _thread: &ThreadKey, _turn_id: &str, _signal: Signal) -> Result<()> {
            deny()
        }
        async fn claim(
            &self,
            _thread: &ThreadKey,
            _owner: &str,
            _ttl: Duration,
        ) -> Result<Option<Lease>> {
            deny()
        }
        async fn renew(&self, _lease: &Lease, _ttl: Duration) -> Result<()> {
            deny()
        }
        async fn release(&self, _lease: &Lease, _delay: Duration) -> Result<()> {
            deny()
        }
        async fn frame(&self, _lease: &Lease) -> Result<Frame> {
            deny()
        }
        async fn record_failure(&self, _lease: &Lease, _failure: Failure) -> Result<RecoveryView> {
            deny()
        }
        async fn commit(&self, _lease: &Lease, _commit: Commit) -> Result<CommitReceipt> {
            deny()
        }
        async fn receipt(
            &self,
            _thread: &ThreadKey,
            _turn_id: &str,
            _commit_id: &str,
        ) -> Result<Option<CommitReceipt>> {
            deny()
        }
        async fn prepare_run(&self, _lease: &Lease, _request_id: &str) -> Result<RunView> {
            deny()
        }
        async fn run(&self, _thread: &ThreadKey, _turn_id: &str, _run_id: &str) -> Result<RunView> {
            deny()
        }
        async fn runs(
            &self,
            _thread: &ThreadKey,
            _turn_id: &str,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<RunPage> {
            deny()
        }
        async fn steps(
            &self,
            _thread: &ThreadKey,
            _turn_id: &str,
            _run_id: &str,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<StepPage> {
            deny()
        }
        async fn model_calls(
            &self,
            _thread: &ThreadKey,
            _turn_id: &str,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<ModelCallPage> {
            deny()
        }
        async fn journal(&self, _lease: &Lease, _after: u64, _limit: usize) -> Result<Vec<Value>> {
            deny()
        }
        async fn read_journal(
            &self,
            _thread: &ThreadKey,
            _after: u64,
            _limit: usize,
        ) -> Result<Vec<Value>> {
            deny()
        }
        async fn execution(
            &self,
            _thread: &ThreadKey,
            _execution_id: &str,
        ) -> Result<ToolExecution> {
            deny()
        }
        async fn begin_dispatch(
            &self,
            _lease: &Lease,
            _execution_id: &str,
        ) -> Result<ToolExecution> {
            deny()
        }
        async fn accept_result(
            &self,
            _thread: &ThreadKey,
            _execution_id: &str,
            _result: ExecutionResult,
        ) -> Result<ToolExecution> {
            deny()
        }
        async fn ready(
            &self,
            _selectors: &[HarnessSelector],
            _limit: usize,
        ) -> Result<Vec<ThreadSpec>> {
            deny()
        }
        async fn checkpoints(
            &self,
            _thread: &ThreadKey,
            _after: u64,
            _limit: usize,
        ) -> Result<Vec<u64>> {
            deny()
        }
        async fn checkpoint(&self, _thread: &ThreadKey, _position: u64) -> Result<CheckpointView> {
            deny()
        }
        async fn events(
            &self,
            _session: &SessionKey,
            _cursor: Option<&str>,
            _limit: usize,
        ) -> Result<EventPage> {
            deny()
        }
        async fn snapshot(&self, _session: &SessionKey) -> Result<Snapshot> {
            deny()
        }
    }

    #[tokio::test]
    async fn a_backend_without_the_branch_methods_answers_unavailable() {
        let backend = BareBackend;
        let session = SessionKey::new("ns", "s1").expect("session key");
        let thread = ThreadKey::new(session, "main").expect("thread key");
        let lease = Lease {
            thread: thread.clone(),
            turn_id: "turn".to_string(),
            owner: "owner".to_string(),
            epoch: 1,
            expires_at: 0,
        };

        let error = backend
            .resources(
                &lease,
                ResourceRequest {
                    request_id: "r1".to_string(),
                    operation: ResourceOperation::Get,
                    namespace: Vec::new(),
                    key: Some("a".to_string()),
                    value: Value::Null,
                    expected_version: 0,
                    limit: 0,
                    offset: 0,
                },
            )
            .await
            .expect_err("no resources");
        assert_eq!(error.code(), ErrorCode::Unavailable);

        let error = backend
            .delegate(
                &lease,
                "exec",
                "r1",
                DelegationTarget {
                    child: ThreadSpec {
                        key: thread.clone(),
                        binding: ThreadBinding::new("harness-a", "def-1", "state-1")
                            .expect("binding"),
                    },
                    input: Value::Null,
                },
            )
            .await
            .expect_err("no delegations");
        assert_eq!(error.code(), ErrorCode::Unavailable);

        let error = backend
            .restore(Restoration {
                request_id: "r1".to_string(),
                expected_position: 0,
                target: ThreadSpec {
                    key: thread,
                    binding: ThreadBinding::new("harness-a", "def-1", "state-1").expect("binding"),
                },
                checkpoint: serde_json::json!({ "messages": [] }),
                journal: Vec::new(),
                resources: Vec::new(),
            })
            .await
            .expect_err("no restorations");
        assert_eq!(error.code(), ErrorCode::Unavailable);
    }
}
