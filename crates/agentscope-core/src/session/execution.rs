//! Execution-side inbox consumer, mirroring `SessionExecution`.
//!
//! Validation runs under the native execution writer lease: a command is admitted only
//! against the committed turns and pending interactions, never against caller-supplied
//! assumptions about what the session contains.

use std::sync::Arc;

use super::{
    pending_interactions, session_error, Command, SessionInbox, SessionLog, SessionRecorder,
    SessionTurns,
};
use crate::agent::RuntimeContext;
use crate::message::Msg;
use crate::state::AgentState;

/// Runtime-context key carrying the [`SessionExecution`] for this run.
pub const EXECUTION_CONTEXT_KEY: &str = "agentscope.session.execution";

/// Execution-side inbox consumer.
#[derive(Clone)]
pub struct SessionExecution {
    log: Arc<dyn SessionLog>,
    command: Command,
    inputs: Vec<Msg>,
    opened: bool,
}

impl std::fmt::Debug for SessionExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionExecution")
            .field("command", &self.command)
            .field("opened", &self.opened)
            .finish_non_exhaustive()
    }
}

impl SessionExecution {
    /// Creates an execution for one accepted command.
    pub fn new(log: Arc<dyn SessionLog>, command: Command) -> Self {
        Self {
            log,
            command,
            inputs: Vec::new(),
            opened: false,
        }
    }

    /// Returns the execution carried by a runtime context, if any.
    pub fn from_context(context: &RuntimeContext) -> Option<Arc<SessionExecution>> {
        context.extension::<SessionExecution>(EXECUTION_CONTEXT_KEY)
    }

    /// The command this execution serves.
    pub fn command(&self) -> &Command {
        &self.command
    }

    /// The input messages admitted for this run.
    pub fn inputs(&self) -> &[Msg] {
        &self.inputs
    }

    /// Admits the command against the committed prefix and opens the inbox turn.
    ///
    /// Rejects a command that already ran, a second submit while a turn is unfinished, a
    /// continuation of anything other than the latest unfinished turn, and an answer to an
    /// interaction that is no longer pending.
    pub async fn prepare(&mut self, recorder: &SessionRecorder) -> crate::error::Result<()> {
        let view = SessionTurns::read(&*self.log).await?;
        if view.started.contains(&self.command.id) {
            return Err(session_error("Command already executed"));
        }
        let latest = view.latest().cloned();
        let pending = pending_interactions(&*self.log).await?;

        match self.command.kind.as_str() {
            "submit" => {
                if latest.as_ref().is_some_and(|turn| !turn.ended()) {
                    return Err(session_error(
                        "Continue the unfinished task before starting another task",
                    ));
                }
                if !pending.is_empty() {
                    return Err(session_error("Answer pending interactions first"));
                }
                self.inputs = self.command.messages.clone();
            }
            "resume" => {
                self.require_latest_unfinished(latest.as_ref())?;
                if !pending.is_empty() {
                    return Err(session_error("Answer pending interactions first"));
                }
                // Confirmations change tool/permission state rather than becoming chat
                // messages; resolved answers must not be replayed as a blank user message.
                self.inputs = view
                    .inputs_of(&self.command.turn_id.clone().unwrap_or_default())
                    .iter()
                    .filter(|message| !message.metadata.contains_key(Msg::METADATA_CONFIRM_RESULTS))
                    .cloned()
                    .collect();
            }
            "respond" => {
                self.require_latest_unfinished(latest.as_ref())?;
                for id in &self.command.request_ids {
                    let request = pending.get(id).ok_or_else(|| {
                        session_error(format!("Interaction is no longer pending: {id}"))
                    })?;
                    let turn_matches = request.turn_id.as_deref()
                        == self.command.turn_id.as_deref()
                        && request.turn_id.is_some();
                    if !turn_matches {
                        return Err(session_error(format!(
                            "Interaction is no longer pending: {id}"
                        )));
                    }
                }
                self.inputs = self.command.messages.clone();
            }
            other => {
                return Err(session_error(format!(
                    "Command kind does not start an execution: {other}"
                )));
            }
        }

        recorder.append(
            "inbox/started",
            &serde_json::json!({
                "commandId": self.command.id,
                "kind": self.command.kind,
            }),
        )?;
        let inbox = SessionInbox::new(Arc::clone(&self.log))?;
        let turn_id = self.effective_turn_id()?;
        inbox.open(&turn_id, recorder.run_id()).await?;
        self.opened = true;
        Ok(())
    }

    fn require_latest_unfinished(&self, latest: Option<&super::Turn>) -> crate::error::Result<()> {
        let matches = latest.is_some_and(|turn| {
            !turn.ended() && Some(turn.turn_id.as_str()) == self.command.turn_id.as_deref()
        });
        if !matches {
            return Err(session_error(
                "Only the latest unfinished task can continue",
            ));
        }
        Ok(())
    }

    fn effective_turn_id(&self) -> crate::error::Result<String> {
        self.command
            .turn_id
            .clone()
            .filter(|turn| !turn.trim().is_empty())
            .ok_or_else(|| session_error("Execution command requires a turn id"))
    }

    /// Applies pending `inject`/`steer` input to the working state before a step.
    ///
    /// Messages are deduplicated by id, checkpointed first and only then marked consumed, so a
    /// retried step never loses or duplicates the input.
    pub async fn before_step(
        &self,
        recorder: &SessionRecorder,
        state: &mut AgentState,
    ) -> crate::error::Result<()> {
        let inbox = SessionInbox::new(Arc::clone(&self.log))?;
        let applied = SessionTurns::read(&*self.log).await?.applied;
        let mut known = recorder.accepted_message_ids();
        for message in &state.context {
            known.insert(message.id.clone());
        }
        let snapshot = inbox.snapshot().await?;
        let mut accepted = Vec::new();
        for command in &snapshot.commands {
            if snapshot.rejected.contains_key(&command.id)
                || applied.contains(&command.id)
                || !matches!(command.kind.as_str(), "inject" | "steer")
            {
                continue;
            }
            if command.kind == "steer"
                && command.turn_id.as_deref() != self.command.turn_id.as_deref()
            {
                continue;
            }
            for message in &command.messages {
                if known.insert(message.id.clone()) {
                    state.context.push(message.clone());
                }
            }
            accepted.push(command.id.clone());
        }
        if !accepted.is_empty() {
            recorder.checkpoint_now(state, "session_input").await?;
            recorder
                .record(
                    "inbox/applied",
                    &serde_json::json!({ "commandIds": accepted }),
                )
                .await?;
        }
        Ok(())
    }

    /// Serializes terminal admission with steering acceptance.
    ///
    /// Returns `true` when unapplied steering for this turn still exists, so the caller keeps
    /// consuming instead of closing and losing the accepted guidance.
    pub async fn continue_or_close(&mut self, recorder: &SessionRecorder) -> crate::error::Result<bool> {
        let inbox = SessionInbox::new(Arc::clone(&self.log))?;
        let applied = SessionTurns::read(&*self.log).await?.applied;
        let snapshot = inbox.snapshot().await?;
        let pending = snapshot.commands.iter().any(|command| {
            command.kind == "steer"
                && command.turn_id.as_deref() == self.command.turn_id.as_deref()
                && !applied.contains(&command.id)
        });
        if !pending {
            self.close(recorder).await?;
        }
        Ok(pending)
    }

    /// Closes the inbox turn once.
    pub async fn close(&mut self, recorder: &SessionRecorder) -> crate::error::Result<()> {
        if !self.opened {
            return Ok(());
        }
        let inbox = SessionInbox::new(Arc::clone(&self.log))?;
        let turn_id = self.effective_turn_id()?;
        inbox.close(&turn_id, recorder.run_id()).await?;
        self.opened = false;
        Ok(())
    }

    /// Command ids applied so far (for callers projecting progress).
    pub async fn applied_ids(&self) -> crate::error::Result<std::collections::BTreeSet<String>> {
        Ok(SessionTurns::read(&*self.log).await?.applied)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::message::{Msg, ToolResultBlock, ToolUseBlock};
    use crate::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionEvent,
    };

    async fn journal() -> Arc<dyn SessionLog> {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        Arc::new(JournalSessionLog::new(storage, "s1"))
    }

    async fn seed(log: &dyn SessionLog, events: Vec<SessionEvent>) {
        let writer = log.acquire("seed", Duration::from_secs(60)).await.unwrap();
        log.commit(&writer, "seed", 0, &events).await.unwrap();
        log.release(&writer).await.unwrap();
    }

    fn raw(seq: u64, kind: &str, run: Option<&str>, turn: Option<&str>, payload: String) -> SessionEvent {
        SessionEvent::new(
            format!("e{seq}"),
            seq,
            0,
            kind,
            run.map(str::to_string),
            turn.map(str::to_string),
            true,
            payload,
        )
        .unwrap()
    }

    fn submit(id: &str, turn: &str) -> Command {
        Command::new(id, "submit", Some(turn.into()), vec![Msg::user("hi")], vec![]).unwrap()
    }

    #[tokio::test]
    async fn submit_is_admitted_and_opens_the_turn() {
        let log = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), submit("c1", "t1"));
        execution.prepare(&recorder).await.unwrap();
        assert_eq!(execution.inputs().len(), 1);

        let inbox = SessionInbox::new(Arc::clone(&log)).unwrap();
        let snapshot = inbox.snapshot().await.unwrap();
        assert_eq!(snapshot.turn_id.as_deref(), Some("t1"));
        assert_eq!(snapshot.run_id.as_deref(), Some(recorder.run_id()));
        recorder.close().await;
    }

    #[tokio::test]
    async fn a_second_submit_is_rejected_while_a_turn_is_unfinished() {
        let log = journal().await;
        seed(
            &*log,
            vec![raw(1, "run/start", Some("run-0"), Some("t1"), "{}".into())],
        )
        .await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t2")).await.unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), submit("c2", "t2"));
        let err = execution.prepare(&recorder).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("Continue the unfinished task before starting another task"),
            "{err}"
        );
        recorder.close().await;
    }

    #[tokio::test]
    async fn submit_is_rejected_while_interactions_are_pending() {
        let log = journal().await;
        seed(
            &*log,
            vec![raw(
                1,
                "interaction/requested",
                Some("run-0"),
                Some("t1"),
                serde_json::json!({ "requestId": "r1", "kind": "external_execution" }).to_string(),
            )],
        )
        .await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t2")).await.unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), submit("c2", "t2"));
        let err = execution.prepare(&recorder).await.unwrap_err();
        assert!(err.to_string().contains("Answer pending interactions first"), "{err}");
        recorder.close().await;
    }

    #[tokio::test]
    async fn resume_requires_the_latest_unfinished_turn_and_drops_confirmations() {
        let log = journal().await;
        let mut confirm = Msg::user("");
        confirm
            .metadata
            .insert(Msg::METADATA_CONFIRM_RESULTS.into(), serde_json::json!([]));
        seed(
            &*log,
            vec![
                raw(1, "run/start", Some("run-0"), Some("t1"), "{}".into()),
                raw(
                    2,
                    "input/received",
                    None,
                    Some("t1"),
                    serde_json::json!({ "messages": [Msg::user("original"), confirm] }).to_string(),
                ),
                raw(3, "run/end", Some("run-0"), Some("t1"), serde_json::json!({ "status": "interrupted" }).to_string()),
            ],
        )
        .await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let command = Command::new("c-resume", "resume", Some("t1".into()), vec![], vec![]).unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), command);
        execution.prepare(&recorder).await.unwrap();
        // The original user input is replayed; the confirmation answer is not.
        assert_eq!(execution.inputs().len(), 1);
        assert_eq!(execution.inputs()[0].text_content(), "original");
        recorder.close().await;
    }

    #[tokio::test]
    async fn resume_is_rejected_for_a_finished_turn() {
        let log = journal().await;
        seed(
            &*log,
            vec![
                raw(1, "run/start", Some("run-0"), Some("t1"), "{}".into()),
                // Real terminal facts are `turn/<status>` plus `run/end`; the turn status is
                // folded from `run/end`, so a bare `turn/completed` would leave it running.
                raw(
                    2,
                    "turn/completed",
                    Some("run-0"),
                    Some("t1"),
                    serde_json::json!({ "status": "completed" }).to_string(),
                ),
                raw(
                    3,
                    "run/end",
                    Some("run-0"),
                    Some("t1"),
                    serde_json::json!({ "status": "completed" }).to_string(),
                ),
            ],
        )
        .await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t2")).await.unwrap();
        let command = Command::new("c-resume", "resume", Some("t1".into()), vec![], vec![]).unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), command);
        let err = execution.prepare(&recorder).await.unwrap_err();
        assert!(
            err.to_string().contains("Only the latest unfinished task"),
            "{err}"
        );
        recorder.close().await;
    }

    #[tokio::test]
    async fn respond_requires_the_request_to_still_be_pending() {
        let log = journal().await;
        seed(
            &*log,
            vec![
                raw(1, "run/start", Some("run-0"), Some("t1"), "{}".into()),
                raw(2, "run/end", Some("run-0"), Some("t1"), serde_json::json!({ "status": "suspended" }).to_string()),
                raw(
                    3,
                    "interaction/requested",
                    Some("run-0"),
                    Some("t1"),
                    serde_json::json!({ "requestId": "r1", "kind": "confirmation" }).to_string(),
                ),
            ],
        )
        .await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();

        let ok = Command::new("c1", "respond", Some("t1".into()), vec![Msg::user("")], vec!["r1".into()]).unwrap();
        SessionExecution::new(Arc::clone(&log), ok)
            .prepare(&recorder)
            .await
            .unwrap();

        let stale = Command::new("c2", "respond", Some("t1".into()), vec![], vec!["gone".into()]).unwrap();
        let err = SessionExecution::new(Arc::clone(&log), stale)
            .prepare(&recorder)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no longer pending"), "{err}");
        recorder.close().await;
    }

    #[tokio::test]
    async fn steer_and_inject_are_applied_once_before_a_step() {
        let log = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), submit("c1", "t1"));
        execution.prepare(&recorder).await.unwrap();

        let inbox = SessionInbox::new(Arc::clone(&log)).unwrap();
        inbox
            .accept(&Command::new("s1", "steer", Some("t1".into()), vec![Msg::user("steer")], vec![]).unwrap())
            .await
            .unwrap();
        inbox
            .accept(&Command::new("i1", "inject", None, vec![Msg::user("inject")], vec![]).unwrap())
            .await
            .unwrap();

        let mut state = crate::state::AgentState::builder().session_id("s1").build();
        execution.before_step(&recorder, &mut state).await.unwrap();
        assert_eq!(state.context.len(), 2);
        assert!(execution.applied_ids().await.unwrap().contains("s1"));

        // A second step does not duplicate the messages.
        execution.before_step(&recorder, &mut state).await.unwrap();
        assert_eq!(state.context.len(), 2);
        recorder.close().await;
    }

    #[tokio::test]
    async fn steering_for_another_turn_is_not_applied() {
        let log = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), submit("c1", "t1"));
        execution.prepare(&recorder).await.unwrap();
        let inbox = SessionInbox::new(Arc::clone(&log)).unwrap();
        inbox
            .accept(&Command::new("s9", "steer", Some("other".into()), vec![Msg::user("x")], vec![]).unwrap())
            .await
            .unwrap();
        let mut state = crate::state::AgentState::builder().session_id("s1").build();
        execution.before_step(&recorder, &mut state).await.unwrap();
        assert!(state.context.is_empty());
        recorder.close().await;
    }

    #[tokio::test]
    async fn continue_or_close_keeps_the_turn_open_for_unapplied_steering() {
        let log = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), submit("c1", "t1"));
        execution.prepare(&recorder).await.unwrap();
        let inbox = SessionInbox::new(Arc::clone(&log)).unwrap();
        inbox
            .accept(&Command::new("s1", "steer", Some("t1".into()), vec![Msg::user("more")], vec![]).unwrap())
            .await
            .unwrap();

        assert!(execution.continue_or_close(&recorder).await.unwrap());
        // Still open because the steering has not been consumed.
        assert_eq!(
            inbox.snapshot().await.unwrap().run_id.as_deref(),
            Some(recorder.run_id())
        );
        recorder.close().await;
    }

    #[tokio::test]
    async fn close_is_idempotent_and_clears_the_active_run() {
        let log = journal().await;
        let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1")).await.unwrap();
        let mut execution = SessionExecution::new(Arc::clone(&log), submit("c1", "t1"));
        execution.prepare(&recorder).await.unwrap();
        assert!(!execution.continue_or_close(&recorder).await.unwrap());
        let inbox = SessionInbox::new(Arc::clone(&log)).unwrap();
        assert!(inbox.snapshot().await.unwrap().run_id.is_none());
        execution.close(&recorder).await.unwrap();
        recorder.close().await;
    }
}
