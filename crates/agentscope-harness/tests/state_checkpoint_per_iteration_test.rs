//! Regression test for the per-iteration state checkpoint.
//!
//! The agent-level checkpoint alone only persists at turn boundaries, so an abrupt process
//! death (`kill -9`, OOM kill, power loss) — none of which can run shutdown code — would lose
//! the whole turn. `StateCheckpointMiddleware` therefore also persists after every reasoning
//! iteration, capping the loss at the in-flight iteration.
//!
//! This test asserts that behaviour from the outside: a scripted multi-iteration run must
//! produce at least one stored snapshot whose context is *shorter* than the final one.

use std::sync::{Arc, Mutex};

use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::state::AgentStateStore;
use agentscope_core::tool::Toolkit;
use agentscope_core::session::SessionHistoryMode;
use agentscope_harness::agent::HarnessAgent;
use async_trait::async_trait;
use futures::StreamExt;

/// A store that records the context length of every `agent_state` snapshot it receives.
struct RecordingStore {
    snapshots: Arc<Mutex<Vec<usize>>>,
}

impl RecordingStore {
    fn new() -> Self {
        Self {
            snapshots: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl AgentStateStore for RecordingStore {
    async fn save(&self, _u: Option<&str>, _s: &str, key: &str, value: serde_json::Value) {
        if key != "agent_state" {
            return;
        }
        let len = value
            .get("context")
            .and_then(|c| c.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        self.snapshots.lock().unwrap().push(len);
    }

    async fn save_list(&self, _u: Option<&str>, _s: &str, _k: &str, _v: &[serde_json::Value]) {}

    async fn get(&self, _u: Option<&str>, _s: &str, _k: &str) -> Option<serde_json::Value> {
        None
    }

    async fn get_list(&self, _u: Option<&str>, _s: &str, _k: &str) -> Vec<serde_json::Value> {
        Vec::new()
    }

    async fn exists(&self, _u: Option<&str>, _s: &str) -> bool {
        false
    }

    async fn delete(&self, _u: Option<&str>, _s: &str) {}

    async fn list_session_ids(&self, _u: Option<&str>) -> Vec<String> {
        Vec::new()
    }
}

/// A model that pops queued responses, falling back to a plain "done".
struct ScriptedModel {
    responses: Arc<Mutex<Vec<ChatResponse>>>,
}

#[async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let responses = Arc::clone(&self.responses);
        Box::pin(futures::stream::once(async move {
            let mut g = responses.lock().unwrap();
            let r = if g.is_empty() {
                text_response("done")
            } else {
                g.remove(0)
            };
            Ok(r)
        }))
    }

    fn model_name(&self) -> &str {
        "scripted"
    }
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![CoreBlock::Text(TextBlock::new(text))])
        .build()
}

fn tool_use_response(id: &str, name: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![CoreBlock::ToolUse(
            agentscope_core::message::ToolUseBlock::new(id, name, serde_json::Map::new()),
        )])
        .build()
}

fn noop_toolkit() -> Toolkit {
    let toolkit = Toolkit::new();
    toolkit.register_tool(Arc::new(agentscope_core::tool::FunctionTool::new(
        "noop",
        "does nothing",
        serde_json::json!({"type": "object"}),
        Box::new(|_args: serde_json::Map<String, serde_json::Value>,
                  _ctx: agentscope_core::tool::ToolExecutionContext| {
            Ok(agentscope_core::message::ToolResultBlock::text("ok"))
        }),
    )));
    toolkit
}

#[tokio::test]
async fn per_iteration_checkpoint_stores_intermediate_progress() {
    let store = Arc::new(RecordingStore::new());
    let snapshots = Arc::clone(&store.snapshots);

    // Two tool-call iterations, then a terminating text reply: three reasoning
    // iterations in one turn.
    let model = Arc::new(ScriptedModel {
        responses: Arc::new(Mutex::new(vec![
            tool_use_response("c1", "noop"),
            tool_use_response("c2", "noop"),
            text_response("finished"),
        ])),
    });

    let agent = HarnessAgent::builder()
        .name("t-per-iteration")
        .model(model)
        .toolkit(noop_toolkit())
        .session_history_mode(SessionHistoryMode::Legacy)
        .state_store(Arc::clone(&store) as Arc<dyn AgentStateStore>)
        .session_id("sess-per-iteration")
        .build();

    let events: Vec<_> = agent
        .stream_events(
            vec![Msg::user("go")],
            agentscope_core::agent::StreamOptions::defaults(),
        )
        .collect()
        .await;
    for ev in events {
        ev.expect("agent run must succeed");
    }

    let snaps = snapshots.lock().unwrap().clone();
    assert!(
        snaps.len() >= 2,
        "expected several checkpoints across the turn, got {snaps:?}"
    );
    let longest = *snaps.iter().max().unwrap();
    assert!(
        snaps.iter().any(|&n| n > 1 && n < longest),
        "expected an intermediate checkpoint holding real progress rather than only the \
         turn boundary; got {snaps:?}"
    );
}
