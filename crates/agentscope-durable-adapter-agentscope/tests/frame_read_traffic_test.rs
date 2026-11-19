//! What `frame()` reads out of one scope document.
//!
//! A tick builds the frame several times, and a frame is three records (thread, turn, checkpoint).
//! Reading it through a whole-scope transaction used to parse the entire session document — every
//! journal event of every past turn — into a value tree just to pull those three out, and the peak
//! grew with the journal behind the turn. This pins the fix: a `frame()` on a file store parses no
//! whole document, it picks the parts it needs.
//!
//! The traffic counters are process-global, so this binary holds one test.

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::journal::JournalEvent;
use agentscope_core::state::AgentState;
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::store::file::FileStore;
use agentscope_durable::store::RecordStore;
use agentscope_durable::traffic::{read_stats, reset_read_stats};
use agentscope_durable::types::*;
use agentscope_durable::SessionBackend;
use serde_json::json;

#[tokio::test]
async fn a_frame_picks_its_three_records_instead_of_parsing_the_whole_scope() {
    let dir = std::env::temp_dir().join(format!(
        "durable-frame-traffic-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let store: Arc<dyn RecordStore> = Arc::new(FileStore::open(&dir).expect("file store"));
    let backend = Arc::new(TxnBackend::with_store(store));
    let key = SessionKey::new("ns", "s-frame-traffic").expect("session key");
    backend
        .create(SessionSpec {
            key: key.clone(),
            primary_binding: ThreadBinding::new("durable-host", "def-1", "agentscope/state/1")
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

    // A history of committed facts, plus the checkpoint that carries the state they produced: the
    // whole-scope transaction this test refuses would parse all of it into a value tree.
    let state = AgentState::builder()
        .session_id("s-frame-traffic")
        .user_id("alice")
        .cur_iter(2)
        .build();
    let entries: Vec<serde_json::Value> = (1..=500)
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

    reset_read_stats();
    let frame = backend.frame(&lease).await.expect("frame");
    let stats = read_stats();

    assert_eq!(
        stats.scope_parses, 0,
        "frame() must not parse a whole scope document (stats {stats:?})"
    );
    assert!(
        stats.scope_picks >= 1,
        "frame() should pick its parts, not parse (stats {stats:?})"
    );
    // The frame still carries the committed checkpoint it was built from.
    assert_eq!(
        frame.checkpoint,
        serde_json::to_value(&state).expect("state json")
    );

    let _ = std::fs::remove_dir_all(&dir);
}
