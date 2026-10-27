//! Integration tests for the harness middleware: inbox draining (InboxMiddleware)
//! and async-tool offloading (AsyncToolMiddleware).
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::{ContentBlock, Msg, MsgRole, ToolUseBlock};
use agentscope_core::middleware::{
    ActingInput, ActingResultsHandle, EventStream, Middleware, Next, ReasoningInput,
};
use agentscope_core::model::GenerateOptions;
use agentscope_harness::{
    AsyncToolMiddleware, AsyncToolRegistry, Filesystem, InboxMiddleware, LocalFilesystem,
    MessageBus, WorkspaceAsyncToolRegistry, WorkspaceMessageBus,
};
use futures::StreamExt;
use serde_json::json;

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_harness_mw_{tag}_{}",
        std::process::id()
    ));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}

fn empty_stream() -> EventStream {
    Box::pin(futures::stream::empty())
}

fn json_map(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

// ---- InboxMiddleware ----

#[tokio::test]
async fn inbox_middleware_drains_and_injects_hints() {
    let dir = temp_dir("inbox").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = Arc::new(WorkspaceMessageBus::new(fs, "/bus"));
    let session = "sess-1";

    // Push a hint payload into the per-session inbox.
    bus.inbox_push(
        session,
        json!({ "id": "h1", "hint": "Hello from inbox", "source": "system" }),
    )
    .await
    .unwrap();

    let mw = InboxMiddleware::new(bus.clone());
    let ctx = RuntimeContext::new().with_session_id(session);

    let captured = Arc::new(Mutex::new(None::<Vec<Msg>>));
    let captured2 = Arc::clone(&captured);
    let next: Next<ReasoningInput> = Box::new(move |input: ReasoningInput| {
        *captured2.lock().unwrap() = Some(input.messages.clone());
        empty_stream()
    });

    let input = ReasoningInput::new(vec![], vec![], GenerateOptions::default());
    let events = mw.on_reasoning(&ctx, input, next).collect::<Vec<_>>().await;

    // A HintBlock event was emitted.
    let hint_events = events
        .iter()
        .filter(|r| r.as_ref().ok().map(|e| e.event_type()) == Some(AgentEventType::HintBlock))
        .count();
    assert_eq!(
        hint_events, 1,
        "expected one HintBlock event, got: {:?}",
        events
    );

    // The reasoning input passed downstream contains one user-role notification per hint,
    // carrying a provenance sentence and structured metadata (Java `injectHintsToContext`).
    let msgs = captured
        .lock()
        .unwrap()
        .clone()
        .expect("next was not invoked");
    assert_eq!(msgs.len(), 1, "one notification per hint, got: {msgs:?}");
    let last = msgs.last().expect("expected an injected hint message");
    assert_eq!(last.role(), MsgRole::User, "notifications are user-role");
    let text: String = last
        .content()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(tb) => Some(tb.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Automated background event from system"),
        "missing provenance sentence: {text}"
    );
    assert!(
        text.contains("Hello from inbox"),
        "missing hint text: {text}"
    );
    assert_eq!(
        last.metadata().get(Msg::METADATA_SYNTHETIC),
        Some(&serde_json::Value::Bool(true)),
        "notifications must be marked synthetic"
    );
    assert_eq!(
        last.metadata()
            .get(Msg::METADATA_REMINDER_KIND)
            .and_then(|v| v.as_str()),
        Some("background_event")
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn inbox_middleware_no_session_passes_through() {
    let dir = temp_dir("inbox_nosess").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = Arc::new(WorkspaceMessageBus::new(fs, "/bus"));
    let mw = InboxMiddleware::new(bus.clone());

    let ctx = RuntimeContext::new(); // no session_id
    let invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let invoked2 = Arc::clone(&invoked);
    let next: Next<ReasoningInput> = Box::new(move |_: ReasoningInput| {
        invoked2.store(true, Ordering::SeqCst);
        empty_stream()
    });
    let input = ReasoningInput::new(vec![], vec![], GenerateOptions::default());
    mw.on_reasoning(&ctx, input, next).collect::<Vec<_>>().await;
    assert!(
        invoked.load(Ordering::SeqCst),
        "next should be invoked directly"
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// ---- AsyncToolMiddleware ----

#[tokio::test(flavor = "multi_thread")]
async fn async_tool_offloads_slow_tool_to_background() {
    let dir = temp_dir("asynctool").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = Arc::new(WorkspaceMessageBus::new(fs.clone(), "/bus"));
    let registry = Arc::new(WorkspaceAsyncToolRegistry::new(fs, "/async-tools"));

    let session = "sess-async";
    let mw = AsyncToolMiddleware::builder(bus.clone(), Duration::from_millis(100))
        .async_tool_registry(registry.clone())
        .build();
    let ctx = RuntimeContext::new()
        .with_session_id(session)
        .with_user_id("user-1")
        .with_agent_id("agent-1");

    // Inner tool execution is slow: it sleeps well past the offload timeout.
    let next: Next<ActingInput> = Box::new(move |_input: ActingInput| {
        Box::pin(futures::stream::once(async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok(AgentEvent::with_fields(
                AgentEventType::ToolResultTextDelta,
                json_map(json!({ "delta": "slow result text" })),
            ))
        }))
    });

    let tool_call = ToolUseBlock::new("call-1", "slow_tool", json_map(json!({})));
    let results = ActingResultsHandle::new();
    let input = ActingInput::with_results(vec![tool_call], results.clone());

    let events = mw.on_acting(&ctx, input, next).collect::<Vec<_>>().await;

    // Placeholder ToolResult events were emitted (offload path).
    let starts = events
        .iter()
        .filter(|r| {
            r.as_ref().ok().map(|e| e.event_type()) == Some(AgentEventType::ToolResultStart)
        })
        .count();
    assert_eq!(
        starts, 1,
        "expected placeholder ToolResultStart, got {:?}",
        events
    );

    // The placeholder is persisted through the acting results handle — that is the
    // Rust equivalent of Java writing the placeholder into the mutable AgentState
    // (`on_acting` only receives `&RuntimeContext`).
    let snapshot = results.snapshot();
    assert_eq!(
        snapshot.executed, 0,
        "a placeholder must not count as an execution"
    );
    assert!(
        !snapshot.finished,
        "the core did not finish before the offload"
    );
    assert_eq!(
        snapshot.results.len(),
        1,
        "one placeholder result per tool call"
    );
    let placeholder = format!("{:?}", snapshot.results[0].output);
    assert!(
        placeholder.contains("running in background"),
        "placeholder text missing from the recorded result: {placeholder}"
    );

    // The slow tool continues in the background and eventually delivers to inbox.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        bus.inbox_has_messages(session).await.unwrap(),
        "background result should have been pushed to the inbox"
    );

    // The async-tool record is no longer RUNNING (completed).
    let stale = registry
        .find_stale(session, chrono::Duration::seconds(0))
        .await
        .unwrap();
    assert!(
        stale.is_empty(),
        "record should have been completed, not stale"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn async_tool_passes_through_fast_tool() {
    let dir = temp_dir("asynctool_fast").await;
    let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
    let bus = Arc::new(WorkspaceMessageBus::new(fs, "/bus"));

    let session = "sess-fast";
    let mw = AsyncToolMiddleware::new(bus.clone(), Duration::from_millis(200));
    let ctx = RuntimeContext::new().with_session_id(session);

    // Inner completes immediately.
    let next: Next<ActingInput> = Box::new(move |_input: ActingInput| {
        Box::pin(futures::stream::once(async {
            Ok(AgentEvent::with_fields(
                AgentEventType::ToolResultStart,
                json_map(json!({ "replyId": "r1", "toolCallId": "c1", "toolName": "fast" })),
            ))
        }))
    });

    let tool_call = ToolUseBlock::new("c1", "fast_tool", json_map(json!({})));
    let results = ActingResultsHandle::new();
    let input = ActingInput::with_results(vec![tool_call], results.clone());

    let events: Vec<Result<AgentEvent>> = mw.on_acting(&ctx, input, next).collect().await;

    // Passthrough: exactly the one real event, no placeholder deltas.
    assert_eq!(
        events.len(),
        1,
        "expected passthrough of a single event, got {:?}",
        events
    );
    assert_eq!(
        events[0].as_ref().unwrap().event_type(),
        AgentEventType::ToolResultStart
    );
    assert!(
        results.snapshot().results.is_empty(),
        "a passthrough must not record a placeholder"
    );

    // Nothing offloaded -> inbox stays empty.
    assert!(!bus.inbox_has_messages(session).await.unwrap());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}
