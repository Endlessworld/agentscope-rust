//! ACP Agent CLI — interactive coding agent client over stdio.
//!
//! Spawns `acp_harness_agent` as a subprocess and communicates via the
//! Agent Client Protocol. Provides a REPL for sending prompts and
//! receiving streaming responses with tool-call visibility.
//!
//! Usage:
//!   cargo run -p agentscope-harness --example acp_agent_cli
//!   (or run the built acp_agent_cli.exe next to acp_harness_agent.exe)
//!
//! The agent subprocess is resolved as a sibling `acp_harness_agent.exe` next
//! to this binary, or via the `ACP_AGENT_EXE` environment variable.
//!
//! Environment variables (inherited by the agent subprocess):
//!   CODING_MODEL_ID  — model id (default: mimo-v2.5)
//!   OPENAI_API_KEY   — API key
//!   OPENAI_BASE_URL  — custom API base URL

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCallContent, ToolCallLocation,
    ToolCallStatus, ToolKind,
};
use std::collections::HashMap;
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

/// Build the `AcpAgent` that spawns the harness agent as a subprocess.
///
/// The agent executable is resolved from the directory of the currently running
/// CLI binary (they are built together), and can be overridden with the
/// `ACP_AGENT_EXE` environment variable.
fn build_agent_command() -> Result<AcpAgent> {
    // 1) explicit override via env var
    if let Ok(exe) = std::env::var("ACP_AGENT_EXE") {
        if !exe.trim().is_empty() {
            return AcpAgent::from_args([exe])
                .map_err(|e| CliError::AgentCommand(e.to_string()));
        }
    }
    // 2) sibling executable next to this CLI binary
    let mut path = std::env::current_exe()
        .map_err(|e| CliError::AgentCommand(e.to_string()))?;
    path.set_file_name("acp_harness_agent.exe");
    if !path.exists() {
        return Err(CliError::AgentCommand(format!(
            "agent executable not found at {} (set ACP_AGENT_EXE to override)",
            path.display()
        )));
    }
    AcpAgent::from_args([path.to_string_lossy().to_string()])
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
    truncate_str(&v.to_string(), max)
}

/// Truncates to `max` bytes, snapping back to a UTF-8 character boundary so
/// multi-byte characters are never split.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push('…');
    out
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

/// Pretty label for an ACP `ToolKind`.
fn kind_label(kind: &ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch-mode",
        _ => "other",
    }
}

/// Per-tool-call rendering state. Streaming updates repeat the same `raw_input`
/// / `raw_output` payloads, so the last printed values are tracked to avoid
/// flooding the terminal.
#[derive(Default)]
struct ToolView {
    name: Option<String>,
    title: Option<String>,
    kind: Option<&'static str>,
    last_input: Option<String>,
    last_output: Option<String>,
}

/// Renders a tool-call notification into the lines to print, updating the
/// per-call view. Covers every field the agent pushes: name, title, status,
/// kind, locations, raw input and output (preferring structured `content`).
fn render_tool(
    views: &mut HashMap<String, ToolView>,
    id: &str,
    name: Option<String>,
    title: Option<String>,
    kind: Option<ToolKind>,
    status: Option<ToolCallStatus>,
    locations: &[ToolCallLocation],
    content: &[ToolCallContent],
    raw_input: Option<&serde_json::Value>,
    raw_output: Option<&serde_json::Value>,
) -> Vec<String> {
    let view = views.entry(id.to_string()).or_default();
    if name.is_some() {
        view.name = name;
    }
    if title.is_some() {
        view.title = title;
    }
    let mut lines = Vec::new();
    if let Some(status) = status {
        let label = view.name.as_deref().or(view.title.as_deref()).unwrap_or(id);
        lines.push(format!("[tool] {} ({})", label, status_label(&status)));
    }
    if let Some(kind) = kind {
        let label = kind_label(&kind);
        if view.kind.is_none() {
            view.kind = Some(label);
            lines.push(format!("{DIM}  kind: {label}{RESET}"));
        }
    }
    if !locations.is_empty() {
        let paths: Vec<String> = locations
            .iter()
            .map(|l| match l.line {
                Some(line) => format!("{}:{}", l.path.display(), line),
                None => l.path.display().to_string(),
            })
            .collect();
        lines.push(format!("{DIM}  at : {}{RESET}", paths.join(", ")));
    }
    if let Some(input) = raw_input {
        let rendered = truncate_json(input, 2000);
        if view.last_input.as_deref() != Some(rendered.as_str()) {
            view.last_input = Some(rendered.clone());
            lines.push(format!("{DIM}  in : {rendered}{RESET}"));
        }
    }
    let output = content
        .iter()
        .find_map(|c| match c {
            ToolCallContent::Content(c) => {
                let text = content_to_string(&c.content);
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        })
        .or_else(|| raw_output.map(|v| v.to_string()));
    if let Some(output) = output {
        let rendered = truncate_str(&output, 2000);
        if view.last_output.as_deref() != Some(rendered.as_str()) {
            view.last_output = Some(rendered.clone());
            lines.push(format!("{DIM}  out: {rendered}{RESET}"));
        }
    }
    lines
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
///
/// `last_was_thinking` tracks whether the most recent output was thinking
/// text so that a blank line can be inserted before the next non-thinking
/// content (tool call or agent message).
async fn process_update(
    output: &mut Output,
    update: agent_client_protocol::SessionMessage,
    views: &mut HashMap<String, ToolView>,
    last_was_thinking: &mut bool,
) -> Result<bool> {
    match update {
        agent_client_protocol::SessionMessage::SessionMessage(message) => {
            MatchDispatch::new(message)
                .if_notification(async |notif: SessionNotification| {
                    match notif.update {
                        SessionUpdate::AgentMessageChunk(chunk) => {
                            let text = content_to_string(&chunk.content);
                            if !text.is_empty() {
                                if *last_was_thinking {
                                    output.line("");
                                    *last_was_thinking = false;
                                }
                                output.write(&text);
                            }
                        }
                        SessionUpdate::AgentThoughtChunk(chunk) => {
                            // Thinking text rendered in light gray.
                            let text = content_to_string(&chunk.content);
                            if !text.is_empty() {
                                output.write(&format!("{DIM}{text}{RESET}"));
                                *last_was_thinking = true;
                            }
                        }
                        SessionUpdate::ToolCall(tc) => {
                            if *last_was_thinking {
                                output.line("");
                                *last_was_thinking = false;
                            }
                            for line in render_tool(
                                views,
                                &tc.tool_call_id.0,
                                tc.name.clone(),
                                Some(tc.title.clone()),
                                Some(tc.kind),
                                Some(tc.status),
                                &tc.locations,
                                &tc.content,
                                tc.raw_input.as_ref(),
                                tc.raw_output.as_ref(),
                            ) {
                                output.line(&line);
                            }
                        }
                        SessionUpdate::ToolCallUpdate(u) => {
                            if *last_was_thinking {
                                output.line("");
                                *last_was_thinking = false;
                            }
                            for line in render_tool(
                                views,
                                &u.tool_call_id.0,
                                u.fields.name.clone(),
                                u.fields.title.clone(),
                                u.fields.kind,
                                u.fields.status,
                                u.fields.locations.as_deref().unwrap_or(&[]),
                                u.fields.content.as_deref().unwrap_or(&[]),
                                u.fields.raw_input.as_ref(),
                                u.fields.raw_output.as_ref(),
                            ) {
                                output.line(&line);
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
    let mut views: HashMap<String, ToolView> = HashMap::new();
    loop {
        let input = match read_input()? {
            Some(s) if s.is_empty() => continue,
            Some(s) if s == "/quit" || s == "/exit" => break,
            Some(s) => s,
            None => break,
        };

        session.send_prompt(input)?;

        let mut output = Output::new();
        let mut last_was_thinking = false;
        loop {
            let update = session.read_update().await?;
            if process_update(&mut output, update, &mut views, &mut last_was_thinking).await? {
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

    let agent = build_agent_command()?;
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