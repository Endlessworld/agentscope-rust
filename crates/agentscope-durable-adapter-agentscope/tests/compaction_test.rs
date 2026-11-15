//! Scope compaction and event pruning: what a big session gives up, and what it must not.
//!
//! The trigger is a byte count on the scope document, so reaching it takes a real conversation
//! state written over and over. These tests do exactly that and then check the three claims a
//! compaction makes: the document shrinks back to about one state, a restart folds to the *same*
//! projection field by field, and no fact recovery depends on is dropped — including a tool call
//! that was dispatched and never answered.

use std::path::PathBuf;
use std::sync::Arc;

use agentscope_core::journal::{
    transcript, JournalEvent, JournalProjection, ThreadJournal, Writer,
};
use agentscope_core::message::Msg;
use agentscope_core::state::AgentState;
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::store::file::FileStore;
use agentscope_durable::store::RecordStore;
use agentscope_durable::types::*;
use agentscope_durable::SessionBackend;
use agentscope_durable_adapter_agentscope::{BackendThreadJournal, STATE_VERSION};
use serde_json::{json, Value};

const SESSION: &str = "compaction";
const HARNESS: &str = "durable-host";

/// The call left dispatched and unanswered on purpose.
const CALL_ID: &str = "unanswered";

/// A scratch root outside the repository, unique per call.
fn scratch_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "agentscope-compaction-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

// ---------------------------------------------------------------------------
// Fixture: a file store, one thread, one writer lease
// ---------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
    backend: Arc<TxnBackend>,
    thread: ThreadKey,
    turn_id: String,
    journal: Arc<BackendThreadJournal>,
}

impl Fixture {
    async fn open(name: &str) -> Self {
        let root = scratch_dir(name);
        let store: Arc<dyn RecordStore> = Arc::new(FileStore::open(&root).expect("file store"));
        let backend = Arc::new(TxnBackend::with_store(Arc::clone(&store)));
        let key = SessionKey::new("ns", SESSION).expect("session key");
        backend
            .create(SessionSpec {
                key: key.clone(),
                primary_binding: ThreadBinding::new(HARNESS, "def-1", STATE_VERSION)
                    .expect("binding"),
                metadata: Value::Null,
            })
            .await
            .expect("create session");
        let thread = ThreadKey::new(key, "main").expect("thread key");
        let receipt = backend
            .submit(&thread, "req-1", json!({ "text": "go" }))
            .await
            .expect("submit");
        let journal = Arc::new(BackendThreadJournal::new(
            Arc::clone(&backend) as Arc<dyn SessionBackend>,
            thread.clone(),
        ));
        journal
            .acquire("compactor", std::time::Duration::from_secs(120))
            .await
            .expect("writer lease");
        Self {
            root,
            backend,
            thread,
            turn_id: receipt.turn_id,
            journal,
        }
    }

    fn writer(&self) -> Writer {
        let lease = self.journal.lease().expect("lease");
        Writer {
            owner: lease.owner.clone(),
            epoch: lease.epoch,
        }
    }

    /// One journal batch, exactly as the recorder would hand it over.
    async fn commit(&self, batch: &str, facts: &[(&str, Value)]) {
        let head = self.journal.head().await.expect("head");
        let events: Vec<JournalEvent> = facts
            .iter()
            .enumerate()
            .map(|(index, (event_type, payload))| {
                let seq = head.seq + 1 + index as u64;
                JournalEvent::from_payload(
                    format!("{batch}-{seq}"),
                    seq,
                    1_700_000_000_000 + seq as i64,
                    *event_type,
                    None,
                    Some(self.turn_id.clone()),
                    true,
                    payload,
                )
                .expect("journal event")
            })
            .collect();
        self.journal
            .commit(&self.writer(), batch, head.seq, &events)
            .await
            .expect("journal batch");
    }

    /// A batch that rewrites the same conversation state, as every checkpoint of a turn does.
    async fn commit_state(&self, batch: &str, state: &AgentState) {
        self.commit(
            batch,
            &[
                ("message/assistant", message_payload("working")),
                ("state/checkpoint", checkpoint_payload(state)),
            ],
        )
        .await;
    }

    fn scope_bytes(&self) -> u64 {
        std::fs::read_dir(self.root.join("scopes"))
            .expect("scopes")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.metadata().map(|meta| meta.len()).unwrap_or(0))
            .sum()
    }

    /// The session root record of the one scope this fixture writes.
    ///
    /// What a compaction measured is durable metadata the session API does not expose, so a test
    /// that asks whether the *next* commit paid for another trim has to read it.
    fn root_record(&self) -> Value {
        let path = std::fs::read_dir(self.root.join("scopes"))
            .expect("scopes")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .expect("one scope document");
        let document: Value =
            serde_json::from_slice(&std::fs::read(path).expect("scope")).expect("json");
        document
            .get("")
            .cloned()
            .expect("the root record is the empty part")
    }

    /// How many checkpoint payloads in the scope document still carry a state.
    ///
    /// The payloads are JSON *strings* inside the entries, so the count is the number of times the
    /// field name occurs in the document bytes. A superseded checkpoint keeps its entry but its
    /// payload is replaced by a marker, which is exactly what this counts.
    fn state_payloads(&self) -> usize {
        let path = std::fs::read_dir(self.root.join("scopes"))
            .expect("scopes")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .expect("one scope document");
        let document = std::fs::read_to_string(&path).expect("scope");
        document.matches("stateJson").count()
    }

    /// A reader that shares nothing with the writer but the directory, i.e. a restart.
    fn reopen(&self) -> Arc<BackendThreadJournal> {
        let store: Arc<dyn RecordStore> =
            Arc::new(FileStore::open(&self.root).expect("reopened store"));
        let backend = Arc::new(TxnBackend::with_store(store));
        Arc::new(BackendThreadJournal::new(
            backend as Arc<dyn SessionBackend>,
            self.thread.clone(),
        ))
    }
}

fn message_payload(text: &str) -> Value {
    json!({ "message": serde_json::to_value(Msg::assistant(text)).expect("message") })
}

/// The payload shape core's `checkpoint_payload` writes: the state as a JSON *string*.
fn checkpoint_payload(state: &AgentState) -> Value {
    json!({
        "projectorVersion": 1,
        "stateJson": serde_json::to_string(state).expect("state json"),
        "reason": "tick",
    })
}

/// A conversation state of roughly `per_message` bytes per message.
fn big_state(messages: usize, per_message: usize) -> AgentState {
    let mut state = AgentState::builder()
        .session_id(SESSION)
        .user_id("alice")
        .build();
    for index in 0..messages {
        state
            .context_mut()
            .push(Msg::user(format!("{index}:{}", "x".repeat(per_message))));
    }
    state
}

/// The projection without its position, which is the only field a new fact may move.
fn comparable(projection: &JournalProjection) -> (Option<String>, Vec<String>, Vec<String>, usize) {
    (
        projection.state_json.clone(),
        projection.uncertain_tool_calls.iter().cloned().collect(),
        projection.active_runs.iter().cloned().collect(),
        projection.verifications.len(),
    )
}

fn transcript_messages(transcript: &agentscope_core::journal::Transcript) -> Vec<String> {
    transcript
        .messages
        .iter()
        .map(|message| format!("{}:{}", message.id, message.text_content()))
        .collect()
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_scope_over_the_threshold_is_compacted_without_losing_a_fact() {
    let fixture = Fixture::open("compaction").await;
    // ~12 KB of conversation per checkpoint: under the blob threshold, so the payload stays in the
    // document and is what a trim can still give up. (A payload above the threshold is stored out
    // of line, which leaves nothing here to reclaim.)
    let state = big_state(4, 3_000);
    let state_bytes = serde_json::to_string(&state).expect("state json").len() as u64;

    // The first batch also dispatches a call that is never answered.
    fixture
        .commit(
            "batch-tool",
            &[
                (
                    "tool/requested",
                    json!({ "call": {
                        "type": "tool_use",
                        "id": CALL_ID,
                        "name": "slow",
                        "input": {},
                    }}),
                ),
                ("tool/dispatch", json!({ "toolCallId": CALL_ID })),
                ("state/checkpoint", checkpoint_payload(&state)),
            ],
        )
        .await;

    let mut previous_bytes = fixture.scope_bytes();
    // The tool batch above and every batch below carries one checkpoint entry.
    let mut checkpoints = 1usize;
    let mut previous_projection = JournalProjection::read(&*fixture.reopen())
        .await
        .expect("projection");
    let mut previous_transcript = transcript(&*fixture.reopen()).await.expect("transcript");
    let mut compacted: Option<(u64, u64)> = None;
    // Enough batches of a 12 KB state to pass one mebibyte.
    for index in 0..120 {
        fixture
            .commit_state(&format!("batch-{index}"), &state)
            .await;
        checkpoints += 1;
        let bytes = fixture.scope_bytes();
        let projection = JournalProjection::read(&*fixture.reopen())
            .await
            .expect("projection");
        let rebuilt = transcript(&*fixture.reopen()).await.expect("transcript");
        if bytes + 4 * state_bytes < previous_bytes {
            assert_eq!(
                comparable(&previous_projection),
                comparable(&projection),
                "compaction must not change what the journal folds to"
            );
            // The compacting batch adds one message of its own, so the transcript grows by
            // exactly that: nothing it held before may disappear.
            let now = transcript_messages(&rebuilt);
            let held = transcript_messages(&previous_transcript);
            assert_eq!(
                &now[..held.len()],
                held.as_slice(),
                "the transcript is rebuilt from entries compaction keeps"
            );
            compacted = Some((previous_bytes, bytes));
            break;
        }
        previous_bytes = bytes;
        previous_projection = projection;
        previous_transcript = rebuilt;
    }
    let (before, after) = compacted.expect("a compaction must have run");
    eprintln!("COMPACT scope_bytes {before} -> {after} (state {state_bytes})");

    // (a) the document fell back to one state and the tail the journal keeps.
    assert!(before > 1 << 20, "the trigger is the mebibyte threshold");
    assert!(
        after * 4 < before,
        "the compacted scope is much smaller: {after} after {before}"
    );
    assert_eq!(
        fixture.state_payloads(),
        1,
        "only the newest checkpoint still carries a state, every older payload is a marker"
    );

    // (b) a restart folds to the same state, the same uncertainty and the same runs.
    let reader = fixture.reopen();
    let projection = JournalProjection::read(&*reader).await.expect("projection");
    assert_eq!(comparable(&projection), comparable(&previous_projection));
    let restored = projection.restore().expect("restore").expect("a state");
    assert_eq!(restored, state, "every field of the state survives");
    // Only the current checkpoint position is left, and it is still readable.
    let positions = fixture
        .backend
        .checkpoints(&fixture.thread, 0, 100)
        .await
        .expect("checkpoints");
    let current = fixture
        .backend
        .thread_state(&fixture.thread)
        .await
        .expect("thread state")
        .checkpoint_metadata
        .expect("metadata")
        .position
        .parse::<u64>()
        .expect("position");
    assert_eq!(
        positions.last().copied(),
        Some(current),
        "the current position is kept"
    );
    let view = fixture
        .backend
        .checkpoint(&fixture.thread, current)
        .await
        .expect("the current checkpoint is still readable");
    assert_eq!(
        serde_json::from_value::<AgentState>(view.checkpoint).expect("state"),
        state
    );

    // (c) the dispatched call that was never answered is still a durable record, and the turn
    // still reports it as unsettled: a compaction must never turn a lost outcome into "no call".
    let execution = fixture
        .backend
        .execution(&fixture.thread, &format!("{}:{CALL_ID}", fixture.turn_id))
        .await
        .expect("the dispatch survived the compaction");
    assert!(execution.dispatch_attempted, "the call is still dispatched");
    assert!(
        execution.result.is_none(),
        "no result may be invented by a compaction"
    );
    let lease = fixture.journal.lease().expect("lease");
    let frame = fixture.backend.frame(&lease).await.expect("frame");
    assert!(
        frame.tools.iter().any(|tool| tool.result.is_none()),
        "the unsettled tool is still open: {:?}",
        frame.tools
    );
    // The entries that name the facts are still there, so a reconciliation still finds them.
    let events = agent_events(&fixture).await;
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "tool/dispatch"),
        "the dispatch entry survives compaction"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "state/checkpoint")
            .count(),
        checkpoints,
        "every checkpoint entry stays, only its payload is given up"
    );

    let _ = std::fs::remove_dir_all(&fixture.root);
}

async fn agent_events(fixture: &Fixture) -> Vec<JournalEvent> {
    let state = fixture
        .backend
        .thread_state(&fixture.thread)
        .await
        .expect("thread state");
    if state.journal_position == 0 {
        return Vec::new();
    }
    fixture
        .backend
        .read_journal(&fixture.thread, 0, 1000)
        .await
        .expect("journal")
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<JournalEvent>(entry).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Event retention
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paging_after_a_prune_returns_every_retained_event_exactly_once() {
    let fixture = Fixture::open("prune-pages").await;
    let state = big_state(1, 10);
    for index in 0..8 {
        fixture
            .commit_state(&format!("batch-{index}"), &state)
            .await;
    }
    let total: u64 = fixture
        .backend
        .snapshot(&fixture.thread.session)
        .await
        .expect("snapshot")
        .sequence
        .parse()
        .expect("sequence");
    assert!(total >= 8, "the commits wrote public events: {total}");

    let keep = 5;
    let watermark = total - keep;
    let dropped = fixture
        .backend
        .prune_events(&fixture.thread.session, keep)
        .await
        .expect("prune");
    assert_eq!(dropped, watermark, "everything up to the watermark");

    // What survived, read in one page, is the reference the paged walk has to reproduce.
    let retained = fixture
        .backend
        .events(&fixture.thread.session, None, 100)
        .await
        .expect("the retained window");
    let expected: Vec<u64> = retained
        .events
        .iter()
        .map(|event| event.sequence.parse::<u64>().expect("sequence"))
        .collect();
    assert_eq!(
        expected,
        (watermark + 1..=total).collect::<Vec<u64>>(),
        "exactly the newest window is retained"
    );

    // Two events at a time, from the retained start, following the cursor: no page may ask for a
    // limit the backend rejects, and the walk must be complete, ordered and duplicate-free.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..32 {
        let page = fixture
            .backend
            .events(&fixture.thread.session, cursor.as_deref(), 2)
            .await
            .expect("a page after the prune");
        assert!(page.events.len() <= 2, "a page honours its limit");
        for event in &page.events {
            seen.push(event.sequence.parse::<u64>().expect("sequence"));
        }
        let next = page.cursor.clone();
        if page.events.is_empty() || next == cursor {
            break;
        }
        cursor = next;
    }
    assert_eq!(seen, expected, "every retained event once, in order");

    // A cursor into the pruned prefix is still refused, so a client learns to load a snapshot
    // instead of silently missing the events it had not read yet.
    let stale = format!("{}:1", fixture.thread.session.storage_key());
    let refused = fixture
        .backend
        .events(&fixture.thread.session, Some(&stale), 2)
        .await
        .expect_err("a cursor below the watermark is refused");
    assert!(
        format!("{refused}").contains("snapshot"),
        "the refusal says what to do: {refused}"
    );

    let _ = std::fs::remove_dir_all(&fixture.root);
}

#[tokio::test]
async fn pruning_events_keeps_the_newest_window_and_refuses_a_stale_cursor() {
    let fixture = Fixture::open("prune-events").await;
    let state = big_state(1, 10);
    for index in 0..6 {
        fixture
            .commit_state(&format!("batch-{index}"), &state)
            .await;
    }
    let before = fixture
        .backend
        .snapshot(&fixture.thread.session)
        .await
        .expect("snapshot");
    let total: u64 = before.sequence.parse().expect("sequence");
    assert!(total > 5, "the commits wrote public events: {total}");

    let dropped = fixture
        .backend
        .prune_events(&fixture.thread.session, 3)
        .await
        .expect("prune");
    assert_eq!(dropped, total - 3, "everything outside the newest three");

    // A client with no cursor reads from the retained start instead of being refused.
    let page = fixture
        .backend
        .events(&fixture.thread.session, None, 100)
        .await
        .expect("events from the retained start");
    assert_eq!(page.events.len(), 3, "exactly the newest three");

    // A client with a cursor below the watermark is told to load a snapshot.
    let stale = format!("{}:1", fixture.thread.session.storage_key());
    let refused = fixture
        .backend
        .events(&fixture.thread.session, Some(&stale), 10)
        .await;
    assert!(
        refused.is_err(),
        "a cursor into pruned history must be refused: {refused:?}"
    );

    // Pruning again keeps nothing it already dropped.
    let again = fixture
        .backend
        .prune_events(&fixture.thread.session, 3)
        .await
        .expect("prune");
    assert_eq!(again, 0);

    let _ = std::fs::remove_dir_all(&fixture.root);
}

// ---------------------------------------------------------------------------
// Required extension facts
// ---------------------------------------------------------------------------

/// A compaction must keep the finalization frames a replay needs.
///
/// They are `required` extension events, so a reader that does not find them (or cannot decode
/// them) stops recovery instead of continuing with half a turn. Nothing about them is
/// reclaimable: they are journal entries, not `state/checkpoint` payloads, checkpoint history
/// records or public `event:` rows, which are the only three things a compaction gives up.
#[tokio::test]
async fn a_compaction_keeps_the_finalization_frames_a_replay_needs() {
    use agentscope_durable_adapter_agentscope::{FINALIZATION_COMPLETED, FINALIZATION_STARTED};

    let fixture = Fixture::open("compaction-finalization").await;
    let state = big_state(4, 3_000);
    let state_bytes = serde_json::to_string(&state).expect("state json").len() as u64;
    let reply = Msg::assistant("first");

    // The facts a finished prompt leaves: the reply frame, the state it belongs to, and the
    // completion of that frame.
    fixture
        .commit(
            "batch-finalization",
            &[
                (
                    FINALIZATION_STARTED,
                    json!({ "id": "frame-1", "result": reply }),
                ),
                ("message/assistant", message_payload("first")),
                ("state/checkpoint", checkpoint_payload(&state)),
                (FINALIZATION_COMPLETED, json!({ "id": "frame-1" })),
            ],
        )
        .await;

    // Push the scope past the mebibyte threshold, exactly as a long session does.
    let mut previous_bytes = fixture.scope_bytes();
    let mut compacted = false;
    for index in 0..120 {
        fixture
            .commit_state(&format!("batch-{index}"), &state)
            .await;
        let bytes = fixture.scope_bytes();
        if bytes + 4 * state_bytes < previous_bytes {
            compacted = true;
            break;
        }
        previous_bytes = bytes;
    }
    assert!(compacted, "a compaction must have run");

    // Both frames are still journal entries, still in order, still naming the same frame.
    let frames: Vec<(String, String)> = agent_events(&fixture)
        .await
        .into_iter()
        .filter(|event| event.event_type.starts_with("durable/"))
        .map(|event| {
            let id = event
                .data()
                .ok()
                .and_then(|data| data.get("id").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            (event.event_type, id)
        })
        .collect();
    assert_eq!(
        frames,
        [
            (FINALIZATION_STARTED.to_string(), "frame-1".to_string()),
            (FINALIZATION_COMPLETED.to_string(), "frame-1".to_string()),
        ],
        "the opening frame survives with the completion it pairs with"
    );

    // And the replay that used to stop still folds over them: required facts are validated
    // while the folded prefix is read, so a dropped or unregistered one fails here.
    JournalProjection::read(&*fixture.reopen())
        .await
        .expect("a replay after compaction still reads the required frames");

    let _ = std::fs::remove_dir_all(&fixture.root);
}

// ---------------------------------------------------------------------------
// The cost of a trim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_scope_that_cannot_shrink_is_not_walked_again_for_a_few_kilobytes() {
    let fixture = Fixture::open("trim-guard").await;
    // A scope that cannot shrink: a transcript of messages, each below the blob threshold so it
    // stays in the document, and no fact a trim is allowed to give up (no superseded checkpoint
    // payload, no event outside the retention window). This is the scope that used to pay for
    // three key walks on every commit and get smaller by nothing. One batch, so the commit that
    // trims is the one after it and not one inside the loop.
    let text = "z".repeat(12 * 1024);
    let facts: Vec<(&str, Value)> = (0..90)
        .map(|index| {
            (
                "message/assistant",
                message_payload(&format!("{index}:{text}")),
            )
        })
        .collect();
    fixture.commit("batch-big", &facts).await;
    let over = fixture.scope_bytes();
    assert!(over > 1 << 20, "the scope is over the threshold: {over}");

    // The commit that finds it over the threshold trims, and cannot know what it left behind.
    fixture
        .commit(
            "batch-trim",
            &[("message/assistant", message_payload("one"))],
        )
        .await;
    let mid = fixture.root_record();
    assert_eq!(
        mid["compacted_bytes"].as_u64(),
        Some(0),
        "a trim cannot measure its own result"
    );
    assert_eq!(mid["compact_pending"].as_bool(), Some(true));

    // The commit after it does, and is what the next trim is measured against.
    fixture
        .commit(
            "batch-measure",
            &[("message/assistant", message_payload("two"))],
        )
        .await;
    let recorded = fixture.root_record();
    let after = recorded["compacted_bytes"]
        .as_u64()
        .expect("the commit after a trim records its size");
    assert_eq!(recorded["compact_pending"].as_bool(), Some(false));
    assert!(after > 1 << 20, "this scope did not shrink: {after}");

    // Two more commits of a few hundred bytes each. Neither is a tenth of the scope, so neither
    // pays for the three walks again: the recorded size stands unchanged while the scope is still
    // over the threshold, which is what makes the skip the guard and not the threshold.
    for index in 0..2 {
        fixture
            .commit(
                &format!("batch-small-{index}"),
                &[("message/assistant", message_payload("growth"))],
            )
            .await;
        let now = fixture.root_record();
        assert_eq!(
            now["compacted_bytes"].as_u64(),
            Some(after),
            "no trim may run for a few hundred bytes"
        );
        assert_eq!(now["compact_pending"].as_bool(), Some(false));
        assert!(
            fixture.scope_bytes() > 1 << 20,
            "still over the threshold, so the growth guard is what skipped the trim"
        );
    }

    let _ = std::fs::remove_dir_all(&fixture.root);
}
