//! A settled tool batch is checkpointed before the next model step (P0-7).
//!
//! Before this test existed the only checkpoints around a tool call were `before_acting`
//! and `before_model`; the settled tool results reached the journal one model round later,
//! so a crash in between replayed every dispatch as uncertain even though the tools had
//! already run and returned.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentscope_core::agent::{Extensions, ReActAgent, StreamOptions};
use agentscope_core::message::{ContentBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::session::{
    AtomicSessionStorage, Command, JournalSessionLog, MemorySessionStorage, SessionExecution,
    SessionInbox, SessionLog, SessionRecorder, EXECUTION_CONTEXT_KEY, RECORDER_CONTEXT_KEY,
};
use agentscope_core::state::AgentState;
use agentscope_core::{FunctionTool, ToolResultBlock, ToolUseBlock, Toolkit};
use futures::StreamExt;

async fn journal() -> Arc<dyn SessionLog> {
    let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
    Arc::new(JournalSessionLog::new(storage, "acting-checkpoint"))
}

/// First step asks for a tool call, second step answers with text.
struct ToolThenText {
    calls: AtomicUsize,
}

impl Model for ToolThenText {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::Result<ChatResponse>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::once(async move {
            let content = if call == 0 {
                vec![ContentBlock::ToolUse(ToolUseBlock::new(
                    "call-1",
                    "echo",
                    Default::default(),
                ))]
            } else {
                vec![ContentBlock::Text(TextBlock::new("done"))]
            };
            Ok(ChatResponse::builder().content(content).build())
        }))
    }

    fn model_name(&self) -> &str {
        "tool-then-text"
    }
}

fn echo_tool() -> Toolkit {
    let toolkit = Toolkit::new();
    toolkit.register_tool(Arc::new(FunctionTool::new(
        "echo",
        "echoes",
        serde_json::json!({"type": "object"}),
        Box::new(|_, _| Ok(ToolResultBlock::text("echoed"))),
    )));
    toolkit
}

#[tokio::test]
async fn settled_tool_results_are_checkpointed_before_the_next_model_step() {
    let log = journal().await;
    let inbox = Arc::new(SessionInbox::new(Arc::clone(&log)).unwrap());
    let submit = Command::new(
        "c1",
        "submit",
        Some("t1".to_string()),
        vec![Msg::user("run the tool")],
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

    let agent = ReActAgent::builder()
        .model(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .toolkit(echo_tool())
        .todo_tool(false)
        .max_iters(4)
        .build()
        .unwrap();

    let mut extensions = Extensions::new();
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::new(recorder));
    extensions.insert(EXECUTION_CONTEXT_KEY, execution);

    let mut stream =
        agent.stream_events_with_extensions(inputs, StreamOptions::defaults(), extensions);
    while let Some(event) = stream.next().await {
        event.expect("the run must not fail");
    }

    let head = log.head().await.unwrap();
    let events = log.scan(0, head.seq).await.unwrap();
    let checkpoints: Vec<(u64, String, String)> = events
        .iter()
        .filter(|event| event.event_type == "state/checkpoint")
        .map(|event| {
            let payload: serde_json::Value = event.payload().expect("valid payload");
            (
                event.seq,
                payload["reason"].as_str().unwrap_or_default().to_string(),
                payload["stateJson"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();

    let (after_seq, _, after_state) = checkpoints
        .iter()
        .find(|(_, reason, _)| reason == "after_acting")
        .expect("the settled tool batch must be checkpointed");
    let state: AgentState = AgentState::from_json_str(after_state).unwrap();
    let tool_message = state
        .context
        .iter()
        .find(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult(_)))
        })
        .expect("the checkpoint must already contain the tool result");
    assert_eq!(tool_message.role, agentscope_core::message::MsgRole::Tool);

    // It must land before the next model step's own checkpoint, not after the turn ended.
    if let Some((before_model, _, _)) = checkpoints
        .iter()
        .filter(|(_, reason, _)| reason == "before_model")
        .max_by_key(|(seq, _, _)| *seq)
    {
        assert!(
            before_model > after_seq,
            "before_model ({before_model}) must follow after_acting ({after_seq})"
        );
    }
}
