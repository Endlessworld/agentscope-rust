//! Caller-authorized checks over already persisted evidence, mirroring
//! `io.agentscope.harness.agent.verification.VerificationService`.
//!
//! Verification never runs as a model tool and never re-executes an action: the caller hands
//! in a [`SettledAction`] whose lifecycle fact is already durable, plus a
//! [`DeterministicVerifier`] — a trusted, synchronous, bounded, side-effect-free check (never an
//! LLM, never a command runner). The resulting [`TaskVerification`] is journaled as a
//! `verification/result` fact **before** the model-facing [`TaskContextState`] changes, so a
//! commit that was not acknowledged can never read back as a passing check.
//!
//! Invariants enforced here, in this order (the order matters: each check narrows what the next
//! one may trust):
//!
//! 1. The criterion exists, is an [`RequirementKind::AcceptanceCriterion`] and is
//!    [`RequirementStatus::Confirmed`]. A candidate is a claim, never authority to check.
//! 2. The task context has a pinned subject version, so the evidence binding is defined.
//! 3. The evidence belongs to the settled action being cited and to that exact binding; stale
//!    or unversioned evidence must not be upgraded into a fresh conclusion.
//!
//! A `PASSED` outcome is one bounded check, never a verdict on overall task completion.
//!
//! # Not ported yet
//!
//! The Java service is broader than this module on purpose; the following pieces are absent and
//! each absence is deliberate rather than accidental:
//!
//! - **`StateStoreActionObserver` fallback storage.** Java reloads settled evidence from an
//!   `AgentStateStore` when no recorder is attached and stores the report under
//!   `verification_<id>`. Rust has no equivalent observation store (`ActionObservation` carries
//!   identities only, not evidence), so the caller must supply the evidence and own its
//!   durability. Introducing a store here would create a parallel persistence path next to the
//!   session journal.
//! - **`recorder.findLast("action/end", ...)`.** Rust `SessionRecorder` has no read-back of
//!   buffered/committed facts, and the Rust `action/end` payload has no `evidenceBindings`, so
//!   the evidence is passed in instead of re-read. See [`SettledAction::evidence_binding`].
//! - **`CustomEvent("task_verification")` emission.** Not wired: no in-band observer consumes it
//!   yet, and the durable `verification/result` fact is the source of truth for projections.
//! - **`VerificationService::load(userId, sessionId, id)`.** Loading a *stored report* requires
//!   the state-store key space above; `SessionProjection::verifications` already replays
//!   committed facts. The name-keyed verifier registry is ported as [`load`].
//! - **`recorder.captureState(...)` / `flushNow()`.** A checkpoint after recording needs the
//!   whole `AgentState`; this module only receives the task context, so the owning agent loop
//!   remains responsible for checkpointing after a verification.

mod verifiers;

pub use verifiers::{
    load, JsonResultSchemaVerifier, ShellExitCodeVerifier, JSON_RESULT_SCHEMA_VERIFIER,
    SHELL_EXIT_CODE_VERIFIER,
};

use std::panic::{catch_unwind, AssertUnwindSafe};

use agentscope_core::message::ToolResultBlock;
use agentscope_core::observation::ActionObservation;
use agentscope_core::session::SessionRecorder;
use agentscope_core::state::{
    EvidenceBinding, RequirementKind, RequirementStatus, TaskContextState, TaskVerification,
    VerificationOutcome,
};

use crate::{Error, Result};

/// Trusted, synchronous, bounded, side-effect-free check; never an LLM or command runner.
///
/// Mirrors `DeterministicVerifier`. Implementations must not mutate shared state: the verdict is
/// the only output, and a panic is converted into an [`VerificationOutcome::Error`] report
/// instead of failing the caller.
pub trait DeterministicVerifier: Send + Sync {
    /// Stable identifier persisted as `verifierId`, so a conclusion stays attributable.
    fn id(&self) -> &str;

    /// Checks `evidence` and returns a bounded conclusion.
    fn verify(&self, evidence: &SettledAction) -> Verdict;
}

/// One verifier conclusion. Mirrors `DeterministicVerifier.Verdict`.
///
/// `reason` must be nonblank and at most 2000 characters; [`verify`] enforces that when it builds
/// the [`TaskVerification`] (Java rejects it in the record constructor), so an out-of-bounds
/// reason is an error rather than a truncated fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// What the check concluded.
    pub outcome: VerificationOutcome,
    /// Why, in bounded free text.
    pub reason: String,
}

impl Verdict {
    /// Creates a verdict.
    pub fn new(outcome: VerificationOutcome, reason: impl Into<String>) -> Self {
        Self {
            outcome,
            reason: reason.into(),
        }
    }
}

/// An action whose lifecycle fact is already persisted, together with the evidence it produced.
///
/// Mirrors Java `StoredActionObservation`. The caller constructs this **only after** the action
/// settled: the type documents that contract, it cannot enforce it.
#[derive(Debug, Clone, PartialEq)]
pub struct SettledAction {
    /// The settled observation (action id, execution identity, status).
    pub observation: ActionObservation,
    /// The tool result, when the action returned one.
    pub result: Option<ToolResultBlock>,
    /// The subject/contract binding the action ran under.
    ///
    /// Java keeps this on `ActionObservation`; the Rust observation deliberately carries
    /// identities only (`core::observation` documents the gap), so the harness-side evidence
    /// record holds it. `None` means the action was unversioned and can never verify anything.
    pub evidence_binding: Option<EvidenceBinding>,
}

/// Verifies the settled action `action_id` against the confirmed acceptance criterion
/// `requirement_id`, journals the report and records it on `task`.
///
/// `recorder` is optional: when present the `verification/result` fact must be committed before
/// the task context changes, so an unacknowledged commit leaves the state untouched and the
/// error propagates. Without a recorder the caller owns durability of the returned report.
///
/// Errors are plain messages that match the Java texts verbatim, because they are part of the
/// contract a caller may branch on.
pub async fn verify(
    task: &mut TaskContextState,
    requirement_id: &str,
    action_id: &str,
    evidence: &SettledAction,
    verifier: &dyn DeterministicVerifier,
    recorder: Option<&SessionRecorder>,
) -> Result<TaskVerification> {
    // Snapshot first: every check below must see one revision, and `record_verification` below
    // must fail rather than apply on top of a concurrent change.
    let snapshot = task.snapshot();
    let requirement = snapshot
        .requirements()
        .iter()
        .find(|item| item.id == requirement_id)
        .cloned()
        .ok_or_else(|| Error::Other("Unknown criterion".to_string()))?;
    if requirement.kind != RequirementKind::AcceptanceCriterion
        || requirement.status != RequirementStatus::Confirmed
    {
        return Err(Error::Other(
            "Only confirmed acceptance criteria can be checked".to_string(),
        ));
    }
    // Without a pinned subject version there is no binding, so nothing can be proven current.
    let binding = snapshot
        .evidence_binding()
        .ok_or_else(|| Error::Other("Pin the task subject version before execution".to_string()))?;

    let action = &evidence.observation;
    if action.action_id != action_id {
        return Err(Error::Other("Evidence identity mismatch".to_string()));
    }
    if evidence.evidence_binding.as_ref() != Some(&binding) {
        return Err(Error::Other(
            "Action belongs to a different or unversioned subject/contract".to_string(),
        ));
    }

    // A verifier is caller code, so a panic is a broken check, not a failed assertion: record
    // `ERROR` instead of unwinding into the session.
    let verdict = match catch_unwind(AssertUnwindSafe(|| verifier.verify(evidence))) {
        Ok(verdict) => verdict,
        Err(_) => Verdict::new(
            VerificationOutcome::Error,
            "Verifier failed; no acceptance conclusion",
        ),
    };

    let report = TaskVerification::new(
        uuid::Uuid::new_v4().to_string(),
        requirement_id,
        action_id,
        verifier.id(),
        verdict.outcome,
        verdict.reason,
        binding,
        chrono::Utc::now().timestamp_millis(),
    )?;

    // Persist before updating the model-facing task state: a failed commit cannot pass.
    if let Some(recorder) = recorder {
        let payload = serde_json::json!({
            "verification": &report,
            "criterion": &requirement,
        });
        recorder.record("verification/result", &payload).await?;
    }
    task.record_verification(report.clone(), snapshot.revision())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agentscope_core::message::ToolResultBlock;
    use agentscope_core::observation::ActionStatus;
    use agentscope_core::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionLog, SessionRecorder,
    };
    use agentscope_core::state::{
        RequirementAuthority, RequirementDecision, TaskContextState, TaskScope,
    };

    use super::*;

    const ACTION_ID: &str = "action-1";

    /// A task pinned to `tree-1` with one confirmed acceptance criterion.
    fn task() -> (TaskContextState, String) {
        let mut task = TaskContextState::new();
        task.begin_task(
            TaskScope::new("task-1", "ship the change", "user:1").unwrap(),
            0,
        )
        .unwrap();
        let requirement_id = task
            .propose(
                RequirementKind::AcceptanceCriterion,
                "unit tests pass",
                "user:1",
            )
            .unwrap();
        let revision = task.revision();
        task.decide(
            &requirement_id,
            RequirementStatus::Confirmed,
            RequirementDecision::new(RequirementAuthority::Caller, "caller:1").unwrap(),
            revision,
        )
        .unwrap();
        let revision = task.revision();
        task.set_subject_version("tree-1", revision).unwrap();
        (task, requirement_id)
    }

    /// The settled evidence a caller would hand in for `task`.
    fn evidence(task: &TaskContextState) -> SettledAction {
        SettledAction {
            observation: ActionObservation {
                action_id: ACTION_ID.to_string(),
                tool_call_id: "call-1".to_string(),
                tool_name: "shell".to_string(),
                agent_id: Some("agent".to_string()),
                user_id: Some("u".to_string()),
                session_id: Some("s1".to_string()),
                started_at: 1,
                ended_at: Some(2),
                status: ActionStatus::Returned,
                error_type: None,
                execution_details: None,
            },
            result: Some(ToolResultBlock::text("all green")),
            evidence_binding: task.evidence_binding(),
        }
    }

    struct PassingVerifier;

    impl DeterministicVerifier for PassingVerifier {
        fn id(&self) -> &str {
            "unit-tests"
        }

        fn verify(&self, evidence: &SettledAction) -> Verdict {
            assert_eq!(evidence.observation.action_id, ACTION_ID);
            assert!(evidence.result.is_some());
            Verdict::new(VerificationOutcome::Passed, "observed the expected state")
        }
    }

    struct PanickingVerifier;

    impl DeterministicVerifier for PanickingVerifier {
        fn id(&self) -> &str {
            "panics"
        }

        fn verify(&self, _evidence: &SettledAction) -> Verdict {
            panic!("verifier exploded")
        }
    }

    fn journal() -> Arc<dyn SessionLog> {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        Arc::new(JournalSessionLog::new(storage, "s1"))
    }

    #[tokio::test]
    async fn passed_verdict_is_recorded_on_the_task() {
        let (mut task, requirement_id) = task();
        let evidence = evidence(&task);
        let revision = task.revision();

        let report = verify(
            &mut task,
            &requirement_id,
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.outcome, VerificationOutcome::Passed);
        assert_eq!(report.reason, "observed the expected state");
        assert_eq!(report.requirement_id, requirement_id);
        assert_eq!(report.action_id, ACTION_ID);
        assert_eq!(report.verifier_id, "unit-tests");
        assert_eq!(report.id.len(), 36, "a hyphenated UUID, like Java");
        assert_eq!(report.binding, task.evidence_binding().unwrap());
        assert!(report.recorded_at > 0);

        assert_eq!(task.verifications().len(), 1);
        assert_eq!(task.verifications()[0], report);
        assert_eq!(task.revision(), revision + 1);
    }

    #[tokio::test]
    async fn unconfirmed_or_unknown_criterion_is_refused() {
        let mut task = TaskContextState::new();
        task.begin_task(
            TaskScope::new("task-1", "ship the change", "user:1").unwrap(),
            0,
        )
        .unwrap();
        let candidate = task
            .propose(RequirementKind::AcceptanceCriterion, "tests pass", "user:1")
            .unwrap();
        let revision = task.revision();
        task.set_subject_version("tree-1", revision).unwrap();
        let evidence = evidence(&task);

        let error = verify(
            &mut task,
            &candidate,
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "Only confirmed acceptance criteria can be checked");

        let error = verify(
            &mut task,
            "missing-criterion",
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "Unknown criterion");

        assert!(task.verifications().is_empty());
    }

    #[tokio::test]
    async fn unpinned_subject_is_refused() {
        let mut task = TaskContextState::new();
        task.begin_task(
            TaskScope::new("task-1", "ship the change", "user:1").unwrap(),
            0,
        )
        .unwrap();
        let requirement_id = task
            .propose(RequirementKind::AcceptanceCriterion, "tests pass", "user:1")
            .unwrap();
        let revision = task.revision();
        task.decide(
            &requirement_id,
            RequirementStatus::Confirmed,
            RequirementDecision::new(RequirementAuthority::Caller, "caller:1").unwrap(),
            revision,
        )
        .unwrap();
        assert_eq!(task.evidence_binding(), None);
        let evidence = evidence(&task);

        let error = verify(
            &mut task,
            &requirement_id,
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "Pin the task subject version before execution");
        assert!(task.verifications().is_empty());
    }

    #[tokio::test]
    async fn evidence_identity_and_binding_must_match() {
        let (mut task, requirement_id) = task();
        let mut evidence = evidence(&task);

        evidence.observation.action_id = "another-action".to_string();
        let error = verify(
            &mut task,
            &requirement_id,
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, "Evidence identity mismatch");

        // Unversioned evidence stated as `None`.
        evidence.observation.action_id = ACTION_ID.to_string();
        evidence.evidence_binding = None;
        let error = verify(
            &mut task,
            &requirement_id,
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "Action belongs to a different or unversioned subject/contract"
        );

        // Evidence pinned before the subject moved must not be upgraded to the new version.
        evidence.evidence_binding = task.evidence_binding();
        let revision = task.revision();
        task.set_subject_version("tree-2", revision).unwrap();
        let error = verify(
            &mut task,
            &requirement_id,
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "Action belongs to a different or unversioned subject/contract"
        );

        assert!(task.verifications().is_empty());
    }

    #[tokio::test]
    async fn panicking_verifier_records_an_error_outcome() {
        let (mut task, requirement_id) = task();
        let evidence = evidence(&task);

        let report = verify(
            &mut task,
            &requirement_id,
            ACTION_ID,
            &evidence,
            &PanickingVerifier,
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.outcome, VerificationOutcome::Error);
        assert_eq!(report.reason, "Verifier failed; no acceptance conclusion");
        assert_eq!(report.verifier_id, "panics");
        assert_eq!(task.verifications().len(), 1);
    }

    #[tokio::test]
    async fn recorder_journals_the_fact_and_a_failed_commit_keeps_state_unchanged() {
        let log = journal();
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
            .await
            .unwrap();
        let (mut task, requirement_id) = task();
        let evidence = evidence(&task);

        let report = verify(
            &mut task,
            &requirement_id,
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            Some(&recorder),
        )
        .await
        .unwrap();

        let events = log.read_after(0, 100).await.unwrap();
        let fact = events
            .iter()
            .find(|event| event.event_type == "verification/result")
            .expect("verification/result must be a durable fact");
        let data = fact.data().unwrap();
        assert_eq!(data["verification"]["id"], serde_json::json!(report.id));
        assert_eq!(data["verification"]["outcome"], serde_json::json!("PASSED"));
        assert_eq!(
            data["verification"]["requirementId"],
            serde_json::json!(requirement_id)
        );
        assert_eq!(data["criterion"]["id"], serde_json::json!(requirement_id));
        assert_eq!(task.verifications().len(), 1);

        // A recorder that can no longer acknowledge must fail the call before the model-facing
        // state moves: no report, and the earlier report survives untouched.
        recorder.close().await;
        let revision = task.revision();
        let error = verify(
            &mut task,
            &requirement_id,
            ACTION_ID,
            &evidence,
            &PassingVerifier,
            Some(&recorder),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Session recorder closed"), "{error}");
        assert_eq!(task.revision(), revision);
        assert_eq!(task.verifications().len(), 1);
        assert_eq!(task.verifications()[0].id, report.id);
    }
}
