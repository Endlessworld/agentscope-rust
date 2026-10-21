//! Tests for `ConflictPolicy` on the state checkpoint.
//!
//! A checkpoint save is a compare-and-swap: it carries the version this process last
//! observed, so a write from another process surfaces as a conflict. Java resolves that
//! conflict with `ConflictPolicy` (`OVERWRITE` / `FAIL` / `APPEND_MERGE`); these tests
//! pin the same three behaviours from the outside.

use std::sync::Arc;

use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::state::{AgentState, AgentStateStore, ConflictPolicy, InMemoryAgentStateStore};
use agentscope_core::session::SessionHistoryMode;
use agentscope_harness::agent::HarnessAgent;

/// A model that always answers with a fixed text block.
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

fn agent(
    store: Arc<dyn AgentStateStore>,
    session: &str,
    policy: ConflictPolicy,
) -> HarnessAgent {
    HarnessAgent::builder()
        .name("policy")
        .model(Arc::new(ScriptedModel))
        .session_history_mode(SessionHistoryMode::Legacy)
        .state_store(store)
        .session_id(session)
        .conflict_policy(policy)
        .max_iters(2)
        .build()
}

/// Runs one turn and returns the first error, if the run reported one.
async fn run(agent: &HarnessAgent, text: &str) -> Option<agentscope_core::error::Error> {
    use futures::StreamExt;
    let stream = agent.stream_events(vec![Msg::user(text)], StreamOptions::defaults());
    let events: Vec<_> = stream.collect().await;
    events.into_iter().find_map(|ev| ev.err())
}

/// Simulates another process advancing the slot: load, append, save.
async fn competing_write(store: &Arc<dyn AgentStateStore>, session: &str, text: &str) {
    let mut state: AgentState = store
        .get(None, session, "agent_state")
        .await
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    state.context_mut().push(Msg::user(text));
    store
        .save(None, session, "agent_state", serde_json::to_value(&state).unwrap())
        .await;
}

async fn stored_texts(store: &Arc<dyn AgentStateStore>, session: &str) -> Vec<String> {
    let state: AgentState = store
        .get(None, session, "agent_state")
        .await
        .and_then(|v| serde_json::from_value(v).ok())
        .expect("a checkpoint must exist");
    state.context().iter().map(|m| m.text_content()).collect()
}

#[tokio::test]
async fn overwrite_policy_is_the_default_and_wins_last() {
    let store: Arc<dyn AgentStateStore> = Arc::new(InMemoryAgentStateStore::new());
    let agent = agent(Arc::clone(&store), "sess-overwrite", ConflictPolicy::default());

    assert!(run(&agent, "first").await.is_none());
    competing_write(&store, "sess-overwrite", "from another process").await;

    // The conflict is resolved by overwriting: no error, and the competing write is lost.
    assert!(run(&agent, "second").await.is_none());
    let texts = stored_texts(&store, "sess-overwrite").await;
    assert!(texts.iter().any(|t| t.contains("second")), "got {texts:?}");
    assert!(
        !texts.iter().any(|t| t.contains("from another process")),
        "OVERWRITE must replace the competing write, got {texts:?}"
    );
}

#[tokio::test]
async fn fail_policy_surfaces_the_conflict_and_writes_nothing() {
    let store: Arc<dyn AgentStateStore> = Arc::new(InMemoryAgentStateStore::new());
    let agent = agent(Arc::clone(&store), "sess-fail", ConflictPolicy::Fail);

    assert!(run(&agent, "first").await.is_none());
    competing_write(&store, "sess-fail", "from another process").await;
    let before = stored_texts(&store, "sess-fail").await;

    let error = run(&agent, "second").await.expect("FAIL must surface an error");
    assert!(
        matches!(
            error,
            agentscope_core::error::Error::ConcurrentSessionModification { .. }
        ),
        "expected a conflict error, got {error:?}"
    );
    assert_eq!(
        stored_texts(&store, "sess-fail").await,
        before,
        "FAIL must not persist anything"
    );
}

#[tokio::test]
async fn append_merge_policy_keeps_both_writers() {
    let store: Arc<dyn AgentStateStore> = Arc::new(InMemoryAgentStateStore::new());
    let agent = agent(Arc::clone(&store), "sess-merge", ConflictPolicy::AppendMerge);

    assert!(run(&agent, "first").await.is_none());
    competing_write(&store, "sess-merge", "from another process").await;

    assert!(run(&agent, "second").await.is_none());
    let texts = stored_texts(&store, "sess-merge").await;
    assert!(texts.iter().any(|t| t.contains("from another process")), "got {texts:?}");
    assert!(texts.iter().any(|t| t.contains("second")), "got {texts:?}");
    // The pre-conflict history must not be duplicated by the merge.
    let firsts = texts.iter().filter(|t| t.contains("first")).count();
    assert_eq!(firsts, 1, "APPEND_MERGE must not duplicate the committed prefix: {texts:?}");
}
