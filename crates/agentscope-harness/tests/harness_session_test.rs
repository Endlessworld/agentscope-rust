//! Behaviour tests for `AgentSession`: durable task operations over the native journal.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::session::{InMemorySessionLogStore, SessionKey, SessionLogStore};
use agentscope_harness::agent::session::{AgentSession, SessionAnswer};
use agentscope_harness::agent::HarnessAgent;

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
    let receipt = session.inject("Additional material: plan A is weekly").await.unwrap();
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
    let err = session.steer(None, "prioritize staffing cost").await.unwrap_err();
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
async fn an_interrupted_turn_blocks_submission_until_it_is_abandoned() {
    // Regression for the "prompt hangs with no output" report: an interrupted run leaves the
    // turn non-terminal, so a later submit used to wait forever on a `queued` receipt.
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

    // The dispatcher refuses a new submit while the old turn is open — and says so instead of
    // leaving the caller on a `queued` receipt forever.
    let blocked = session.submit("second").await.unwrap();
    let blocked = session.await_task(&blocked.command_id).await.unwrap();
    assert_eq!(blocked.status, "rejected", "{blocked:?}");
    assert_eq!(blocked.error.as_deref(), Some("turn_unfinished"));

    // Closing the abandoned turn unblocks the session.
    let closed = session.abandon_unfinished().await.unwrap();
    assert_eq!(closed.as_deref(), Some("turn-stale"));
    assert!(session.open_turn().await.unwrap().is_none());

    let next = session.submit("second").await.unwrap();
    let settled = session.await_task(&next.command_id).await.unwrap();
    assert_eq!(settled.status, "completed", "{settled:?}");

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
