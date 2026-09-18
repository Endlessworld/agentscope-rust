//! Regression test for resuming a session whose persisted history contains an
//! invalid tool-call pairing (an id answered twice, as produced by an
//! interrupted / max-iterations turn).
//!
//! Such a history used to be sent to the provider verbatim, so every follow-up
//! prompt failed with HTTP 400 "Messages with role 'tool' must be a response to a
//! preceding message with 'tool_calls'".

use std::sync::{Arc, Mutex};

use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock, ToolResultBlock, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::state::{AgentState, AgentStateStore, JsonFileAgentStateStore};
use agentscope_harness::agent::HarnessAgent;
use futures::StreamExt;

/// A model that records the message list it is called with.
struct RecordingModel {
    seen: Arc<Mutex<Vec<Vec<Msg>>>>,
}

#[async_trait::async_trait]
impl Model for RecordingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.seen.lock().unwrap().push(messages);
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }
    fn model_name(&self) -> &str {
        "recording"
    }
}

fn assistant_tool_call(id: &str) -> Msg {
    Msg::builder()
        .role(agentscope_core::message::MsgRole::Assistant)
        .content(vec![CoreBlock::ToolUse(ToolUseBlock::new(
            id,
            "read_file",
            serde_json::Map::new(),
        ))])
        .build()
}

fn tool_answer(id: &str) -> Msg {
    Msg::builder()
        .role(agentscope_core::message::MsgRole::Tool)
        .content(vec![CoreBlock::ToolResult(
            ToolResultBlock::builder()
                .id(id)
                .name("read_file")
                .output(CoreBlock::Text(TextBlock::new("ok")))
                .build(),
        )])
        .build()
}

#[tokio::test]
async fn resume_prunes_duplicated_tool_result_before_model_call() {
    let root = std::env::temp_dir().join(format!(
        "agentscope_resume_dangling_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store: Arc<dyn AgentStateStore> = Arc::new(JsonFileAgentStateStore::new(&root));

    // Persisted history of a session that ended mid-turn: the same tool-call id
    // is answered twice (see the max-iterations收尾 path).
    let broken = AgentState::builder()
        .session_id("sess-broken")
        .add_message(Msg::user("read the config"))
        .add_message(assistant_tool_call("call_1"))
        .add_message(tool_answer("call_1"))
        .add_message(tool_answer("call_1"))
        .build();
    store
        .save(
            None,
            "sess-broken",
            "agent_state",
            serde_json::to_value(&broken).unwrap(),
        )
        .await;

    let model = Arc::new(RecordingModel {
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let seen = Arc::clone(&model.seen);
    let agent = HarnessAgent::builder()
        .name("resume-dangling")
        .model(model)
        .state_store(Arc::clone(&store))
        .session_id("sess-broken")
        .build();

    let events: Vec<_> = agent
        .stream_events(vec![Msg::user("continue")], StreamOptions::defaults())
        .collect()
        .await;
    for ev in events {
        ev.expect("agent run must succeed");
    }

    let calls = seen.lock().unwrap().clone();
    assert!(!calls.is_empty(), "the model must have been called");
    let sent = &calls[0];
    let answers = sent
        .iter()
        .flat_map(|m| m.tool_result_blocks())
        .filter(|tr| tr.id.as_deref() == Some("call_1"))
        .count();
    assert_eq!(
        answers, 1,
        "a tool_call id must be answered exactly once in the request sent to the provider"
    );
    assert!(
        sent.iter().any(|m| m.text_content() == "continue"),
        "the new user turn must be sent"
    );

    let _ = std::fs::remove_dir_all(&root);
}
