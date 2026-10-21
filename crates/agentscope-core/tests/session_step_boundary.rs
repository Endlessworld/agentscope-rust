//! Step-boundary coverage for the session execution runtime.
//!
//! Java opens a step only for a real reasoning iteration (`ReActAgent.reasoning` checks
//! `maxIters` *before* `recorder.step`), and folds accepted guidance in at that same boundary.
//! These tests drive the real `ReActAgent` loop with a recorder attached so the ordering is
//! exercised in production wiring, not in isolation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_core::agent::{Extensions, ReActAgent, StreamOptions};
use agentscope_core::message::{ContentBlock, Msg, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::session::{
    AtomicSessionStorage, Command, JournalSessionLog, MemorySessionStorage, SessionExecution,
    SessionInbox, SessionLog, SessionRecorder, EXECUTION_CONTEXT_KEY, RECORDER_CONTEXT_KEY,
};
use agentscope_core::state::AgentState;
use futures::StreamExt;

async fn journal() -> Arc<dyn SessionLog> {
    let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
    Arc::new(JournalSessionLog::new(storage, "step-boundary"))
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

fn text_reply(text: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![ContentBlock::Text(
            agentscope_core::message::TextBlock::new(text),
        )])
        .build()
}

/// Always asks for one more `todo_write`, so the loop only stops on `max_iters`.
struct ToolLoopModel {
    calls: AtomicUsize,
}

impl Model for ToolLoopModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::Result<ChatResponse>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut item = serde_json::Map::new();
        item.insert(
            "content".to_string(),
            serde_json::Value::String("step one".to_string()),
        );
        item.insert(
            "status".to_string(),
            serde_json::Value::String("in_progress".to_string()),
        );
        let mut input = serde_json::Map::new();
        input.insert(
            "todos".to_string(),
            serde_json::Value::Array(vec![item.into()]),
        );
        let response = ChatResponse::builder()
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                format!("t{call}"),
                "todo_write",
                input,
            ))])
            .build();
        Box::pin(futures::stream::once(async move { Ok(response) }))
    }

    fn model_name(&self) -> &str {
        "tool-loop"
    }
}

struct QuietModel {
    calls: AtomicUsize,
    seen: Mutex<Vec<Vec<Msg>>>,
}

impl Model for QuietModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::Result<ChatResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(messages);
        Box::pin(futures::stream::once(async move { Ok(text_reply("done")) }))
    }

    fn model_name(&self) -> &str {
        "quiet"
    }
}

async fn submit_turn(log: &Arc<dyn SessionLog>) -> SessionInbox {
    let inbox = SessionInbox::new(Arc::clone(log)).unwrap();
    let submit = Command::new(
        "c1",
        "submit",
        Some("t1".to_string()),
        vec![Msg::user("start")],
        Vec::new(),
    )
    .unwrap();
    inbox.accept(&submit).await.unwrap();
    inbox
}

async fn drain(agent: &ReActAgent, inputs: Vec<Msg>, extensions: Extensions) {
    let mut stream =
        agent.stream_events_with_extensions(inputs, StreamOptions::defaults(), extensions);
    while let Some(event) = stream.next().await {
        event.expect("the session runtime must not fail the run");
    }
}

fn count(events: &[agentscope_core::session::SessionEvent], kind: &str) -> usize {
    events
        .iter()
        .filter(|event| event.event_type == kind)
        .count()
}

#[tokio::test]
async fn every_reasoning_iteration_opens_one_step_and_summarizing_opens_none() {
    let log = journal().await;
    let inbox = submit_turn(&log).await;
    let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
        .await
        .unwrap();
    let mut execution = SessionExecution::new(
        Arc::clone(&log),
        inbox.snapshot().await.unwrap().commands[0].clone(),
    );
    execution.prepare(&recorder).await.unwrap();
    let inputs = execution.inputs().to_vec();

    let mut state = AgentState::default();
    for message in &inputs {
        state.context_mut().push(message.clone());
    }
    recorder.start(&inputs, &state).await.unwrap();

    let agent = ReActAgent::builder()
        .model(Arc::new(ToolLoopModel {
            calls: AtomicUsize::new(0),
        }))
        .max_iters(2)
        .build()
        .unwrap();

    let mut extensions = Extensions::new();
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::new(recorder));
    extensions.insert(EXECUTION_CONTEXT_KEY, execution);
    drain(&agent, inputs, extensions).await;

    let events = log.read_after(0, 1024).await.unwrap();
    // Iterations 0 and 1 are steps; iteration 2 only summarizes, so it must open none.
    assert_eq!(
        count(&events, "step/start"),
        2,
        "one step per reasoning iteration"
    );
    assert_eq!(
        count(&events, "step/end"),
        1,
        "each new step closes the previous one"
    );
}

#[tokio::test]
async fn guidance_accepted_before_the_run_is_folded_into_the_first_step() {
    let log = journal().await;
    let inbox = submit_turn(&log).await;
    // Accepted while the turn is still queued: the settle loop cannot see it, so the first
    // `before_step` is the only place that can fold it in.
    let steer = Command::new(
        "c2",
        "steer",
        Some("t1".to_string()),
        vec![Msg::user("guidance")],
        Vec::new(),
    )
    .unwrap();
    inbox.accept(&steer).await.unwrap();

    let recorder = SessionRecorder::open(Arc::clone(&log), Some("t1"))
        .await
        .unwrap();
    let mut execution = SessionExecution::new(
        Arc::clone(&log),
        inbox.snapshot().await.unwrap().commands[0].clone(),
    );
    execution.prepare(&recorder).await.unwrap();
    let inputs = execution.inputs().to_vec();

    let mut state = AgentState::default();
    for message in &inputs {
        state.context_mut().push(message.clone());
    }
    recorder.start(&inputs, &state).await.unwrap();

    let model = Arc::new(QuietModel {
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
    drain(&agent, inputs, extensions).await;

    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "one step, one model call"
    );
    let seen = model.seen.lock().unwrap();
    let first = seen.first().expect("first model call");
    assert!(
        first
            .iter()
            .any(|message| text_of(message).contains("guidance")),
        "pre-run guidance must reach the first step"
    );

    let events = log.read_after(0, 1024).await.unwrap();
    let applied = events
        .iter()
        .filter(|event| event.event_type == "inbox/applied")
        .count();
    assert_eq!(applied, 1, "guidance is applied exactly once");
}
