//! Queue extension: a message admitted while another turn runs must be durable, visible in the
//! session's queue and drained by the next prompt.
//!
//! The queue is durable state, not a client-side list: the turn is admitted by `submit` and is
//! `queued` in the store from the moment the extension answers. Nothing ticks it there — a prompt
//! is the only thing that ticks this session — so the next prompt has to drain it, in order, before
//! admitting its own message.
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{
    ContentBlock, SessionId, SessionNotification, StopReason, TextContent,
};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::AcpAgentSession;
use agentscope_harness::agent::HarnessAgent;

/// Records the text of every message each model call sees, then answers "done".
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

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("acp-queue-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

fn build_session(tag: &str, seen: Arc<Mutex<Vec<Vec<String>>>>) -> AcpAgentSession {
    let dir = scratch_dir(tag);
    let agent = HarnessAgent::builder()
        .name("acp-queue-test")
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

#[tokio::test]
async fn a_queued_message_is_durable_and_can_be_withdrawn() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let session = build_session("withdraw", Arc::clone(&seen));

    let (turn_id, position) = session.queue(prompt("later"), "q-1").await.expect("queue");
    assert_eq!(position, 1);
    let queued = session.queued().await.expect("list");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].turn_id, turn_id);
    assert_eq!(queued[0].status, "queued");
    assert_eq!(queued[0].position, 1);

    // A retry carries the same idempotency key and reaches the same turn, never a second one.
    let (again, again_position) = session.queue(prompt("later"), "q-1").await.expect("retry");
    assert_eq!(again, turn_id);
    assert_eq!(again_position, 1);
    assert_eq!(session.queued().await.expect("list").len(), 1);

    assert!(session
        .remove_queued(&turn_id, "remove-1")
        .await
        .expect("remove"));
    assert!(session.queued().await.expect("list").is_empty());
    assert!(!session
        .remove_queued(&turn_id, "remove-2")
        .await
        .expect("remove again"));
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing may run before a prompt drains the queue"
    );
}

#[tokio::test]
async fn the_next_prompt_drains_the_queue_in_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut session = build_session("drain", Arc::clone(&seen));

    let (queued_turn, _) = session
        .queue(prompt("queued message"), "q-1")
        .await
        .expect("queue");

    let notifications: Arc<Mutex<Vec<SessionNotification>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&notifications);
    let stop = session
        .run_prompt(prompt("prompt message"), move |notification| {
            captured.lock().unwrap().push(notification);
        })
        .await;
    assert_eq!(stop, StopReason::EndTurn);

    let calls = seen.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        2,
        "the queued turn and the prompt each run once"
    );
    assert!(calls[0].iter().any(|text| text.contains("queued message")));
    assert!(
        calls[0].iter().all(|text| !text.contains("prompt message")),
        "the queued turn must run before the prompt that drains it"
    );
    assert!(calls[1].iter().any(|text| text.contains("prompt message")));

    assert!(session.queued().await.expect("list").is_empty());

    // Every update of the queued turn names it, so a client can bind the streamed answer to the
    // bubble it queued.
    let tagged = notifications.lock().unwrap().iter().any(|notification| {
        notification
            .meta
            .as_ref()
            .and_then(|meta| meta.get("turnId"))
            == Some(&serde_json::Value::String(queued_turn.clone()))
    });
    assert!(
        tagged,
        "the queued turn's updates must carry its id in _meta.turnId"
    );
}
