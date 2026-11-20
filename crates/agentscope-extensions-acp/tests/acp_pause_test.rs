//! Pause/resume: a paused turn keeps its position and runs only when the client asks.
//!
//! The pause is durable state (`Paused`), not a client-side flag: the turn never becomes
//! terminal, so resuming continues it in place instead of replaying the prompt.
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{
    ContentBlock, SessionId, SessionNotification, StopReason, TextContent,
};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpAgentSession;
use agentscope_harness::agent::HarnessAgent;

/// Records the text of every model call and answers "done".
struct RecordingModel {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl Model for RecordingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.seen
            .lock()
            .unwrap()
            .push(messages.iter().map(|m| m.text_content()).collect());
        Box::pin(futures::stream::once(async {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "recording"
    }
}

fn build_session(tag: &str, seen: Arc<Mutex<Vec<Vec<String>>>>) -> AcpAgentSession {
    let dir = std::env::temp_dir().join(format!("acp-pause-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let agent = HarnessAgent::builder()
        .name("acp-pause-test")
        .model(Arc::new(RecordingModel { seen }))
        .session_id(format!("s-{tag}"))
        .cwd(dir.clone())
        .workspace(dir.join(".agentscope").join("workspace"))
        .disable_memory_hooks()
        .max_iters(3)
        .build();
    AcpAgentSession::new(
        SessionId::new(format!("s-{tag}")),
        dir,
        agent,
        Vec::new(),
        Vec::new(),
    )
}

fn prompt(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(text.to_string()))]
}

/// Answers after a pause, so the test has a running turn to pause.
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
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "slow"
    }
}

fn build_slow_session(tag: &str) -> AcpAgentSession {
    let dir = std::env::temp_dir().join(format!("acp-pause-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let agent = HarnessAgent::builder()
        .name("acp-pause-test")
        .model(Arc::new(SlowModel))
        .session_id(format!("s-{tag}"))
        .cwd(dir.clone())
        .workspace(dir.join(".agentscope").join("workspace"))
        .disable_memory_hooks()
        .max_iters(3)
        .build();
    AcpAgentSession::new(
        SessionId::new(format!("s-{tag}")),
        dir,
        agent,
        Vec::new(),
        Vec::new(),
    )
}

#[tokio::test]
async fn pausing_a_running_turn_answers_the_prompt_and_keeps_it_resumable() {
    let mut session = build_slow_session("running");
    let pauser = session.clone();
    let prompt_task =
        tokio::spawn(async move { session.run_prompt(prompt("slow question"), |_| {}).await });

    // The model sleeps for 500ms; pausing at 100ms lands inside the run.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (turn_id, status) = pauser.pause("p-1").await.expect("pause");
    assert!(
        status == "pausing" || status == "paused",
        "the pause must be acknowledged, got {status}"
    );

    let stop = prompt_task.await.expect("the prompt task");
    assert_eq!(
        stop,
        StopReason::Cancelled,
        "a paused turn answers its prompt instead of running to the end"
    );

    let queued = pauser.queued().await.expect("list");
    assert_eq!(queued.len(), 1, "the paused turn is still unfinished");
    assert_eq!(queued[0].turn_id, turn_id);
    assert_eq!(queued[0].status, "paused");
}

#[tokio::test]
async fn a_paused_turn_stays_paused_until_the_client_resumes_it() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let session = build_session("resume", Arc::clone(&seen));

    let (turn_id, _) = session
        .queue(prompt("paused message"), "q-1")
        .await
        .expect("queue");
    let (paused, status) = session.pause("p-1").await.expect("pause");
    assert_eq!(paused, turn_id);
    assert_eq!(status, "paused");

    let queued = session.queued().await.expect("list");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].status, "paused");
    assert!(
        seen.lock().unwrap().is_empty(),
        "a paused turn must not turn into a run by itself"
    );

    let notifications: Arc<Mutex<Vec<SessionNotification>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&notifications);
    let stop = session
        .resume_turn(&turn_id, "r-1", move |notification| {
            captured.lock().unwrap().push(notification);
        })
        .await
        .expect("resume");
    assert_eq!(stop, StopReason::EndTurn);

    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "resuming runs the turn exactly once");
    assert!(calls[0].iter().any(|text| text.contains("paused message")));
    assert!(session.queued().await.expect("list").is_empty());

    let tagged = notifications.lock().unwrap().iter().any(|notification| {
        notification
            .meta
            .as_ref()
            .and_then(|meta| meta.get("turnId"))
            == Some(&serde_json::Value::String(turn_id.clone()))
    });
    assert!(tagged, "a resumed turn's updates carry its id too");
}
