//! Client-side terminal-result extraction, mirroring the Java A2A client's
//! `io.agentscope.core.a2a.agent.event` handlers.
//!
//! Java resolves the reply of one remote call inside `TaskEventHandler` (a full `Task` payload)
//! and `TaskUpdateEventHandler` (a `TaskStatusUpdateEvent` plus the artifacts accumulated in
//! `ClientEventContext`). Both handlers apply the same priority rules, which this module keeps
//! as pure functions so they are testable without a network or a running agent:
//!
//! * a **completed** task with non-empty artifacts answers with those artifacts, so a status
//!   message such as `done` never hides the real output;
//! * a completed task without artifacts answers with its final status message;
//! * a **failed / canceled / rejected** task answers with its status message (partial artifacts
//!   never win) — for status updates Java synthesizes `A2A task ended with state: <state>`.

use agentscope_core::message::Msg;

use crate::message::{MessageConstants, MessageConvert};
use crate::model::{Artifact, Part, StreamEvent, Task, TaskState, TaskStatusUpdateEvent};

/// Whether a task carries output artifacts, mirroring Java's
/// `task.artifacts() != null && !task.artifacts().isEmpty()`.
fn has_artifacts(artifacts: &[Artifact]) -> bool {
    !artifacts.is_empty()
}

/// Terminal reply of a `Task` payload, mirroring Java `TaskEventHandler#handle`.
///
/// Returns `None` while the task is not terminal: the caller keeps waiting for more events.
pub fn task_result_message(task: &Task, agent_name: &str) -> Option<Msg> {
    let status = &task.status;
    if !status.state.is_terminal() {
        return None;
    }
    let artifacts = task.artifacts.as_deref().unwrap_or_default();
    if status.state == TaskState::Completed && has_artifacts(artifacts) {
        return Some(MessageConvert::convert_from_artifacts(
            artifacts, agent_name,
        ));
    }
    if let Some(message) = &status.message {
        return Some(MessageConvert::convert_from_message(message, agent_name));
    }
    Some(MessageConvert::convert_from_artifacts(
        artifacts, agent_name,
    ))
}

/// Terminal reply of a `TaskStatusUpdateEvent`, mirroring Java
/// `TaskUpdateEventHandler.TaskStatusUpdateEventHandler#handle`.
///
/// `artifacts` are the artifacts accumulated so far (Java reads `context.getTask().artifacts()`).
/// Returns `None` while the update is not final.
pub fn status_update_result_message(
    update: &TaskStatusUpdateEvent,
    artifacts: &[Artifact],
    agent_name: &str,
) -> Option<Msg> {
    if !update.is_final {
        return None;
    }
    let state = update.status.state;
    if state != TaskState::Completed {
        // Java: "A2A task ended with state: " + state [+ ", message: " + status text].
        let mut text = format!("A2A task ended with state: {}", state.as_str());
        if let Some(message) = &update.status.message {
            let converted = MessageConvert::convert_from_message(message, agent_name);
            text.push_str(&format!(", message: {}", converted.text_content()));
        }
        return Some(Msg::builder().text_content(text).build());
    }
    if !has_artifacts(artifacts) {
        if let Some(message) = &update.status.message {
            return Some(MessageConvert::convert_from_message(message, agent_name));
        }
    }
    Some(MessageConvert::convert_from_artifacts(
        artifacts, agent_name,
    ))
}

/// Folds `message/stream` events into the terminal reply of one call.
///
/// Mirrors what the Java client keeps in `ClientEventContext` (`setTask` on every task event)
/// plus the result rules of the two handlers. The first terminal result wins, matching Java's
/// `context.complete(msg)` which ignores duplicate terminal events.
#[derive(Debug, Default)]
pub struct TaskResultAccumulator {
    artifacts: Vec<Artifact>,
    result: Option<Msg>,
}

impl TaskResultAccumulator {
    /// Creates an empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one event and returns the terminal reply once it is known.
    pub fn observe(&mut self, event: &StreamEvent, agent_name: &str) -> Option<Msg> {
        if self.result.is_some() {
            return self.result.clone();
        }
        let candidate = match event {
            StreamEvent::Task(task) => {
                if let Some(artifacts) = &task.artifacts {
                    self.artifacts = artifacts.clone();
                }
                task_result_message(task, agent_name)
            }
            StreamEvent::ArtifactUpdate(update) => {
                self.merge_artifact(update);
                None
            }
            StreamEvent::StatusUpdate(update) => {
                status_update_result_message(update, &self.artifacts, agent_name)
            }
            StreamEvent::Message(_) => None,
        };
        if candidate.is_some() {
            self.result = candidate.clone();
        }
        candidate
    }

    /// The artifacts seen so far.
    pub fn artifacts(&self) -> &[Artifact] {
        &self.artifacts
    }

    /// Folds one artifact update into the accumulated artifacts.
    ///
    /// An update with `append: true` carries more parts of an artifact that was already seen
    /// (A2A `TaskUpdater#addArtifact` semantics), so its parts are merged instead of starting a
    /// second artifact. A text part that continues the same streamed block (same
    /// `_agentscope_msg_id`) has its text appended, so chunked output stays one block instead of
    /// becoming one block per chunk.
    fn merge_artifact(&mut self, update: &crate::model::TaskArtifactUpdateEvent) {
        if update.append == Some(true) {
            if let Some(existing) = self
                .artifacts
                .iter_mut()
                .find(|artifact| artifact.artifact_id == update.artifact.artifact_id)
            {
                for part in &update.artifact.parts {
                    let continued = match (existing.parts.last_mut(), part) {
                        (Some(Part::Text(last)), Part::Text(next)) => {
                            let same_block = MessageConstants::metadata_value(
                                last.metadata.as_ref(),
                                MessageConstants::MSG_ID_METADATA_KEY,
                            ) == MessageConstants::metadata_value(
                                next.metadata.as_ref(),
                                MessageConstants::MSG_ID_METADATA_KEY,
                            );
                            if same_block {
                                last.text.push_str(&next.text);
                            }
                            same_block
                        }
                        _ => false,
                    };
                    if !continued {
                        existing.parts.push(part.clone());
                    }
                }
                return;
            }
        }
        self.artifacts.push(update.artifact.clone());
    }

    /// The terminal reply, when one was produced.
    pub fn result(&self) -> Option<&Msg> {
        self.result.as_ref()
    }

    /// Consumes the accumulator and yields the terminal reply.
    pub fn finish(self) -> Option<Msg> {
        self.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, MessageRole, Part, TaskStatus, TaskStatusUpdateEvent};

    fn status_message(text: &str) -> Message {
        Message::new("m-status", MessageRole::Agent, vec![Part::text(text)])
    }

    fn completed_task(artifacts: Option<Vec<Artifact>>, message: Option<&str>) -> Task {
        let mut status = TaskStatus::new(TaskState::Completed);
        status.message = message.map(status_message);
        let mut task = Task::new("t-1", status);
        task.artifacts = artifacts;
        task
    }

    fn final_update(state: TaskState, message: Option<&str>) -> TaskStatusUpdateEvent {
        let mut status = TaskStatus::new(state);
        status.message = message.map(status_message);
        let mut update = TaskStatusUpdateEvent::new("t-1", status, true);
        update.task_id = "t-1".to_string();
        update
    }

    // Java: A2aAgentTest#call prefers the artifacts of a completed task over its status message
    // (6e1ee68d7).
    #[test]
    fn completed_task_prefers_artifacts_over_the_status_message() {
        let task = completed_task(
            Some(vec![Artifact::new(
                "art-1",
                vec![Part::text("real answer")],
            )]),
            Some("done"),
        );
        let msg = task_result_message(&task, "remote").expect("terminal task yields a reply");
        assert_eq!(msg.text_content(), "real answer");
    }

    // Java: fall back to the status message when a completed task has no artifacts.
    #[test]
    fn completed_task_falls_back_to_the_status_message() {
        let task = completed_task(None, Some("done"));
        let msg = task_result_message(&task, "remote").expect("terminal task yields a reply");
        assert_eq!(msg.text_content(), "done");
    }

    // Java: failed terminal states keep the status message ahead of partial artifacts.
    #[test]
    fn failed_task_keeps_the_status_message_ahead_of_partial_artifacts() {
        let mut status = TaskStatus::new(TaskState::Failed);
        status.message = Some(status_message("provider exploded"));
        let mut task = Task::new("t-1", status);
        task.artifacts = Some(vec![Artifact::new("art-1", vec![Part::text("partial")])]);

        let msg = task_result_message(&task, "remote").expect("terminal task yields a reply");
        assert_eq!(msg.text_content(), "provider exploded");
    }

    #[test]
    fn non_terminal_tasks_have_no_reply_yet() {
        let task = Task::new("t-1", TaskStatus::new(TaskState::Working));
        assert!(task_result_message(&task, "remote").is_none());
    }

    // Java: 00e4a66b6 — a completed status update without artifacts keeps its status message.
    #[test]
    fn final_update_keeps_the_completed_status_message_without_artifacts() {
        let update = final_update(TaskState::Completed, Some("the answer"));
        let msg = status_update_result_message(&update, &[], "remote")
            .expect("final update yields a reply");
        assert_eq!(msg.text_content(), "the answer");
    }

    // Java: artifacts still win for a completed update that carries both.
    #[test]
    fn final_update_prefers_accumulated_artifacts() {
        let update = final_update(TaskState::Completed, Some("done"));
        let artifacts = vec![Artifact::new("art-1", vec![Part::text("real answer")])];
        let msg = status_update_result_message(&update, &artifacts, "remote")
            .expect("final update yields a reply");
        assert_eq!(msg.text_content(), "real answer");
    }

    // Java: a completed update with neither artifacts nor a message completes with an empty reply.
    #[test]
    fn final_update_without_any_output_yields_an_empty_reply() {
        let update = final_update(TaskState::Completed, None);
        let msg = status_update_result_message(&update, &[], "remote")
            .expect("final update yields a reply");
        assert_eq!(msg.text_content(), "");
    }

    // Java: non-completed terminal updates synthesize the state error text, never the artifacts.
    #[test]
    fn final_failed_update_reports_state_and_status_message() {
        let update = final_update(TaskState::Failed, Some("boom"));
        let artifacts = vec![Artifact::new("art-1", vec![Part::text("partial")])];
        let msg = status_update_result_message(&update, &artifacts, "remote")
            .expect("final update yields a reply");
        assert_eq!(
            msg.text_content(),
            "A2A task ended with state: failed, message: boom"
        );
    }

    #[test]
    fn non_final_updates_have_no_reply_yet() {
        let mut status = TaskStatus::new(TaskState::Working);
        status.message = Some(status_message("working"));
        let update = TaskStatusUpdateEvent::new("t-1", status, false);
        assert!(status_update_result_message(&update, &[], "remote").is_none());
    }

    // Java: TaskUpdateEventHandler accumulates `TaskArtifactUpdateEvent`s before the final update.
    #[test]
    fn streamed_events_accumulate_artifacts_before_the_terminal_update() {
        use crate::model::TaskArtifactUpdateEvent;

        let mut accumulator = TaskResultAccumulator::new();
        let artifact = TaskArtifactUpdateEvent::new(
            "t-1",
            Artifact::new("art-1", vec![Part::text("streamed answer")]),
        );
        assert!(accumulator
            .observe(&StreamEvent::ArtifactUpdate(artifact), "remote")
            .is_none());
        assert_eq!(accumulator.artifacts().len(), 1);

        let done = accumulator.observe(
            &StreamEvent::StatusUpdate(final_update(TaskState::Completed, Some("done"))),
            "remote",
        );
        let msg = done.expect("the final update yields the reply");
        assert_eq!(msg.text_content(), "streamed answer");
        // The first terminal result wins, mirroring `context.complete(msg)`.
        let duplicate = accumulator.observe(
            &StreamEvent::StatusUpdate(final_update(TaskState::Completed, Some("again"))),
            "remote",
        );
        assert_eq!(duplicate.unwrap().text_content(), "streamed answer");
        assert_eq!(
            accumulator.finish().unwrap().text_content(),
            "streamed answer"
        );
    }

    #[test]
    fn a_task_event_also_feeds_the_terminal_reply() {
        let mut accumulator = TaskResultAccumulator::new();
        let task = completed_task(
            Some(vec![Artifact::new(
                "art-1",
                vec![Part::text("task answer")],
            )]),
            Some("done"),
        );
        let msg = accumulator
            .observe(&StreamEvent::Task(task), "remote")
            .expect("a terminal task yields the reply");
        assert_eq!(msg.text_content(), "task answer");
    }

    // Java: an `append: true` artifact update extends the artifact it belongs to.
    #[test]
    fn appended_artifact_chunks_extend_the_same_artifact() {
        use crate::model::TaskArtifactUpdateEvent;

        let mut accumulator = TaskResultAccumulator::new();
        for chunk in ["Hel", "lo"] {
            let mut update = TaskArtifactUpdateEvent::new(
                "t-1",
                Artifact::new("art-1", vec![Part::text(chunk)]),
            );
            update.append = Some(true);
            assert!(accumulator
                .observe(&StreamEvent::ArtifactUpdate(update), "remote")
                .is_none());
        }
        assert_eq!(accumulator.artifacts().len(), 1);

        let msg = accumulator
            .observe(
                &StreamEvent::StatusUpdate(final_update(TaskState::Completed, None)),
                "remote",
            )
            .expect("the final update yields the reply");
        assert_eq!(msg.text_content(), "Hello");
    }
}
