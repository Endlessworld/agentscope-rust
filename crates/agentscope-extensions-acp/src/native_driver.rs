//! The driver that runs a session without `agentscope-durable`.
//!
//! Same trait as [`crate::session::DurableDriver`], no journal, no leases: a turn is one
//! `HarnessAgent::stream_events` run whose events go through the shared
//! [`crate::session::render_event`], and the transcript it produces is appended to
//! [`SessionHistory`]. `replay`, `resume` and `view` read that history back.
//!
//! It is not selected at runtime yet - the registry still holds a `DurableDriver` - so it is
//! marked `allow(dead_code)` until the switch lands.

#![allow(dead_code)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use agent_client_protocol::v1::schema::{MessageId, SessionId, StopReason};
use agentscope_core::agent::StreamOptions;
use agentscope_core::event::AgentEventType;
use agentscope_core::message::{ChatUsage, Msg};
use agentscope_core::model::DEFAULT_CONTEXT_WINDOW;
use agentscope_harness::agent::HarnessAgent;
use async_trait::async_trait;
use futures::StreamExt;

use crate::event::ToolEventMapper;
use crate::session::{render_event, RenderCtx};
use crate::session_history::{SessionHistory, TurnRecord};
use crate::turn_driver::{
    DriverError, DriverInput, EventSink, Guarantees, ReplayItem, ResumeOutcome, TurnDriver,
    TurnOutcome, TurnStatusView, TurnView,
};

/// A session driven by its own files instead of the durable runtime.
pub(crate) struct NativeDriver {
    /// `None` in a build (or a test) that only replays a history.
    agent: Option<Arc<HarnessAgent>>,
    session_id: SessionId,
    /// Where the history lives. Opened per call rather than up front: a session is built while
    /// the protocol is still answering `session/new`, and a cwd that cannot be written should
    /// fail the prompt that needs it, not the announcement of the session.
    root: std::path::PathBuf,
    /// How many transcript messages the agent has already been given. A restarted process
    /// starts at zero, so a fresh agent receives the whole history; a second turn in one
    /// process only sends what the agent has not seen.
    delivered: std::sync::Mutex<usize>,
}

impl NativeDriver {
    /// Builds the driver for `<root>/acp-sessions/<session_id>/`.
    pub(crate) fn open(
        root: &Path,
        session_id: SessionId,
        agent: Option<Arc<HarnessAgent>>,
    ) -> Self {
        Self {
            agent,
            session_id,
            root: root.to_path_buf(),
            delivered: std::sync::Mutex::new(0),
        }
    }

    fn history(&self) -> std::result::Result<SessionHistory, DriverError> {
        SessionHistory::open(&self.root, &self.session_id.to_string()).map_err(DriverError::Failed)
    }

    fn status_of(name: &str) -> TurnStatusView {
        match name {
            "queued" => TurnStatusView::Queued,
            "running" => TurnStatusView::Running,
            "waiting" => TurnStatusView::Waiting,
            "completed" => TurnStatusView::Completed,
            "failed" => TurnStatusView::Failed,
            "cancelled" => TurnStatusView::Cancelled,
            "interrupted" => TurnStatusView::Interrupted,
            _ => TurnStatusView::Idle,
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// The name a stop reason is remembered under, so a restart does not depend on a code name.
fn stop_reason_name(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Cancelled => "cancelled",
        StopReason::Refusal => "refusal",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        _ => "end_turn",
    }
}

/// Resolves when the peer cancels the prompt's JSON-RPC request; never otherwise.
async fn wait_for_cancel(cancellation: Option<&agent_client_protocol::RequestCancellation>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Resolves when a cancel advances the session's generation past this turn's.
async fn wait_for_epoch(epoch: &AtomicU64, admitted: u64) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if epoch.load(Ordering::SeqCst) != admitted {
            return;
        }
    }
}

/// Resolves when the session is torn down (`session/close`, eviction, shutdown).
async fn wait_for_close(closed: &AtomicBool) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if closed.load(Ordering::SeqCst) {
            return;
        }
    }
}

#[async_trait]
impl TurnDriver for NativeDriver {
    async fn run(
        &self,
        input: DriverInput,
        sink: EventSink,
    ) -> std::result::Result<TurnOutcome, DriverError> {
        // A wakeup carries no message: the durable driver uses one to advance a turn an earlier
        // run left open. A native run writes its turn as it goes, so there is never one open -
        // and nothing to advance, which is why this needs no agent either.
        let Some(message) = input.message else {
            return Ok(TurnOutcome {
                stop_reason: StopReason::EndTurn,
                usage: None,
            });
        };
        let Some(agent) = &self.agent else {
            return Err(DriverError::Unavailable(
                "this native driver has no agent to run".to_string(),
            ));
        };
        let caps = input.caps;
        let started_at_ms = now_ms();
        let turn_id = format!("turn-{started_at_ms}");
        let running = TurnRecord {
            turn_id: turn_id.clone(),
            status: TurnStatusView::Running.as_str().to_string(),
            started_at_ms,
            updated_at_ms: started_at_ms,
            stop_reason: None,
        };
        self.history()?
            .record_turn(running)
            .map_err(DriverError::Failed)?;
        self.history()?
            .append_message(&message)
            .map_err(DriverError::Failed)?;

        // The agent holds no memory of a previous process: the transcript is what a restarted
        // session has, so the messages this agent has not been given yet go with the prompt.
        // Within one process the agent already carries them, and resending would duplicate the
        // conversation (the durable driver restores a checkpoint and does not resend either).
        let transcript = self.history()?.messages().map_err(DriverError::Failed)?;
        // Scoped so the guard is dropped before the first await: a `std::sync::MutexGuard` is
        // not `Send`, and this future has to be.
        let unseen = {
            let mut delivered = self
                .delivered
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let unseen = transcript.len().saturating_sub(*delivered);
            *delivered = transcript.len();
            unseen
        };
        let mut conversation: Vec<Msg> = transcript[transcript.len() - unseen..].to_vec();
        if conversation.is_empty() {
            conversation.push(message.clone());
        }

        // One stable message id per turn: every chunk and tool call shares it, so a client
        // groups them into one assistant message. This is the id the durable driver uses too.
        let message_id = MessageId::new(format!("turn-{started_at_ms}"));
        let mut tool_events = ToolEventMapper::new()
            .with_message_id(Some(message_id.clone()))
            .with_plan_operations(caps.supports_plan_operations);
        let context_window = agent
            .agent()
            .map(|agent| agent.model().context_window_size() as u64)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW as u64);

        let mut stream = agent.stream_events(conversation, StreamOptions::defaults());
        let mut resumes = 0usize;
        let mut turn_usage = ChatUsage::default();
        let mut emitted_text = false;
        let mut reply: Option<Msg> = None;
        let mut failure: Option<String> = None;
        let mut cancelled = false;
        loop {
            tokio::select! {
                item = stream.next() => match item {
                    Some(Ok(agent_event)) => {
                        if agent_event.event_type() == AgentEventType::AgentResult {
                            // A run the host interrupted is continued, not ended: the turn is
                            // still valid and discarding the prompt would lose the user's input.
                            // The durable driver resumes the stored turn; here the agent's own
                            // context is the state, so a fresh stream continues it.
                            if crate::event::result_generate_reason(&agent_event)
                                == Some(agentscope_core::message::GenerateReason::Interrupted)
                            {
                                resumes += 1;
                                if resumes > crate::session::MAX_INTERRUPTED_RESUMES {
                                    failure = Some(
                                        "the run kept being interrupted before it finished"
                                            .to_string(),
                                    );
                                    break;
                                }
                                sink(crate::event::message_chunk(
                                    self.session_id.clone(),
                                    "\n\n[run interrupted; continuing]\n\n".to_string(),
                                    Some(message_id.clone()),
                                ));
                                stream =
                                    agent.stream_events(Vec::new(), StreamOptions::defaults());
                                continue;
                            }
                            if let Some(parsed) = agent_event
                                .data()
                                .fields
                                .get("reply")
                                .and_then(|value| serde_json::from_value::<Msg>(value.clone()).ok())
                            {
                                reply = Some(parsed);
                            }
                        }
                        let mut render = RenderCtx {
                            session_id: &self.session_id,
                            message_id: Some(message_id.clone()),
                            caps: &caps,
                            context_window,
                            emitted_text,
                            tool_events: &mut tool_events,
                        };
                        render_event(
                            &mut render,
                            &agent_event,
                            &mut turn_usage,
                            &mut |notification| sink(notification),
                        );
                        emitted_text = render.emitted_text;
                    }
                    Some(Err(error)) => {
                        failure = Some(error.to_string());
                        break;
                    }
                    None => break,
                },
                _ = wait_for_cancel(input.cancellation.as_ref()) => {
                    cancelled = true;
                    break;
                }
                _ = wait_for_epoch(&input.cancel_epoch, input.admitted_epoch) => {
                    cancelled = true;
                    break;
                }
                _ = wait_for_close(&input.closed) => {
                    cancelled = true;
                    break;
                }
            }
        }

        if cancelled {
            // A cancelled parent must not leave its children running: they keep billing with
            // nobody left to receive their result, and the next prompt finds them "running".
            let session_id = self.session_id.to_string();
            let context =
                agentscope_core::agent::RuntimeContext::new().with_session_id(session_id.clone());
            agent
                .task_repository()
                .cancel_session_tasks(&context, &session_id)
                .await;
        }
        if let Some(error) = &failure {
            sink(crate::event::message_chunk(
                self.session_id.clone(),
                crate::session::error_chunk_text(error),
                Some(message_id.clone()),
            ));
        }
        let status = if cancelled {
            TurnStatusView::Cancelled
        } else if failure.is_some() {
            TurnStatusView::Failed
        } else {
            TurnStatusView::Completed
        };
        let stop_reason = match status {
            TurnStatusView::Cancelled => StopReason::Cancelled,
            TurnStatusView::Failed => StopReason::Refusal,
            _ => StopReason::EndTurn,
        };
        if let Some(reply) = &reply {
            self.history()?
                .append_message(reply)
                .map_err(DriverError::Failed)?;
        }
        self.history()?
            .record_turn(TurnRecord {
                turn_id,
                status: status.as_str().to_string(),
                started_at_ms,
                updated_at_ms: now_ms(),
                stop_reason: Some(stop_reason_name(&stop_reason).to_string()),
            })
            .map_err(DriverError::Failed)?;
        Ok(TurnOutcome {
            stop_reason,
            usage: Some(turn_usage),
        })
    }

    async fn cancel(&self, _turn_id: &str) -> std::result::Result<(), DriverError> {
        Err(DriverError::Unavailable(
            "a native run is stopped through the generation its prompt was admitted with"
                .to_string(),
        ))
    }

    async fn view(&self) -> std::result::Result<TurnView, DriverError> {
        let turns = self.history()?.turns().map_err(DriverError::Failed)?;
        let Some(last) = turns.last() else {
            return Ok(TurnView::default());
        };
        Ok(TurnView {
            status: Self::status_of(&last.status),
            waiting_for: None,
            usage: None,
            stop_reason: None,
        })
    }

    async fn replay(&self) -> std::result::Result<Vec<ReplayItem>, DriverError> {
        let messages = self.history()?.messages().map_err(DriverError::Failed)?;
        Ok(messages.into_iter().map(ReplayItem::Message).collect())
    }

    async fn resume(&self) -> std::result::Result<ResumeOutcome, DriverError> {
        let mut resumed = 0usize;
        for turn in self.history()?.turns().map_err(DriverError::Failed)? {
            // A turn that already reads as interrupted was put there by an earlier resume; a
            // terminal one is finished. Everything else was cut short by the process ending.
            if turn.terminal() || turn.status == TurnStatusView::Interrupted.as_str() {
                continue;
            }
            let mut interrupted = turn;
            interrupted.status = TurnStatusView::Interrupted.as_str().to_string();
            interrupted.updated_at_ms = now_ms();
            self.history()?
                .record_turn(interrupted)
                .map_err(DriverError::Failed)?;
            resumed += 1;
        }
        Ok(ResumeOutcome {
            resumed_turns: resumed,
            reconciled_tools: 0,
        })
    }

    fn guarantees(&self) -> Guarantees {
        Guarantees::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-native-driver-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn driver(root: &Path, id: &str) -> NativeDriver {
        NativeDriver::open(root, SessionId::new(id.to_string()), None)
    }

    #[tokio::test]
    async fn a_replay_returns_the_transcript_in_order() {
        let root = scratch("replay");
        let driver = driver(&root, "s1");
        driver
            .history()
            .expect("open")
            .append_message(&Msg::user("hello"))
            .expect("append");
        driver
            .history()
            .expect("open")
            .append_message(&Msg::assistant("hi"))
            .expect("append");
        let items = driver.replay().await.expect("replay");
        assert_eq!(items.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn resume_marks_an_unfinished_turn_interrupted_once() {
        let root = scratch("resume");
        let driver = driver(&root, "s1");
        driver
            .history()
            .expect("open")
            .record_turn(TurnRecord {
                turn_id: "t1".to_string(),
                status: "running".to_string(),
                started_at_ms: 1,
                updated_at_ms: 1,
                stop_reason: None,
            })
            .expect("record");
        let first = driver.resume().await.expect("resume");
        assert_eq!(first.resumed_turns, 1);
        let second = driver.resume().await.expect("resume");
        assert_eq!(
            second.resumed_turns, 0,
            "an interrupted turn is not rewritten"
        );
        let view = driver.view().await.expect("view");
        assert_eq!(view.status, TurnStatusView::Interrupted);
        assert!(
            !view.status.terminal(),
            "an interrupted turn may be resumed"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn view_of_a_session_that_never_ran_is_idle() {
        let root = scratch("idle");
        let driver = driver(&root, "s1");
        let view = driver.view().await.expect("view");
        assert_eq!(view.status, TurnStatusView::Idle);
        let guarantees = driver.guarantees();
        assert!(
            !guarantees.cross_process && !guarantees.crash_atomic && !guarantees.multi_host,
            "the native driver promises no physical property"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_run_without_an_agent_says_so() {
        let root = scratch("noagent");
        let driver = driver(&root, "s1");
        let sink: EventSink = Arc::new(|_| {});
        let error = driver
            .run(
                DriverInput {
                    message: Some(Msg::user("hello")),
                    wakeup: false,
                    cancel_epoch: Arc::new(AtomicU64::new(0)),
                    admitted_epoch: 0,
                    closed: Arc::new(AtomicBool::new(false)),
                    cancellation: None,
                    pause_requested: Arc::new(AtomicBool::new(false)),
                    caps: crate::turn_driver::PromptCaps {
                        supports_notices: false,
                        supports_plan_operations: false,
                        supports_compaction: false,
                        last_input_tokens: Arc::new(AtomicU64::new(0)),
                    },
                },
                sink,
            )
            .await
            .expect_err("no agent");
        assert!(matches!(error, DriverError::Unavailable(_)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_wakeup_with_no_message_is_a_no_op() {
        let root = scratch("wakeup");
        let driver = driver(&root, "s1");
        let sink: EventSink = Arc::new(|_| {});
        let outcome = driver
            .run(
                DriverInput {
                    message: None,
                    wakeup: true,
                    cancel_epoch: Arc::new(AtomicU64::new(0)),
                    admitted_epoch: 0,
                    closed: Arc::new(AtomicBool::new(false)),
                    cancellation: None,
                    pause_requested: Arc::new(AtomicBool::new(false)),
                    caps: crate::turn_driver::PromptCaps {
                        supports_notices: false,
                        supports_plan_operations: false,
                        supports_compaction: false,
                        last_input_tokens: Arc::new(AtomicU64::new(0)),
                    },
                },
                sink,
            )
            .await
            .expect("a wakeup returns without running");
        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert!(outcome.usage.is_none(), "a no-op turn reports no usage");
        let _ = std::fs::remove_dir_all(root);
    }
}
