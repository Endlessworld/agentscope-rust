//! The bounded per-invocation event fold JEV supervision assesses
//! (`JevSupervisionMiddleware.Capture`).
//!
//! Java keeps one `Capture` per invocation: it subscribes to the worker stream, retains only the
//! **event type names** (32 of them), a bounded output tail and a few booleans, and hands the
//! resulting `JevTaskSupervisor.Snapshot` to the supervisor. Nothing here stores thinking text, tool
//! arguments or tool-result payloads.
//!
//! The worker's terminal state is **not** inferred from boundary events. Since
//! `fix(jev): classify supervision outcomes from agent results` (Java `6ec57d245`) the outcome comes
//! from the agent's final result:
//!
//! * `SessionRecorder.outcome(generateReason)` (ported as [`agentscope_core::session::outcome`])
//!   decides `failed` / `interrupted` / `suspended` / `completed`;
//! * a suspended result keeps `worker_active` true, so a pause while waiting for confirmation or a
//!   suspended tool can never make the task eligible for completion review;
//! * `EXCEED_MAX_ITERS` still fails fast, but only for the **own** agent's event — iteration limits
//!   and results forwarded from a child (`source` set) cannot overwrite the parent's outcome;
//! * `REQUEST_STOP` / `ALL_TOOLS_DENIED` are no longer failure signals by themselves: a worker that
//!   continued successfully after an all-tools-denied boundary must stay `CONTINUE`.

use std::collections::VecDeque;
use std::time::Instant;

use agentscope_core::event::{AgentEvent, EventData};
use agentscope_core::message::{ContentBlock, Msg};
use agentscope_core::session::outcome;

use crate::jev::supervision::evidence::SupervisionEvidence;
use crate::jev::supervision::middleware::append_tail;
use crate::jev::supervision::supervisor::{Scope, Snapshot};

/// How many recent event type names the capture retains (Java `events.size() == 32`).
pub const MAX_RECENT_EVENTS: usize = 32;

/// The bounded fold of one invocation (`JevSupervisionMiddleware.Capture`).
#[derive(Debug)]
pub struct SupervisionCapture {
    scope: Scope,
    task: String,
    text_limit: usize,
    started: Instant,
    sequence: u64,
    recent_events: VecDeque<String>,
    output: String,
    truncated: bool,
    completed: bool,
    worker_failed: bool,
    worker_suspended: bool,
}

impl SupervisionCapture {
    /// Starts a capture for `scope` over the invocation's input messages.
    ///
    /// `text_limit` is `supervisor.limits().text_chars` in Java; both the task text and the output
    /// tail are clipped to it, and any clipping (or any non-text block) sets `truncated`.
    pub fn new(scope: Scope, messages: &[Msg], text_limit: usize) -> Self {
        let text_limit = text_limit.max(1);
        let mut task = String::new();
        let mut truncated = false;
        for message in messages {
            truncated |= message
                .content
                .iter()
                .any(|block| !matches!(block, ContentBlock::Text(_)));
            let text = message.text_content();
            if text.is_empty() {
                continue;
            }
            // Java reserves one character so the trailing newline still fits the budget.
            let remaining = text_limit as i64 + 1 - task.chars().count() as i64;
            if remaining > 0 {
                let take = (text.chars().count() as i64).min(remaining) as usize;
                task.extend(text.chars().take(take));
            }
            if task.chars().count() <= text_limit {
                task.push('\n');
            }
        }
        Self {
            scope,
            task,
            text_limit,
            started: Instant::now(),
            sequence: 0,
            recent_events: VecDeque::with_capacity(MAX_RECENT_EVENTS),
            output: String::new(),
            truncated,
            completed: false,
            worker_failed: false,
            worker_suspended: false,
        }
    }

    /// The run scope this capture belongs to.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The observation sequence number (one per retained event plus one for completion).
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The bounded task text.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// The bounded output tail.
    pub fn output(&self) -> &str {
        &self.output
    }

    /// Whether anything was clipped.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Whether the capture saw the worker's final result.
    pub fn completed(&self) -> bool {
        self.completed
    }

    /// Whether the worker's own result was failed or interrupted.
    pub fn worker_failed(&self) -> bool {
        self.worker_failed
    }

    /// Whether the worker's own result paused (confirmation/tool/hook/middleware stop).
    pub fn worker_suspended(&self) -> bool {
        self.worker_suspended
    }

    /// The retained recent event type names, oldest first.
    pub fn recent_events(&self) -> Vec<String> {
        self.recent_events.iter().cloned().collect()
    }

    /// Folds one worker event (`JevSupervisionMiddleware.Capture.event`).
    pub fn event(&mut self, event: &AgentEvent) {
        let event_type = event.event_type().wire_name();
        if event_type.starts_with("THINKING_") {
            return;
        }
        self.sequence += 1;
        let own_event = is_own_event(event.data());
        if own_event && event_type == "EXCEED_MAX_ITERS" {
            // An iteration limit is the agent's own terminal signal and keeps failing immediately.
            self.worker_failed = true;
        }
        if self.recent_events.len() == MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }
        self.recent_events.push_back(event_type.to_string());
        if let AgentEvent::TextBlockDelta(data) = event {
            if let Some(delta) = data.fields.get("delta").and_then(serde_json::Value::as_str) {
                if append_tail(&mut self.output, delta, self.text_limit) {
                    self.truncated = true;
                }
            }
        }
        if event_type.starts_with("DATA_BLOCK_") {
            self.truncated = true;
        }
        if let AgentEvent::AgentResult(data) = event {
            if let Some(reply) = reply_message(data) {
                if own_event {
                    // Phase boundaries are not terminal outcomes: classify the Session lifecycle
                    // instead, so a pause never reads as a failure and a denial that the worker
                    // continued past is not a failure either.
                    match outcome(reply.generate_reason()) {
                        "failed" | "interrupted" => self.worker_failed = true,
                        "suspended" => self.worker_suspended = true,
                        _ => {}
                    }
                }
                self.truncated |= reply
                    .content
                    .iter()
                    .any(|block| !matches!(block, ContentBlock::Text(_)));
                // The final text replaces the streamed tail on both sides of the port.
                self.output.clear();
                if append_tail(&mut self.output, &reply.text_content(), self.text_limit) {
                    self.truncated = true;
                }
            }
        }
    }

    /// Marks the worker stream complete (`doOnComplete(capture::complete)`).
    pub fn complete(&mut self) {
        self.completed = true;
        self.sequence += 1;
    }

    /// Whether the worker is still active (`!complete || workerSuspended`).
    pub fn worker_active(&self) -> bool {
        !self.completed || self.worker_suspended
    }

    /// Builds the supervisor snapshot (`JevTaskSupervisor.Snapshot`).
    pub fn snapshot(&self, evidence: SupervisionEvidence) -> Snapshot {
        Snapshot {
            scope: self.scope.clone(),
            sequence: self.sequence,
            task: self.task.clone(),
            output_tail: self.output.clone(),
            recent_events: self.recent_events(),
            worker_active: self.worker_active(),
            worker_failed: self.worker_failed,
            truncated: self.truncated,
            elapsed_millis: self.started.elapsed().as_millis() as u64,
            evidence,
        }
    }
}

/// Whether the event was emitted by the current agent rather than forwarded from a child.
///
/// Java: `event.getSource() == null || event.getSource().isBlank()`.
fn is_own_event(data: &EventData) -> bool {
    data.source
        .as_deref()
        .map(|source| source.trim().is_empty())
        .unwrap_or(true)
}

/// The reply message an `AGENT_RESULT` event carries.
fn reply_message(data: &EventData) -> Option<Msg> {
    data.fields
        .get("reply")
        .and_then(|value| serde_json::from_value::<Msg>(value.clone()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::EventData;
    use agentscope_core::message::GenerateReason;
    use agentscope_core::message::ThinkingBlock;
    use serde_json::json;

    fn scope() -> Scope {
        Scope::new("u", "s", "r").unwrap()
    }

    fn user(text: &str) -> Msg {
        Msg::user(text)
    }

    fn result_event(reply: Msg, source: Option<&str>) -> AgentEvent {
        let mut data = EventData::default();
        data.source = source.map(|value| value.to_string());
        data.fields
            .insert("reply".into(), serde_json::to_value(reply).unwrap());
        AgentEvent::AgentResult(data)
    }

    fn reply(reason: GenerateReason) -> Msg {
        Msg::assistant("result").with_generate_reason(reason)
    }

    fn fold(events: Vec<AgentEvent>) -> SupervisionCapture {
        let mut capture = SupervisionCapture::new(scope(), &[user("task")], 32);
        for event in &events {
            capture.event(event);
        }
        capture.complete();
        capture
    }

    #[test]
    fn task_is_bounded_and_non_text_input_truncates() {
        let mut assistant = Msg::assistant("second");
        assistant
            .content
            .push(ContentBlock::Thinking(ThinkingBlock::default()));
        let capture = SupervisionCapture::new(scope(), &[user("first"), assistant], 64);
        assert_eq!(capture.task(), "first\nsecond\n");
        assert!(capture.truncated());

        let long = "x".repeat(200);
        let capture = SupervisionCapture::new(scope(), &[user(&long)], 10);
        // Java reserves one character for the newline: when the text is longer, the retained
        // prefix is `max + 1` characters and no newline is appended.
        assert_eq!(capture.task(), "x".repeat(11));
    }

    #[test]
    fn a_completed_result_is_active_only_until_completion() {
        let capture = fold(vec![result_event(reply(GenerateReason::ModelStop), None)]);
        assert!(!capture.worker_failed());
        assert!(!capture.worker_suspended());
        assert!(!capture.worker_active());
        assert_eq!(capture.output(), "result");
    }

    #[test]
    fn suspended_results_stay_active_and_are_not_failures() {
        for reason in [
            GenerateReason::PermissionAsking,
            GenerateReason::ToolSuspended,
            GenerateReason::ReasoningStopRequested,
            GenerateReason::ActingStopRequested,
            GenerateReason::MiddlewareStopRequested,
        ] {
            let capture = fold(vec![result_event(reply(reason), None)]);
            assert!(!capture.worker_failed(), "{reason:?} must not fail");
            assert!(capture.worker_suspended(), "{reason:?} must suspend");
            assert!(capture.worker_active(), "{reason:?} must stay active");
        }
    }

    #[test]
    fn failed_and_interrupted_results_are_failures() {
        for reason in [
            GenerateReason::MaxIterations,
            GenerateReason::AllToolsDenied,
            GenerateReason::Interrupted,
        ] {
            let capture = fold(vec![result_event(reply(reason), None)]);
            assert!(capture.worker_failed(), "{reason:?} must fail");
            assert!(!capture.worker_active(), "{reason:?} must end");
        }
    }

    #[test]
    fn a_successful_continuation_after_denial_is_not_a_failure() {
        // Java regression: `ALL_TOOLS_DENIED, false, false, REVIEW_COMPLETION`.
        let capture = fold(vec![
            AgentEvent::AllToolsDenied(EventData::default()),
            AgentEvent::RequestStop(EventData::default()),
            result_event(reply(GenerateReason::ModelStop), None),
        ]);
        assert!(!capture.worker_failed());
        assert!(!capture.worker_active());
    }

    #[test]
    fn a_forwarded_child_outcome_cannot_overwrite_the_parent() {
        let mut limit = EventData::default();
        limit.source = Some("parent/child".into());
        let capture = fold(vec![
            result_event(reply(GenerateReason::ModelStop), None),
            AgentEvent::ExceedMaxIters(limit),
            result_event(reply(GenerateReason::MaxIterations), Some("parent/child")),
        ]);
        assert!(!capture.worker_failed());
        assert!(!capture.worker_active());
        // The forwarded names are still retained for the supervisor's recent-event view.
        assert_eq!(
            capture.recent_events(),
            vec!["AGENT_RESULT", "EXCEED_MAX_ITERS", "AGENT_RESULT"]
        );
    }

    #[test]
    fn an_own_iteration_limit_fails_immediately() {
        let capture = fold(vec![AgentEvent::ExceedMaxIters(EventData::default())]);
        assert!(capture.worker_failed());
    }

    #[test]
    fn thinking_events_are_dropped_and_recent_events_are_bounded() {
        let mut capture = SupervisionCapture::new(scope(), &[user("task")], 32);
        capture.event(&AgentEvent::ThinkingBlockDelta(EventData::default()));
        assert_eq!(capture.sequence(), 0);
        assert!(capture.recent_events().is_empty());
        for _ in 0..(MAX_RECENT_EVENTS + 5) {
            capture.event(&AgentEvent::ModelCallStart(EventData::default()));
        }
        assert_eq!(capture.recent_events().len(), MAX_RECENT_EVENTS);
        assert_eq!(capture.sequence(), (MAX_RECENT_EVENTS + 5) as u64);
    }

    #[test]
    fn the_output_tail_is_bounded_and_the_final_text_replaces_it() {
        let mut capture = SupervisionCapture::new(scope(), &[user("task")], 8);
        let mut delta = EventData::default();
        delta.fields.insert("delta".into(), json!("0123456789"));
        capture.event(&AgentEvent::TextBlockDelta(delta));
        assert_eq!(capture.output(), "23456789");
        assert!(capture.truncated());

        capture.event(&result_event(reply(GenerateReason::ModelStop), None));
        assert_eq!(capture.output(), "result");
    }

    #[test]
    fn the_snapshot_carries_the_classified_flags() {
        let capture = fold(vec![result_event(
            reply(GenerateReason::ToolSuspended),
            None,
        )]);
        let snapshot = capture.snapshot(SupervisionEvidence::missing());
        assert!(snapshot.worker_active);
        assert!(!snapshot.worker_failed);
        assert_eq!(snapshot.scope, scope());
        assert_eq!(snapshot.sequence, capture.sequence());
        assert!(snapshot.elapsed_millis < 60_000);
    }
}
