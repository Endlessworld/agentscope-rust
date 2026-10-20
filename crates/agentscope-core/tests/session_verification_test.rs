//! `TaskVerification` data model + `verification/result` replay into `SessionProjection`.
//!
//! Scope: the durable record shape, its validation, and the read-only replay pipe. Nothing here
//! decides when a verification runs or what it asserts — that orchestration is the caller's job.

use std::sync::Arc;
use std::time::Duration;

use agentscope_core::session::{
    AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionEvent, SessionLog,
    SessionProjection, Writer,
};
use agentscope_core::state::{AgentState, EvidenceBinding, TaskVerification, VerificationOutcome};
use serde_json::json;

fn event(seq: u64, kind: &str, payload: serde_json::Value) -> SessionEvent {
    SessionEvent::new(
        format!("e{seq}"),
        seq,
        0,
        kind,
        Some("run-1".to_string()),
        Some("t1".to_string()),
        true,
        payload.to_string(),
    )
    .expect("valid event envelope")
}

async fn journal() -> (Arc<dyn SessionLog>, Writer) {
    let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
    let log: Arc<dyn SessionLog> = Arc::new(JournalSessionLog::new(Arc::clone(&storage), "s1"));
    let writer = log
        .acquire("run-1", Duration::from_secs(60))
        .await
        .expect("writer lease");
    (log, writer)
}

async fn commit(log: &dyn SessionLog, writer: &Writer, batch: &str, events: &[SessionEvent]) {
    let head = log.head().await.expect("head");
    log.commit(writer, batch, head.seq, events)
        .await
        .expect("commit");
}

fn binding() -> EvidenceBinding {
    EvidenceBinding::new("task-1", 3, "workspace@v7").expect("valid binding")
}

fn verification(id: &str, outcome: VerificationOutcome) -> TaskVerification {
    TaskVerification::new(
        id,
        "req-1",
        "call-1",
        "verifier-a",
        outcome,
        "order ORD-42 is confirmed",
        binding(),
        1_700_000_000_000,
    )
    .expect("valid verification")
}

fn checkpoint(seq: u64, summary: &str) -> SessionEvent {
    let mut state = AgentState::builder().session_id("s1").build();
    state.summary = summary.to_string();
    event(
        seq,
        "state/checkpoint",
        json!({ "stateJson": state.to_json().expect("state json") }),
    )
}

fn verification_event(seq: u64, report: &TaskVerification) -> SessionEvent {
    event(
        seq,
        "verification/result",
        json!({ "verification": serde_json::to_value(report).expect("serializable report") }),
    )
}

// ---------------------------------------------------------------------------------------------
// (a) a committed verification/result is replayed with every field intact
// ---------------------------------------------------------------------------------------------
#[tokio::test]
async fn committed_verification_is_replayed_with_its_fields() {
    let (log, writer) = journal().await;
    let report = verification("ver-1", VerificationOutcome::Passed);
    commit(&*log, &writer, "b1", &[verification_event(1, &report)]).await;

    let projection = SessionProjection::read(&*log).await.expect("projection");
    assert_eq!(projection.verifications.len(), 1);
    let replayed = &projection.verifications[0];
    assert_eq!(replayed, &report);
    assert_eq!(replayed.id, "ver-1");
    assert_eq!(replayed.requirement_id, "req-1");
    assert_eq!(replayed.action_id, "call-1");
    assert_eq!(replayed.verifier_id, "verifier-a");
    assert_eq!(replayed.outcome, VerificationOutcome::Passed);
    assert_eq!(replayed.reason, "order ORD-42 is confirmed");
    assert_eq!(replayed.recorded_at, 1_700_000_000_000);
    assert_eq!(replayed.binding.task_id, "task-1");
    assert_eq!(replayed.binding.contract_version, 3);
    assert_eq!(replayed.binding.subject_version, "workspace@v7");
}

// ---------------------------------------------------------------------------------------------
// (b) replaying the same id again overwrites rather than duplicating
// ---------------------------------------------------------------------------------------------
#[tokio::test]
async fn same_id_committed_twice_keeps_one_latest_record() {
    let (log, writer) = journal().await;
    let first = verification("ver-1", VerificationOutcome::Failed);
    let second = verification("ver-1", VerificationOutcome::Passed);
    commit(&*log, &writer, "b1", &[verification_event(1, &first)]).await;
    commit(&*log, &writer, "b2", &[verification_event(2, &second)]).await;

    let projection = SessionProjection::read(&*log).await.expect("projection");
    assert_eq!(projection.verifications.len(), 1, "deduplicated by id");
    assert_eq!(projection.verifications[0].outcome, VerificationOutcome::Passed);
    assert_eq!(projection.verifications[0], second);
}

// ---------------------------------------------------------------------------------------------
// (c) a checkpoint drops verifications recorded before it
// ---------------------------------------------------------------------------------------------
#[tokio::test]
async fn checkpoint_clears_verifications_recorded_before_it() {
    let (log, writer) = journal().await;
    let stale = verification("ver-old", VerificationOutcome::Failed);
    let fresh = verification("ver-new", VerificationOutcome::Passed);
    commit(
        &*log,
        &writer,
        "b1",
        &[verification_event(1, &stale), checkpoint(2, "first")],
    )
    .await;
    commit(&*log, &writer, "b2", &[verification_event(3, &fresh)]).await;

    let projection = SessionProjection::read(&*log).await.expect("projection");
    assert_eq!(projection.verifications.len(), 1);
    assert_eq!(projection.verifications[0].id, "ver-new");
    assert_eq!(projection.restore().expect("restore").unwrap().summary, "first");
}

// ---------------------------------------------------------------------------------------------
// (d) a structurally invalid payload fails the read instead of being skipped
// ---------------------------------------------------------------------------------------------
#[tokio::test]
async fn malformed_verification_payload_fails_the_projection() {
    let (log, writer) = journal().await;
    let mut value = serde_json::to_value(verification("ver-1", VerificationOutcome::Passed))
        .expect("serializable report");
    value["outcome"] = json!("SUCCEDED"); // misspelled: must not silently degrade to UNKNOWN
    commit(
        &*log,
        &writer,
        "b1",
        &[event(1, "verification/result", json!({ "verification": value }))],
    )
    .await;

    let error = SessionProjection::read(&*log)
        .await
        .expect_err("malformed verification must be an error");
    let message = error.to_string();
    assert!(message.contains("SUCCEDED"), "{message}");
}

// A missing `verification` key mirrors the Java `data.get("verification") != null` guard: skip.
#[tokio::test]
async fn verification_event_without_a_payload_is_skipped() {
    let (log, writer) = journal().await;
    commit(
        &*log,
        &writer,
        "b1",
        &[event(1, "verification/result", json!({ "criterion": "unused" }))],
    )
    .await;

    let projection = SessionProjection::read(&*log).await.expect("projection");
    assert!(projection.verifications.is_empty());
}

// ---------------------------------------------------------------------------------------------
// (e) bounded-field validation returns Err (never panics)
// ---------------------------------------------------------------------------------------------
#[test]
fn blank_verification_id_is_rejected() {
    let error = TaskVerification::new(
        "  ",
        "req-1",
        "call-1",
        "verifier-a",
        VerificationOutcome::Passed,
        "ok",
        binding(),
        0,
    )
    .expect_err("blank id must be rejected");
    assert!(error.to_string().contains("verification ID"), "{error}");
}

#[test]
fn oversized_reason_is_rejected() {
    let error = TaskVerification::new(
        "ver-1",
        "req-1",
        "call-1",
        "verifier-a",
        VerificationOutcome::Failed,
        "x".repeat(2001),
        binding(),
        0,
    )
    .expect_err("overlong reason must be rejected");
    assert!(error.to_string().contains("at most 2000"), "{error}");
}

#[test]
fn at_the_limit_reason_and_id_are_accepted() {
    let report = TaskVerification::new(
        "v".repeat(200),
        "req-1",
        "call-1",
        "verifier-a",
        VerificationOutcome::Unknown,
        "r".repeat(2000),
        binding(),
        0,
    )
    .expect("exact-limit values are valid");
    assert_eq!(report.reason.len(), 2000);
}

#[test]
fn negative_contract_version_is_rejected() {
    let error = EvidenceBinding::new("task-1", -1, "workspace@v7")
        .expect_err("negative contract version must be rejected");
    assert!(error.to_string().contains("Negative contract version"), "{error}");
}

#[test]
fn verification_carries_the_java_wire_field_names() {
    let value = serde_json::to_value(verification("ver-1", VerificationOutcome::Error))
        .expect("serializable report");
    assert_eq!(value["requirementId"], json!("req-1"));
    assert_eq!(value["actionId"], json!("call-1"));
    assert_eq!(value["verifierId"], json!("verifier-a"));
    assert_eq!(value["recordedAt"], json!(1_700_000_000_000i64));
    assert_eq!(value["outcome"], json!("ERROR"));
    assert_eq!(value["binding"]["contractVersion"], json!(3));
    assert_eq!(value["binding"]["subjectVersion"], json!("workspace@v7"));

    let back: TaskVerification = serde_json::from_value(value).expect("round trip");
    assert_eq!(back, verification("ver-1", VerificationOutcome::Error));
}
