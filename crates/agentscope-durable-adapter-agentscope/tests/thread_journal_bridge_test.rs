//! The journal bridge must commit native facts and the turn revision in one transaction.

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::journal::{JournalEvent, ThreadJournal};
use agentscope_durable::backend::TxnBackend;
use agentscope_durable::types::*;
use agentscope_durable::SessionBackend;
use agentscope_durable_adapter_agentscope::BackendThreadJournal;
use serde_json::{json, Value};

async fn fixture() -> (
    Arc<TxnBackend>,
    BackendThreadJournal,
    ThreadKey,
    TurnReceipt,
) {
    let backend = Arc::new(TxnBackend::in_memory());
    let session = SessionKey::new("ns", "s1").expect("session key");
    backend
        .create(SessionSpec {
            key: session.clone(),
            primary_binding: ThreadBinding::new("harness-a", "def-1", "state-1").expect("binding"),
            metadata: Value::Null,
        })
        .await
        .expect("create session");
    let thread = ThreadKey::new(session, "main").expect("thread key");
    let receipt = backend
        .submit(&thread, "req-1", json!({ "text": "hi" }))
        .await
        .expect("submit");
    let journal = BackendThreadJournal::new(backend.clone(), thread.clone());
    (backend, journal, thread, receipt)
}

fn event(seq: u64) -> JournalEvent {
    JournalEvent::from_payload(
        format!("evt-{seq}"),
        seq,
        1_700_000_000_000 + seq as i64,
        "assistant.message",
        None,
        None,
        true,
        &json!({ "seq": seq }),
    )
    .expect("event")
}

#[tokio::test]
async fn a_journal_batch_and_its_turn_revision_commit_together() {
    let (backend, journal, thread, receipt) = fixture().await;
    assert_eq!(journal.head().await.expect("head").seq, 0);

    let writer = journal
        .acquire("owner-a", Duration::from_secs(30))
        .await
        .expect("acquire");
    let head = journal
        .commit(&writer, "batch-1", 0, &[event(1), event(2)])
        .await
        .expect("commit");
    assert_eq!(head.seq, 2);

    // One durable commit moved both: the journal position and the turn revision.
    let turn = backend.turn(&thread, &receipt.turn_id).await.expect("turn");
    assert_eq!(turn.revision, 1);
    assert_eq!(turn.status, TurnStatus::Running);

    let events = journal.read_after(0, 10).await.expect("read");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[1].seq, 2);
}

#[tokio::test]
async fn a_stale_journal_position_is_rejected() {
    let (_backend, journal, _thread, _receipt) = fixture().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(30))
        .await
        .expect("acquire");
    journal
        .commit(&writer, "batch-1", 0, &[event(1)])
        .await
        .expect("commit");

    // Replaying the same batch is idempotent, another batch at the taken position is not.
    journal
        .commit(&writer, "batch-1", 0, &[event(1)])
        .await
        .expect("idempotent replay");
    let stale = journal.commit(&writer, "batch-2", 0, &[event(9)]).await;
    assert!(stale.is_err(), "a taken journal position must fail");
    assert_eq!(journal.head().await.expect("head").seq, 1);
}

#[tokio::test]
async fn a_non_contiguous_batch_never_reaches_the_store() {
    let (_backend, journal, _thread, _receipt) = fixture().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(30))
        .await
        .expect("acquire");
    let gap = journal.commit(&writer, "batch-gap", 0, &[event(2)]).await;
    assert!(gap.is_err(), "a gap in the sequence must be rejected");
    assert_eq!(journal.head().await.expect("head").seq, 0);
}

#[tokio::test]
async fn a_second_writer_cannot_acquire_an_active_lease() {
    let (_backend, journal, _thread, _receipt) = fixture().await;
    let writer = journal
        .acquire("owner-a", Duration::from_secs(30))
        .await
        .expect("acquire");
    let blocked = journal.acquire("owner-b", Duration::from_secs(30)).await;
    assert!(
        blocked.is_err(),
        "an active lease must block another writer"
    );

    journal
        .renew(&writer, Duration::from_secs(60))
        .await
        .expect("renew");
    journal.release(&writer).await.expect("release");
    journal
        .acquire("owner-b", Duration::from_secs(30))
        .await
        .expect("takeover after release");
}
