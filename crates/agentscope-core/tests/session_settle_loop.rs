//! Settle-loop coverage for the session execution runtime.
//!
//! A completed result does not end the turn while accepted guidance is still unconsumed
//! (`SessionExecution::continue_or_close`, mirroring Java `settleSession`). These tests drive
//! the real `ReActAgent` loop with a recorder and an execution attached, so the production
//! wiring is exercised rather than the method in isolation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::{Extensions, ReActAgent, StreamOptions};
use agentscope_core::message::{ContentBlock, Msg};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::session::{
    AtomicSessionStorage, Command, JournalSessionLog, MemorySessionStorage, SessionExecution,
    SessionInbox, SessionLog, SessionRecorder, EXECUTION_CONTEXT_KEY, RECORDER_CONTEXT_KEY,
};
use agentscope_core::state::AgentState;
use futures::StreamExt;

async fn journal() -> Arc<dyn SessionLog> {
    let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
    Arc::new(JournalSessionLog::new(storage, "settle-loop"))
}

fn text_reply(text: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![ContentBlock::Text(
            agentscope_core::message::TextBlock::new(text),
        )])
        .build()
}

/// Accepts a steering command while the first model call is running.
///
/// Guidance that arrives *during* a step cannot be applied by that step, so it must leave the
/// turn open for one more iteration. Accepting it before the run would be folded in by the
/// first `before_step` and would never exercise the settle loop.
struct SteeringModel {
    inbox: Arc<SessionInbox>,
    steer: Command,
    calls: AtomicUsize,
    seen: Mutex<Vec<Vec<Msg>>>,
}

impl Model for SteeringModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::Result<ChatResponse>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(messages);
        let inbox = Arc::clone(&self.inbox);
        let steer = self.steer.clone();
        Box::pin(futures::stream::once(async move {
            if call == 0 {
                inbox
                    .accept(&steer)
                    .await
                    .map_err(|error| agentscope_core::Error::Agent(error.to_string()))?;
            }
            Ok(text_reply(if call == 0 { "first" } else { "second" }))
        }))
    }

    fn model_name(&self) -> &str {
        "steering"
    }
}

fn text_of(message: &Msg) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_completed_turn_continues_while_steering_is_unconsumed() {
    let log = journal().await;
    let inbox = Arc::new(SessionInbox::new(Arc::clone(&log)).unwrap());

    let submit = Command::new(
        "c1",
        "submit",
        Some("t1".to_string()),
        vec![Msg::user("start")],
        Vec::new(),
    )
    .unwrap();
    let submit = inbox.accept(&submit).await.unwrap();

    let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
        .await
        .unwrap();
    let mut execution = SessionExecution::new(Arc::clone(&log), submit);
    execution.prepare(&recorder).await.unwrap();
    let inputs = execution.inputs().to_vec();

    let mut state = AgentState::default();
    for message in &inputs {
        state.context_mut().push(message.clone());
    }
    recorder.start(&inputs, &state).await.unwrap();

    let steer = Command::new(
        "c2",
        "steer",
        Some("t1".to_string()),
        vec![Msg::user("guidance")],
        Vec::new(),
    )
    .unwrap();
    let model = Arc::new(SteeringModel {
        inbox: Arc::clone(&inbox),
        steer,
        calls: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });

    let agent = ReActAgent::builder()
        .model(model.clone())
        .todo_tool(false)
        .build()
        .unwrap();

    let mut extensions = Extensions::new();
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::new(recorder));
    extensions.insert(EXECUTION_CONTEXT_KEY, execution);

    let mut stream =
        agent.stream_events_with_extensions(inputs, StreamOptions::defaults(), extensions);
    while let Some(event) = stream.next().await {
        event.expect("the settle loop must not fail the run");
    }

    // Two steps: the first completed without consuming the guidance accepted during it.
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        2,
        "a completed turn with unconsumed steering must run one more step"
    );
    let seen = model.seen.lock().unwrap();
    let second = seen.get(1).expect("second model call");
    assert!(
        second
            .iter()
            .any(|message| text_of(message).contains("guidance")),
        "the steered message must reach the continued step"
    );

    // The inbox turn closes once nothing is left to consume.
    let snapshot = SessionInbox::new(Arc::clone(&log))
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    assert!(
        snapshot.turn_id.is_none(),
        "the inbox turn must close after the settle loop"
    );
}

#[tokio::test]
async fn a_completed_turn_closes_when_no_guidance_is_pending() {
    let log = journal().await;
    let inbox = Arc::new(SessionInbox::new(Arc::clone(&log)).unwrap());
    let submit = Command::new(
        "c1",
        "submit",
        Some("t1".to_string()),
        vec![Msg::user("start")],
        Vec::new(),
    )
    .unwrap();
    let submit = inbox.accept(&submit).await.unwrap();

    let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
        .await
        .unwrap();
    let mut execution = SessionExecution::new(Arc::clone(&log), submit);
    execution.prepare(&recorder).await.unwrap();
    let inputs = execution.inputs().to_vec();

    let mut state = AgentState::default();
    for message in &inputs {
        state.context_mut().push(message.clone());
    }
    recorder.start(&inputs, &state).await.unwrap();

    let quiet = QuietModel {
        calls: AtomicUsize::new(0),
    };

    let agent = ReActAgent::builder()
        .model(Arc::new(quiet))
        .todo_tool(false)
        .build()
        .unwrap();

    let mut extensions = Extensions::new();
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::new(recorder));
    extensions.insert(EXECUTION_CONTEXT_KEY, execution);

    let mut stream =
        agent.stream_events_with_extensions(inputs, StreamOptions::defaults(), extensions);
    while let Some(event) = stream.next().await {
        event.expect("no guidance, no failure");
    }

    let calls = agent.model();
    assert!(
        calls.model_name() == "quiet",
        "the agent must keep the configured model"
    );
    let snapshot = SessionInbox::new(Arc::clone(&log))
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    assert!(
        snapshot.turn_id.is_none(),
        "the inbox turn must close when nothing is pending"
    );
}

struct QuietModel {
    calls: AtomicUsize,
}

impl Model for QuietModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::Result<ChatResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::once(async { Ok(text_reply("done")) }))
    }

    fn model_name(&self) -> &str {
        "quiet"
    }
}
