//! A session's durable task operations and read views, mirroring `AgentSession`.
//!
//! Commands are accepted synchronously into the inbox; this runtime owns execution
//! subscriptions, independently of observers. Obtaining a handle and reading views never
//! starts inference.
//!
//! Scope note: pending `steer`/`inject` input is applied once before the run starts. Rust's
//! `ReActAgent` exposes no "between reasoning steps" hook today, so mid-run delivery is
//! documented as unsupported rather than silently claimed.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::{Extensions, RuntimeContext, StreamOptions};
use agentscope_core::event::AgentEvent;
use agentscope_core::message::{ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock};
use agentscope_core::session::{
    outcome, pending_interactions, session_error, transcript, Command, SessionEvent,
    SessionExecution, SessionInbox, SessionKey, SessionLog, SessionProjection, SessionRecorder,
    SessionTurns, Transcript, EXECUTION_CONTEXT_KEY, RECORDER_CONTEXT_KEY,
};
use agentscope_core::state::AgentState;
use futures::StreamExt;

use super::answer::SessionAnswer;
use crate::agent::HarnessAgent;

/// How often the dispatcher looks for accepted work.
const DISPATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// A command receipt. A queued task may not yet have a run id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub command_id: String,
    pub turn_id: String,
    pub run_id: Option<String>,
    pub status: String,
    pub error: Option<String>,
}

impl Task {
    /// Whether the task can no longer change without another command.
    pub fn settled(&self) -> bool {
        !matches!(self.status.as_str(), "queued" | "running" | "starting")
    }
}

/// The receipt for a `steer`/`inject` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputReceipt {
    pub input_id: String,
    pub turn_id: Option<String>,
}

/// Mutable runtime state of a session handle.
#[derive(Default)]
struct SessionState {
    stop: Option<Arc<AtomicBool>>,
    active_command: Option<String>,
    active_run: Option<String>,
    error: Option<String>,
    closed: bool,
    /// Receives every event of dispatched executions. A host that renders streaming
    /// output (ACP, a CLI) installs one instead of driving `stream_events` itself, so
    /// the journal writer and the task bookkeeping stay in one place.
    sink: Option<Arc<dyn Fn(&AgentEvent) + Send + Sync>>,
}

/// A session's durable task operations and read views.
pub struct AgentSession {
    agent: Arc<HarnessAgent>,
    context: RuntimeContext,
    key: SessionKey,
    log: Arc<dyn SessionLog>,
    inbox: SessionInbox,
    state: Arc<Mutex<SessionState>>,
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("session_id", &self.key.session_id)
            .finish_non_exhaustive()
    }
}

impl AgentSession {
    /// Opens a handle for a logical session. Opening never writes to the journal.
    pub fn open(
        agent: Arc<HarnessAgent>,
        context: RuntimeContext,
        key: SessionKey,
        log: Arc<dyn SessionLog>,
    ) -> crate::Result<Self> {
        let inbox = SessionInbox::new(Arc::clone(&log))?;
        Ok(Self {
            agent,
            context,
            key,
            log,
            inbox,
            state: Arc::new(Mutex::new(SessionState::default())),
        })
    }

    /// The logical session id.
    pub fn session_id(&self) -> &str {
        &self.key.session_id
    }

    /// The native journal behind this session.
    pub fn log(&self) -> Arc<dyn SessionLog> {
        Arc::clone(&self.log)
    }

    /// Complete committed messages as of the current head.
    pub async fn transcript(&self) -> crate::Result<Transcript> {
        transcript(&*self.log).await
    }

    /// Read-only recovery view (uncertain tool calls, active runs, state).
    pub async fn inspect(&self) -> crate::Result<SessionProjection> {
        SessionProjection::read(&*self.log).await
    }

    /// Outstanding interaction requests at the current prefix.
    pub async fn pending(&self) -> crate::Result<BTreeMap<String, SessionEvent>> {
        pending_interactions(&*self.log).await
    }

    /// The last dispatch error, if any.
    pub fn execution_error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    /// Installs the subscriber that receives every event of dispatched executions.
    ///
    /// The runtime keeps rendering independent of task bookkeeping: a host renders from the
    /// sink while [`await_task`](Self::await_task) tracks settlement. The sink is captured
    /// once per execution, so replacing it never splits a single turn between two renderers.
    pub fn set_event_sink(&self, sink: Arc<dyn Fn(&AgentEvent) + Send + Sync>) {
        self.state.lock().unwrap().sink = Some(sink);
    }

    /// The committed head of the journal (sequence, writer lease, commit id).
    pub async fn head(&self) -> crate::Result<agentscope_core::session::Head> {
        self.log.head().await
    }

    /// Submits a new task with a fresh idempotency key.
    pub async fn submit(&self, input: impl Into<String>) -> crate::Result<Task> {
        let key = uuid::Uuid::new_v4().to_string();
        self.submit_with_key(&key, input).await
    }

    /// Submits a new task; retry with the same key and identical input to get the same receipt.
    pub async fn submit_with_key(
        &self,
        idempotency_key: &str,
        input: impl Into<String>,
    ) -> crate::Result<Task> {
        self.ensure_open()?;
        let input = input.into();
        if input.is_empty() {
            return Err(session_error("A new task requires input"));
        }
        if idempotency_key.trim().is_empty() || idempotency_key.len() > 200 {
            return Err(session_error("Invalid idempotency key"));
        }
        let turn_id = turn_id_for(idempotency_key);
        let command = Command::new(
            idempotency_key,
            "submit",
            Some(turn_id),
            freeze(idempotency_key, vec![Msg::user(input)]),
            Vec::new(),
        )?;
        let accepted = self.inbox.accept(&command).await?;
        self.start();
        self.receipt(&accepted).await
    }

    /// Continues the latest unfinished task with a new run.
    pub async fn resume(&self, turn_id: &str) -> crate::Result<Task> {
        self.ensure_open()?;
        let turns = SessionTurns::read(&*self.log).await?;
        let continue_allowed = turns
            .latest()
            .is_some_and(|turn| turn.turn_id == turn_id && !turn.ended());
        if !continue_allowed {
            return Err(session_error(
                "Only the latest unfinished task can continue",
            ));
        }
        if !self.pending().await?.is_empty() {
            return Err(session_error("Answer pending interactions first"));
        }
        if !self.inspect().await?.uncertain_tool_calls.is_empty() {
            return Err(session_error("Reconcile uncertain tool outcomes first"));
        }
        self.require_idle()?;

        let snapshot = self.inbox.snapshot().await?;
        let already_queued = snapshot.commands.iter().any(|existing| {
            existing.kind == "resume"
                && existing.turn_id.as_deref() == Some(turn_id)
                && !turns.started.contains(&existing.id)
                && !snapshot.rejected.contains_key(&existing.id)
        });
        if already_queued {
            return Err(session_error("Continuation already queued"));
        }
        let command = Command::new(
            uuid::Uuid::new_v4().to_string(),
            "resume",
            Some(turn_id.to_string()),
            Vec::new(),
            Vec::new(),
        )?;
        let accepted = self.inbox.accept(&command).await?;
        self.start();
        self.receipt(&accepted).await
    }

    /// Answers one pending interaction.
    pub async fn respond(
        &self,
        request_id: &str,
        answer: SessionAnswer,
    ) -> crate::Result<Task> {
        let mut answers = BTreeMap::new();
        answers.insert(request_id.to_string(), answer);
        self.respond_all(answers).await
    }

    /// Answers several pending interactions that all belong to one turn.
    pub async fn respond_all(
        &self,
        answers: BTreeMap<String, SessionAnswer>,
    ) -> crate::Result<Task> {
        self.ensure_open()?;
        self.require_idle()?;
        if answers.is_empty() {
            return Err(session_error("An answer is required"));
        }
        let requests = self.pending().await?;
        let mut turn: Option<String> = None;
        let mut results: Vec<ToolResultBlock> = Vec::new();
        let mut confirmations: Vec<agentscope_core::event::ConfirmResult> = Vec::new();

        for (id, answer) in &answers {
            let request = requests
                .get(id)
                .ok_or_else(|| session_error(format!("No pending interaction: {id}")))?;
            let request_turn = request.turn_id.clone().ok_or_else(|| {
                session_error(format!("Pending request has no logical turn: {id}"))
            })?;
            if let Some(existing) = &turn {
                if *existing != request_turn {
                    return Err(session_error("Answers belong to different tasks"));
                }
            }
            let data = request.data()?;
            let kind = data
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let call: ToolUseBlock = match data.get("call") {
                Some(value) => serde_json::from_value(value.clone())?,
                None => {
                    return Err(session_error(format!(
                        "Pending interaction has no tool call: {id}"
                    )))
                }
            };
            match kind.as_str() {
                "external_execution" => {
                    let result = answer
                        .to_tool_result(&call.id, &call.name)
                        .ok_or_else(|| {
                            session_error(format!(
                                "Answer type does not match interaction: {id}"
                            ))
                        })?;
                    results.push(result);
                }
                "confirmation" => {
                    let result = answer.to_confirm_result(&call).ok_or_else(|| {
                        session_error(format!("Answer type does not match interaction: {id}"))
                    })?;
                    if result.tool_call.as_ref().is_some_and(|c| c.id != call.id) {
                        return Err(session_error("Confirmation tool ID mismatch"));
                    }
                    confirmations.push(result);
                }
                _ => {
                    return Err(session_error(format!(
                        "Reply through the interaction's owning service: {id}"
                    )))
                }
            }
            turn = Some(request_turn);
        }

        let mut messages: Vec<Msg> = Vec::new();
        if !results.is_empty() {
            let blocks: Vec<ContentBlock> = results
                .into_iter()
                .map(ContentBlock::ToolResult)
                .collect();
            messages.push(
                Msg::builder()
                    .role(MsgRole::Tool)
                    .content(blocks)
                    .build(),
            );
        }
        if !confirmations.is_empty() {
            let mut msg = Msg::builder()
                .role(MsgRole::User)
                .content(vec![ContentBlock::Text(TextBlock::new(String::new()))])
                .build();
            msg.metadata.insert(
                Msg::METADATA_CONFIRM_RESULTS.to_string(),
                serde_json::to_value(&confirmations)?,
            );
            messages.push(msg);
        }

        let sorted: Vec<String> = answers.keys().cloned().collect();
        let joined = sorted.join("\n");
        let command_id = stable_id("answer", &joined);
        let command = Command::new(
            command_id,
            "respond",
            turn,
            freeze("answer", messages),
            sorted,
        )?;
        let accepted = self.inbox.accept(&command).await?;
        self.start();
        self.receipt(&accepted).await
    }

    /// Adds guidance for the running task (or for `expected_turn` only).
    pub async fn steer(
        &self,
        expected_turn: Option<&str>,
        input: impl Into<String>,
    ) -> crate::Result<InputReceipt> {
        self.ensure_open()?;
        let id = uuid::Uuid::new_v4().to_string();
        let snapshot = self.inbox.snapshot().await?;
        let head = self.log.head().await?;
        let running = snapshot
            .run_id
            .as_deref()
            .filter(|run| Some(*run) == head.owner.as_deref() && head.lease_until > now_millis());
        if running.is_none() {
            return Err(session_error(
                "No running task accepts steering; submit or resume explicitly",
            ));
        }
        let turn_id = snapshot.turn_id.clone();
        if let Some(expected) = expected_turn {
            if Some(expected) != turn_id.as_deref() {
                return Err(session_error("Task is no longer running"));
            }
        }
        let command = Command::new(
            &id,
            "steer",
            turn_id.clone(),
            freeze(&id, vec![Msg::user(input)]),
            Vec::new(),
        )?;
        self.inbox.accept(&command).await?;
        Ok(InputReceipt {
            input_id: id,
            turn_id,
        })
    }

    /// Persists reference material without starting execution.
    pub async fn inject(&self, context: impl Into<String>) -> crate::Result<InputReceipt> {
        self.ensure_open()?;
        let id = uuid::Uuid::new_v4().to_string();
        let command = Command::new(
            &id,
            "inject",
            None,
            freeze(&id, vec![Msg::user(context)]),
            Vec::new(),
        )?;
        self.inbox.accept(&command).await?;
        Ok(InputReceipt {
            input_id: id,
            turn_id: None,
        })
    }

    /// Requests interruption of the current execution.
    ///
    /// A cooperative request, not a completion acknowledgement: the task keeps reporting
    /// `running` until the execution actually stops.
    pub async fn interrupt(&self) -> crate::Result<bool> {
        self.ensure_open()?;
        let snapshot = self.inbox.snapshot().await?;
        let Some(run_id) = snapshot.run_id.clone() else {
            return Ok(false);
        };
        let command = Command::new(
            uuid::Uuid::new_v4().to_string(),
            "interrupt",
            snapshot.turn_id.clone(),
            Vec::new(),
            vec![run_id],
        )?;
        // Signal before recording. Accepting input needs the journal writer lease, which the
        // running execution holds until it stops: recording first would make `interrupt` wait
        // for the run it is trying to stop (up to the lease timeout).
        self.agent.interrupt();
        // Record the command off the caller's path. The interrupt is already delivered, so a
        // busy journal must not turn it into an error, and the host must not block on it.
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            let _ = inbox.accept(&command).await;
        });
        Ok(true)
    }

    /// Starts dispatch; `submit`/`respond`/`resume` already call this.
    pub fn start(&self) {
        let mut guard = self.state.lock().unwrap();
        if guard.closed || guard.stop.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        guard.stop = Some(Arc::clone(&stop));
        drop(guard);

        let agent = Arc::clone(&self.agent);
        let log = Arc::clone(&self.log);
        let inbox = self.inbox.clone();
        let context = self.context.clone();
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                if let Err(error) = dispatch_once(&agent, &log, &inbox, &context, &state).await {
                    state.lock().unwrap().error = Some(error.to_string());
                }
                tokio::time::sleep(DISPATCH_INTERVAL).await;
            }
        });
    }

    /// Every accepted execution task, keyed by turn.
    pub async fn tasks(&self) -> crate::Result<Vec<Task>> {
        let snapshot = self.inbox.snapshot().await?;
        let mut seen: BTreeMap<String, Task> = BTreeMap::new();
        for command in &snapshot.commands {
            if command.execution() {
                let task = self.receipt(command).await?;
                seen.insert(task.turn_id.clone(), task);
            }
        }
        Ok(seen.into_values().collect())
    }

    /// The current receipt of one accepted command.
    pub async fn task(&self, command_id: &str) -> crate::Result<Task> {
        let snapshot = self.inbox.snapshot().await?;
        let command = snapshot
            .command(command_id)
            .filter(|command| command.execution())
            .ok_or_else(|| session_error("Unknown task submission"))?;
        self.receipt(command).await
    }

    /// Polls one task until it settles. Observing never starts or cancels work.
    pub async fn await_task(&self, command_id: &str) -> crate::Result<Task> {
        loop {
            let task = self.task(command_id).await?;
            if task.settled() {
                return Ok(task);
            }
            tokio::time::sleep(DISPATCH_INTERVAL).await;
        }
    }

    fn ensure_open(&self) -> crate::Result<()> {
        if self.state.lock().unwrap().closed {
            return Err(session_error("Session runtime is closed"));
        }
        Ok(())
    }

    fn require_idle(&self) -> crate::Result<()> {
        if self.state.lock().unwrap().active_run.is_some() {
            return Err(session_error("Wait for the current execution to stop"));
        }
        Ok(())
    }

    async fn receipt(&self, command: &Command) -> crate::Result<Task> {
        let snapshot = self.inbox.snapshot().await?;
        if let Some(reason) = snapshot.rejected.get(&command.id) {
            return Ok(Task {
                command_id: command.id.clone(),
                turn_id: command.turn_id.clone().unwrap_or_default(),
                run_id: None,
                status: "rejected".into(),
                error: Some(reason.clone()),
            });
        }
        let head = self.log.head().await?;
        let mut run: Option<String> = None;
        let mut status = "queued".to_string();
        for event in self.log.scan(0, head.seq).await? {
            let records_start = event.event_type == "inbox/started"
                && event
                    .data()
                    .ok()
                    .and_then(|d| {
                        d.get("commandId")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .as_deref()
                    == Some(command.id.as_str());
            if records_start {
                run = event.execution_run_id.clone();
                status = "running".into();
            }
            if let Some(run_id) = &run {
                if event.execution_run_id.as_deref() == Some(run_id.as_str())
                    && event.event_type == "run/end"
                {
                    status = event
                        .data()
                        .ok()
                        .and_then(|d| {
                            d.get("status")
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "completed".into());
                }
            }
        }
        {
            let guard = self.state.lock().unwrap();
            if guard.active_command.as_deref() == Some(command.id.as_str()) {
                if let Some(active) = &guard.active_run {
                    run = Some(active.clone());
                }
                if status == "queued" {
                    status = "starting".into();
                }
            }
        }
        // A `running` receipt whose writer lease is gone was interrupted, not finished.
        if status == "running" {
            let owns = run.as_deref() == head.owner.as_deref() && head.lease_until > now_millis();
            if !owns {
                status = "interrupted".into();
            }
        }
        Ok(Task {
            command_id: command.id.clone(),
            turn_id: command.turn_id.clone().unwrap_or_default(),
            run_id: run,
            status,
            error: None,
        })
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        // Stopping dispatch is the only cleanup a dropped handle owns; the journal keeps the
        // accepted work so another runtime can resume it later.
        let mut guard = self.state.lock().unwrap();
        guard.closed = true;
        if let Some(stop) = guard.stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

/// One dispatch tick: consume interrupts, then launch the next accepted task.
async fn dispatch_once(
    agent: &Arc<HarnessAgent>,
    log: &Arc<dyn SessionLog>,
    inbox: &SessionInbox,
    context: &RuntimeContext,
    state: &Arc<Mutex<SessionState>>,
) -> crate::Result<()> {
    let active = state.lock().unwrap().active_run.is_some();
    if active {
        // Mark consumed interrupt commands so they are not replayed.
        let snapshot = inbox.snapshot().await?;
        for command in &snapshot.commands {
            if command.kind == "interrupt" && !snapshot.handled.contains(&command.id) {
                inbox.handled(command).await?;
            }
        }
        return Ok(());
    }

    // Never launch while another writer owns the journal.
    let head = log.head().await?;
    if head.owner.is_some() && head.lease_until > now_millis() {
        return Ok(());
    }

    let turns = SessionTurns::read(&**log).await?;
    let snapshot = inbox.snapshot().await?;
    let latest = turns.latest().cloned();
    for command in &snapshot.commands {
        if !command.execution()
            || turns.started.contains(&command.id)
            || snapshot.rejected.contains_key(&command.id)
        {
            continue;
        }
        // A new submit waits until the unfinished turn is continued.
        if latest.as_ref().is_some_and(|turn| !turn.ended()) && command.kind == "submit" {
            continue;
        }
        launch(agent, log, inbox, context, state, command.clone()).await?;
        return Ok(());
    }
    Ok(())
}

/// Opens a recorder, admits the command and runs the agent to completion.
async fn launch(
    agent: &Arc<HarnessAgent>,
    log: &Arc<dyn SessionLog>,
    inbox: &SessionInbox,
    context: &RuntimeContext,
    state: &Arc<Mutex<SessionState>>,
    command: Command,
) -> crate::Result<()> {
    let turn_id = command
        .turn_id
        .clone()
        .ok_or_else(|| session_error("Execution command requires a turn id"))?;
    let recorder = match SessionRecorder::open(Arc::clone(log), Some(&turn_id)).await {
        Ok(recorder) => Arc::new(recorder),
        Err(error) => {
            inbox.reject(&command, &error.to_string()).await?;
            return Err(error);
        }
    };
    let mut execution = SessionExecution::new(Arc::clone(log), command.clone());

    let mut working = match SessionProjection::read(&**log).await?.restore()? {
        Some(state) => state,
        None => AgentState::builder()
            .session_id(&turn_id)
            .user_id(context.user_id().unwrap_or_default())
            .build(),
    };
    if let Err(error) = execution.prepare(&recorder).await {
        inbox.reject(&command, &error.to_string()).await?;
        recorder.close().await;
        return Err(error);
    }

    // Guidance accepted before this run starts is folded in by the agent loop's step boundary
    // (Java `reasoning` is the only caller of `beforeStep`), so the launch path must not apply
    // it a second time.
    let mut inputs = execution.inputs().to_vec();
    if inputs.is_empty() {
        inputs = working.context.clone();
    }
    // The admitted input must be part of the working state before the first checkpoint:
    // `input/received` is journal metadata, while `message/user` facts come from the state
    // snapshot (Java `SessionRecorder.captureState` does the same).
    for message in &inputs {
        if !working.context.iter().any(|existing| existing.id == message.id) {
            working.context.push(message.clone());
        }
    }
    if let Err(error) = recorder.start(&inputs, &working).await {
        inbox.reject(&command, &error.to_string()).await?;
        recorder.close().await;
        return Err(error);
    }
    if let Err(error) = recorder.checkpoint_now(&working, "session_input").await {
        inbox.reject(&command, &error.to_string()).await?;
        recorder.close().await;
        return Err(error);
    }

    {
        let mut guard = state.lock().unwrap();
        guard.active_command = Some(command.id.clone());
        guard.active_run = Some(recorder.run_id().to_string());
        guard.error = None;
    }

    let mut extensions = Extensions::new();
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));
    extensions.insert(EXECUTION_CONTEXT_KEY, execution.clone());

    // Capture the subscriber once per execution so a host that replaces it mid-run gets a
    // clean cut instead of a half-rendered turn split across two renderers.
    let sink = state.lock().unwrap().sink.clone();

    let react = agent.agent().cloned();
    let result: crate::Result<Option<Msg>> = match react {
        Some(react) => {
            let mut stream = react.stream_events_with_extensions(
                inputs,
                StreamOptions::defaults(),
                extensions,
            );
            let mut reply = None;
            let mut failure = None;
            while let Some(event) = stream.next().await {
                // Forward before interpreting: the host renders the same events the runtime
                // uses for bookkeeping, including the terminal `AgentResult`.
                if let (Ok(observed), Some(sink)) = (&event, &sink) {
                    sink(observed);
                }
                match event {
                    Ok(AgentEvent::AgentResult(data)) => {
                        if let Some(value) = data.fields.get("reply") {
                            reply = serde_json::from_value(value.clone()).ok();
                        }
                    }
                    Ok(_) => {}
                    Err(error) => failure = Some(error),
                }
            }
            match failure {
                Some(error) => Err(error),
                None => Ok(reply),
            }
        }
        None => Err(agentscope_core::Error::Agent(
            "HarnessAgent has no model configured; cannot run".into(),
        )),
    };

    let status = match &result {
        Ok(Some(message)) => outcome(message.generate_reason()),
        Ok(None) => "completed",
        Err(agentscope_core::Error::Interrupted(_)) => "interrupted",
        Err(_) => "failed",
    };
    if let Ok(Some(message)) = &result {
        let _ = recorder.result(Some(message));
    }
    let finish = recorder.finish(&working, status).await;
    let _ = execution.close(&recorder).await;

    {
        let mut guard = state.lock().unwrap();
        guard.active_command = None;
        guard.active_run = None;
        if let Err(error) = &result {
            guard.error = Some(error.to_string());
        }
    }

    // A run that never started must not leave a phantom `queued` receipt forever.
    if let Err(error) = &result {
        let started = SessionTurns::read(&**log).await?.started.contains(&command.id);
        if !started {
            inbox.reject(&command, &error.to_string()).await?;
        }
    }
    finish?;
    Ok(())
}

/// A deterministic id derived from `seed`.
///
/// Determinism matters: a retried submission must produce the same turn id (and therefore the
/// same idempotency receipt) without depending on a random generator. `uuid` only enables the
/// `v4` feature here, so the digest is built with the `sha2` dependency already in use.
fn stable_id(prefix: &str, seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("{prefix}-{}", &hex[..32])
}

/// Deterministic turn id for an idempotency key.
fn turn_id_for(idempotency_key: &str) -> String {
    stable_id("turn", &format!("turn:{idempotency_key}"))
}

/// Freezes input into durable messages with stable ids.
fn freeze(command_id: &str, input: Vec<Msg>) -> Vec<Msg> {
    input
        .into_iter()
        .enumerate()
        .map(|(index, mut message)| {
            message.id = format!("input-{command_id}-{index}");
            message
        })
        .collect()
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
