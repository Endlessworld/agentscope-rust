//! Tests for the transcript store and `TranscriptMiddleware`.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::Msg;
use agentscope_core::middleware::{AgentInput, Middleware, Next};
use futures::stream::StreamExt;

use agentscope_harness::agent::transcript::{
    FilesystemTranscriptStore, TranscriptRef, TranscriptStore,
};
use agentscope_harness::agent::middleware::TranscriptMiddleware;
use agentscope_harness::filesystem::LocalFilesystem;

#[tokio::test]
async fn transcript_store_append_and_read() {
    let dir = std::env::temp_dir().join(format!("as_tx_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs: Arc<dyn agentscope_harness::filesystem::Filesystem> =
        Arc::new(LocalFilesystem::new(dir.clone()));
    let store = Arc::new(FilesystemTranscriptStore::new(Arc::clone(&fs), "transcripts"));
    let r#ref = TranscriptRef::new("tenantA", "agent1", "sess1");

    let msgs = vec![serde_json::to_value(Msg::user("hello")).unwrap()];
    let evs = vec![serde_json::to_value(AgentEvent::new(AgentEventType::AgentStart)).unwrap()];

    let seq = store.append_segment(&r#ref, &msgs, &evs).await.unwrap();
    assert_eq!(seq, 1);
    // second turn
    store.append_segment(&r#ref, &msgs, &evs).await.unwrap();

    let segments = store.read_segments(&r#ref).await.unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].seq, 1);
    assert_eq!(segments[1].seq, 2);
    assert_eq!(segments[0].messages.len(), 1);

    let sessions = store.list_sessions("tenantA", "agent1").await.unwrap();
    assert!(sessions.contains(&"sess1".to_string()));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transcript_middleware_records_turn() {
    let dir = std::env::temp_dir().join(format!("as_tx_mw_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs: Arc<dyn agentscope_harness::filesystem::Filesystem> =
        Arc::new(LocalFilesystem::new(dir.clone()));
    let store = Arc::new(FilesystemTranscriptStore::new(Arc::clone(&fs), "transcripts"));
    let mw = TranscriptMiddleware::new(Arc::clone(&store) as Arc<dyn TranscriptStore>);

    let ctx = RuntimeContext::default()
        .with_session_id("sessX")
        .with_agent_id("agentX");
    let input = AgentInput::new(vec![Msg::user("hi")]);
    let event = Ok(AgentEvent::new(AgentEventType::AgentEnd));
    let next: Next<AgentInput> = Box::new(move |_in| {
        futures::stream::iter(vec![event]).boxed()
    });

    let stream = mw.on_agent(&ctx, input, next);
    let events: Vec<_> = stream.collect().await;
    assert_eq!(events.len(), 1);

    let r#ref = TranscriptRef::new("default", "agentX", "sessX");
    let segments = store.read_segments(&r#ref).await.unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].messages.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
