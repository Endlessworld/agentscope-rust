//! An offloaded tool call must not leave the journal with an unresolved dispatch.
//!
//! `AsyncToolMiddleware` answers an offloaded call with a placeholder while the real work
//! continues in the background. The journal, however, only saw the `tool/dispatch` fact:
//! the placeholder lived in the acting results handle only, so every later open of the
//! session demanded `Explicit reconciliation required for unknown tool outcomes` for a call
//! whose outcome was in fact deferred, not unknown. The middleware now records the deferral
//! (a suspended `tool/result`) at the moment it offloads.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::agent::{Extensions, StreamOptions};
use agentscope_core::journal::{
    AtomicJournalStorage, AtomicThreadJournal, InvocationRecorder, JournalProjection,
    MemoryJournalStorage, ThreadJournal, RECORDER_CONTEXT_KEY,
};
use agentscope_core::message::{
    ContentBlock as CoreBlock, JsonMap, Msg, TextBlock, ToolResultBlock, ToolResultState,
    ToolUseBlock,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::{Tool, ToolExecutionContext, Toolkit};
use agentscope_harness::agent::HarnessAgent;
use futures::StreamExt;

/// Answers from a fixed script; the second call returns text so the turn can finish.
struct ScriptedModel {
    responses: Mutex<Vec<ChatResponse>>,
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        let mut script = self.responses.lock().unwrap();
        let response = if script.is_empty() {
            text_response("done")
        } else {
            script.remove(0)
        };
        Box::pin(futures::stream::once(async move { Ok(response) }))
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

fn slow_tool_call(id: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![CoreBlock::ToolUse(ToolUseBlock::new(
            id,
            "slow_tool",
            JsonMap::new(),
        ))])
        .build()
}

/// Sleeping tool: long enough to always exceed the configured offload timeout.
#[derive(Debug)]
struct SlowTool;

#[async_trait::async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow_tool"
    }
    fn description(&self) -> &str {
        "sleeps for a while"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn call(
        &self,
        _args: JsonMap,
        _ctx: ToolExecutionContext,
    ) -> agentscope_core::error::Result<ToolResultBlock> {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(ToolResultBlock::builder()
            .id("call")
            .name("slow_tool")
            .output(CoreBlock::Text(TextBlock::new("slow result text")))
            .build())
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "agentscope_async_tool_journal_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

async fn journal() -> Arc<dyn ThreadJournal> {
    let storage: Arc<dyn AtomicJournalStorage> = Arc::new(MemoryJournalStorage::new());
    Arc::new(AtomicThreadJournal::new(storage, "async-tool-journal"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offloaded_call_records_a_deferred_outcome_in_the_journal() {
    let dir = temp_dir("offload");
    let toolkit = Toolkit::default();
    toolkit.register_tool(Arc::new(SlowTool));

    let agent = HarnessAgent::builder()
        .name("async-journal")
        .model(Arc::new(ScriptedModel {
            responses: Mutex::new(vec![slow_tool_call("c1"), text_response("done")]),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .toolkit(toolkit)
        .disable_web_tools()
        .max_iters(4)
        .async_tool_timeout(Duration::from_millis(100))
        .build();

    let log = journal().await;
    let recorder = Arc::new(
        InvocationRecorder::open(Arc::clone(&log), Some("t1"))
            .await
            .unwrap(),
    );
    let mut extensions = Extensions::new();
    extensions.insert_arc(RECORDER_CONTEXT_KEY, Arc::clone(&recorder));

    let events: Vec<_> = agent
        .stream_events_with_extensions(
            vec![Msg::user("run the slow tool")],
            StreamOptions::default(),
            extensions,
        )
        .collect()
        .await;
    assert!(
        !events.iter().any(|event| event.is_err()),
        "agent run surfaced an error: {:?}",
        events.iter().find(|event| event.is_err())
    );

    let projection = JournalProjection::read(&*log).await.unwrap();
    assert!(
        projection.uncertain_tool_calls.is_empty(),
        "an offloaded call must not stay uncertain: {:?}",
        projection.uncertain_tool_calls
    );

    // The deferral itself is durable: a suspended `tool/result` for the offloaded call.
    let head = log.head().await.unwrap();
    let facts = log.scan(0, head.seq).await.unwrap();
    let types: Vec<&str> = facts.iter().map(|fact| fact.event_type.as_str()).collect();
    assert!(
        types.contains(&"tool/dispatch"),
        "the dispatch fact is the premise of this test: {types:?}"
    );
    let deferred = facts
        .iter()
        .filter(|fact| fact.event_type == "tool/result")
        .filter_map(|fact| fact.data().ok())
        .any(|data| {
            data.get("message")
                .and_then(|value| serde_json::from_value::<Msg>(value.clone()).ok())
                .map(|message| {
                    message.tool_result_blocks().any(|result| {
                        result.id.as_deref() == Some("c1")
                            && result.state == ToolResultState::Suspended
                    })
                })
                .unwrap_or(false)
        });
    assert!(
        deferred,
        "the offload must record a suspended tool/result for the call: {types:?}"
    );

    // A later open must succeed instead of demanding explicit reconciliation.
    recorder.close().await;
    let reopened = InvocationRecorder::open(Arc::clone(&log), Some("t2")).await;
    assert!(
        reopened.is_ok(),
        "reopening the session must not demand reconciliation: {:?}",
        reopened.err()
    );
    if let Ok(recorder) = reopened {
        recorder.close().await;
    }

    // Give the background execution time to finish before the temp dir is removed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = std::fs::remove_dir_all(&dir);
}
