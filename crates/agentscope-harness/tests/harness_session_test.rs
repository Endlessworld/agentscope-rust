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
async fn reading_views_never_starts_inference() {
    let (session, dir) = open_session("readonly", "ok").await;
    // Views over an untouched journal: empty, and still nothing queued afterwards.
    assert!(session.transcript().await.unwrap().messages.is_empty());
    assert!(session.pending().await.unwrap().is_empty());
    assert!(session.tasks().await.unwrap().is_empty());
    assert_eq!(session.head().await.unwrap().seq, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
