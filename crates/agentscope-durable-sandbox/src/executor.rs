//! The durable executor port, answered by a sandbox.
//!
//! One execution is one sandbox handle plus the spec it was launched with. The registry below is
//! in-process on purpose: a handle is a name that only the process that spawned it can poll, and
//! a second process that asks about the same execution must be told `UNKNOWN` rather than be
//! handed a guess. `UNKNOWN` is concluded but never success, so the turn stops claiming an
//! outcome it cannot prove and a human (or a reconciliation) settles it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentscope_durable::executor::{Executor, ExecutorCapabilities};
use agentscope_durable::{DurableError, ExecutionRequest, ExecutionResult, ExecutionState, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::bindings::{BindingRecord, BindingState, BindingStore};
use crate::output::capture;
use crate::sandbox::{ExecSpec, Handle, Poll, Sandbox, SandboxError, SandboxErrorKind};

/// `result.outcome` values. The durable state is three-valued per stream (pending/succeeded/
/// failed) and the outcome names the finer fact a report needs.
pub const OUTCOME_RUNNING: &str = "RUNNING";
pub const OUTCOME_SUCCEEDED: &str = "SUCCEEDED";
pub const OUTCOME_FAILED: &str = "FAILED";
pub const OUTCOME_TIMED_OUT: &str = "TIMED_OUT";
pub const OUTCOME_CANCELLED: &str = "CANCELLED";

const NO_HANDLE: &str = "This process holds no sandbox handle for the execution, so its outcome \
     cannot be reported";

/// The sandbox an execution was launched into, plus its deadline.
#[derive(Clone)]
struct Live {
    handle: Handle,
    spec: ExecSpec,
    deadline: Option<Instant>,
}

/// Implements [`Executor`] over any [`Sandbox`].
///
/// The runtime hands over an [`ExecutionRequest`]; this type reads [`ExecSpec`] out of its
/// arguments, spawns once per execution identity, and reports what the sandbox says. A repeated
/// `start` for an execution already in flight polls the existing handle instead of launching a
/// second process (`idempotent_start`), which is what makes a dispatch retry safe.
pub struct SandboxExecutor {
    sandbox: Arc<dyn Sandbox>,
    live: Mutex<HashMap<String, Live>>,
    /// Where execution identities survive this process; absent keeps the old in-process behavior.
    bindings: Option<Arc<dyn BindingStore>>,
}

impl SandboxExecutor {
    pub fn new<S: Sandbox + 'static>(sandbox: S) -> Self {
        Self::with_shared(Arc::new(sandbox))
    }

    pub fn with_shared(sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            sandbox,
            live: Mutex::new(HashMap::new()),
            bindings: None,
        }
    }

    /// Records every execution's identity so a restarted process can adopt the handle.
    pub fn with_bindings(mut self, bindings: Arc<dyn BindingStore>) -> Self {
        self.bindings = Some(bindings);
        self
    }

    /// How many executions this process still has a handle for. Terminal observations are
    /// forgotten, so the map stays as small as the in-flight work.
    pub fn live_executions(&self) -> usize {
        self.live.lock().map(|live| live.len()).unwrap_or(0)
    }

    fn entry(&self, execution_id: &str) -> Option<Live> {
        self.live.lock().ok()?.get(execution_id).cloned()
    }

    fn forget(&self, execution_id: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(execution_id);
        }
    }

    /// Reads the execution's current fact without starting anything.
    ///
    /// A handle this process does not hold may still be live: the store is the second place a
    /// restarted host looks, and adopting the record is what makes the execution resumable.
    async fn observe(&self, execution_id: &str) -> Result<ExecutionResult> {
        let live = match self.entry(execution_id) {
            Some(live) => live,
            None => match self.attach(execution_id).await? {
                Adopted::Live(live) => live,
                Adopted::Released => return Ok(released_result()),
                Adopted::Absent => return Ok(ExecutionResult::unknown(NO_HANDLE)),
            },
        };
        let poll = match self.sandbox.poll(&live.handle).await {
            Ok(poll) => poll,
            // A handle the sandbox no longer knows says nothing about the effect; a guess here
            // would either duplicate or drop work.
            Err(error) if error.kind() == SandboxErrorKind::NotFound => {
                self.forget(execution_id);
                self.mark_gone(execution_id)?;
                return Ok(ExecutionResult::unknown(format!(
                    "the sandbox no longer knows this execution: {error}"
                )));
            }
            Err(error) => return Err(durable(error)),
        };
        match poll {
            Poll::Running { progress } => {
                if is_expired(live.deadline) {
                    return self.time_out(execution_id, &live).await;
                }
                Ok(pending_result(&live.handle, progress))
            }
            Poll::Exited {
                code,
                stdout,
                stderr,
                truncated,
            } => {
                self.forget(execution_id);
                self.mark_gone(execution_id)?;
                Ok(exited_result(
                    &live.handle,
                    code,
                    &stdout,
                    &stderr,
                    truncated,
                    live.spec.max_output_bytes,
                ))
            }
        }
    }

    /// Stops an execution that outlived its budget.
    ///
    /// The kill is attempted and whatever output the sandbox can still report is kept, so the
    /// turn records *why* the command was stopped (`TIMED_OUT`, the budget, the exit code the
    /// kill produced) instead of a bare failure.
    async fn time_out(&self, execution_id: &str, live: &Live) -> Result<ExecutionResult> {
        let kill_error = self.sandbox.kill(&live.handle).await.err();
        let last = self.sandbox.poll(&live.handle).await.ok();
        self.forget(execution_id);
        self.mark_gone(execution_id)?;

        let mut payload = base(OUTCOME_TIMED_OUT);
        if let Some(timeout_ms) = live.spec.timeout_ms {
            payload.insert("timeoutMs".into(), json!(timeout_ms));
        }
        let mut truncated = false;
        if let Some(Poll::Exited {
            code,
            stdout,
            stderr,
            truncated: reported,
        }) = last
        {
            payload.insert("exitCode".into(), json!(code));
            truncated |= put_stream(
                &mut payload,
                "stdout",
                &stdout,
                live.spec.max_output_bytes,
                reported,
            );
            truncated |= put_stream(
                &mut payload,
                "stderr",
                &stderr,
                live.spec.max_output_bytes,
                reported,
            );
        }
        payload.insert("truncated".into(), json!(truncated));
        if let Some(error) = &kill_error {
            payload.insert("killError".into(), json!(error.message()));
        }
        let budget = live
            .spec
            .timeout_ms
            .map(|ms| format!(" after {ms} ms"))
            .unwrap_or_default();
        Ok(ExecutionResult {
            state: ExecutionState::Cancelled,
            handle: Some(live.handle.id.clone()),
            result: Value::Object(payload),
            error: Some(format!("the sandboxed command was stopped{budget}")),
        })
    }
}

#[async_trait]
impl Executor for SandboxExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            lookup_by_execution_id: true,
            idempotent_start: true,
            cancellation: true,
        }
    }

    async fn start(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        // A second dispatch of the same identity is a retry, not a second effect.
        if self.entry(&request.execution_id).is_some() {
            return self.observe(&request.execution_id).await;
        }
        match self.attach(&request.execution_id).await? {
            Adopted::Live(_) => return self.observe(&request.execution_id).await,
            Adopted::Released => return Ok(released_result()),
            Adopted::Absent => {}
        }
        let spec = ExecSpec::from_arguments(&request.call.arguments).map_err(durable)?;
        let handle = match self.sandbox.spawn(spec.clone()).await {
            Ok(handle) => handle,
            // The sandbox was asked and did not answer: the process may be running. Saying so is
            // the only answer that cannot cause a duplicate effect.
            Err(error) if error.kind() == SandboxErrorKind::Unavailable => {
                return Ok(ExecutionResult::unknown(format!(
                    "the sandbox did not accept the execution: {error}"
                )));
            }
            Err(error) => return Err(durable(error)),
        };
        let deadline = spec
            .timeout_ms
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        // The handle is recorded before anything observes it: a crash between the spawn and the
        // first poll still leaves a restart something to adopt.
        if let Err(error) = self.record_attached(request, &handle, &spec, deadline) {
            let _ = self.sandbox.kill(&handle).await;
            return Err(error);
        }
        if let Ok(mut live) = self.live.lock() {
            live.insert(
                request.execution_id.clone(),
                Live {
                    handle,
                    spec,
                    deadline,
                },
            );
        }
        self.observe(&request.execution_id).await
    }

    async fn query(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        self.observe(&request.execution_id).await
    }

    async fn cancel(&self, request: &ExecutionRequest) -> Result<ExecutionResult> {
        let live = match self.entry(&request.execution_id) {
            Some(live) => live,
            None => match self.attach(&request.execution_id).await? {
                Adopted::Live(live) => live,
                Adopted::Released => return Ok(released_result()),
                Adopted::Absent => return Ok(ExecutionResult::unknown(NO_HANDLE)),
            },
        };
        // A command that finished before the request landed is a fact worth keeping: report what
        // really happened instead of overwriting it with a cancellation.
        if let Ok(Poll::Exited {
            code,
            stdout,
            stderr,
            truncated,
        }) = self.sandbox.poll(&live.handle).await
        {
            self.forget(&request.execution_id);
            self.mark_gone(&request.execution_id)?;
            return Ok(exited_result(
                &live.handle,
                code,
                &stdout,
                &stderr,
                truncated,
                live.spec.max_output_bytes,
            ));
        }
        match self.sandbox.kill(&live.handle).await {
            Ok(()) => {
                self.forget(&request.execution_id);
                self.mark_gone(&request.execution_id)?;
                Ok(ExecutionResult {
                    state: ExecutionState::Cancelled,
                    handle: Some(live.handle.id.clone()),
                    result: Value::Object(base(OUTCOME_CANCELLED)),
                    error: Some("the sandboxed command was cancelled".to_string()),
                })
            }
            // Not confirmed: the process may still be running, and only the sandbox can say.
            Err(error) => Ok(ExecutionResult::unknown(format!(
                "the sandbox did not confirm the cancellation: {error}"
            ))),
        }
    }
}

/// What the store said about an execution this process does not hold.
enum Adopted {
    Live(Live),
    /// Stopping was already requested, or the execution is over: the answer is `CANCELLED`.
    Released,
    /// Nothing is recorded.
    Absent,
}

impl SandboxExecutor {
    async fn attach(&self, execution_id: &str) -> Result<Adopted> {
        let Some(store) = &self.bindings else {
            return Ok(Adopted::Absent);
        };
        let Some(record) = store.get(execution_id)? else {
            return Ok(Adopted::Absent);
        };
        match record.state {
            BindingState::Attached => {
                let live = live_from(&record);
                if let Ok(mut live_map) = self.live.lock() {
                    live_map.insert(execution_id.to_string(), live.clone());
                }
                Ok(Adopted::Live(live))
            }
            // The previous host died between the barrier and the kill: retry the kill, then
            // close the record so the next sweep does not see it again.
            BindingState::Releasing => {
                let _ = self.sandbox.kill(&record.handle).await;
                self.mark(&record, BindingState::Gone)?;
                Ok(Adopted::Released)
            }
            BindingState::Gone => Ok(Adopted::Released),
        }
    }

    fn mark(&self, record: &BindingRecord, state: BindingState) -> Result<()> {
        let Some(store) = &self.bindings else {
            return Ok(());
        };
        let mut next = record.clone();
        next.state = state;
        next.updated_at = now_millis();
        store.put(&next)
    }

    fn mark_gone(&self, execution_id: &str) -> Result<()> {
        let Some(store) = &self.bindings else {
            return Ok(());
        };
        let Some(record) = store.get(execution_id)? else {
            return Ok(());
        };
        self.mark(&record, BindingState::Gone)
    }

    fn record_attached(
        &self,
        request: &ExecutionRequest,
        handle: &Handle,
        spec: &ExecSpec,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let Some(store) = &self.bindings else {
            return Ok(());
        };
        let deadline_ms = deadline.map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            now_millis() + remaining.as_millis() as u64
        });
        store.put(&BindingRecord {
            execution_id: request.execution_id.clone(),
            thread: request.thread.clone(),
            handle: handle.clone(),
            spec: spec.clone(),
            deadline_ms,
            state: BindingState::Attached,
            updated_at: now_millis(),
        })
    }

    /// Sweeps the store: `Releasing` records are closed, expired `Attached` ones are stopped.
    pub async fn keepalive(&self, limit: usize) -> Result<usize> {
        let Some(store) = &self.bindings else {
            return Ok(0);
        };
        let now = now_millis();
        let mut handled = 0;
        for record in store.live(limit)? {
            match record.state {
                BindingState::Releasing => {
                    let _ = self.sandbox.kill(&record.handle).await;
                    self.mark(&record, BindingState::Gone)?;
                    handled += 1;
                }
                BindingState::Attached => {
                    if !record.deadline_ms.is_some_and(|deadline| deadline <= now) {
                        continue;
                    }
                    if let Ok(Poll::Running { .. }) = self.sandbox.poll(&record.handle).await {
                        let _ = self.sandbox.kill(&record.handle).await;
                    }
                    self.forget(&record.execution_id);
                    self.mark(&record, BindingState::Gone)?;
                    handled += 1;
                }
                BindingState::Gone => {}
            }
        }
        Ok(handled)
    }
}

fn live_from(record: &BindingRecord) -> Live {
    let deadline = record.deadline_ms.map(|deadline_ms| {
        let remaining = deadline_ms.saturating_sub(now_millis());
        Instant::now() + Duration::from_millis(remaining)
    });
    Live {
        handle: record.handle.clone(),
        spec: record.spec.clone(),
        deadline,
    }
}

fn released_result() -> ExecutionResult {
    ExecutionResult {
        state: ExecutionState::Cancelled,
        handle: None,
        result: Value::Object(base(OUTCOME_CANCELLED)),
        error: Some("the execution was released".to_string()),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn is_expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn durable(error: SandboxError) -> DurableError {
    match error.kind() {
        SandboxErrorKind::Invalid => DurableError::invalid(error.message()),
        SandboxErrorKind::NotFound => DurableError::not_found(error.message()),
        SandboxErrorKind::Unavailable => DurableError::unavailable(error.message()),
    }
}

fn base(outcome: &str) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("outcome".into(), json!(outcome));
    payload
}

/// No verdict yet: `PENDING`, which the runtime treats as neither success nor failure.
fn pending_result(handle: &Handle, progress: u8) -> ExecutionResult {
    let mut payload = base(OUTCOME_RUNNING);
    payload.insert("progress".into(), json!(progress));
    ExecutionResult {
        state: ExecutionState::Pending,
        handle: Some(handle.id.clone()),
        result: Value::Object(payload),
        error: None,
    }
}

fn exited_result(
    handle: &Handle,
    code: i32,
    stdout: &[u8],
    stderr: &[u8],
    reported_truncated: bool,
    max_output_bytes: usize,
) -> ExecutionResult {
    let succeeded = code == 0;
    let mut payload = base(if succeeded {
        OUTCOME_SUCCEEDED
    } else {
        OUTCOME_FAILED
    });
    payload.insert("exitCode".into(), json!(code));
    let mut truncated = put_stream(
        &mut payload,
        "stdout",
        stdout,
        max_output_bytes,
        reported_truncated,
    );
    truncated |= put_stream(
        &mut payload,
        "stderr",
        stderr,
        max_output_bytes,
        reported_truncated,
    );
    payload.insert("truncated".into(), json!(truncated));
    ExecutionResult {
        state: if succeeded {
            ExecutionState::Succeeded
        } else {
            ExecutionState::Failed
        },
        handle: Some(handle.id.clone()),
        result: Value::Object(payload),
        error: (code != 0).then(|| format!("the sandboxed command exited with code {code}")),
    }
}

/// Records one stream: the kept text, the kept byte count, the original byte count and whether
/// anything was dropped. Returns whether this stream was truncated.
fn put_stream(
    payload: &mut Map<String, Value>,
    name: &str,
    bytes: &[u8],
    max: usize,
    reported_truncated: bool,
) -> bool {
    let captured = capture(bytes, max);
    let truncated = captured.truncated || reported_truncated;
    payload.insert(name.to_string(), json!(captured.text()));
    payload.insert(format!("{name}Bytes"), json!(captured.bytes.len()));
    payload.insert(format!("{name}TotalBytes"), json!(captured.total));
    payload.insert(format!("{name}Truncated"), json!(truncated));
    truncated
}
