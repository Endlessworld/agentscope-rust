//! What one restart reads out of the journal.
//!
//! A tick could not decide anything until it had replayed the whole thread journal, and the
//! backend served that replay one page at a time from the beginning. These are the numbers: an
//! `open` on a thread with a long committed history reads the entries the checkpoint does not
//! already cover, not the history in front of them.
//!
//! The counters are process-global, so this file holds one test: a second one would run beside it
//! and be measured into it.

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::agent::AgentExecutionPosition;
use agentscope_core::journal::JournalEvent;
use agentscope_core::message::Msg;
use agentscope_core::state::AgentState;
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::traffic::{read_stats, reset_read_stats};
use agentscope_durable::types::*;
use agentscope_durable::SessionBackend;
use agentscope_durable_adapter_agentscope::AgentScopeExecutionHost;
use serde_json::json;

/// How many facts the thread committed before the invocation this test opens.
const HISTORY: u64 = 2_500;

async fn running_turn() -> (Arc<TxnBackend>, ThreadKey, Lease) {
    let backend = Arc::new(TxnBackend::in_memory());
    let key = SessionKey::new("ns", "s-traffic").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new("durable-host", "def-1", "state-1")
                .expect("binding"),
            metadata: serde_json::Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(key, "main").expect("thread key");
    backend
        .submit(&thread, "req-1", json!({ "text": "go" }))
        .await
        .expect("submit");
    let lease = backend
        .claim(&thread, "owner-a", Duration::from_secs(60))
        .await
        .expect("claim")
        .expect("a claimable turn");
    (backend, thread, lease)
}

#[tokio::test]
async fn a_restart_reads_the_journal_from_the_checkpoint_not_from_the_start() {
    let (backend, thread, lease) = running_turn().await;
    // The history of the thread, committed the way the journal bridge commits it: one entry per
    // fact, plus the state checkpoint that carries the result of all of them.
    let state = AgentState::builder()
        .session_id("s-traffic")
        .user_id("alice")
        .cur_iter(3)
        .add_message(Msg::user("go"))
        .build();
    let entries: Vec<serde_json::Value> = (1..=HISTORY)
        .map(|index| {
            let event = JournalEvent::from_payload(
                format!("seed-{index}"),
                index,
                1_700_000_000_000 + index as i64,
                "message/assistant",
                Some("run-0".to_string()),
                Some(lease.turn_id.clone()),
                true,
                &json!({ "index": index, "text": "x".repeat(64) }),
            )
            .expect("journal event");
            serde_json::to_value(event).expect("entry")
        })
        .collect();
    let turn = backend.turn(&thread, &lease.turn_id).await.expect("turn");
    let mut commit = Commit::new("seed", turn.revision, TurnStatus::Running);
    commit.journal = Some(JournalAppend {
        expected_position: 0,
        entries,
    });
    commit.checkpoint = serde_json::to_value(&state).expect("state json");
    backend.commit(&lease, commit).await.expect("seed commit");
    let seeded = backend.thread_state(&thread).await.expect("thread state");
    assert_eq!(seeded.journal_position, HISTORY, "the history is committed");

    reset_read_stats();
    let started = std::time::Instant::now();
    let host = AgentScopeExecutionHost::open(
        Arc::clone(&backend) as Arc<dyn SessionBackend>,
        thread.clone(),
        lease.clone(),
        "run-1",
    )
    .await
    .expect("host");
    let elapsed = started.elapsed();
    let stats = read_stats();
    eprintln!(
        "open with {HISTORY} committed facts: {} entries in {} page(s), {elapsed:?}",
        stats.journal_entries, stats.journal_reads
    );

    // The point of the change: what an `open` reads follows the checkpoint, not the history.
    // Before the cursor was pushed down and the replay started at the checkpoint, the same call
    // read all 2,500 entries in three pages; now it reads the single entry that produced the
    // committed state.
    assert_eq!(
        stats.journal_entries, 1,
        "one entry, not {HISTORY} (elapsed {elapsed:?}, stats {stats:?})"
    );
    assert_eq!(stats.journal_reads, 1, "one page, not three ({stats:?})");
    // The covered entry is what proves this turn already ran, so its input is not delivered again.
    assert!(
        host.turn_started(),
        "a turn whose checkpoint covers its facts already ran"
    );
    assert_eq!(
        host.position(),
        AgentExecutionPosition::new(3, 2),
        "the checkpoint is still the source of the iteration"
    );
}
