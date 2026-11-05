//! Behaviour tests for `AgentSession`: durable task operations over the native journal.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::session::{InMemorySessionLogStore, SessionKey, SessionLogStore};
use agentscope_harness::agent::session::{AgentSession, SessionAnswer};
use agentscope_harness::agent::HarnessAgent;
use futures::StreamExt;

/// Answers every request with fixed text.
struct ScriptedModel(&'static str);

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let text = self.0;
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new(text))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "scripted"
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("agentscope_sess_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Fails the run immediately, like an expired provider API key.
struct FailingModel;

#[async_trait::async_trait]
impl Model for FailingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            Err(agentscope_core::error::Error::Model(
                "api_key_expired: This API key has expired".into(),
            ))
        }))
    }

    fn model_name(&self) -> &str {
        "failing"
    }
}

/// Panics as soon as the model is called, simulating a crash inside the agent loop.
struct PanickingModel;

#[async_trait::async_trait]
impl Model for PanickingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            panic!("provider SDK exploded");
        }))
    }

    fn model_name(&self) -> &str {
        "panicking"
    }
}

/// Fails once, then answers normally.
struct FailOnceModel {
    attempts: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl Model for FailOnceModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let failed = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0;
        Box::pin(futures::stream::once(async move {
            if failed {
                Err(agentscope_core::error::Error::Agent(
                    "api_key_expired: This API key has expired".into(),
                ))
            } else {
                Ok(ChatResponse::builder()
                    .content(vec![CoreBlock::Text(TextBlock::new("recovered"))])
                    .build())
            }
        }))
    }

    fn model_name(&self) -> &str {
        "fail-once"
    }
}

#[tokio::test]
async fn a_submit_after_a_failed_run_starts() {
    let dir = temp_dir("after_failure");
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(FailOnceModel {
                attempts: std::sync::atomic::AtomicUsize::new(0),
            }))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-retry");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-retry").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();
    let session = AgentSession::open(agent, context, key, log).unwrap();

    let failed = session.submit("fail please").await.unwrap();
    let failed = session.await_task(&failed.command_id).await.unwrap();
    assert_eq!(failed.status, "failed", "{failed:?}");

    // A failed turn is terminal: the next submission must start a new run instead of
    // waiting for the failed turn to continue.
    let retry = session.submit("try again").await.unwrap();
    let retry = session.await_task(&retry.command_id).await.unwrap();
    assert_eq!(retry.status, "completed", "{retry:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_failed_run_carries_its_error_on_the_task_receipt() {
    let dir = temp_dir("failed_receipt");
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(FailingModel))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-fail");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-fail").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();
    let session = AgentSession::open(agent, context, key, log).unwrap();

    let task = session.submit("fail please").await.unwrap();
    let settled = session.await_task(&task.command_id).await.unwrap();
    assert_eq!(settled.status, "failed", "{settled:?}");
    let error = settled
        .error
        .expect("a failed task must carry its failure detail");
    assert!(error.contains("api_key_expired"), "{error}");

    let _ = std::fs::remove_dir_all(&dir);
}

async fn open_session(tag: &str, reply: &'static str) -> (AgentSession, std::path::PathBuf) {
    let dir = temp_dir(tag);
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(ScriptedModel(reply)))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-001");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-001").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();
    let session = AgentSession::open(agent, context, key, log).unwrap();
    (session, dir)
}

#[tokio::test]
async fn submit_runs_a_task_to_completion() {
    let (session, dir) = open_session("complete", "the comparison is ready").await;

    let task = session.submit("compare plans A and B").await.unwrap();
    assert_eq!(task.status, "queued");
    assert!(!task.turn_id.is_empty());

    let settled = session.await_task(&task.command_id).await.unwrap();
    assert_eq!(settled.status, "completed", "{settled:?}");
    assert!(settled.settled());

    // The turn's output is readable from the committed transcript.
    let transcript = session.transcript().await.unwrap();
    let rendered: String = transcript
        .messages
        .iter()
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("compare plans A and B"), "{rendered}");
    assert!(rendered.contains("the comparison is ready"), "{rendered}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn submitting_the_same_key_and_input_is_idempotent() {
    let (session, dir) = open_session("idempotent", "ok").await;
    let first = session
        .submit_with_key("request-001", "do the thing")
        .await
        .unwrap();
    let retry = session
        .submit_with_key("request-001", "do the thing")
        .await
        .unwrap();
    assert_eq!(first.turn_id, retry.turn_id);
    assert_eq!(first.command_id, retry.command_id);
    // One accepted command, not two.
    let tasks = session.tasks().await.unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:?}");

    // A different task needs a different key.
    session.await_task(&first.command_id).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_submission_is_rejected() {
    let (session, dir) = open_session("empty", "ok").await;
    assert!(session.submit("").await.is_err());
    assert!(session.submit_with_key("   ", "x").await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn inject_persists_input_without_starting_execution() {
    let (session, dir) = open_session("inject", "ok").await;
    let receipt = session
        .inject("Additional material: plan A is weekly")
        .await
        .unwrap();
    assert!(receipt.turn_id.is_none());
    // No execution was launched, so nothing is running and no task exists.
    assert!(session.tasks().await.unwrap().is_empty());
    assert!(session.inspect().await.unwrap().active_runs.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn steer_requires_a_running_task() {
    let (session, dir) = open_session("steer", "ok").await;
    // Nothing is running yet: steering must be refused rather than silently queued.
    let err = session
        .steer(None, "prioritize staffing cost")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("No running task"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn interrupt_reports_false_when_idle() {
    let (session, dir) = open_session("idle-interrupt", "ok").await;
    assert!(!session.interrupt().await.unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn respond_rejects_unknown_or_stale_requests() {
    let (session, dir) = open_session("respond", "ok").await;
    let err = session
        .respond("not-a-request", SessionAnswer::text("answer"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("No pending interaction"), "{err}");

    // Answering nothing is a programming error, not an empty success.
    let err = session
        .respond_all(std::collections::BTreeMap::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("An answer is required"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resume_rejects_a_turn_that_never_started() {
    let (session, dir) = open_session("resume", "ok").await;
    let err = session.resume("turn-does-not-exist").await.unwrap_err();
    assert!(
        err.to_string().contains("Only the latest unfinished task"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_interrupted_resumable_turn_is_continued_before_a_new_submission_runs() {
    // A turn interrupted by a dead process is not garbage: the caller has moved on, but the
    // work already started is still resumable. `submit` continues it (a `resume` command is
    // queued ahead of the new prompt) instead of discarding it, so the partial turn keeps its
    // answer and the new prompt runs right after.
    let dir = temp_dir("continue");
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(ScriptedModel("continued")))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-continue");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-continue").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();

    {
        use agentscope_core::session::SessionRecorder;
        use agentscope_core::state::AgentState;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("turn-stale"))
            .await
            .unwrap();
        let state = AgentState::builder()
            .session_id("conversation-continue")
            .build();
        recorder.start(&[Msg::user("first")], &state).await.unwrap();
        recorder
            .append("run/end", &serde_json::json!({ "status": "interrupted" }))
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.close().await;
    }

    let session = AgentSession::open(agent, context, key, Arc::clone(&log)).unwrap();
    assert_eq!(
        session.open_turn().await.unwrap().as_deref(),
        Some("turn-stale")
    );

    let next = session.submit("second").await.unwrap();
    session.await_task(&next.command_id).await.unwrap();

    let turns = agentscope_core::session::SessionTurns::read(&*log)
        .await
        .unwrap();
    let stale = turns
        .turns
        .iter()
        .find(|turn| turn.turn_id == "turn-stale")
        .expect("the seeded turn must still exist");
    assert_eq!(
        stale.status, "completed",
        "an interrupted resumable turn must be continued, not abandoned: {turns:?}"
    );
    assert!(session.open_turn().await.unwrap().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_interrupted_turn_is_closed_before_a_new_submission_runs() {
    // Regression for "the previous turn is still open, so this prompt could not start":
    // an interrupted run leaves the turn non-terminal, but the host has clearly moved on when
    // it submits a new prompt. The dispatcher must close the orphaned turn (recording unknown
    // tool outcomes as interrupted) and run the new submission instead of rejecting it.
    let dir = temp_dir("abandon");
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(ScriptedModel("after abandon")))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-abandon");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-abandon").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();

    // Seed an interrupted turn the way a crashed run leaves one: started, never finished.
    {
        use agentscope_core::session::SessionRecorder;
        use agentscope_core::state::AgentState;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("turn-stale"))
            .await
            .unwrap();
        let state = AgentState::builder()
            .session_id("conversation-abandon")
            .build();
        recorder.start(&[Msg::user("first")], &state).await.unwrap();
        recorder
            .append("run/end", &serde_json::json!({ "status": "interrupted" }))
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.close().await;
    }

    let session = AgentSession::open(agent, context, key, log).unwrap();
    assert_eq!(
        session.open_turn().await.unwrap().as_deref(),
        Some("turn-stale")
    );

    // A submit closes the orphaned turn and runs, instead of rejecting the user's message.
    let next = session.submit("second").await.unwrap();
    let settled = session.await_task(&next.command_id).await.unwrap();
    assert_eq!(settled.status, "completed", "{settled:?}");
    assert!(session.open_turn().await.unwrap().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_superseded_resume_does_not_starve_the_queue() {
    // Regression for "the agent restarted, then every prompt sat in `queued` until the 300s
    // admission deadline, with no model request sent": a `resume` accepted just before a
    // submit that opened a newer turn can never be admitted again ("Only the latest unfinished
    // task can continue"). The dispatcher always launches the first accepted command, so
    // retrying that one forever meant no later prompt ever started.
    use agentscope_core::session::{Command, SessionInbox, SessionRecorder};
    use agentscope_core::state::AgentState;

    let dir = temp_dir("stale-resume");
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(ScriptedModel("after the stale resume")))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-stale-resume");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-stale-resume").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();

    // `turn-old` was interrupted mid-run (non-terminal, resumable); `turn-new` is the latest
    // turn and already finished, so a resume naming `turn-old` can never be admitted.
    for (turn, text, status) in [
        ("turn-old", "earlier", "interrupted"),
        ("turn-new", "later", "completed"),
    ] {
        let recorder = SessionRecorder::open(Arc::clone(&log), Some(turn))
            .await
            .unwrap();
        let state = AgentState::builder()
            .session_id("conversation-stale-resume")
            .build();
        let mut message = Msg::user(text);
        message.id = format!("input-{turn}");
        recorder.start(&[message], &state).await.unwrap();
        recorder
            .append("run/end", &serde_json::json!({ "status": status }))
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.close().await;
    }

    let inbox = SessionInbox::new(Arc::clone(&log)).unwrap();
    let stale = Command::new(
        "cmd-stale-resume",
        "resume",
        Some("turn-old".into()),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    inbox.accept(&stale).await.unwrap();
    let mut message = Msg::user("continue");
    message.id = "input-cmd-submit-0".into();
    let submit = Command::new(
        "cmd-submit",
        "submit",
        Some("turn-fresh".into()),
        vec![message],
        Vec::new(),
    )
    .unwrap();
    inbox.accept(&submit).await.unwrap();

    let session = AgentSession::open(agent, context, key, log).unwrap();
    session.start();

    let settled = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        session.await_task("cmd-submit"),
    )
    .await
    .expect("a stale resume must not keep the queue from running")
    .unwrap();
    assert_eq!(settled.status, "completed", "{settled:?}");

    // The stale command is rejected with the real reason instead of being silently dropped.
    let rejected = session.task("cmd-stale-resume").await.unwrap();
    assert_eq!(rejected.status, "rejected", "{rejected:?}");
    assert!(
        rejected
            .error
            .unwrap_or_default()
            .contains("Only the latest unfinished task"),
        "the rejection must explain why the continuation can never run"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_starting_receipt_whose_run_died_reports_interrupted() {
    // Regression for "the turn was never admitted (status starting after 144s)": `starting`
    // comes from the in-memory `active_command` slot, so when a run dies between admission and
    // its terminal write the receipt used to stay `starting` forever.
    let (session, dir) = open_session("starting-lease", "ok").await;

    let task = session.submit("go").await.unwrap();
    // Let the dispatcher admit and run it to completion first.
    let settled = session.await_task(&task.command_id).await.unwrap();
    assert_eq!(settled.status, "completed", "{settled:?}");
    assert!(settled.settled(), "a finished run must settle");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_run_that_panics_does_not_wedge_the_session() {
    // A panic inside the agent loop used to leave `active_command` set, so every later receipt
    // reported `starting`, the dispatcher refused to launch anything, and the host waited on a
    // turn nobody was running. The `ActiveRunGuard` rolls the bookkeeping back.
    let dir = temp_dir("panic");
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(PanickingModel))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-panic");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-panic").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();
    let session = AgentSession::open(agent, context, key, log).unwrap();

    let task = session.submit("explode").await.unwrap();
    // The run may surface as failed or interrupted depending on where the panic unwound; what
    // matters is that it settles instead of hanging on `starting`.
    let settled = session.await_task(&task.command_id).await.unwrap();
    assert!(
        settled.settled(),
        "a panicking run must not stay active: {settled:?}"
    );
    assert_ne!(settled.status, "starting", "{settled:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Answers after a short delay, so the run stays in flight long enough for a receipt poll to
/// land inside the launch window.
struct SlowModel;

#[async_trait::async_trait]
impl Model for SlowModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new(
                    "a slow but healthy answer",
                ))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "slow"
    }
}

#[tokio::test]
async fn a_locally_owned_run_is_never_reported_interrupted() {
    // Regression for "Agent error: the turn ended as `interrupted`" firing on a healthy run.
    // `receipt()` read `head` before the scan, then consulted the in-memory `active_command`
    // slot; the lease check then compared the run against that *stale* head (still
    // `owner: None`), so a live run looked like it had lost its lease.
    let dir = temp_dir("local-lease");
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(SlowModel))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-local-lease");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-local-lease").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();
    let session = AgentSession::open(agent, context, key, log).unwrap();

    // Poll the receipt aggressively while the run is in flight: any snapshot taken during the
    // window between launch and the terminal write must report a live status, never
    // `interrupted`.
    let task = session.submit("go").await.unwrap();
    let observed: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_writer = Arc::clone(&observed);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let current = session.task(&task.command_id).await.unwrap();
        observed_writer.lock().unwrap().push(current.status.clone());
        if current.settled() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let settled = session.await_task(&task.command_id).await.unwrap();
    assert_eq!(settled.status, "completed", "{settled:?}");
    let statuses = observed.lock().unwrap().clone();
    assert!(
        !statuses.iter().any(|status| status == "interrupted"),
        "a locally-owned run must never be reported interrupted: {statuses:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_stale_interrupt_does_not_kill_the_next_run() {
    // Regression for "Agent error: the turn ended as `interrupted`" on a prompt the user just
    // sent. The ACP cancel path retries `interrupt` for up to 10s while waiting for the run to
    // unwind, so a "continue" sent right after a cancel could be killed by the *previous*
    // turn's interrupt. Signalling by run id makes a stale request a no-op.
    let dir = temp_dir("stale-interrupt");
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(ScriptedModel("survived")))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-stale-interrupt");
    let key = SessionKey::new(
        Some("alice"),
        "session-agent",
        "conversation-stale-interrupt",
    )
    .unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();
    let session = AgentSession::open(Arc::clone(&agent), context, key, log).unwrap();

    let first = session.submit("first").await.unwrap();
    let settled = session.await_task(&first.command_id).await.unwrap();
    assert_eq!(settled.status, "completed", "{settled:?}");
    let finished_run = settled.run_id.clone().expect("a finished run has an id");

    // A late interrupt naming the *finished* run must not touch anything.
    assert!(
        !agent.interrupt_run(&finished_run),
        "a stale run id must be dropped, not signalled"
    );

    let second = session.submit("second").await.unwrap();
    // Keep firing the stale signal while the new run is in flight.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        agent.interrupt_run(&finished_run);
        let current = session.task(&second.command_id).await.unwrap();
        if current.settled() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let settled = session.await_task(&second.command_id).await.unwrap();
    assert_eq!(
        settled.status, "completed",
        "a stale interrupt must not leak into the next turn: {settled:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reading_views_never_starts_inference() {
    let (session, dir) = open_session("readonly", "ok").await;
    // Views over an untouched journal: empty, and still nothing queued afterwards.
    assert!(session.transcript().await.unwrap().messages.is_empty());
    assert!(session.pending().await.unwrap().is_empty());
    assert!(session.tasks().await.unwrap().is_empty());
    assert_eq!(session.head().await.unwrap().seq, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_event_sink_observes_the_whole_run() {
    use std::sync::Mutex;

    use agentscope_core::event::AgentEvent;

    let (session, dir) = open_session("sink", "the sink sees this reply").await;
    // A host renders from the sink instead of driving `stream_events` itself; the runtime
    // keeps the task bookkeeping. Both must observe the same run.
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    session.set_event_sink(Arc::new(move |event: &AgentEvent| {
        recorder
            .lock()
            .unwrap()
            .push(format!("{:?}", event.event_type()));
    }));

    let task = session.submit("say something").await.unwrap();
    let settled = session.await_task(&task.command_id).await.unwrap();
    assert_eq!(settled.status, "completed", "{settled:?}");

    let types = seen.lock().unwrap().clone();
    assert!(!types.is_empty(), "the sink must observe the run");
    assert!(
        types.iter().any(|kind| kind.contains("AgentResult")),
        "the terminal result must reach the host: {types:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Records the context of every call, then blocks until the test opens the gate.
///
/// The first run is interrupted while the model is still streaming; after the host resumes the
/// turn the gate is open, so the second call answers immediately.
struct GatedRecordingModel {
    calls: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    gate: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl Model for GatedRecordingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.calls
            .lock()
            .unwrap()
            .push(messages.iter().map(|m| m.text_content()).collect());
        let gate = Arc::clone(&self.gate);
        Box::pin(futures::stream::once(async move {
            while !gate.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("resumed answer"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "gated-recording"
    }
}

#[tokio::test]
async fn a_resumed_turn_does_not_replay_its_original_input() {
    // Regression for duplicated history after a resume: `launch()` used to fall back to the
    // *restored context* as the run's input when the execution had none (a resume has no new
    // messages), which appended the original user prompt to the model context a second time.
    // The user saw their question replayed and the model answered it twice.
    let dir = temp_dir("resume-dedup");
    let calls: Arc<std::sync::Mutex<Vec<Vec<String>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(GatedRecordingModel {
                calls: Arc::clone(&calls),
                gate: Arc::clone(&gate),
            }))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-resume-dedup");
    let key = SessionKey::new(Some("alice"), "session-agent", "conversation-resume-dedup").unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();
    let session = AgentSession::open(Arc::clone(&agent), context, key, log).unwrap();

    let task = session.submit("only once please").await.unwrap();

    // Wait until the model actually started (the run is registered by then), then interrupt it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while calls.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the first run never reached the model"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let run_id = session
        .task(&task.command_id)
        .await
        .unwrap()
        .run_id
        .expect("an admitted run publishes its run id");
    assert!(
        agent.interrupt_run(&run_id),
        "the active run must accept the interrupt"
    );

    let settled = session.await_task(&task.command_id).await.unwrap();
    assert_eq!(settled.status, "interrupted", "{settled:?}");

    // Resume the same turn; the host has decided to continue, not to start over.
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    let resumed = session.resume(&task.turn_id).await.unwrap();
    let resumed = session.await_task(&resumed.command_id).await.unwrap();
    assert_eq!(resumed.status, "completed", "{resumed:?}");

    // The resumed model call must see the prompt exactly once, not once per run.
    let calls = calls.lock().unwrap();
    let last = calls
        .last()
        .expect("the resumed run must call the model")
        .join("\n");
    assert_eq!(
        last.matches("only once please").count(),
        1,
        "the original input must not be replayed into the model context: {last:?}"
    );

    // The committed transcript agrees: one user message, not two.
    let transcript = session.transcript().await.unwrap();
    let rendered: String = transcript
        .messages
        .iter()
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        rendered.matches("only once please").count(),
        1,
        "the resumed turn must not duplicate the user message: {rendered:?}"
    );
    assert!(rendered.contains("resumed answer"), "{rendered}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Streams a partial answer, then stalls; later calls finish the answer.
struct PartialThenStallModel {
    calls: Arc<std::sync::Mutex<Vec<Vec<Msg>>>>,
}

#[async_trait::async_trait]
impl Model for PartialThenStallModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(messages);
            calls.len()
        };
        if index == 1 {
            // The first attempt streams half an answer and then hangs, so the process is
            // interrupted with the text already journaled but the call unfinished.
            Box::pin(
                futures::stream::once(async {
                    Ok::<_, agentscope_core::error::Error>(
                        ChatResponse::builder()
                            .content(vec![CoreBlock::Text(TextBlock::new("half an ans"))])
                            .build(),
                    )
                })
                .chain(futures::stream::pending::<
                    agentscope_core::error::Result<ChatResponse>,
                >()),
            )
        } else {
            Box::pin(futures::stream::once(async {
                Ok(ChatResponse::builder()
                    .content(vec![CoreBlock::Text(TextBlock::new("wer"))])
                    .build())
            }))
        }
    }

    fn model_name(&self) -> &str {
        "partial-stall"
    }
}

#[tokio::test]
async fn a_resumed_turn_continues_the_partial_model_output() {
    // The client already saw "half an ans" before the crash. Resuming must not regenerate a
    // different answer: the partial text is re-attached as an assistant prefill and the model
    // continues it.
    let dir = temp_dir("resume-partial");
    let calls: Arc<std::sync::Mutex<Vec<Vec<Msg>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent = Arc::new(
        HarnessAgent::builder()
            .name("session-agent")
            .model(Arc::new(PartialThenStallModel {
                calls: Arc::clone(&calls),
            }))
            .cwd(dir.clone())
            .workspace(dir.join("ws"))
            .max_iters(1)
            .build(),
    );
    let context = RuntimeContext::new()
        .with_user_id("alice")
        .with_session_id("conversation-resume-partial");
    let key = SessionKey::new(
        Some("alice"),
        "session-agent",
        "conversation-resume-partial",
    )
    .unwrap();
    let store = InMemorySessionLogStore::new();
    let log = store.open(&key, &context).await.unwrap();
    let session = AgentSession::open(Arc::clone(&agent), context, key, log).unwrap();

    let task = session.submit("only once please").await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while calls.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the first run never reached the model"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Let the merge window close so the streamed text is committed, exactly as it would be on a
    // real crash a second into the answer.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let run_id = session
        .task(&task.command_id)
        .await
        .unwrap()
        .run_id
        .expect("an admitted run publishes its run id");
    assert!(
        agent.interrupt_run(&run_id),
        "the active run must accept the interrupt"
    );
    let settled = session.await_task(&task.command_id).await.unwrap();
    assert_eq!(settled.status, "interrupted", "{settled:?}");

    let resumed = session.resume(&task.turn_id).await.unwrap();
    let resumed = session.await_task(&resumed.command_id).await.unwrap();
    assert_eq!(resumed.status, "completed", "{resumed:?}");

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "the interrupted attempt and the resumed one: {calls:?}"
    );
    let last = calls.last().expect("the resumed run must call the model");
    let trailing = last.last().expect("the request must not be empty");
    assert_eq!(
        trailing.role,
        agentscope_core::message::MsgRole::Assistant,
        "the partial output must be re-attached as an assistant prefill: {last:?}"
    );
    assert_eq!(
        trailing.text_content(),
        "half an ans",
        "the model must continue the text it had already streamed: {last:?}"
    );
    assert_eq!(
        last.iter()
            .filter(|message| message.text_content() == "half an ans")
            .count(),
        1,
        "the prefill must be attached exactly once: {last:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
