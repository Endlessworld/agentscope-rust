//! Tests for `SessionSearchTool` / `SessionListTool` / `SessionHistoryTool`
//! and `WebFetchTool` / `WebSearchTool`.

use std::sync::Arc;

use agentscope_core::message::JsonMap;
use agentscope_core::tool::{Tool, ToolExecutionContext};

use agentscope_harness::agent::memory::session::{SessionEntry, SessionTree};
use agentscope_harness::agent::workspace::WorkspaceManager;
use agentscope_harness::tool::{
    SessionHistoryTool, SessionListTool, SessionSearchTool, WebFetchTool, WebSearchTool,
};

/// Unique per-test temp dir, following the existing test style.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("as_sess_web_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn exec_ctx() -> ToolExecutionContext {
    ToolExecutionContext::default()
}

async fn call_text(tool: &dyn Tool, args: JsonMap) -> String {
    let block = tool.call(args, exec_ctx()).await.unwrap();
    block.text_content()
}

/// Writes a sample session (context + log) for `researcher`.
fn seed_session(workspace: &std::path::Path) {
    let mut tree = SessionTree::new();
    tree.append(SessionEntry::message(None, "user", "How do quantum computers work?"));
    tree.append(SessionEntry::message(
        Some(tree.get_all_entries()[0].id()),
        "assistant",
        "Quantum computers use qubits and superposition.",
    ));

    let dir = workspace.join("agents/researcher/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("s1.jsonl"), tree.to_jsonl()).unwrap();
    std::fs::write(dir.join("s1.log.jsonl"), tree.to_jsonl()).unwrap();
}

#[tokio::test]
async fn session_search_finds_keyword() {
    let tmp = temp_dir("search");
    seed_session(&tmp);
    let tool = SessionSearchTool::new(Arc::new(WorkspaceManager::new(&tmp)));

    let out = call_text(
        &tool,
        serde_json::json!({"query": "qubits"}).as_object().unwrap().clone(),
    )
    .await;
    assert!(out.contains("Found 1 matches"), "got: {out}");
    assert!(out.contains("qubits"));

    let miss = call_text(
        &tool,
        serde_json::json!({"query": "nonexistent-xyz"}).as_object().unwrap().clone(),
    )
    .await;
    assert!(miss.contains("No matches found"));
}

#[tokio::test]
async fn session_search_requires_query() {
    let tmp = temp_dir("search_empty");
    let tool = SessionSearchTool::new(Arc::new(WorkspaceManager::new(&tmp)));
    let err = call_text(&tool, JsonMap::new()).await;
    assert!(err.contains("query is required"));
}

#[tokio::test]
async fn session_list_shows_sessions() {
    let tmp = temp_dir("list");
    seed_session(&tmp);
    let tool = SessionListTool::new(Arc::new(WorkspaceManager::new(&tmp)));

    let out = call_text(
        &tool,
        serde_json::json!({"agentId": "researcher"}).as_object().unwrap().clone(),
    )
    .await;
    assert!(out.contains("Sessions for agent researcher"));
    assert!(out.contains("- s1"));

    let none = call_text(
        &tool,
        serde_json::json!({"agentId": "ghost"}).as_object().unwrap().clone(),
    )
    .await;
    assert!(none.contains("No sessions found"));
}

#[tokio::test]
async fn session_history_returns_last_messages() {
    let tmp = temp_dir("history");
    seed_session(&tmp);
    let tool = SessionHistoryTool::new(Arc::new(WorkspaceManager::new(&tmp)));

    let out = call_text(
        &tool,
        serde_json::json!({"agentId": "researcher", "sessionId": "s1"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .await;
    assert!(out.contains("[user]: How do quantum computers work?"));
    assert!(out.contains("[assistant]"));

    let missing = call_text(
        &tool,
        serde_json::json!({"agentId": "researcher", "sessionId": "nope"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .await;
    assert!(missing.contains("Session not found"));
}

#[tokio::test]
async fn web_fetch_rejects_non_http() {
    let tool = WebFetchTool::new();
    let out = call_text(
        &tool,
        serde_json::json!({"url": "ftp://example.com"}).as_object().unwrap().clone(),
    )
    .await;
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
    let out = call_text(
        &tool,
        serde_json::json!({"query": "rust"}).as_object().unwrap().clone(),
    )
    .await;
    assert!(out.contains("TAVILY_API_KEY is not set"));
}
