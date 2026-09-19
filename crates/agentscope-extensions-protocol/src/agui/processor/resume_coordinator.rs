//! Interrupt resume contract coordination, mirroring `AguiResumeCoordinator`.
//!
//! The coordinator is scoped to the request-processor layer: the adapter and converters stay
//! stateless with respect to open interrupts, while the processor remembers the latest interrupt
//! outcome per AG-UI thread and validates the next request against the official resume contract.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

use agentscope_core::RuntimeContext;

use crate::agui::adapter::agent_adapter::put_resume_interrupts;
use crate::agui::constants::TOOL_CALL_INTERRUPT_REASON;
use crate::agui::event::{AguiEvent, Interrupt, RunFinishedOutcome};
use crate::agui::model::RunAgentInput;

/// The AG-UI error code emitted when the resume contract is violated.
pub const CONTRACT_ERROR_CODE: &str = "AGUI_INTERRUPT_CONTRACT_ERROR";

/// The result of validating a run input against the open-interrupt contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeContractResult {
    /// Processing may continue.
    Proceed,
    /// Processing must stop with the given error message.
    Error(String),
}

impl ResumeContractResult {
    /// Whether the result is an error.
    pub fn is_error(&self) -> bool {
        matches!(self, ResumeContractResult::Error(_))
    }

    /// The error message, when present.
    pub fn message(&self) -> Option<&str> {
        match self {
            ResumeContractResult::Error(message) => Some(message),
            ResumeContractResult::Proceed => None,
        }
    }

    /// The proceed result.
    pub fn proceed() -> Self {
        ResumeContractResult::Proceed
    }

    /// A contract error with the given message.
    pub fn error(message: impl Into<String>) -> Self {
        ResumeContractResult::Error(message.into())
    }
}

/// Tracks open interrupts and the active run per AG-UI thread.
#[derive(Debug, Default)]
pub struct AguiResumeCoordinator {
    pending_interrupts_by_thread: RwLock<BTreeMap<String, BTreeMap<String, Interrupt>>>,
    active_runs_by_thread: RwLock<BTreeMap<String, String>>,
}

impl AguiResumeCoordinator {
    /// Creates an empty coordinator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates whether the input satisfies the currently supported AG-UI resume contract.
    ///
    /// A thread with no open interrupts must not send `resume`; a thread with open interrupts
    /// must send exactly one entry per open interrupt, each with a supported status.
    pub fn validate(&self, input: &RunAgentInput) -> ResumeContractResult {
        let pending = self.pending_interrupts_for(&input.thread_id);
        if pending.is_empty() {
            return if input.has_resume() {
                ResumeContractResult::error("RunAgentInput.resume does not match any open interrupt")
            } else {
                ResumeContractResult::proceed()
            };
        }

        if !input.has_resume() {
            return ResumeContractResult::error(
                "Thread has unresolved interrupts; RunAgentInput.resume must address all of them",
            );
        }

        if let ResumeContractResult::Error(message) = validate_resume_statuses(&input.resume) {
            return ResumeContractResult::Error(message);
        }

        let mut resume_ids: BTreeSet<String> = BTreeSet::new();
        for resume in &input.resume {
            if !resume_ids.insert(resume.interrupt_id.clone()) {
                return ResumeContractResult::error(format!(
                    "RunAgentInput.resume contains duplicate interruptId: {}",
                    resume.interrupt_id
                ));
            }
        }

        if resume_ids != pending.keys().cloned().collect::<BTreeSet<_>>() {
            let pending_ids: BTreeSet<&String> = pending.keys().collect();
            let missing: Vec<&String> = pending_ids
                .iter()
                .filter(|id| !resume_ids.contains(**id))
                .map(|id| *id)
                .collect();
            let unknown: Vec<&String> = resume_ids
                .iter()
                .filter(|id| !pending.contains_key(*id))
                .collect();
            return ResumeContractResult::error(format!(
                "RunAgentInput.resume must cover all open interrupts. missing={missing:?}, unknown={unknown:?}"
            ));
        }

        ResumeContractResult::proceed()
    }

    /// Marks a run as active for its thread.
    ///
    /// AG-UI thread execution is serialized because unresolved interrupt state and AgentScope
    /// session state are both scoped by `threadId`; allowing concurrent runs on one thread would
    /// make the final interrupt state depend on completion order.
    pub fn begin_run(&self, input: &RunAgentInput) -> ResumeContractResult {
        let contract = self.validate(input);
        if contract.is_error() {
            return contract;
        }
        let mut active = self.active_runs_by_thread.write().unwrap_or_else(|e| e.into_inner());
        match active.get(&input.thread_id) {
            Some(run_id) => ResumeContractResult::error(format!(
                "Thread already has an active run; wait for run {run_id} to finish before starting \
                 another run on the same thread"
            )),
            None => {
                active.insert(input.thread_id.clone(), input.run_id.clone());
                ResumeContractResult::proceed()
            }
        }
    }

    /// Clears the active-run marker if it still belongs to the finishing run.
    pub fn finish_run(&self, thread_id: &str, run_id: &str) {
        let mut active = self.active_runs_by_thread.write().unwrap_or_else(|e| e.into_inner());
        if active.get(thread_id).map(String::as_str) == Some(run_id) {
            active.remove(thread_id);
        }
    }

    /// Whether the thread currently has an active run.
    pub fn has_active_run(&self, thread_id: &str) -> bool {
        self.active_runs_by_thread
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(thread_id)
    }

    /// The open interrupts of a thread, keyed by interrupt id.
    pub fn pending_interrupts_for(&self, thread_id: &str) -> BTreeMap<String, Interrupt> {
        self.pending_interrupts_by_thread
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(thread_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Adds known originating interrupts to the runtime context used for resume conversion.
    ///
    /// Only `tool_call` interrupts with a stable tool-call id are passed through: those are the
    /// only ones whose identity the resume path can recover.
    pub fn add_resume_interrupts(
        &self,
        input: &RunAgentInput,
        runtime_context: RuntimeContext,
    ) -> RuntimeContext {
        if !input.has_resume() {
            return runtime_context;
        }
        let pending = self.pending_interrupts_for(&input.thread_id);
        if pending.is_empty() {
            return runtime_context;
        }
        let mut resume_interrupts: BTreeMap<String, Interrupt> = BTreeMap::new();
        for resume in &input.resume {
            if let Some(interrupt) = pending.get(&resume.interrupt_id) {
                if should_pass_resume_interrupt(interrupt) {
                    resume_interrupts.insert(resume.interrupt_id.clone(), interrupt.clone());
                }
            }
        }
        if resume_interrupts.is_empty() {
            return runtime_context;
        }
        let mut context = runtime_context;
        put_resume_interrupts(&mut context, &resume_interrupts.into_iter().collect());
        context
    }

    /// Tracks interrupt outcomes emitted by the adapter for subsequent resume requests.
    ///
    /// `run_error_seen` prevents a late `RUN_FINISHED` following a `RUN_ERROR` from clearing the
    /// interrupt state that the error left behind.
    pub fn track_pending_interrupts(
        &self,
        thread_id: &str,
        run_id: &str,
        event: &AguiEvent,
        run_error_seen: bool,
    ) {
        let AguiEvent::RunFinished { outcome, .. } = event else {
            return;
        };
        let active = self.active_runs_by_thread.read().unwrap_or_else(|e| e.into_inner());
        if active.get(thread_id).map(String::as_str) != Some(run_id) {
            return;
        }
        drop(active);

        let mut pending = self.pending_interrupts_by_thread.write().unwrap_or_else(|e| e.into_inner());
        match outcome {
            Some(RunFinishedOutcome::Interrupt { interrupts }) if !interrupts.is_empty() => {
                let mut by_id = BTreeMap::new();
                for interrupt in interrupts {
                    by_id.insert(interrupt.id.clone(), interrupt.clone());
                }
                pending.insert(thread_id.to_string(), by_id);
            }
            _ if !run_error_seen => {
                pending.remove(thread_id);
            }
            _ => {}
        }
    }

    /// Builds the AG-UI error lifecycle used when the resume contract is violated.
    ///
    /// `emit_run_finished_after_error` appends `RUN_FINISHED` for legacy clients.
    pub fn contract_error_events(
        &self,
        input: &RunAgentInput,
        message: &str,
        emit_run_finished_after_error: bool,
    ) -> Vec<AguiEvent> {
        let mut events = vec![
            AguiEvent::RunStarted {
                thread_id: input.thread_id.clone(),
                run_id: input.run_id.clone(),
                parent_run_id: None,
                input: serde_json::to_value(input).ok(),
                timestamp: None,
                raw_event: None,
            },
            AguiEvent::RunError {
                thread_id: input.thread_id.clone(),
                run_id: input.run_id.clone(),
                message: message.to_string(),
                code: Some(CONTRACT_ERROR_CODE.to_string()),
                timestamp: Some(chrono::Utc::now().timestamp_millis()),
                raw_event: None,
            },
        ];
        if emit_run_finished_after_error {
            events.push(AguiEvent::RunFinished {
                thread_id: input.thread_id.clone(),
                run_id: input.run_id.clone(),
                result: None,
                outcome: None,
                timestamp: None,
                raw_event: None,
            });
        }
        events
    }
}

/// Validates that every resume entry uses a supported status.
fn validate_resume_statuses(resumes: &[crate::agui::model::AguiResume]) -> ResumeContractResult {
    for resume in resumes {
        if !resume.is_resolved() && !resume.is_cancelled() {
            return ResumeContractResult::error(format!(
                "RunAgentInput.resume contains unsupported status: {}",
                resume.status
            ));
        }
    }
    ResumeContractResult::proceed()
}

/// Whether an interrupt's identity can be recovered by the resume path.
fn should_pass_resume_interrupt(interrupt: &Interrupt) -> bool {
    if interrupt.reason != TOOL_CALL_INTERRUPT_REASON {
        return false;
    }
    interrupt
        .tool_call_id
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agui::model::AguiResume;
    use serde_json::json;

    fn run_input(thread_id: &str, run_id: &str) -> RunAgentInput {
        RunAgentInput::of(thread_id, run_id)
    }

    fn resume(id: &str, status: &str) -> AguiResume {
        AguiResume::new(id, status, Some(json!("payload")))
    }

    fn interrupt(id: &str, tool_call_id: Option<&str>) -> Interrupt {
        let mut interrupt = Interrupt::new(id, TOOL_CALL_INTERRUPT_REASON);
        interrupt.tool_call_id = tool_call_id.map(str::to_string);
        interrupt
    }

    #[test]
    fn a_thread_without_open_interrupts_cannot_send_resume() {
        let coordinator = AguiResumeCoordinator::new();
        assert_eq!(
            coordinator.validate(&run_input("t", "r")),
            ResumeContractResult::Proceed
        );
        let with_resume = run_input("t", "r").with_resume(vec![resume(
            "i1",
            AguiResume::STATUS_RESOLVED,
        )]);
        let result = coordinator.validate(&with_resume);
        assert!(result.is_error());
        assert_eq!(
            result.message(),
            Some("RunAgentInput.resume does not match any open interrupt")
        );
    }

    #[test]
    fn a_thread_with_open_interrupts_requires_a_full_resume_set() {
        let coordinator = AguiResumeCoordinator::new();
        // Seed the open interrupts through a finished run.
        let input = run_input("t", "r1");
        assert!(!coordinator.begin_run(&input).is_error());
        coordinator.track_pending_interrupts(
            "t",
            "r1",
            &AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r1".to_string(),
                result: None,
                outcome: Some(RunFinishedOutcome::Interrupt {
                    interrupts: vec![interrupt("i1", Some("c1")), interrupt("i2", Some("c2"))],
                }),
                timestamp: None,
                raw_event: None,
            },
            false,
        );
        assert_eq!(coordinator.pending_interrupts_for("t").len(), 2);

        // Omitting one of them is rejected.
        let partial = run_input("t", "r2").with_resume(vec![resume(
            "i1",
            AguiResume::STATUS_RESOLVED,
        )]);
        let result = coordinator.validate(&partial);
        assert!(result.is_error());
        assert!(result.message().unwrap().contains("missing="));
        assert!(result.message().unwrap().contains("i2"));

        // Addressing an unknown interrupt is rejected too.
        let unknown = run_input("t", "r3").with_resume(vec![
            resume("i1", AguiResume::STATUS_RESOLVED),
            resume("i2", AguiResume::STATUS_RESOLVED),
            resume("i9", AguiResume::STATUS_RESOLVED),
        ]);
        let result = coordinator.validate(&unknown);
        assert!(result.is_error());
        assert!(result.message().unwrap().contains("unknown"));
        assert!(result.message().unwrap().contains("i9"));

        // A complete set is accepted.
        let complete = run_input("t", "r4").with_resume(vec![
            resume("i1", AguiResume::STATUS_RESOLVED),
            resume("i2", AguiResume::STATUS_CANCELLED),
        ]);
        assert_eq!(coordinator.validate(&complete), ResumeContractResult::Proceed);
    }

    #[test]
    fn duplicate_and_unsupported_resume_entries_are_rejected() {
        let coordinator = AguiResumeCoordinator::new();
        let input = run_input("t", "r1");
        coordinator.begin_run(&input);
        coordinator.track_pending_interrupts(
            "t",
            "r1",
            &AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r1".to_string(),
                result: None,
                outcome: Some(RunFinishedOutcome::Interrupt {
                    interrupts: vec![interrupt("i1", Some("c1"))],
                }),
                timestamp: None,
                raw_event: None,
            },
            false,
        );

        let duplicate = run_input("t", "r2").with_resume(vec![
            resume("i1", AguiResume::STATUS_RESOLVED),
            resume("i1", AguiResume::STATUS_RESOLVED),
        ]);
        let result = coordinator.validate(&duplicate);
        assert!(result.is_error());
        assert!(result.message().unwrap().contains("duplicate interruptId"));

        let unsupported = run_input("t", "r3").with_resume(vec![resume("i1", "pending")]);
        let result = coordinator.validate(&unsupported);
        assert!(result.is_error());
        assert!(result.message().unwrap().contains("unsupported status"));
    }

    #[test]
    fn a_thread_without_resume_cannot_start_when_interrupts_are_open() {
        let coordinator = AguiResumeCoordinator::new();
        coordinator.begin_run(&run_input("t", "r1"));
        coordinator.track_pending_interrupts(
            "t",
            "r1",
            &AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r1".to_string(),
                result: None,
                outcome: Some(RunFinishedOutcome::Interrupt {
                    interrupts: vec![interrupt("i1", Some("c1"))],
                }),
                timestamp: None,
                raw_event: None,
            },
            false,
        );
        let result = coordinator.begin_run(&run_input("t", "r2"));
        assert!(result.is_error());
        assert!(result.message().unwrap().contains("unresolved interrupts"));
        let _ = CONTRACT_ERROR_CODE;
    }

    #[test]
    fn runs_on_the_same_thread_are_serialized() {
        let coordinator = AguiResumeCoordinator::new();
        assert_eq!(
            coordinator.begin_run(&run_input("t", "r1")),
            ResumeContractResult::Proceed
        );
        assert!(coordinator.has_active_run("t"));
        let second = coordinator.begin_run(&run_input("t", "r2"));
        assert!(second.is_error());
        assert!(second.message().unwrap().contains("already has an active run"));
        assert!(second.message().unwrap().contains("r1"));

        // A different thread is unaffected.
        assert!(!coordinator.begin_run(&run_input("other", "r3")).is_error());

        // Finishing with a stale run id does not clear the marker.
        coordinator.finish_run("t", "r2");
        assert!(coordinator.has_active_run("t"));
        coordinator.finish_run("t", "r1");
        assert!(!coordinator.has_active_run("t"));
        assert_eq!(
            coordinator.begin_run(&run_input("t", "r4")),
            ResumeContractResult::Proceed
        );
    }

    #[test]
    fn a_clean_finish_clears_the_pending_interrupts() {
        let coordinator = AguiResumeCoordinator::new();
        coordinator.begin_run(&run_input("t", "r1"));
        coordinator.track_pending_interrupts(
            "t",
            "r1",
            &AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r1".to_string(),
                result: None,
                outcome: Some(RunFinishedOutcome::Success),
                timestamp: None,
                raw_event: None,
            },
            false,
        );
        assert!(coordinator.pending_interrupts_for("t").is_empty());
    }

    #[test]
    fn a_run_error_preserves_the_last_known_interrupt_state() {
        let coordinator = AguiResumeCoordinator::new();
        coordinator.begin_run(&run_input("t", "r1"));
        coordinator.track_pending_interrupts(
            "t",
            "r1",
            &AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r1".to_string(),
                result: None,
                outcome: Some(RunFinishedOutcome::Interrupt {
                    interrupts: vec![interrupt("i1", Some("c1"))],
                }),
                timestamp: None,
                raw_event: None,
            },
            false,
        );
        assert_eq!(coordinator.pending_interrupts_for("t").len(), 1);
        // A legacy RUN_FINISHED after RUN_ERROR must not clear the open interrupt.
        coordinator.track_pending_interrupts(
            "t",
            "r1",
            &AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r1".to_string(),
                result: None,
                outcome: None,
                timestamp: None,
                raw_event: None,
            },
            true,
        );
        assert_eq!(coordinator.pending_interrupts_for("t").len(), 1);
    }

    #[test]
    fn events_for_other_runs_or_other_event_types_are_ignored() {
        let coordinator = AguiResumeCoordinator::new();
        coordinator.begin_run(&run_input("t", "r1"));
        // A stale run id is ignored.
        coordinator.track_pending_interrupts(
            "t",
            "r9",
            &AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r9".to_string(),
                result: None,
                outcome: Some(RunFinishedOutcome::Interrupt {
                    interrupts: vec![interrupt("i1", Some("c1"))],
                }),
                timestamp: None,
                raw_event: None,
            },
            false,
        );
        assert!(coordinator.pending_interrupts_for("t").is_empty());
        // Non-terminal events are ignored.
        coordinator.track_pending_interrupts(
            "t",
            "r1",
            &AguiEvent::RunStarted {
                thread_id: "t".to_string(),
                run_id: "r1".to_string(),
                parent_run_id: None,
                input: None,
                timestamp: None,
                raw_event: None,
            },
            false,
        );
        assert!(coordinator.pending_interrupts_for("t").is_empty());
    }

    #[test]
    fn resume_interrupts_are_passed_only_for_correlatable_tool_calls() {
        let coordinator = AguiResumeCoordinator::new();
        coordinator.begin_run(&run_input("t", "r1"));
        let mut schema_interrupt = Interrupt::new("i3", "input_required");
        schema_interrupt.tool_call_id = Some("c3".to_string());
        let mut blank_interrupt = Interrupt::new("i2", TOOL_CALL_INTERRUPT_REASON);
        blank_interrupt.tool_call_id = Some("   ".to_string());
        coordinator.track_pending_interrupts(
            "t",
            "r1",
            &AguiEvent::RunFinished {
                thread_id: "t".to_string(),
                run_id: "r1".to_string(),
                result: None,
                outcome: Some(RunFinishedOutcome::Interrupt {
                    interrupts: vec![
                        interrupt("i1", Some("c1")),
                        blank_interrupt,
                        schema_interrupt,
                    ],
                }),
                timestamp: None,
                raw_event: None,
            },
            false,
        );
        let input = run_input("t", "r2").with_resume(vec![
            resume("i1", AguiResume::STATUS_RESOLVED),
            resume("i2", AguiResume::STATUS_RESOLVED),
            resume("i3", AguiResume::STATUS_RESOLVED),
        ]);
        let context = coordinator.add_resume_interrupts(&input, RuntimeContext::default());
        let passed = crate::agui::adapter::resume_interrupts(&context);
        assert_eq!(passed.len(), 1);
        assert!(passed.contains_key("i1"));
    }

    #[test]
    fn add_resume_interrupts_is_a_noop_without_resume_entries() {
        let coordinator = AguiResumeCoordinator::new();
        let context = coordinator.add_resume_interrupts(
            &run_input("t", "r1"),
            RuntimeContext::default(),
        );
        assert!(crate::agui::adapter::resume_interrupts(&context).is_empty());
    }

    #[test]
    fn contract_errors_use_the_agui_interrupt_error_code() {
        let coordinator = AguiResumeCoordinator::new();
        let input = run_input("t", "r");
        let events = coordinator.contract_error_events(&input, "bad resume", false);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type().unwrap().as_str(), "RUN_STARTED");
        match &events[1] {
            AguiEvent::RunError { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("AGUI_INTERRUPT_CONTRACT_ERROR"));
                assert_eq!(message, "bad resume");
            }
            other => panic!("expected RunError, got {other:?}"),
        }

        let legacy = coordinator.contract_error_events(&input, "bad resume", true);
        assert_eq!(legacy.len(), 3);
        assert_eq!(legacy[2].event_type().unwrap().as_str(), "RUN_FINISHED");
    }

    #[test]
    fn contract_result_helpers_expose_errors() {
        assert!(!ResumeContractResult::proceed().is_error());
        assert_eq!(ResumeContractResult::proceed().message(), None);
        let error = ResumeContractResult::error("nope");
        assert!(error.is_error());
        assert_eq!(error.message(), Some("nope"));
    }
}
