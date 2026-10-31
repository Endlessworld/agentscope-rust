//! MEM-01 acceptance: the long-term-memory middleware must work when mounted through the
//! **real** `HarnessAgent` assembly entry (`builder().middleware(chain)`).
//!
//! The test drives the whole turn (no adapter internals are poked):
//!
//! 1. a `Mem0LongTermMemory` adapter sits on an in-memory transport that answers the
//!    search endpoint with a canned memory and records every request;
//! 2. a recording stub model captures exactly what the provider would receive;
//! 3. the assertions cover both directions of the Java `StaticLongTermMemoryHook`:
//!    the retrieved memory reaches the model request, and the finished conversation is
//!    posted back to the backend.

use std::sync::{Arc, Mutex};

use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use agentscope_core::middleware::MiddlewareChain;
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_mem::http::HttpTransport;
use agentscope_extensions_mem::{
    Mem0ApiType, Mem0Client, Mem0LongTermMemory, Mem0LongTermMemoryConfig,
    MemLongTermMemoryMiddleware, Result, LONG_TERM_MEMORY_MESSAGE_NAME,
    PLATFORM_MEMORIES_ENDPOINT, PLATFORM_SEARCH_ENDPOINT,
};
use agentscope_harness::agent::HarnessAgent;
use async_trait::async_trait;
use futures::StreamExt;

const MEMORY_TEXT: &str = "The user likes oolong tea.";
const USER_QUERY: &str = "what tea do I like?";
const REPLY_TEXT: &str = "Sure, oolong.";

/// In-memory [`HttpTransport`]: answers the two Mem0 platform endpoints and records every
/// `(url, body)` pair, so the test can assert on the actual wire traffic.
#[derive(Default)]
struct RecordingTransport {
    requests: Mutex<Vec<(String, String)>>,
}

impl RecordingTransport {
    fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl HttpTransport for RecordingTransport {
    async fn post_json(
        &self,
        url: &str,
        _headers: &[(String, String)],
        body: &str,
    ) -> Result<String> {
        self.requests
            .lock()
            .unwrap()
            .push((url.to_string(), body.to_string()));
        if url.ends_with(PLATFORM_SEARCH_ENDPOINT) {
            Ok(format!("{{\"results\":[{{\"memory\":\"{MEMORY_TEXT}\"}}]}}"))
        } else {
            Ok("{}".to_string())
        }
    }
}

/// Answers every request with fixed text and records the messages the provider received.
#[derive(Default)]
struct RecordingModel {
    calls: Mutex<Vec<Vec<Msg>>>,
}

impl RecordingModel {
    fn calls(&self) -> Vec<Vec<Msg>> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Model for RecordingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.calls.lock().unwrap().push(messages);
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new(REPLY_TEXT))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "recording-stub"
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentscope_mem_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn the_middleware_retrieves_before_reasoning_and_records_after_the_turn() {
    let dir = temp_dir("wiring");
    let transport: Arc<RecordingTransport> = Arc::new(RecordingTransport::default());
    let client = Arc::new(
        Mem0Client::with_transport(
            transport.clone(),
            "https://mem0.test",
            Some("key".into()),
            Mem0ApiType::Platform,
        )
        .expect("valid base url"),
    );
    let adapter = Mem0LongTermMemory::new(
        client,
        Mem0LongTermMemoryConfig {
            user_id: Some("alice".into()),
            ..Default::default()
        },
    )
    .expect("configured adapter");

    let model = Arc::new(RecordingModel::default());
    let mut chain = MiddlewareChain::new();
    chain.add(Box::new(MemLongTermMemoryMiddleware::new(Arc::new(adapter))));

    let agent = HarnessAgent::builder()
        .name("mem-agent")
        .model(model.clone())
        .middleware(chain)
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .max_iters(1)
        .disable_memory_hooks()
        .disable_workspace_context()
        .disable_session_persistence()
        .build();

    let mut events = agent.stream_events(vec![Msg::user(USER_QUERY)], StreamOptions::defaults());
    while let Some(event) = events.next().await {
        event.expect("the turn must succeed");
    }

    // ---- (1) retrieval is injected into the model request ----
    let calls = model.calls();
    assert!(!calls.is_empty(), "the agent must reach the model");
    let injected_calls = calls
        .iter()
        .filter(|messages| {
            messages
                .iter()
                .any(|message| message.name() == Some(LONG_TERM_MEMORY_MESSAGE_NAME))
        })
        .count();
    assert_eq!(
        injected_calls, 1,
        "the memory message must be injected exactly once per turn: {calls:?}"
    );
    let injected = calls[0]
        .iter()
        .find(|message| message.name() == Some(LONG_TERM_MEMORY_MESSAGE_NAME))
        .expect("the retrieved memory must be part of the model request");
    let text = injected.text_content();
    assert!(text.contains("<long_term_memory>"), "{text}");
    assert!(text.contains(MEMORY_TEXT), "{text}");
    assert!(
        text.ends_with("</long_term_memory>"),
        "the Java wrapper must be reproduced verbatim: {text}"
    );

    // ---- (2) the query was searched exactly once, from the harness-built context ----
    let requests = transport.requests();
    let searches: Vec<_> = requests
        .iter()
        .filter(|(url, _)| url.ends_with(PLATFORM_SEARCH_ENDPOINT))
        .collect();
    assert_eq!(searches.len(), 1, "requests: {requests:?}");
    assert!(searches[0].1.contains(USER_QUERY), "{}", searches[0].1);

    // ---- (3) the finished turn is recorded (user input + assistant reply) ----
    let adds: Vec<_> = requests
        .iter()
        .filter(|(url, _)| url.ends_with(PLATFORM_MEMORIES_ENDPOINT))
        .collect();
    assert_eq!(adds.len(), 1, "requests: {requests:?}");
    assert!(adds[0].1.contains(USER_QUERY), "{}", adds[0].1);
    assert!(adds[0].1.contains(REPLY_TEXT), "{}", adds[0].1);

    // The injected memory message is request-scoped: it must not become conversation
    // history (Java only rewrote `PreReasoningEvent.getInputMessages()`).
    assert!(
        !adds[0].1.contains(LONG_TERM_MEMORY_MESSAGE_NAME),
        "{}",
        adds[0].1
    );

    let _ = std::fs::remove_dir_all(&dir);
}
