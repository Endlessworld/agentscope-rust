//! End-to-end ACP stdio integration test: spawns the built `acp_harness_agent`
//! subprocess (exactly like `acp_agent_cli` does), sends a real prompt through
//! the Agent Client Protocol over stdio, and asserts that thinking deltas reach
//! the client as `AgentThoughtChunk` notifications.
//!
//! Requires a live model service configured via environment variables
//! (CODING_MODEL_ID / OPENAI_API_KEY / OPENAI_BASE_URL), same as the agent.
//!
//! Run:
//!   cargo test -p agentscope-harness --test acp_stdio_thought_test -- --nocapture

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};

/// Converts a `ContentBlock` to displayable text (mirrors the CLI).
fn content_to_string(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(t) => t.text.clone(),
        _ => String::new(),
    }
}

/// Locates the built `acp_harness_agent` executable under this test binary's
/// profile directory (debug or release), mirroring how the CLI resolves it.
fn agent_exe() -> PathBuf {
    // Prefer ACP_AGENT_EXE if set (allows pointing at a specific build).
    if let Ok(exe) = std::env::var("ACP_AGENT_EXE") {
        if !exe.trim().is_empty() {
            return PathBuf::from(exe);
        }
    }
    // Fall back to the binary built directly under target/{profile}/.
    let exe = std::env::current_exe().expect("current_exe");
    // .../target/debug/deps/acp_stdio_thought_test-<hash>.exe
    let profile_dir = exe
        .parent() // deps
        .and_then(|p| p.parent()) // profile dir
        .expect("profile dir");
    let candidate = profile_dir.join("acp_harness_agent.exe");
    assert!(
        candidate.exists(),
        "agent executable not found at {}; build it with \
         `cargo build -p agentscope-harness --bin acp_harness_agent`",
        candidate.display()
    );
    candidate
}

#[tokio::test]
async fn stdio_agent_streams_thinking_to_client() {
    let agent = AcpAgent::from_args([agent_exe().to_string_lossy().to_string()])
        .expect("build AcpAgent from exe path");

    let updates: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let updates_c = Arc::clone(&updates);

    let result = agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                use agent_client_protocol::schema::v1::SessionUpdate;
                let mut u = updates_c.lock().unwrap();
                match notification.update {
                    SessionUpdate::AgentThoughtChunk(chunk) => {
                        let text = content_to_string(&chunk.content);
                        u.push(format!("THOUGHT:{text}"));
                    }
                    SessionUpdate::AgentMessageChunk(chunk) => {
                        let text = content_to_string(&chunk.content);
                        u.push(format!("MESSAGE:{text}"));
                    }
                    SessionUpdate::ToolCall(tc) => u.push(format!("TOOL_CALL:{}", tc.title)),
                    SessionUpdate::ToolCallUpdate(_) => u.push("TOOL_CALL_UPDATE".to_string()),
                    _ => {}
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                let id = request.options.first().map(|opt| opt.option_id.clone());
                if let Some(id) = id {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    ))
                } else {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
            // Initialize.
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            // Create a session in the current directory.
            let new_session = connection
                .send_request(NewSessionRequest::new(
                    std::env::current_dir().expect("current_dir"),
                ))
                .block_task()
                .await?;
            let session_id = new_session.session_id;

            // Send a real prompt that encourages reasoning.
            let prompt = "What is 2 + 2? Think step by step, then answer.";
            let prompt_response = connection
                .send_request(PromptRequest::new(
                    session_id,
                    vec![ContentBlock::Text(TextContent::new(prompt.to_string()))],
                ))
                .block_task()
                .await?;

            eprintln!("StopReason: {:?}", prompt_response.stop_reason);
            Ok(())
        })
        .await;

    result.expect("ACP stdio connection should succeed");

    let updates = updates.lock().unwrap().clone();
    eprintln!("\n=== Collected {} notifications ===", updates.len());
    for u in &updates {
        eprintln!("  {u}");
    }

    let has_thought = updates.iter().any(|u| u.starts_with("THOUGHT:"));
    let has_message = updates.iter().any(|u| u.starts_with("MESSAGE:"));
    assert!(
        has_thought,
        "expected AgentThoughtChunk (thinking) to reach the client over stdio, \
         got {updates:?}"
    );
    assert!(
        has_message,
        "expected at least one agent message chunk, got {updates:?}"
    );
}
