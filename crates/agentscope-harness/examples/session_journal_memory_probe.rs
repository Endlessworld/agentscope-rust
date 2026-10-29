//! Session journal memory probe.
//!
//! Reproduces the EventLog hot path per turn (`start` -> `state/checkpoint` -> `finish`) against
//! a real `SessionRecorder`, and reports allocator counters so the effect of implementation
//! changes is measurable without a model server.
//!
//! Usage:
//!   cargo run --release -p agentscope-harness --example session_journal_memory_probe -- \
//!       --turns 50 --payload-kb 2048 --backend file --root target/probe
//!
//! `--backend none` skips the journal entirely and is the "no history" baseline; `legacy` is not
//! modelled here because Legacy mode does not write a journal at all (that is the point).
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use agentscope_core::message::Msg;
use agentscope_core::session::{JournalSessionLog, RecorderBaseline, SessionLog, SessionRecorder};
use agentscope_core::state::AgentState;
use agentscope_harness::agent::session::{SqliteSessionStorage, WorkspaceSessionStorage};
use agentscope_harness::filesystem::LocalFilesystem;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn record_alloc(size: usize) {
    TOTAL.fetch_add(size, Ordering::Relaxed);
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[inline]
fn record_free(size: usize) {
    LIVE.fetch_sub(size, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_free(layout.size());
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            record_free(layout.size());
            record_alloc(new_size);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    for (index, value) in args.iter().enumerate() {
        if value == name {
            if let Some(next) = args.get(index + 1) {
                return next.clone();
            }
        }
    }
    default.to_string()
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let turns: usize = arg("--turns", "50").parse()?;
    let payload_kb: usize = arg("--payload-kb", "2048").parse()?;
    let seeded_override: usize = arg("--seeded", "0").parse()?;
    let backend = arg("--backend", "file");
    let root = arg("--root", "target/session-probe");

    let root_path = std::path::PathBuf::from(&root);
    std::fs::create_dir_all(&root_path)?;

    let log: Option<Arc<dyn SessionLog>> = match backend.as_str() {
        "none" => None,
        "file" => {
            let storage = Arc::new(WorkspaceSessionStorage::new(Arc::new(
                LocalFilesystem::new(root_path.join("workspace")),
            )));
            Some(Arc::new(JournalSessionLog::new(storage, "probe")))
        }
        "sqlite" => {
            let storage = Arc::new(SqliteSessionStorage::new(&root_path));
            Some(Arc::new(JournalSessionLog::new(storage, "probe")))
        }
        other => return Err(format!("unknown backend: {other}").into()),
    };

    println!("backend={backend} turns={turns} payload_kb={payload_kb}");
    println!("turn,elapsed_ms,total_mb,peak_mb,live_mb");

    let mut state = AgentState::default();
    // Grow the context until its serialized form reaches the requested checkpoint size: that is
    // what a real `state/checkpoint` payload contains (context included), and restore() parses it
    // back as JSON on the next turn.
    let target = payload_kb * 1024;
    let mut seeded = 0usize;
    if seeded_override > 0 {
        // Fixed message count: lets the same workload be compared across formats.
        for index in 0..seeded_override {
            state
                .context
                .push(Msg::user(format!("probe-{index}-{}", "y".repeat(8 * 1024))));
            seeded += 1;
        }
    } else {
        while state.to_json()?.len() < target {
            state.context.push(Msg::user(format!(
                "probe-{seeded}-{}",
                "y".repeat(8 * 1024)
            )));
            seeded += 1;
        }
    }
    println!(
        "seeded_messages={seeded} state_mb={:.2}",
        mb(state.to_json()?.len())
    );
    let input = vec![Msg::user("probe")];

    for turn in 0..turns {
        let started = Instant::now();
        if let Some(log) = &log {
            // One recorder per turn, exactly like the agent loop: `finish` closes it and
            // releases the writer lease. The harness supplies the live state as the baseline
            // (session.rs:1350) so opening the recorder does not replay the journal.
            let phase_start = TOTAL.load(Ordering::Relaxed);
            let head = log.head().await?;
            let baseline = RecorderBaseline {
                seq: head.seq,
                commit_id: head.commit_id.clone(),
                state_json: state.to_json()?,
                uncertain_tool_calls: Default::default(),
                active_runs: Default::default(),
                resumed_turn: false,
            };
            let phase_baseline = TOTAL.load(Ordering::Relaxed);
            let mut recorder = SessionRecorder::open_with_baseline(
                Arc::clone(log),
                Some(&format!("turn-{turn}")),
                None,
                baseline,
            )
            .await?;
            let phase_open = TOTAL.load(Ordering::Relaxed);
            recorder.start(&input, &state).await?;
            let phase_start_rec = TOTAL.load(Ordering::Relaxed);
            // A real turn appends to the context and then checkpoints through `capture_state`
            // (the same entry the checkpoint middleware uses).
            state
                .context
                .push(Msg::user(format!("turn-{turn}-{}", "z".repeat(4 * 1024))));
            recorder.capture_state(&state, "probe")?;
            let phase_record = TOTAL.load(Ordering::Relaxed);
            recorder.finish(&state, "completed").await?;
            let phase_finish = TOTAL.load(Ordering::Relaxed);
            if turn == 0 || turn + 1 == turns {
                println!(
                    "PHASE turn={turn} baseline={:.1} open={:.1} start={:.1} checkpoint={:.1} finish={:.1}",
                    mb(phase_baseline - phase_start),
                    mb(phase_open - phase_baseline),
                    mb(phase_start_rec - phase_open),
                    mb(phase_record - phase_start_rec),
                    mb(phase_finish - phase_record),
                );
            }
        } else {
            // Baseline: keep the same payload alive so the comparison is about the journal,
            // not about the size of the string itself.
            std::hint::black_box(state.to_json()?);
        }
        println!(
            "{turn},{},{:.1},{:.1},{:.1}",
            started.elapsed().as_millis(),
            mb(TOTAL.load(Ordering::Relaxed)),
            mb(PEAK.load(Ordering::Relaxed)),
            mb(LIVE.load(Ordering::Relaxed)),
        );
    }

    println!(
        "SUMMARY total_mb={:.1} peak_mb={:.1} live_mb={:.1} payload_mb={:.1}",
        mb(TOTAL.load(Ordering::Relaxed)),
        mb(PEAK.load(Ordering::Relaxed)),
        mb(LIVE.load(Ordering::Relaxed)),
        mb(payload_kb * 1024),
    );
    Ok(())
}
