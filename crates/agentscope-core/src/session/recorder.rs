//! Per-execution ordered acceptance and acknowledged checkpoints, mirroring
//! `SessionRecorder`.
//!
//! The recorder holds the execution writer lease for the lifetime of a run. Facts are buffered
//! in acceptance order and published by one commit; a failed commit is retained as a sticky
//! failure so later writes never pretend to have been persisted.
//!
//! Two independent duties that Java bundles into one 1s heartbeat thread are kept apart here:
//!
//! * **Lease renewal** is a background tick. A run can be silent for minutes (one long model
//!   stream, one slow tool) while it still owns the session; without renewal the lease expires
//!   and the next fact fails with `Session writer ownership expired or fenced` — which for an
//!   observation is fatal, because the tool already ran and must not be replayed.
//! * **Publishing facts** stays caller-driven ([`SessionRecorder::flush`]). A background commit
//!   could race the agent loop's own flush on the same pending buffer and turn a healthy run
//!   into a sticky "Stale committed prefix" failure.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{
    session_error, SessionEvent, SessionLog, SessionLogExporter, SessionProjection, Writer,
};
use crate::agent::RuntimeContext;
use crate::message::{ContentBlock, GenerateReason, Msg, MsgRole};
use crate::state::AgentState;

/// Runtime-context key carrying the active [`SessionRecorder`].
pub const RECORDER_CONTEXT_KEY: &str = "agentscope.session.recorder";
/// Runtime-context key carrying the turn id pinned for this execution.
pub const TURN_ID_CONTEXT_KEY: &str = "agentscope.session.turnId";
/// Runtime-context key carrying the parent session recorder.
pub const PARENT_CONTEXT_KEY: &str = "agentscope.session.parent";

/// The execution writer lease duration.
const LEASE: Duration = Duration::from_secs(120);

/// Lower bound for the renewal tick, so a deliberately short lease (tests) still renews.
const MIN_RENEW_INTERVAL: Duration = Duration::from_millis(50);

/// Renewal cadence for a lease: a quarter of it, keeping Java's generous safety margin.
fn renew_interval(lease: Duration) -> Duration {
    std::cmp::max(lease / 4, MIN_RENEW_INTERVAL)
}

/// Pending buffer ceiling before acceptance refuses to grow it further.
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

/// The logical outcome of a normally returned result, independent of the caller's handle.
///
/// Mirrors Java `SessionRecorder.outcome`.
pub fn outcome(reason: GenerateReason) -> &'static str {
    match reason {
        GenerateReason::ToolSuspended
        | GenerateReason::PermissionAsking
        | GenerateReason::ReasoningStopRequested
        | GenerateReason::ActingStopRequested
        | GenerateReason::MiddlewareStopRequested => "suspended",
        GenerateReason::Interrupted => "interrupted",
        GenerateReason::MaxIterations | GenerateReason::AllToolsDenied => "failed",
        _ => "completed",
    }
}

/// Journal-safe view of [`crate::model::GenerateOptions`].
///
/// Unlike Java, Rust `GenerateOptions` is not `Serialize`: it also carries secrets (`api_key`)
/// and opaque passthrough maps (`additional_headers` / `additional_body_params`). The journal
/// therefore records only the generation-relevant, non-secret subset; the request content is
/// already covered by `messages` and `tools`.
fn options_payload(options: &crate::model::GenerateOptions) -> serde_json::Value {
    serde_json::json!({
        "modelName": options.model_name,
        "stream": options.stream,
        "temperature": options.temperature,
        "topP": options.top_p,
        "topK": options.top_k,
        "maxTokens": options.max_tokens,
        "maxCompletionTokens": options.max_completion_tokens,
        "frequencyPenalty": options.frequency_penalty,
        "presencePenalty": options.presence_penalty,
        "thinkingBudget": options.thinking_budget,
        "reasoningEffort": options.reasoning_effort,
        "seed": options.seed,
        "cacheControl": options.cache_control,
        "parallelToolCalls": options.parallel_tool_calls,
        "toolChoice": options.tool_choice,
        "responseFormat": options.response_format,
    })
}

/// Mutable recording state guarded by one mutex.
#[derive(Default)]
struct RecorderState {
    pending: Vec<SessionEvent>,
    pending_bytes: usize,
    durable_seq: u64,
    failure: Option<String>,
    closed: bool,
    batch_id: Option<String>,
    result_status: Option<String>,
    result_reason: Option<String>,
    result_json: Option<String>,
    step_id: Option<String>,
    action_ids: HashMap<String, String>,
    previous_state: Option<String>,
    previous_messages: Vec<String>,
    recorded_messages: HashSet<String>,
    input_messages: HashSet<String>,
}

/// Per-execution ordered acceptance and acknowledged checkpoints.
pub struct SessionRecorder {
    log: Arc<dyn SessionLog>,
    writer: Writer,
    run_id: String,
    turn_id: String,
    resumed_turn: bool,
    initial: SessionProjection,
    state: Mutex<RecorderState>,
    exporter: Option<Arc<SessionLogExporter>>,
    /// The lease duration this recorder acquires and renews with.
    lease: Duration,
    /// Background lease renewal; `None` when no Tokio runtime was available at open time.
    heartbeat: Mutex<Option<LeaseHeartbeat>>,
}

impl Drop for SessionRecorder {
    fn drop(&mut self) {
        // A recorder that is dropped without `close()` means its run unwound abnormally (a
        // panic in the agent loop, a future dropped mid-run). The writer lease must not be left
        // to expire on its own: the session would stay owned by a dead run for the whole lease
        // duration, and the next turn would wait for it.
        let already_closed = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed;
        if already_closed {
            return;
        }
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed = true;
        // Stop renewing first so a tick cannot reclaim the lease after the release.
        if let Some(mut heartbeat) = self
            .heartbeat
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            heartbeat.stop();
        }
        // `release` is async and `Drop` is not, so hand it to the runtime. Without one the
        // process is going away and the lease dies with it.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let log = Arc::clone(&self.log);
            let writer = self.writer.clone();
            handle.spawn(async move {
                let _ = log.release(&writer).await;
            });
        }
    }
}

/// Renews the execution writer lease while the recorder is open.
///
/// Mirrors the renewal half of Java's `SessionRecorder` heartbeat thread. It deliberately does
/// **not** flush: a background commit would race the agent loop's own flush on the same pending
/// buffer. Renewal alone is what keeps a long model stream or a slow tool from losing ownership.
struct LeaseHeartbeat {
    stop: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl LeaseHeartbeat {
    fn start(log: Arc<dyn SessionLog>, writer: Writer, lease: Duration) -> Option<Self> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let task_stop = Arc::clone(&stop);
        let interval = renew_interval(lease);
        let handle = runtime.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if task_stop.load(Ordering::SeqCst) {
                    return;
                }
                if let Err(error) = log.renew(&writer, lease).await {
                    // The lease is gone (expired, fenced, sealed or released elsewhere).
                    // Stop renewing; the next flush surfaces the failure instead of this
                    // task silently writing against a journal another writer now owns.
                    tracing::warn!("session writer lease renewal failed: {error}");
                    return;
                }
            }
        });
        Some(Self { stop, handle: Some(handle) })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }

    /// Aborts the task and waits until it is really gone.
    ///
    /// `abort` alone is not enough before releasing the lease: a renewal that is already
    /// awaiting storage could still land afterwards and reclaim ownership.
    async fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for SessionRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRecorder")
            .field("run_id", &self.run_id)
            .field("turn_id", &self.turn_id)
            .field("resumed_turn", &self.resumed_turn)
            .finish_non_exhaustive()
    }
}

impl SessionRecorder {
    /// Opens a recorder for `turn_id`, allocating a fresh run id.
    pub async fn open(
        log: Arc<dyn SessionLog>,
        turn_id: Option<&str>,
    ) -> crate::error::Result<Self> {
        Self::open_with_exporter(log, turn_id, None).await
    }

    /// Opens a recorder for `turn_id` with an optional committed-event exporter in one call.
    ///
    /// Equivalent to [`SessionRecorder::open`] when `exporter` is `None`. This is the seam
    /// used by the agent loop: the run id stays internal while the exporter is supplied.
    pub async fn open_with_exporter(
        log: Arc<dyn SessionLog>,
        turn_id: Option<&str>,
        exporter: Option<Arc<SessionLogExporter>>,
    ) -> crate::error::Result<Self> {
        Self::open_with(
            log,
            turn_id,
            &uuid::Uuid::new_v4().to_string(),
            exporter,
        )
        .await
    }

    /// Opens a recorder with an explicit run id, optionally exporting committed events.
    pub async fn open_with(
        log: Arc<dyn SessionLog>,
        turn_id: Option<&str>,
        run_id: &str,
        exporter: Option<Arc<SessionLogExporter>>,
    ) -> crate::error::Result<Self> {
        Self::open_with_lease(log, turn_id, run_id, exporter, LEASE).await
    }

    /// Opens a recorder whose writer lease (and renewal cadence) is explicit.
    ///
    /// Production callers use [`SessionRecorder::open_with`]; the shorter lease exists so a test
    /// can observe renewal without waiting two minutes.
    pub(crate) async fn open_with_lease(
        log: Arc<dyn SessionLog>,
        turn_id: Option<&str>,
        run_id: &str,
        exporter: Option<Arc<SessionLogExporter>>,
        lease: Duration,
    ) -> crate::error::Result<Self> {
        let turn_id = match turn_id {
            Some(id) if id.trim().is_empty() => {
                return Err(session_error("Run and turn IDs must not be blank"));
            }
            Some(id) => id.to_string(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        if run_id.trim().is_empty() {
            return Err(session_error("Run and turn IDs must not be blank"));
        }

        let writer = log.acquire(run_id, lease).await?;
        let prepared = async {
            let initial = SessionProjection::read(&*log).await?;
            let durable_seq = initial.as_of_seq;
            let mut seen_turn = false;
            for event in log.scan(0, durable_seq).await? {
                if event.execution_run_id.as_deref() == Some(run_id) {
                    return Err(session_error(
                        "Run ID already used; prepare a new execution to resume",
                    ));
                }
                if event.turn_id.as_deref() != Some(turn_id.as_str()) {
                    continue;
                }
                seen_turn = true;
                let completed = event.event_type == "turn/completed"
                    || (event.event_type == "turn/end"
                        && event
                            .data()
                            .ok()
                            .and_then(|d| d.get("status").and_then(|s| s.as_str()).map(str::to_string))
                            .as_deref()
                            == Some("completed"));
                if completed {
                    return Err(session_error(
                        "Turn already completed; use a new turn ID",
                    ));
                }
            }
            if !initial.uncertain_tool_calls.is_empty() {
                return Err(session_error(format!(
                    "Explicit reconciliation required for unknown tool outcomes: {:?}",
                    initial.uncertain_tool_calls
                )));
            }
            let previous_state = initial.state_json.clone();
            let previous_messages = match initial.restore()? {
                Some(state) => message_json(&state)?,
                None => Vec::new(),
            };
            let mut recorded_messages = HashSet::new();
            for message in super::transcript(&*log).await?.messages {
                recorded_messages.insert(message.id);
            }
            Ok((initial, durable_seq, seen_turn, previous_state, previous_messages, recorded_messages))
        }
        .await;

        let (initial, durable_seq, resumed_turn, previous_state, previous_messages, recorded) =
            match prepared {
                Ok(values) => values,
                Err(error) => {
                    let _ = log.release(&writer).await;
                    return Err(error);
                }
            };

        Ok(Self {
            heartbeat: Mutex::new(LeaseHeartbeat::start(Arc::clone(&log), writer.clone(), lease)),
            lease,
            log,
            writer,
            run_id: run_id.to_string(),
            turn_id,
            resumed_turn,
            initial,
            state: Mutex::new(RecorderState {
                durable_seq,
                previous_state,
                previous_messages,
                recorded_messages: recorded,
                ..RecorderState::default()
            }),
            exporter,
        })
    }

    /// Returns the recorder carried by a runtime context, if any.
    pub fn from_context(context: &RuntimeContext) -> Option<Arc<SessionRecorder>> {
        context.extension::<SessionRecorder>(RECORDER_CONTEXT_KEY)
    }

    /// The execution run id.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The logical turn id.
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The committed prefix the recorder started from.
    pub fn initial(&self) -> &SessionProjection {
        &self.initial
    }

    /// The last durably committed sequence number.
    pub fn durable_seq(&self) -> u64 {
        self.lock().durable_seq
    }

    /// Message ids already present in the committed transcript.
    pub fn accepted_message_ids(&self) -> HashSet<String> {
        self.lock().recorded_messages.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RecorderState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn check(state: &RecorderState) -> crate::error::Result<()> {
        if state.closed {
            return Err(session_error("Session recorder closed"));
        }
        if let Some(failure) = &state.failure {
            return Err(session_error(format!("Session persistence failed: {failure}")));
        }
        Ok(())
    }

    /// Buffers a fact, returning its assigned sequence number.
    pub fn append<T: serde::Serialize>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> crate::error::Result<u64> {
        self.append_required(event_type, payload, true)
    }

    /// Buffers a fact; `required` facts stop recovery when an extension type is unknown.
    pub fn append_required<T: serde::Serialize>(
        &self,
        event_type: &str,
        payload: &T,
        required: bool,
    ) -> crate::error::Result<u64> {
        let mut guard = self.lock();
        Self::check(&guard)?;
        let payload_json = serde_json::to_string(payload)?;
        let bytes = payload_json.len();
        if guard.pending_bytes + bytes > MAX_PENDING_BYTES {
            return Err(session_error("Session pending buffer exhausted"));
        }
        let seq = guard.durable_seq + guard.pending.len() as u64 + 1;
        let event = SessionEvent::new(
            uuid::Uuid::new_v4().to_string(),
            seq,
            chrono::Utc::now().timestamp_millis(),
            event_type,
            Some(self.run_id.clone()),
            Some(self.turn_id.clone()),
            required,
            payload_json,
        )?;
        super::SessionEventTypes::validate(&event)?;
        guard.pending.push(event);
        guard.pending_bytes += bytes;
        Ok(seq)
    }

    /// Publishes the pending buffer as one commit.
    pub async fn flush(&self) -> crate::error::Result<()> {
        let (batch, events, expected) = {
            let mut guard = self.lock();
            Self::check(&guard)?;
            if guard.pending.is_empty() {
                return Ok(());
            }
            let batch_id = guard
                .batch_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            guard.batch_id = Some(batch_id.clone());
            (batch_id, guard.pending.clone(), guard.durable_seq)
        };
        self.log.renew(&self.writer, self.lease).await?;
        let commit = self.log.commit(&self.writer, &batch, expected, &events).await;
        match commit {
            Ok(head) => {
                // Keep the guard inside its own block: a `MutexGuard` held across the export
                // await would make every recorder future non-`Send`.
                {
                    let mut guard = self.lock();
                    guard.durable_seq = head.seq;
                    guard.pending.clear();
                    guard.pending_bytes = 0;
                    guard.batch_id = None;
                }
                if let Some(exporter) = &self.exporter {
                    // Export is best effort: a failed drain is retried by the next flush and
                    // never fails the execution that produced the facts.
                    let _ = exporter.drain().await;
                }
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                {
                    let mut guard = self.lock();
                    if guard.failure.is_none() {
                        guard.failure = Some(message.clone());
                    }
                }
                Err(session_error(format!("Failed to commit session prefix: {message}")))
            }
        }
    }

    /// Buffers a fact and immediately commits it: an acknowledged boundary.
    pub async fn record<T: serde::Serialize>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> crate::error::Result<u64> {
        let seq = self.append(event_type, payload)?;
        self.flush().await?;
        Ok(seq)
    }

    /// Buffers a fact only while the recorder is still open.
    pub fn record_if_open<T: serde::Serialize>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> crate::error::Result<Option<u64>> {
        if self.lock().closed {
            return Ok(None);
        }
        Ok(Some(self.append(event_type, payload)?))
    }

    /// Records an action id for a tool call, so action and tool facts can be correlated.
    pub fn action_started(&self, tool_call_id: &str, action_id: &str) {
        self.lock()
            .action_ids
            .insert(tool_call_id.to_string(), action_id.to_string());
    }

    /// The action id recorded for a tool call (`""` when unknown).
    pub fn action_id(&self, tool_call_id: &str) -> String {
        self.lock()
            .action_ids
            .get(tool_call_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Records the run/turn start facts and the first input.
    pub async fn start(&self, input: &[Msg], state: &AgentState) -> crate::error::Result<()> {
        if self.initial.state_json.is_none() {
            self.append(
                "migration/baseline",
                &serde_json::json!({ "stateJson": state.to_json()?, "coverage": "baseline_only" }),
            )?;
        }
        if !self.initial.active_runs.is_empty() {
            self.append(
                "recovery/applied",
                &serde_json::json!({
                    "interruptedRuns": self.initial.active_runs,
                    "sourceSeq": self.initial.as_of_seq,
                }),
            )?;
        }
        self.append(
            "run/start",
            &serde_json::json!({ "executionRunId": self.run_id, "coverage": "adapter_chunks" }),
        )?;
        self.append(
            if self.resumed_turn {
                "turn/resumed"
            } else {
                "turn/start"
            },
            &serde_json::json!({ "turnId": self.turn_id }),
        )?;
        {
            let mut guard = self.lock();
            for message in input {
                guard.input_messages.insert(message.id.clone());
            }
        }
        self.append("input/received", &serde_json::json!({ "messages": input }))?;
        self.flush().await
    }

    /// Marks a reasoning iteration boundary.
    pub fn step(&self, iteration: usize) -> crate::error::Result<()> {
        let previous = {
            let mut guard = self.lock();
            guard.step_id.take()
        };
        if let Some(step_id) = previous {
            self.append(
                "step/end",
                &serde_json::json!({ "stepId": step_id, "status": "continued" }),
            )?;
        }
        let step_id = uuid::Uuid::new_v4().to_string();
        self.append(
            "step/start",
            &serde_json::json!({ "stepId": step_id, "iteration": iteration }),
        )?;
        self.lock().step_id = Some(step_id);
        Ok(())
    }

    /// Records the turn's final message and its logical outcome.
    pub fn result(&self, result: Option<&Msg>) -> crate::error::Result<()> {
        let mut guard = self.lock();
        if let Some(message) = result {
            guard.result_json = Some(serde_json::to_string(message)?);
            guard.result_reason = Some(format!("{:?}", message.generate_reason()).to_uppercase());
            guard.result_status = Some(outcome(message.generate_reason()).to_string());
        }
        Ok(())
    }

    /// Snapshots the working state as durable facts plus a checkpoint.
    pub fn capture_state(&self, state: &AgentState, reason: &str) -> crate::error::Result<()> {
        let json = state.to_json()?;
        let messages = message_json(state)?;

        // Decide what changed under the lock, then append with the lock released:
        // `append` takes the same (non-reentrant) mutex.
        let (replaced, durable_seq, fresh, previous_state) = {
            let mut guard = self.lock();
            Self::check(&guard)?;
            let unchanged = guard.previous_state.as_deref() == Some(json.as_str())
                && state
                    .context
                    .iter()
                    .all(|message| guard.recorded_messages.contains(&message.id));
            if unchanged {
                return Ok(());
            }
            let mut prefix = 0;
            while prefix < guard.previous_messages.len()
                && prefix < messages.len()
                && guard.previous_messages[prefix] == messages[prefix]
            {
                prefix += 1;
            }
            let replaced = prefix < guard.previous_messages.len();
            let durable_seq = guard.durable_seq;
            let mut fresh = Vec::new();
            for message in &state.context {
                if guard.recorded_messages.insert(message.id.clone()) {
                    let applied = guard.input_messages.contains(&message.id);
                    fresh.push((message.clone(), applied));
                }
            }
            (
                replaced,
                durable_seq,
                fresh,
                guard.previous_state.clone(),
            )
        };

        if replaced {
            self.append(
                "context/replaced",
                &serde_json::json!({
                    "reason": reason,
                    "messages": state.context,
                    "previousAsOfSeq": durable_seq,
                }),
            )?;
        }

        for (message, applied) in fresh {
            if applied {
                self.append(
                    "input/applied",
                    &serde_json::json!({ "inputId": message.id }),
                )?;
            }
            self.append(message_event_type(&message), &serde_json::json!({ "message": &message }))?;
            for call in message.tool_use_blocks() {
                self.append(
                    "tool/requested",
                    &serde_json::json!({
                        "messageId": message.id,
                        "toolCallId": call.id,
                        "call": call,
                    }),
                )?;
            }
        }

        let previous: serde_json::Value = match previous_state {
            Some(json) => serde_json::from_str(&json)?,
            None => serde_json::Value::Object(Default::default()),
        };
        let current: serde_json::Value = serde_json::from_str(&json)?;
        for (field, event_type) in [
            ("tasks_context", "task/changed"),
            ("plan_mode_context", "plan/changed"),
            ("permission_context", "permission/changed"),
        ] {
            if let Some(value) = current.get(field) {
                if Some(value) != previous.get(field) {
                    self.append(event_type, &serde_json::json!({ "state": value }))?;
                }
            }
        }
        self.append(
            "state/checkpoint",
            &serde_json::json!({
                "projectorVersion": 1,
                "stateJson": json,
                "reason": reason,
            }),
        )?;
        {
            let mut guard = self.lock();
            guard.previous_state = Some(json);
            guard.previous_messages = messages;
        }
        Ok(())
    }

    /// Acknowledged checkpoint for callers already inside this execution.
    pub async fn checkpoint_now(
        &self,
        state: &AgentState,
        reason: &str,
    ) -> crate::error::Result<()> {
        self.capture_state(state, reason)?;
        self.flush().await
    }

    /// Records a prepared model request and its dispatch, mirroring Java `prepareModel`.
    ///
    /// The `before_model` checkpoint runs first, so recovery never replays a model call whose
    /// input the journal does not have. `request/prepared` and `model/dispatch` are published
    /// by one commit: a request that reached the provider must be discoverable as dispatched.
    pub async fn prepare_model(
        &self,
        input: &crate::middleware::ModelCallInput,
        call_id: &str,
        purpose: &str,
        state: Option<&AgentState>,
    ) -> crate::error::Result<()> {
        if let Some(state) = state {
            self.checkpoint_now(state, "before_model").await?;
        }
        self.append(
            "request/prepared",
            &serde_json::json!({
                "modelCallId": call_id,
                "purpose": purpose,
                "model": input.model.model_name(),
                "messages": input.messages,
                "tools": input.tools,
                "options": options_payload(&input.options),
            }),
        )?;
        self.append(
            "model/dispatch",
            &serde_json::json!({ "modelCallId": call_id, "attemptId": call_id }),
        )?;
        self.flush().await
    }

    /// Records the terminal turn/run facts and releases the writer lease.
    pub async fn finish(&self, state: &AgentState, status: &str) -> crate::error::Result<()> {
        self.finish_with_error(state, status, None).await
    }

    /// Records the terminal turn/run facts and releases the writer lease.
    ///
    /// `error` is persisted on `turn/*` and `run/end`, so a failed run can report the
    /// provider/tool failure message instead of a generic "run failed" later on.
    pub async fn finish_with_error(
        &self,
        state: &AgentState,
        status: &str,
        error: Option<&str>,
    ) -> crate::error::Result<()> {
        let result = self.finish_inner(state, status, error).await;
        self.close().await;
        result
    }

    async fn finish_inner(
        &self,
        state: &AgentState,
        status: &str,
        error: Option<&str>,
    ) -> crate::error::Result<()> {
        self.capture_state(state, &format!("run_{status}"))?;
        let (result_json, result_reason, result_status, step_id) = {
            let mut guard = self.lock();
            (
                guard.result_json.clone(),
                guard.result_reason.clone(),
                guard.result_status.clone(),
                guard.step_id.take(),
            )
        };
        let terminal = if status == "completed" {
            result_status.unwrap_or_else(|| status.to_string())
        } else {
            status.to_string()
        };
        if let Some(json) = result_json {
            if terminal != "suspended" {
                self.append(
                    "turn/output",
                    &serde_json::json!({
                        "message": serde_json::from_str::<serde_json::Value>(&json)?,
                        "status": if terminal == "completed" { "completed" } else { "incomplete" },
                    }),
                )?;
            }
        }
        let outcome_payload = {
            let mut payload = serde_json::json!({ "status": terminal, "reason": result_reason });
            if let Some(error) = error.filter(|error| !error.is_empty()) {
                payload["error"] = serde_json::Value::String(error.to_string());
            }
            payload
        };
        if let Some(step_id) = step_id {
            self.append(
                "step/end",
                &serde_json::json!({ "stepId": step_id, "status": terminal }),
            )?;
        }
        self.append(&format!("turn/{terminal}"), &outcome_payload)?;
        self.append("run/end", &outcome_payload)?;
        self.flush().await
    }

    /// Releases the writer lease. Idempotent.
    pub async fn close(&self) {
        {
            let mut guard = self.lock();
            if guard.closed {
                return;
            }
            guard.closed = true;
        }
        // Stop renewing *before* releasing: a tick that lands after the release would claim the
        // lease again and leave the session owned by a recorder nobody will ever close.
        self.stop_heartbeat().await;
        let _ = self.log.release(&self.writer).await;
    }
    /// Stops the background lease renewal and waits for its task to end.
    async fn stop_heartbeat(&self) {
        let heartbeat = self
            .heartbeat
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(heartbeat) = heartbeat {
            heartbeat.shutdown().await;
        }
    }

    /// Records the runtime facts not already captured at a model/tool boundary.
    ///
    /// Interaction requests and resolutions are the highest-value facts here: they are what
    /// `respond` and `resume` resolve against.
    pub fn observe(&self, event: &crate::event::AgentEvent) -> crate::error::Result<()> {
        use crate::event::AgentEvent;
        match event {
            AgentEvent::RequireUserConfirm(data) => {
                for call in tool_calls_of(data) {
                    self.append(
                        "interaction/requested",
                        &serde_json::json!({
                            "requestId": call.id,
                            "kind": "confirmation",
                            "call": call,
                        }),
                    )?;
                }
            }
            AgentEvent::RequireExternalExecution(data) => {
                for call in tool_calls_of(data) {
                    self.append(
                        "interaction/requested",
                        &serde_json::json!({
                            "requestId": call.id,
                            "kind": "external_execution",
                            "call": call,
                        }),
                    )?;
                }
            }
            AgentEvent::ExternalExecutionResult(data) => {
                for result in tool_results_of(data) {
                    self.append(
                        "interaction/resolved",
                        &serde_json::json!({
                            "requestId": result.id,
                            "kind": "external_execution",
                            "result": result,
                        }),
                    )?;
                }
            }
            AgentEvent::UserConfirmResult(data) => {
                for result in tool_results_of(data) {
                    self.append(
                        "interaction/resolved",
                        &serde_json::json!({
                            "requestId": result.id,
                            "kind": "confirmation",
                            "result": result,
                        }),
                    )?;
                }
            }
            AgentEvent::RequestStop(_) => {
                self.append(
                    "run/stop_requested",
                    &serde_json::json!({ "event": "REQUEST_STOP" }),
                )?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn message_event_type(message: &Msg) -> &'static str {
    if message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult(_)))
    {
        "tool/result"
    } else if message.role == MsgRole::Assistant {
        "message/assistant"
    } else if message.role == MsgRole::System {
        "message/system"
    } else {
        "message/user"
    }
}

fn message_json(state: &AgentState) -> crate::error::Result<Vec<String>> {
    state
        .context
        .iter()
        .map(|message| serde_json::to_string(message).map_err(crate::error::Error::from))
        .collect()
}

/// Extracts tool calls from an event payload (`toolCalls` / `tool_calls`).
fn tool_calls_of(data: &crate::event::EventData) -> Vec<crate::message::ToolUseBlock> {
    for key in ["toolCalls", "tool_calls"] {
        if let Some(values) = data.fields.get(key).and_then(|v| v.as_array()) {
            return values
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
        }
    }
    Vec::new()
}

/// Extracts tool results from an event payload (`toolResults` / `tool_results`).
fn tool_results_of(data: &crate::event::EventData) -> Vec<crate::message::ToolResultBlock> {
    for key in ["toolResults", "tool_results"] {
        if let Some(values) = data.fields.get(key).and_then(|v| v.as_array()) {
            return values
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, StorageValue,
    };

    /// Storage that can be told to fail commit writes, to exercise sticky failures.
    struct FailingStorage {
        inner: MemorySessionStorage,
        fail_commits: AtomicBool,
    }

    impl FailingStorage {
        fn new() -> Self {
            Self {
                inner: MemorySessionStorage::new(),
                fail_commits: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl AtomicSessionStorage for FailingStorage {
        async fn read(&self, path: &str) -> crate::error::Result<Option<StorageValue>> {
            self.inner.read(path).await
        }

        async fn compare_and_set(
            &self,
            path: &str,
            expected_version: u64,
            bytes: &[u8],
        ) -> crate::error::Result<bool> {
            if self.fail_commits.load(Ordering::SeqCst) && path.contains("/commits/") {
                return Err(crate::error::Error::Other("storage down".into()));
            }
            self.inner
                .compare_and_set(path, expected_version, bytes)
                .await
        }
    }

    async fn journal() -> (Arc<dyn SessionLog>, Arc<dyn AtomicSessionStorage>) {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        let log: Arc<dyn SessionLog> =
            Arc::new(JournalSessionLog::new(Arc::clone(&storage), "s1"));
        (log, storage)
    }

    async fn seed(log: &dyn SessionLog, owner: &str, events: Vec<SessionEvent>) {
        let writer = log.acquire(owner, LEASE).await.unwrap();
        log.commit(&writer, "seed", 0, &events).await.unwrap();
        log.release(&writer).await.unwrap();
    }

    fn raw(seq: u64, kind: &str, run: Option<&str>, turn: Option<&str>, payload: &str) -> SessionEvent {
        SessionEvent::new(
            format!("e{seq}"),
            seq,
            0,
            kind,
            run.map(str::to_string),
            turn.map(str::to_string),
            true,
            payload,
        )
        .unwrap()
    }

    fn state_with(message: Msg) -> AgentState {
        crate::state::AgentState::builder()
            .session_id("s1")
            .add_message(message)
            .build()
    }

    async fn event_types(log: &dyn SessionLog) -> Vec<String> {
        let head = log.head().await.unwrap();
        log.scan(0, head.seq)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.event_type)
            .collect()
    }

    #[test]
    fn outcome_maps_generate_reasons_like_java() {
        assert_eq!(outcome(GenerateReason::ModelStop), "completed");
        assert_eq!(outcome(GenerateReason::ToolCalls), "completed");
        assert_eq!(outcome(GenerateReason::ToolSuspended), "suspended");
        assert_eq!(outcome(GenerateReason::PermissionAsking), "suspended");
        assert_eq!(outcome(GenerateReason::Interrupted), "interrupted");
        assert_eq!(outcome(GenerateReason::MaxIterations), "failed");
        assert_eq!(outcome(GenerateReason::AllToolsDenied), "failed");
    }

    #[tokio::test]
    async fn start_records_baseline_run_and_input() {
        let (log, _storage) = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let input = vec![Msg::user("hello")];
        let state = crate::state::AgentState::builder().session_id("s1").build();
        recorder.start(&input, &state).await.unwrap();

        let types = event_types(&*log).await;
        assert_eq!(
            types,
            vec![
                "migration/baseline",
                "run/start",
                "turn/start",
                "input/received"
            ]
        );
        assert!(recorder.durable_seq() >= 4);
        // The input message id is remembered so `capture_state` records it as applied.
        assert!(recorder
            .accepted_message_ids()
            .is_empty());
        recorder.close().await;
    }

    #[tokio::test]
    async fn reusing_a_turn_marks_the_run_as_resumed() {
        let (log, _storage) = journal().await;
        seed(
            &*log,
            "run-0",
            vec![raw(1, "run/start", Some("run-0"), Some("t1"), "{}")],
        )
        .await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let state = crate::state::AgentState::builder().session_id("s1").build();
        recorder.start(&[], &state).await.unwrap();
        let types = event_types(&*log).await;
        assert!(types.contains(&"turn/resumed".to_string()), "{types:?}");
        assert!(!types.contains(&"turn/start".to_string()));
        recorder.close().await;
    }

    #[tokio::test]
    async fn completed_turns_and_reused_runs_are_rejected() {
        let (log, _storage) = journal().await;
        seed(
            &*log,
            "run-0",
            vec![
                raw(1, "run/start", Some("run-0"), Some("t1"), "{}"),
                raw(2, "turn/completed", Some("run-0"), Some("t1"), "{}"),
            ],
        )
        .await;
        let err = SessionRecorder::open(Arc::clone(&log), Some("t1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Turn already completed"), "{err}");

        let err = SessionRecorder::open_with(Arc::clone(&log), Some("t2"), "run-0", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Run ID already used"), "{err}");
    }

    #[tokio::test]
    async fn open_refuses_unreconciled_tool_outcomes() {
        let (log, _storage) = journal().await;
        let state = crate::state::AgentState::builder().session_id("s1").build();
        seed(
            &*log,
            "run-0",
            vec![
                raw(
                    1,
                    "state/checkpoint",
                    Some("run-0"),
                    Some("t1"),
                    &format!("{{\"stateJson\":{}}}", serde_json::to_string(&state.to_json().unwrap()).unwrap()),
                ),
                raw(
                    2,
                    "tool/dispatch",
                    Some("run-0"),
                    Some("t1"),
                    "{\"toolCallId\":\"call-1\"}",
                ),
            ],
        )
        .await;
        let err = SessionRecorder::open(Arc::clone(&log), Some("t2"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reconciliation required"), "{err}");
    }

    #[tokio::test]
    async fn stats_are_recorded_once_per_message() {
        let (log, _storage) = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let user = Msg::user("hello");
        let state = state_with(user.clone());

        recorder.capture_state(&state, "before_model").unwrap();
        recorder.flush().await.unwrap();
        // A second capture at the same revision is a no-op.
        recorder.capture_state(&state, "before_model").unwrap();
        recorder.flush().await.unwrap();

        let types = event_types(&*log).await;
        assert_eq!(
            types.iter().filter(|t| *t == "message/user").count(),
            1,
            "{types:?}"
        );
        assert_eq!(
            types.iter().filter(|t| *t == "state/checkpoint").count(),
            1,
            "{types:?}"
        );
        recorder.close().await;
    }

    #[tokio::test]
    async fn a_shrinking_history_records_context_replacement() {
        let (log, _storage) = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let first = Msg::user("first");
        let second = Msg::assistant("second");
        let mut state = crate::state::AgentState::builder()
            .session_id("s1")
            .add_message(first.clone())
            .add_message(second)
            .build();
        recorder.capture_state(&state, "grow").unwrap();
        recorder.flush().await.unwrap();

        // Compaction drops the assistant message; the prefix shrinks.
        state.context = vec![first];
        recorder.capture_state(&state, "compaction").unwrap();
        recorder.flush().await.unwrap();

        let head = log.head().await.unwrap();
        let events = log.scan(0, head.seq).await.unwrap();
        let replaced = events
            .iter()
            .find(|e| e.event_type == "context/replaced")
            .expect("context/replaced recorded");
        let data = replaced.data().unwrap();
        assert_eq!(data["reason"], "compaction");
        assert!(data["previousAsOfSeq"].as_u64().unwrap() >= 1);
        recorder.close().await;
    }

    #[tokio::test]
    async fn finish_writes_terminal_facts_and_releases_the_lease() {
        let (log, _storage) = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let state = state_with(Msg::user("hello"));
        recorder.start(&[], &state).await.unwrap();
        recorder.step(0).unwrap();
        recorder.result(Some(&Msg::assistant("done"))).unwrap();
        recorder.finish(&state, "completed").await.unwrap();

        let types = event_types(&*log).await;
        assert!(types.contains(&"step/end".to_string()), "{types:?}");
        assert!(types.contains(&"turn/output".to_string()), "{types:?}");
        assert!(types.contains(&"turn/completed".to_string()), "{types:?}");
        assert_eq!(types.last().map(String::as_str), Some("run/end"));

        // The writer lease is released, so a new run can take it.
        let next = log.acquire("run-next", LEASE).await.unwrap();
        log.release(&next).await.unwrap();
    }

    #[tokio::test]
    async fn interruption_finishes_the_turn_as_interrupted() {
        let (log, _storage) = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let state = state_with(Msg::user("hello"));
        recorder.finish(&state, "interrupted").await.unwrap();
        let types = event_types(&*log).await;
        assert!(types.contains(&"turn/interrupted".to_string()), "{types:?}");
    }

    #[tokio::test]
    async fn a_failed_commit_is_sticky_and_never_reports_success() {
        let storage = Arc::new(FailingStorage::new());
        let log: Arc<dyn SessionLog> =
            Arc::new(JournalSessionLog::new(Arc::clone(&storage) as Arc<dyn AtomicSessionStorage>, "s1"));
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        storage.fail_commits.store(true, Ordering::SeqCst);

        let err = recorder
            .record("run/start", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed to commit"), "{err}");

        // The pending buffer is retained and every later write reports the failure.
        let err = recorder
            .append("turn/start", &serde_json::json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("persistence failed"), "{err}");
        recorder.close().await;
    }

    #[tokio::test]
    async fn closing_twice_is_idempotent() {
        let (log, _storage) = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        recorder.close().await;
        recorder.close().await;
    }

    #[tokio::test]
    async fn a_silent_run_keeps_its_writer_lease() {
        // Regression: the lease used to be renewed only by `flush()`. A run that stayed silent
        // past the lease (one long model stream, one slow tool) lost ownership, so the tool's
        // `action/end` fact failed with "Session writer ownership expired or fenced" — fatal,
        // because the tool had already run and must not be replayed.
        let (log, _storage) = journal().await;
        let lease = Duration::from_millis(200);
        let recorder = SessionRecorder::open_with_lease(
            Arc::clone(&log),
            Some("t1"),
            "run-1",
            None,
            lease,
        )
        .await
        .unwrap();

        // No flush for several lease lifetimes.
        tokio::time::sleep(lease * 3).await;

        // The next acknowledged fact still succeeds because the heartbeat kept the lease alive.
        recorder
            .record("run/start", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(log.head().await.unwrap().owner.as_deref(), Some("run-1"));
        recorder.close().await;
    }

    #[tokio::test]
    async fn a_closed_recorder_stops_renewing_the_lease() {
        // A heartbeat tick that lands after `close()` would reclaim the lease and leave the
        // session owned by a recorder nobody will ever close again.
        let (log, _storage) = journal().await;
        let lease = Duration::from_millis(200);
        let recorder = SessionRecorder::open_with_lease(
            Arc::clone(&log),
            Some("t1"),
            "run-1",
            None,
            lease,
        )
        .await
        .unwrap();
        recorder.close().await;

        tokio::time::sleep(lease * 3).await;

        let head = log.head().await.unwrap();
        assert_eq!(head.owner, None, "released lease must not be reclaimed");
        // Another run can take the session, which is what a closed recorder promises.
        let next = log.acquire("run-2", lease).await.unwrap();
        log.release(&next).await.unwrap();
    }

    #[tokio::test]
    async fn recorder_is_reachable_through_the_runtime_context() {
        let (log, _storage) = journal().await;
        let recorder = Arc::new(SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap());
        let context = crate::agent::RuntimeContext::new()
            .with_extension_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));
        let found = SessionRecorder::from_context(&context).expect("recorder attached");
        assert_eq!(found.turn_id(), "t1");
        assert!(SessionRecorder::from_context(&crate::agent::RuntimeContext::new()).is_none());
        recorder.close().await;
    }

    #[tokio::test]
    async fn observe_records_interaction_requests_and_resolutions() {
        let (log, _storage) = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let call = crate::message::ToolUseBlock::new("call-1", "create_order", Default::default());
        let mut data = crate::event::EventData::default();
        data.fields
            .insert("toolCalls".into(), serde_json::json!([call]));
        recorder
            .observe(&crate::event::AgentEvent::RequireExternalExecution(data))
            .unwrap();
        recorder.flush().await.unwrap();

        let pending = super::super::pending_interactions(&*log).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending.contains_key("call-1"));

        let mut resolved = crate::event::EventData::default();
        resolved.fields.insert(
            "toolResults".into(),
            serde_json::json!([crate::message::ToolResultBlock::new(
                "call-1",
                "create_order",
                vec![]
            )]),
        );
        recorder
            .observe(&crate::event::AgentEvent::ExternalExecutionResult(resolved))
            .unwrap();
        recorder.flush().await.unwrap();
        assert!(super::super::pending_interactions(&*log).await.unwrap().is_empty());
        recorder.close().await;
    }
}
