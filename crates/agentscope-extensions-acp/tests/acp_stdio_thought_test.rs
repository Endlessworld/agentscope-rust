//! End-to-end ACP stdio integration test: spawns the built `acp_harness_agent`
//! subprocess (exactly like `acp_agent_cli` does), sends a prompt through the
//! Agent Client Protocol over stdio, and asserts that thinking deltas reach the
//! client as `AgentThoughtChunk` notifications.
//!
//! The agent's model is a **local scripted OpenAI-compatible server** (a tiny
//! `TcpListener` speaking SSE on `127.0.0.1`), so the test is deterministic: it
//! does not need a live provider, an API key, or a model that happens to emit
//! reasoning tokens. The child process is pointed at that server through
//! `CODING_PROVIDER` / `CODING_MODEL_ID` / `OPENAI_BASE_URL` / `OPENAI_API_KEY`,
//! with `HOME` redirected to a temp directory so the user's
//! `~/.agentscope/models.json` (and skill repositories) cannot influence it.
//!
//! Run:
//!   cargo test -p agentscope-extensions-acp --test acp_stdio_thought_test -- --nocapture
//!
//! Set `ACP_AGENT_EXE` to point at a specific `acp_harness_agent` build.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, TextContent,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent};

/// Reasoning text the scripted model streams first; the test asserts it reaches
/// the client verbatim, which proves the thought chunk came from the script.
const SCRIPTED_REASONING: &str = "Let me work it out: 2 plus 2 is 4.";
/// Answer text the scripted model streams after the reasoning.
const SCRIPTED_ANSWER: &str = "The answer is 4.";

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
         `cargo build -p agentscope-extensions-acp --bin acp_harness_agent`",
        candidate.display()
    );
    candidate
}

/// Creates a unique scratch directory under the system temp dir.
fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A scripted OpenAI-compatible endpoint: every `POST /chat/completions` is
/// answered with the same short SSE stream (one reasoning delta, one content
/// delta, one terminal chunk).
struct ScriptedLlm {
    /// Base URL to hand to the agent (`http://127.0.0.1:<port>/v1`).
    base_url: String,
    /// Number of requests the server has accepted so far.
    requests: Arc<AtomicUsize>,
}

impl ScriptedLlm {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind scripted llm");
        let addr = listener.local_addr().expect("scripted llm addr");
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        // One request is enough; the thread keeps serving (and stays detached)
        // so a stray retry cannot make the client hang on a closed port.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                counter.fetch_add(1, Ordering::SeqCst);
                if serve_one(&mut stream).is_err() {
                    continue;
                }
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
        }
    }
}

/// The SSE body the scripted server returns: reasoning delta, content delta,
/// terminal chunk, `[DONE]`.
fn scripted_stream_body() -> String {
    let reasoning = serde_json::json!({
        "id": "scripted-chunk-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "scripted-chat",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "reasoning_content": SCRIPTED_REASONING},
            "finish_reason": null
        }]
    });
    let content = serde_json::json!({
        "id": "scripted-chunk-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "scripted-chat",
        "choices": [{
            "index": 0,
            "delta": {"content": SCRIPTED_ANSWER},
            "finish_reason": null
        }]
    });
    let terminal = serde_json::json!({
        "id": "scripted-chunk-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "scripted-chat",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    format!("data: {reasoning}\n\ndata: {content}\n\ndata: {terminal}\n\ndata: [DONE]\n\n")
}

/// Reads one HTTP request (headers + body), then writes the scripted SSE reply.
fn serve_one(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut remaining = content_length.saturating_sub(buf.len() - header_end);
    while remaining > 0 {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        remaining = remaining.saturating_sub(read);
    }

    let body = scripted_stream_body();
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

#[tokio::test]
async fn stdio_agent_streams_thinking_to_client() {
    let llm = ScriptedLlm::start();
    // Isolates the child from the developer's `~/.agentscope/models.json`,
    // `~/.agents/skills`, and keeps journal/log files out of the repo.
    let home = scratch_dir("acp-stdio-thought-home");
    let work = scratch_dir("acp-stdio-thought-work");

    let config = AcpAgentConfig::new(agent_exe())
        // A provider that is *not* in models.json exercises the env fallback,
        // which always speaks Chat Completions.
        .env("CODING_PROVIDER", "scripted-local")
        .env("CODING_MODEL_ID", "scripted-chat")
        .env("OPENAI_BASE_URL", llm.base_url.clone())
        .env("OPENAI_API_KEY", "scripted-test-key")
        .env("HOME", home.to_string_lossy().to_string())
        .env("USERPROFILE", home.to_string_lossy().to_string())
        .env(
            "AGENTSCOPE_LOG_DIR",
            home.join("log").to_string_lossy().to_string(),
        );
    let agent = AcpAgent::new(config);

    let updates: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let updates_c = Arc::clone(&updates);

    let result = agent_client_protocol::Client
        .v1()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                use agent_client_protocol::v1::schema::SessionUpdate;
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

            // Create a session in the scratch working directory.
            let new_session = connection
                .send_request(NewSessionRequest::new(work))
                .block_task()
                .await?;
            let session_id = new_session.session_id;

            let prompt_response = connection
                .send_request(PromptRequest::new(
                    session_id,
                    vec![ContentBlock::Text(TextContent::new(
                        "What is 2 + 2?".to_string(),
                    ))],
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
    eprintln!(
        "=== Scripted model served {} request(s) ===",
        llm.requests.load(Ordering::SeqCst)
    );

    assert!(
        llm.requests.load(Ordering::SeqCst) >= 1,
        "the agent never called the scripted model; notifications: {updates:?}"
    );
    let has_thought = updates.iter().any(|u| {
        u.strip_prefix("THOUGHT:")
            .is_some_and(|t| t.contains("2 plus 2"))
    });
    let has_message = updates.iter().any(|u| {
        u.strip_prefix("MESSAGE:")
            .is_some_and(|t| t.contains("The answer is 4"))
    });
    assert!(
        has_thought,
        "expected the scripted AgentThoughtChunk (thinking) to reach the client \
         over stdio, got {updates:?}"
    );
    assert!(
        has_message,
        "expected the scripted agent message chunk to reach the client, got {updates:?}"
    );
}
