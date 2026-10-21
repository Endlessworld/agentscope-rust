//! End-to-end tests for the state-checkpoint wiring: the full `AgentState`
//! must be persisted to a [`JsonFileAgentStateStore`] after each run and
//! restored on the first run of a restarted agent, resuming the conversation.
use std::sync::Arc;

use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::state::{AgentStateStore, JsonFileAgentStateStore};
use agentscope_core::Agent;
use agentscope_core::session::SessionHistoryMode;
use agentscope_harness::agent::HarnessAgent;

/// A scripted model that always answers "done".
struct ScriptedModel;

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }
    fn model_name(&self) -> &str {
        "scripted"
    }
}

async fn drain(agent: &HarnessAgent, text: &str) {
    use futures::StreamExt;
    let stream = agent.stream_events(vec![Msg::user(text)], StreamOptions::defaults());
    let events: Vec<_> = stream.collect().await;
    for ev in events {
        ev.expect("agent run must succeed");
    }
}

async fn temp_store_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_checkpoint_{tag}_{}",
        std::process::id()
    ));
    let _ = tokio::fs::remove_dir_all(&base).await;
    tokio::fs::create_dir_all(&base).await.unwrap();
    base
}

#[tokio::test]
async fn checkpoint_persists_and_resumes_across_restart() {
    let root = temp_store_dir("resume").await;
    let store = JsonFileAgentStateStore::new(&root);
    let store_arc: Arc<dyn AgentStateStore> = Arc::new(store);

    // ---- "First run": the agent snapshots its state after the prompt. ----
    let agent1 = HarnessAgent::builder()
        .name("ckpt")
        .model(Arc::new(ScriptedModel))
        .session_history_mode(SessionHistoryMode::Legacy)
        .state_store(Arc::clone(&store_arc))
        .session_id("sess-ckpt")
        .user_id("alice")
        .max_iters(3)
        .build();
    drain(&agent1, "hello run 1").await;

    // The state was written to the store, keyed by (alice, sess-ckpt).
    assert!(
        store_arc.exists(Some("alice"), "sess-ckpt").await,
        "checkpoint must exist after the first run"
    );
    let stored: agentscope_core::state::AgentState = serde_json::from_value(
        store_arc
            .get(Some("alice"), "sess-ckpt", "agent_state")
            .await
            .expect("stored state"),
    )
    .expect("valid AgentState");
    assert!(
        stored.context().len() >= 2,
        "history must include user message + assistant reply, got {} messages",
        stored.context().len()
    );

    // ---- "Restart": a brand-new agent (fresh process state) restores the
    // checkpoint on its first run and continues the conversation. ----
    let agent2 = HarnessAgent::builder()
        .name("ckpt")
        .model(Arc::new(ScriptedModel))
        .session_history_mode(SessionHistoryMode::Legacy)
        .state_store(Arc::clone(&store_arc))
        .session_id("sess-ckpt")
        .user_id("alice")
        .max_iters(3)
        .build();

    // Before the first run the fresh agent has an empty context.
    assert_eq!(agent2.agent().unwrap().agent_state().context().len(), 0);

    drain(&agent2, "hello run 2").await;

    // The restored history (run 1) plus the new turn (run 2) are both present.
    let st = agent2.agent().unwrap().agent_state();
    let ctx = st.context();
    let texts: Vec<String> = ctx.iter().map(|m| m.text_content()).collect();
    let joined = texts.join("\n");
    assert!(
        joined.contains("hello run 1"),
        "run-1 user message must be restored, got: {joined:?}"
    );
    assert!(
        joined.contains("hello run 2"),
        "run-2 user message must be appended, got: {joined:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn checkpoint_saves_after_every_run() {
    let root = temp_store_dir("multi").await;
    let store: Arc<dyn AgentStateStore> = Arc::new(JsonFileAgentStateStore::new(&root));

    let agent = HarnessAgent::builder()
        .name("ckpt-multi")
        .model(Arc::new(ScriptedModel))
        .session_history_mode(SessionHistoryMode::Legacy)
        .state_store(Arc::clone(&store))
        .session_id("sess-multi")
        .max_iters(3)
        .build();

    drain(&agent, "first").await;
    let after_first: agentscope_core::state::AgentState = serde_json::from_value(
        store
            .get(None, "sess-multi", "agent_state")
            .await
            .expect("stored state after first run"),
    )
    .unwrap();
    let n_first = after_first.context().len();

    drain(&agent, "second").await;
    let after_second: agentscope_core::state::AgentState = serde_json::from_value(
        store
            .get(None, "sess-multi", "agent_state")
            .await
            .expect("stored state after second run"),
    )
    .unwrap();
    let n_second = after_second.context().len();

    assert!(
        n_second > n_first,
        "checkpoint must be overwritten with the growing history: {n_first} -> {n_second}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
