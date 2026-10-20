//! Tests for `SessionSearchTool` / `SessionListTool` / `SessionHistoryTool`
//! and `WebFetchTool` / `WebSearchTool`.
//!
//! The session tools read the native journal, so these tests seed an in-memory
//! `SessionLogStore` directly (no workspace files, no `sessions.json`).

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{JsonMap, Msg};
use agentscope_core::session::{
    InMemorySessionLogStore, SessionKey, SessionLogStore, SessionRecorder,
};
use agentscope_core::state::AgentState;
use agentscope_core::tool::{Tool, ToolExecutionContext};

use agentscope_harness::tool::{
    SessionHistoryTool, SessionListTool, SessionSearchTool, WebFetchTool, WebSearchTool,
};

/// The user namespace the seeded journals live in.
const USER: &str = "alice";

fn exec_ctx() -> ToolExecutionContext {
    ToolExecutionContext::default()
}

fn ctx() -> RuntimeContext {
    RuntimeContext::new().with_user_id(USER)
}

async fn call_text(tool: &dyn Tool, args: JsonMap) -> String {
    let block = tool.call(args, exec_ctx()).await.unwrap();
    block.text_content()
}

fn args(value: serde_json::Value) -> JsonMap {
    value.as_object().unwrap().clone()
}

/// Commits a journal for `researcher`/`s1` holding an example conversation.
async fn seed_session(store: &InMemorySessionLogStore) {
    let context = ctx();
    let key = SessionKey::new(Some(USER), "researcher", "s1").unwrap();
    let log = store.open(&key, &context).await.unwrap();
    let recorder = SessionRecorder::open(log, None).await.unwrap();
    let mut state = AgentState::builder().session_id("s1").user_id(USER).build();
    state
        .context
        .push(Msg::user("How do quantum computers work?"));
    state.context.push(Msg::assistant(
        "Quantum computers use qubits and superposition.",
    ));
    recorder.start(&[], &state).await.unwrap();
    recorder.finish(&state, "completed").await.unwrap();
}

async fn seeded_store() -> Arc<dyn SessionLogStore> {
    let store = InMemorySessionLogStore::new();
    seed_session(&store).await;
    Arc::new(store)
}

#[tokio::test]
async fn session_search_finds_keyword() {
    let tool = SessionSearchTool::new(seeded_store().await).with_context(ctx());

    let out = call_text(&tool, args(serde_json::json!({"query": "qubits"}))).await;
    assert!(out.contains("Found 1 matches"), "got: {out}");
    assert!(out.contains("qubits"));

    let miss = call_text(&tool, args(serde_json::json!({"query": "nonexistent-xyz"}))).await;
    assert!(miss.contains("No matches found"));
}

#[tokio::test]
async fn session_search_requires_query() {
    let tool = SessionSearchTool::new(seeded_store().await).with_context(ctx());
    let err = call_text(&tool, JsonMap::new()).await;
    assert!(err.contains("query is required"));
}

#[tokio::test]
async fn session_list_shows_sessions() {
    let tool = SessionListTool::new(seeded_store().await).with_context(ctx());

    let out = call_text(&tool, args(serde_json::json!({"agentId": "researcher"}))).await;
    assert!(out.contains("Sessions for agent researcher"));
    assert!(out.contains("- s1"));

    let none = call_text(&tool, args(serde_json::json!({"agentId": "ghost"}))).await;
    assert!(none.contains("No sessions found"));
}

#[tokio::test]
async fn session_history_returns_last_messages() {
    let tool = SessionHistoryTool::new(seeded_store().await).with_context(ctx());

    let out = call_text(
        &tool,
        args(serde_json::json!({"agentId": "researcher", "sessionId": "s1"})),
    )
    .await;
    assert!(out.contains("How do quantum computers work?"), "got: {out}");
    assert!(out.contains("ASSISTANT"), "got: {out}");

    let missing = call_text(
        &tool,
        args(serde_json::json!({"agentId": "researcher", "sessionId": "nope"})),
    )
    .await;
    assert!(missing.contains("No history found"));
}

#[tokio::test]
async fn session_tools_do_not_leak_across_users() {
    // A journal owned by another user must stay invisible to this caller.
    let store = InMemorySessionLogStore::new();
    let other = RuntimeContext::new().with_user_id("bob");
    let key = SessionKey::new(Some("bob"), "researcher", "s1").unwrap();
    let log = store.open(&key, &other).await.unwrap();
    let recorder = SessionRecorder::open(log, None).await.unwrap();
    let mut state = AgentState::builder().session_id("s1").user_id("bob").build();
    state.context.push(Msg::user("qubits are fun"));
    recorder.start(&[], &state).await.unwrap();
    recorder.finish(&state, "completed").await.unwrap();

    let tool =
        SessionSearchTool::new(Arc::new(store) as Arc<dyn SessionLogStore>).with_context(ctx());
    let out = call_text(&tool, args(serde_json::json!({"query": "qubits"}))).await;
    assert!(out.contains("No matches found"), "got: {out}");
}

#[tokio::test]
async fn web_fetch_rejects_non_http() {
    let tool = WebFetchTool::new();
    let out = call_text(&tool, args(serde_json::json!({"url": "ftp://example.com"}))).await;
    assert!(out.contains("only http/https URLs are allowed"));
}

#[tokio::test]
async fn web_fetch_requires_url() {
    let tool = WebFetchTool::new();
    let out = call_text(&tool, JsonMap::new()).await;
    assert!(out.contains("url is required"));
}

#[tokio::test]
async fn web_search_without_key_reports_config_error() {
    // Explicitly construct with no key to avoid depending on ambient env.
    let tool = WebSearchTool::with_api_key("");
    let out = call_text(&tool, args(serde_json::json!({"query": "rust"}))).await;
    assert!(out.contains("DEEPSEEK_API_KEY is not set"));
}
