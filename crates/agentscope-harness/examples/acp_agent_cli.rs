//! ACP Agent CLI — interactive coding agent client over stdio.
//!
//! Spawns `acp_harness_agent` as a subprocess and communicates via the
//! Agent Client Protocol. Provides a REPL for sending prompts and
//! receiving streaming responses with tool-call visibility.
//!
//! Usage:
//!   cargo run -p agentscope-harness --example acp_agent_cli
//!
//! Environment variables (inherited by the agent subprocess):
//!   CODING_MODEL_ID  — model id (default: mimo-v2.5)
//!   OPENAI_API_KEY   — API key
//!   OPENAI_BASE_URL  — custom API base URL

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCallStatus,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{
    AcpAgent, ActiveSession, Agent, Client, ConnectionTo, Dispatch, Handled, UntypedMessage,
};
use std::path::PathBuf;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Error)]
enum CliError {
    #[error("Failed to build agent command: {0}")]
    AgentCommand(String),
    #[error("ACP connection failed: {0}")]
    Connection(#[from] agent_client_protocol::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, CliError>;

#[derive(Debug, Clone)]
struct CliConfig {
    agent_package: String,
    agent_example: String,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            agent_package: "agentscope-harness".to_string(),
            agent_example: "acp_harness_agent".to_string(),
        }
    }
}

/// Build the `AcpAgent` that spawns the harness agent as a subprocess.
fn build_agent_command(config: &CliConfig) -> Result<AcpAgent> {
    AcpAgent::from_args([
        "cargo",
        "run",
        "-p",
        &config.agent_package,
        "--example",
        &config.agent_example,
    ])
    .map_err(|e| CliError::AgentCommand(e.to_string()))
}

/// Converts a `ContentBlock` to displayable text.
fn content_to_string(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(TextContent { text, .. }) => text.clone(),
        ContentBlock::Image(_) => "[image]".into(),
        ContentBlock::Audio(_) => "[audio]".into(),
        _ => String::new(),
    }
}

const DIM: &str = "\x1b[90m"; // bright black — light gray for thinking text
const RESET: &str = "\x1b[0m";

/// Compact single-line rendering of a JSON value, truncated for display.
fn truncate_json(v: &serde_json::Value, max: usize) -> String {
    let mut s = v.to_string();
    if s.len() > max {
        s.truncate(max);
        s.push_str("…");
    }
    s
}

/// Pretty status label for a tool call.
fn status_label(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "running",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "unknown",
    }
}

/// Frame-paced stdout writer. Incoming chunks are queued and drained by a
/// background renderer task on a fixed timer, spreading bursty arrivals into a
/// smooth continuous stream. Text and tool markers share one ordered stream.
struct Output {
    tx: mpsc::UnboundedSender<PacedMsg>,
}

enum PacedMsg {
    Chunk(String),
    Sync(oneshot::Sender<()>),
}

const FRAME_MS: u64 = 16;
const BYTES_PER_FRAME: usize = 40;

/// Number of bytes to emit this frame, clamped to a UTF-8 character boundary so
/// multi-byte characters are never split across frames.
fn frame_cut(pending: &str) -> usize {
    let cap = pending.len().min(BYTES_PER_FRAME);
    if pending.is_char_boundary(cap) {
        cap
    } else {
        // cap fell inside a multi-byte char: step back to the previous boundary
        // (byte 0 is always a boundary, so this terminates).
        let mut idx = cap;
        while idx > 0 && !pending.is_char_boundary(idx) {
            idx -= 1;
        }
        idx
    }
}

impl Output {
    fn new() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<PacedMsg>();
        tokio::spawn(async move {
            use std::io::Write;
            let mut pending = String::new();
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(FRAME_MS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(PacedMsg::Chunk(s)) => pending.push_str(&s),
                        Some(PacedMsg::Sync(ack)) => {
                            if !pending.is_empty() {
                                let mut stdout = std::io::stdout().lock();
                                let _ = stdout.write_all(pending.as_bytes());
                                let _ = stdout.flush();
                                pending.clear();
                            }
                            let _ = ack.send(());
                        }
                        None => break,
                    },
                    _ = interval.tick() => {
                        if pending.is_empty() {
                            continue;
                        }
                        let take = frame_cut(&pending);
                        let out: String = pending.drain(..take).collect();
                        let mut stdout = std::io::stdout().lock();
                        let _ = stdout.write_all(out.as_bytes());
                        let _ = stdout.flush();
                    }
                }
            }
        });
        Self { tx }
    }

    /// Queue one chunk of streamed text (rendered on the frame timer).
    fn write(&mut self, s: &str) {
        let _ = self.tx.send(PacedMsg::Chunk(s.to_string()));
    }

    /// Queue a discrete line (e.g. a tool-call marker).
    fn line(&mut self, s: &str) {
        let _ = self.tx.send(PacedMsg::Chunk(format!("{s}\n")));
    }

    /// Drain all pending output immediately and wait until the renderer has
    /// caught up (used at turn end so the next prompt doesn't interleave).
    async fn flush(&mut self) {
        let (ack, rx) = oneshot::channel();
        let _ = self.tx.send(PacedMsg::Sync(ack));
        let _ = rx.await;
    }
}

/// Process a single session update, returning `true` if the turn ended.
async fn process_update(
    output: &mut Output,
    update: agent_client_protocol::SessionMessage,
) -> Result<bool> {
    match update {
        agent_client_protocol::SessionMessage::SessionMessage(message) => {
            MatchDispatch::new(message)
                .if_notification(async |notif: SessionNotification| {
                    match notif.update {
                        SessionUpdate::AgentMessageChunk(chunk) => {
                            let text = content_to_string(&chunk.content);
                            if !text.is_empty() {
                                output.write(&text);
                            }
                        }
                        SessionUpdate::AgentThoughtChunk(chunk) => {
                            // Thinking text rendered in light gray.
                            let text = content_to_string(&chunk.content);
                            if !text.is_empty() {
                                output.write(&format!("{DIM}{text}{RESET}"));
                            }
                        }
                        SessionUpdate::ToolCall(tc) => {
                            output.line(&format!(
                                "[tool] {} ({})",
                                tc.title,
                                status_label(&tc.status)
                            ));
                            if let Some(input) = &tc.raw_input {
                                output.line(&format!(
                                    "{DIM}  in : {}{RESET}",
                                    truncate_json(input, 2000)
                                ));
                            }
                        }
                        SessionUpdate::ToolCallUpdate(u) => {
                            if let Some(status) = u.fields.status {
                                output.line(&format!("[tool] #{} {}", u.tool_call_id.0, status_label(&status)));
                            }
                            if let Some(title) = &u.fields.title {
                                output.line(&format!("[tool] {title}"));
                            }
                            if let Some(input) = &u.fields.raw_input {
                                output.line(&format!(
                                    "{DIM}  in : {}{RESET}",
                                    truncate_json(input, 2000)
                                ));
                            }
                            if let Some(out) = &u.fields.raw_output {
                                output.line(&format!(
                                    "{DIM}  out: {}{RESET}",
                                    truncate_json(out, 2000)
                                ));
                            }
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .await
                .if_request(
                    async |req: RequestPermissionRequest, responder| {
                        let outcome = req
                            .options
                            .iter()
                            .find(|o| {
                                matches!(
                                    o.kind,
                                    PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                                )
                            })
                            .map_or(
                                RequestPermissionOutcome::Cancelled,
                                |opt| {
                                    RequestPermissionOutcome::Selected(
                                        SelectedPermissionOutcome::new(opt.option_id.clone()),
                                    )
                                },
                            );
                        responder.respond(RequestPermissionResponse::new(outcome))?;
                        Ok(())
                    },
                )
                .await
                .otherwise(async |_| Ok(()))
                .await?;
            Ok(false)
        }
        agent_client_protocol::SessionMessage::StopReason(reason) => {
            match reason {
                StopReason::EndTurn => {}
                StopReason::Cancelled => output.line("[Cancelled]"),
                StopReason::Refusal => output.line("[Agent refused]"),
                _ => {}
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Read a line from stdin, returning `None` on EOF.
fn read_input() -> Result<Option<String>> {
    eprint!("> ");
    std::io::Write::flush(&mut std::io::stderr())?;
    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(input.trim().to_string())),
        Err(e) => Err(CliError::Io(e)),
    }
}

/// Run the interactive REPL loop.
async fn run_repl(session: &mut ActiveSession<'static, Agent>) -> Result<()> {
    loop {
        let input = match read_input()? {
            Some(s) if s.is_empty() => continue,
            Some(s) if s == "/quit" || s == "/exit" => break,
            Some(s) => s,
            None => break,
        };

        session.send_prompt(input)?;

        let mut output = Output::new();
        loop {
            let update = session.read_update().await?;
            if process_update(&mut output, update).await? {
                break;
            }
        }
        output.flush().await;
        println!();
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = CliConfig::default();
    let agent = build_agent_command(&config)?;
    eprintln!("Spawning harness agent...");

    Client.builder()
        .on_receive_dispatch(
            async |message: Dispatch<UntypedMessage, UntypedMessage>, _cx| {
                Ok(Handled::No { message, retry: false })
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(agent, |cx: ConnectionTo<Agent>| async move {
            let _init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            let mut session = cx
                .build_session(PathBuf::from("."))
                .block_task()
                .start_session()
                .await?;

            eprintln!("Connected! Type your prompts below. (Ctrl-D or /quit to exit)\n");

            run_repl(&mut session)
                .await
                .map_err(agent_client_protocol::Error::into_internal_error)?;

            Ok(())
        })
        .await?;

    eprintln!("\nSession ended.");
    Ok(())
}