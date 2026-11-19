//! OFFLINE DIAGNOSTIC ONLY (read-only investigation aid). Drives the REAL ACP session
//! (`AcpAgentSession::run_prompt` -> durable runtime -> FileStore -> AgentScopeHarnessAdapter ->
//! long-lived HarnessAgent) for many growing turns, with a scripted model and no network, and
//! prints per-turn live heap (net live bytes, CountingAlloc) plus OS commit.
//!
//! Answers the question the micro-benchmark could not: does the real durable path retain state
//! versions in memory, or is the live heap flat (one latest copy) while commit is allocator pages?
//!
//!   cargo test -p agentscope-extensions-acp --features alloc-stats --test acp_live_memory_test -- --nocapture --test-threads=1

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_client_protocol::v1::schema::{ContentBlock, SessionId, SessionNotification};
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_extensions_acp::mem;
use agentscope_extensions_acp::AcpAgentSession;
use agentscope_harness::agent::HarnessAgent;

#[cfg(feature = "alloc-stats")]
#[global_allocator]
static ALLOC: mem::counting::CountingAlloc = mem::counting::CountingAlloc;

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// A model that answers every request with a fixed-size text reply and records how many messages
/// it was shown (the live context size the agent actually holds).
struct EchoModel {
    reply_bytes: usize,
    last_messages: AtomicUsize,
    calls: AtomicUsize,
    at_call_peak: AtomicUsize,
    at_call_bytes: AtomicUsize,
}

impl EchoModel {
    fn new(reply_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            reply_bytes,
            last_messages: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            at_call_peak: AtomicUsize::new(0),
            at_call_bytes: AtomicUsize::new(0),
        })
    }
}

impl Model for EchoModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.last_messages.store(messages.len(), Ordering::SeqCst);
        #[cfg(feature = "alloc-stats")]
        {
            if let Some((live, _, _)) = mem::live_heap() {
                self.at_call_peak.fetch_max(live, Ordering::SeqCst);
            }
            let bytes: usize = messages
                .iter()
                .map(|message| {
                    message
                        .content()
                        .iter()
                        .map(|block| match block {
                            CoreBlock::Text(block) => block.text.len(),
                            _ => 64,
                        })
                        .sum::<usize>()
                })
                .sum();
            self.at_call_bytes.store(bytes, Ordering::SeqCst);
        }
        let text = "lorem-ipsum-dolor-".repeat(self.reply_bytes / 18 + 1);
        let text = text[..self.reply_bytes].to_string();
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new(text))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "echo-diag"
    }
}

fn unique_workspace(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("acp-live-mem-{tag}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn footprint(label: &str, model: &EchoModel) {
    let u = mem::usage();
    let msgs = model.last_messages.load(Ordering::SeqCst);
    #[cfg(feature = "alloc-stats")]
    {
        let (live, peak, blocks) = mem::live_heap().expect("alloc-stats on");
        let rs = agentscope_durable::traffic::read_stats();
        let ws = agentscope_durable::store::file::write_stats();
        // One of the two stores is in use; summing keeps the line correct without the test having
        // to know which backend the session picked.
        #[cfg(feature = "durable-sqlite")]
        let (txns, writes, sbytes) = {
            let lite = agentscope_durable_sqlite::write_stats();
            (
                ws.transactions + lite.transactions,
                ws.scope_writes,
                ws.scope_bytes + lite.bytes_written,
            )
        };
        #[cfg(not(feature = "durable-sqlite"))]
        let (txns, writes, sbytes) = (ws.transactions, ws.scope_writes, ws.scope_bytes);
        let call_live = model.at_call_peak.load(Ordering::SeqCst);
        let call_kb = model.at_call_bytes.load(Ordering::SeqCst) / 1024;
        eprintln!(
            "{label:<28} msgs={msgs:<4} live={:7.2}MB peak_live={:7.2}MB | @call_live={:7.2}MB @call_kb={}KB | commit={:7.2}MB peak_commit={:7.2}MB | parses={} picks={} jreads={} jentries={} | txns={} writes={} sbytes={}",
            mb(live), mb(peak), mb(call_live), call_kb, mb(u.commit), mb(u.peak_commit), rs.scope_parses, rs.scope_picks, rs.journal_reads, rs.journal_entries, txns, writes, sbytes
        );
    }
    #[cfg(not(feature = "alloc-stats"))]
    eprintln!(
        "{label:<28} msgs={msgs:<4} commit={:7.2}MB peak_commit={:7.2}MB (no live heap: build with alloc-stats)",
        mb(u.commit), mb(u.peak_commit)
    );
}

/// The core question: drive the real session for many growing turns and watch whether the live
/// heap tracks "one latest context" (roughly linear in context size, no per-version retention) or
/// blows up with the number of turns.
///
/// Runs on one thread on purpose: `diag` windows are thread-local, so on a multi-threaded
/// runtime a phase label sticks to whichever worker last marked and then absorbs unrelated
/// allocations, which makes "allocated by phase" meaningless. One thread keeps the label a
/// faithful "what ran since the last mark".
#[tokio::test(flavor = "current_thread")]
#[ignore = "offline diagnostic: run with --ignored to see the live-heap curve"]
async fn real_acp_session_live_heap_across_growing_turns() {
    let turns: usize = std::env::var("DIAG_TURNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let user_bytes = std::env::var("DIAG_USER_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8 * 1024);
    let reply_bytes = std::env::var("DIAG_REPLY_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8 * 1024);

    let dir = unique_workspace("grow");
    let model = EchoModel::new(reply_bytes);
    let agent = HarnessAgent::builder()
        .name("acp-live-diag")
        .model(Arc::clone(&model) as Arc<dyn Model>)
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .register_filesystem_tools(true)
        .max_iters(5)
        .build();
    let mut session = AcpAgentSession::new(
        SessionId::new("s-live-diag"),
        dir.clone(),
        agent,
        vec![],
        vec![],
    );

    eprintln!("\n=== real ACP session, {turns} growing turns (user {user_bytes}B + reply {reply_bytes}B each) ===");
    footprint("before turn 0", &model);

    #[cfg(feature = "alloc-stats")]
    {
        agentscope_core::diag::register_live(mem::live_bytes);
        agentscope_core::diag::enable();
        agentscope_core::diag::reset();
    }

    for turn in 0..turns {
        agentscope_durable::traffic::reset_read_stats();
        agentscope_durable::store::file::reset_write_stats();
        #[cfg(feature = "durable-sqlite")]
        agentscope_durable_sqlite::reset_write_stats();
        let text = "u".repeat(user_bytes / 2) + &turn.to_string();
        let content = vec![ContentBlock::Text(
            agent_client_protocol::v1::schema::TextContent::new(text),
        )];
        let stop = session
            .run_prompt(content, |_n: SessionNotification| {})
            .await;
        footprint(&format!("after turn {turn} ({stop:?})"), &model);
    }

    // Attribute the remaining high-water mark to the phase that holds it.
    #[cfg(feature = "alloc-stats")]
    {
        eprintln!("--- phase peaks (max live observed while in phase) ---");
        for (label, peak) in agentscope_core::diag::peak_by_label() {
            eprintln!("{label:<24} {:7.2}MB", mb(peak));
        }
        // Labels are process-wide: an allocation made by any thread is charged to whichever
        // phase was marked last. The ordered sequence tells more than the collapsed peaks -- a
        // jump between two consecutive marks is an allocation that happened inside that window
        // on some thread, and a window whose own code allocates nothing shows it as flat.
        let events = agentscope_core::diag::events();
        let start = if std::env::var("DIAG_EVENTS_ALL").is_ok() {
            0
        } else {
            events.len().saturating_sub(26)
        };
        eprintln!("--- phase timeline (last {}) ---", events.len() - start);
        for (index, (label, peak, allocated)) in events[start..].iter().enumerate() {
            eprintln!(
                "{:>3} {label:<24} live-peak {:7.2}MB | allocated {:7.1}KB",
                start + index,
                mb(*peak),
                *allocated as f64 / 1024.0
            );
        }
        // Per-thread allocation attribution: what the phase's own code asked the heap for.
        eprintln!("--- allocated by phase (bytes allocated, summed over all turns) ---");
        for (label, allocated) in agentscope_core::diag::allocated_by_label() {
            eprintln!("{label:<24} {:9.1}KB", allocated as f64 / 1024.0);
        }
    }

    // After the last turn, drop the session and see whether the live heap falls (all state was
    // truly owned by the session) or stays (something outlives it).
    drop(session);
    #[cfg(feature = "alloc-stats")]
    {
        let (live, peak, blocks) = mem::live_heap().expect("alloc-stats on");
        eprintln!(
            "after drop(session)          live={:7.2}MB peak_live={:7.2}MB blocks={blocks}",
            mb(live),
            mb(peak)
        );
    }
}
