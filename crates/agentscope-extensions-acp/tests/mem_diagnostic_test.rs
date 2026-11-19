//! OFFLINE DIAGNOSTIC ONLY (read-only investigation aid).
//!
//! Simulates the exact allocation shape in question: every tick rebuilds a ~1MB state
//! (serialize -> deserialize, like checkpoint encode/decode), keeps only the latest and drops
//! the previous one. Compares OS-visible commit against live heap, and against a control that
//! retains every version. Run with:
//!
//!   cargo test -p agentscope-extensions-acp --features alloc-stats --test mem_diagnostic_test -- --nocapture

use agentscope_extensions_acp::mem;

#[cfg(feature = "alloc-stats")]
#[global_allocator]
static ALLOC: mem::counting::CountingAlloc = mem::counting::CountingAlloc;

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Build a checkpoint-like JSON value of roughly `target` bytes: a context of messages, each with
/// a sizeable content blob, mirroring a state whose history grows with the conversation.
fn build_state(turn: usize, msgs: usize, msg_bytes: usize) -> serde_json::Value {
    let mut context = Vec::with_capacity(msgs);
    for i in 0..msgs {
        let mut content = String::with_capacity(msg_bytes);
        let tag = format!("t{turn:04}-m{i:04}-");
        while content.len() < msg_bytes {
            content.push_str(&tag);
            content.push_str("0123456789abcdef");
        }
        content.truncate(msg_bytes);
        context.push(serde_json::json!({
            "role": if i % 2 == 0 { "user" } else { "assistant" },
            "content": [{ "type": "text", "text": content }],
            "id": format!("msg-{turn}-{i}"),
        }));
    }
    serde_json::json!({
        "session_id": "diag",
        "context": context,
        "reply_id": format!("reply-{turn}"),
    })
}

fn footprint(label: &str) {
    let u = mem::usage();
    #[cfg(feature = "alloc-stats")]
    {
        let (live, peak, blocks) = mem::counting::snapshot();
        eprintln!(
            "{label:<34} commit={:7.1}MB peak_commit={:7.1}MB | live={:7.1}MB peak_live={:7.1}MB blocks={}",
            mb(u.commit),
            mb(u.peak_commit),
            mb(live),
            mb(peak),
            blocks
        );
    }
    #[cfg(not(feature = "alloc-stats"))]
    eprintln!(
        "{label:<34} commit={:7.1}MB peak_commit={:7.1}MB (no live heap: build with alloc-stats)",
        mb(u.commit),
        mb(u.peak_commit)
    );
}

/// Scenario A: the user's assertion. Every tick rebuilds state, keeps only the newest.
/// If commit stays flat here, full state encode/decode + dropping old versions is NOT the cause
/// of the 184MB growth.
#[test]
#[ignore = "offline diagnostic: run with --ignored to see the memory curve"]
fn diag_a_only_latest_kept() {
    let base = mem::usage();
    eprintln!("\n=== A: rebuild ~1MB state/tick, keep only latest ===");
    eprintln!("baseline commit={:.1}MB", mb(base.commit));

    let msgs = 12;
    let msg_bytes = 90 * 1024; // ~1.08 MB per state
    let mut latest: Option<serde_json::Value> = None;
    for turn in 0..120 {
        let state = build_state(turn, msgs, msg_bytes);
        let json = serde_json::to_string(&state).expect("encode");
        let value: serde_json::Value = serde_json::from_str(&json).expect("decode");
        // drop previous, keep only newest
        latest = Some(value);
        if (turn + 1) % 30 == 0 {
            footprint(&format!("A turn {turn} (latest kept)"));
        }
    }
    drop(latest);
    mem::collect();
    footprint("A after drop+collect");
}

/// Scenario B: control. Keep every version alive (proves the commit metric actually moves when
/// memory is genuinely retained, so a flat scenario A is meaningful rather than a broken metric).
#[test]
#[ignore = "offline diagnostic: run with --ignored to see the memory curve"]
fn diag_b_keep_every_version() {
    eprintln!("\n=== B: control, keep ALL versions alive ===");
    let msgs = 12;
    let msg_bytes = 90 * 1024;
    let mut all: Vec<serde_json::Value> = Vec::new();
    for turn in 0..40 {
        let state = build_state(turn, msgs, msg_bytes);
        let json = serde_json::to_string(&state).expect("encode");
        all.push(serde_json::from_str(&json).expect("decode"));
        if (turn + 1) % 10 == 0 {
            footprint(&format!("B turn {turn} ({n} kept)", n = all.len()));
        }
    }
    footprint("B held (no collect)");
    let held = mb(mem::usage().commit);
    drop(all);
    mem::collect();
    footprint("B after drop+collect");
    eprintln!("B commit while held: {held:.1}MB");
}

/// Scenario C: growing history (the real shape). Each tick the context grows by one message,
/// rebuilt from scratch. This tests whether an ever-growing *live* context (even if only one
/// copy exists) raises commit monotonically.
#[test]
#[ignore = "offline diagnostic: run with --ignored to see the memory curve"]
fn diag_c_growing_live_context() {
    eprintln!("\n=== C: live context grows every tick (one copy) ===");
    let msg_bytes = 8 * 1024;
    let mut latest: Option<serde_json::Value> = None;
    for msgs in (4..=200).step_by(16) {
        let state = build_state(msgs, msgs, msg_bytes);
        let json = serde_json::to_string(&state).expect("encode");
        latest = Some(serde_json::from_str(&json).expect("decode"));
        footprint(&format!("C {msgs} msgs (one live copy)"));
    }
    drop(latest);
    mem::collect();
    footprint("C after drop+collect");
}
