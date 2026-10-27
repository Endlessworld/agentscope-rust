//! Wiring test for task projections: a committed `TaskContextState` must reach the provider as a
//! projected view, and the manifest must cite the revision it was compiled from.
//!
//! Asserting on `ContextPipeline` directly would pass even if the agent never registered the
//! compiler or never published the live task state, so this test drives the real
//! `HarnessAgent::build()` path and inspects what the scripted model actually receives.

use std::sync::{Arc, Mutex};

use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_harness::agent::context::{ContextManifest, ContextPolicy, TaskContextOptions};
use agentscope_harness::agent::HarnessAgent;

/// Answers from a fixed script, recording every batch of messages it was asked to complete.
struct ScriptedModel {
    responses: Mutex<Vec<ChatResponse>>,
    seen: Arc<Mutex<Vec<Vec<Msg>>>>,
}

#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.seen.lock().unwrap().push(messages);
        let mut script = self.responses.lock().unwrap();
        let response = if script.is_empty() {
            ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build()
        } else {
            script.remove(0)
        };
        Box::pin(futures::stream::once(async move { Ok(response) }))
    }

    fn model_name(&self) -> &str {
        "scripted"
    }

    fn context_window_size(&self) -> usize {
        128_000
    }
}

/// Flattens text blocks and tool-result outputs: what the provider actually reads.
fn render(messages: &[Msg]) -> String {
    fn collect(blocks: &[CoreBlock], out: &mut String) {
        for block in blocks {
            match block {
                CoreBlock::Text(text) => out.push_str(&text.text),
                CoreBlock::ToolResult(result) => collect(&result.output, out),
                _ => {}
            }
            out.push('\n');
        }
    }
    let mut out = String::new();
    for message in messages {
        collect(message.content(), &mut out);
    }
    out
}

fn todo_write_call(id: &str, status: &str) -> ChatResponse {
    let mut item = serde_json::Map::new();
    item.insert(
        "content".to_string(),
        serde_json::Value::String("step one".to_string()),
    );
    item.insert(
        "status".to_string(),
        serde_json::Value::String(status.to_string()),
    );
    let mut input = serde_json::Map::new();
    input.insert(
        "todos".to_string(),
        serde_json::Value::Array(vec![item.into()]),
    );
    ChatResponse::builder()
        .content(vec![CoreBlock::ToolUse(ToolUseBlock::new(
            id,
            "todo_write",
            input,
        ))])
        .build()
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![CoreBlock::Text(TextBlock::new(text))])
        .build()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base =
        std::env::temp_dir().join(format!("agentscope_taskctx_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn recording_policy(manifests: Arc<Mutex<Vec<ContextManifest>>>) -> ContextPolicy {
    ContextPolicy {
        observer: Arc::new(move |manifest: &ContextManifest| {
            manifests.lock().unwrap().push(manifest.clone());
        }),
        ..ContextPolicy::default()
    }
}

async fn run_scripted(agent: &HarnessAgent) {
    use futures::StreamExt;
    let events: Vec<_> = agent
        .stream_events(vec![Msg::user("plan the work")], StreamOptions::default())
        .collect()
        .await;
    assert!(
        !events.iter().any(|event| event.is_err()),
        "agent run surfaced an error: {:?}",
        events.iter().find(|event| event.is_err())
    );
}

#[tokio::test]
async fn task_projection_runs_inside_the_real_build_path() {
    let dir = temp_dir("project");
    let seen: Arc<Mutex<Vec<Vec<Msg>>>> = Arc::new(Mutex::new(Vec::new()));
    let manifests = Arc::new(Mutex::new(Vec::new()));
    let agent = HarnessAgent::builder()
        .name("tasks")
        .model(Arc::new(ScriptedModel {
            responses: Mutex::new(vec![
                todo_write_call("t1", "in_progress"),
                todo_write_call("t2", "completed"),
                text_response("done"),
            ]),
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .max_iters(4)
        // Nothing else is registered: the projection alone must opt the agent into the compiler.
        .task_context(TaskContextOptions {
            requirements: true,
            verification_results: false,
            requirement_proposals: false,
        })
        .context_policy(recording_policy(Arc::clone(&manifests)))
        .build();

    run_scripted(&agent).await;

    // Auxiliary (memory) calls also compile, so only the reasoning manifests describe the
    // requests whose task revision the model was actually shown.
    let reasoning: Vec<ContextManifest> = manifests
        .lock()
        .unwrap()
        .iter()
        .filter(|manifest| manifest.purpose == "REASONING")
        .cloned()
        .collect();
    let revisions: Vec<u64> = reasoning.iter().map(|m| m.task_revision).collect();
    // Revision 0 for the first request, then one per committed todo_write.
    assert_eq!(
        revisions,
        vec![0, 1, 2],
        "manifest revisions: {revisions:?}"
    );
    for manifest in reasoning.iter().skip(1) {
        assert!(
            manifest
                .transforms
                .iter()
                .any(|t| t == "task_state_projection"),
            "projection transform missing: {:?}",
            manifest.transforms
        );
    }

    // The stale receipt is rewritten for the provider: only the latest revision stays `full`.
    let rendered: Vec<String> = seen.lock().unwrap().iter().map(|m| render(m)).collect();
    let joined = rendered.join("\n=====\n");
    assert!(
        joined.contains("Todo update recorded at revision 1"),
        "stale receipt was not downgraded: {joined}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn task_projection_stays_off_without_opting_in() {
    let dir = temp_dir("nooptin");
    let seen: Arc<Mutex<Vec<Vec<Msg>>>> = Arc::new(Mutex::new(Vec::new()));
    let manifests = Arc::new(Mutex::new(Vec::new()));
    let agent = HarnessAgent::builder()
        .name("plain-tasks")
        .model(Arc::new(ScriptedModel {
            responses: Mutex::new(vec![
                todo_write_call("t1", "in_progress"),
                text_response("done"),
            ]),
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .max_iters(3)
        .context_policy(recording_policy(Arc::clone(&manifests)))
        .build();

    run_scripted(&agent).await;

    assert!(
        manifests.lock().unwrap().is_empty(),
        "the compiler must stay opt-in"
    );
    let rendered: Vec<String> = seen.lock().unwrap().iter().map(|m| render(m)).collect();
    assert!(!rendered.is_empty(), "the model was never called");
    let joined = rendered.join("\n");
    assert!(
        !joined.contains("Todo update recorded at revision"),
        "unexpected projection without opt-in: {joined}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A tool result whose `id` has no matching `tool_use`: only the sanitiser can repair this.
fn orphan_tool_result(id: &str, text: &str) -> Msg {
    let result = agentscope_core::message::ToolResultBlock::builder()
        .id(id)
        .name("ls")
        .output(CoreBlock::Text(TextBlock::new(text)))
        .build();
    Msg::builder()
        .role(agentscope_core::message::MsgRole::Tool)
        .content(vec![CoreBlock::ToolResult(result)])
        .build()
}

#[tokio::test]
async fn the_compiler_accepts_input_the_sanitizer_repaired() {
    // The history carries an orphan tool result, which `ToolCallPairSanitizerMiddleware` strips
    // at the reasoning extension point. The compiler must therefore validate the repaired view —
    // if it saw the raw history instead it would fail with `invalid_tool_pairs`.
    let dir = temp_dir("sanitized");
    let seen: Arc<Mutex<Vec<Vec<Msg>>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = HarnessAgent::builder()
        .name("sanitized")
        .model(Arc::new(ScriptedModel {
            responses: Mutex::new(vec![text_response("done")]),
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .max_iters(1)
        .task_context(TaskContextOptions {
            requirements: true,
            verification_results: false,
            requirement_proposals: false,
        })
        .build();

    use futures::StreamExt;
    let events: Vec<_> = agent
        .stream_events(
            vec![Msg::user("hi"), orphan_tool_result("ghost", "ghost output")],
            StreamOptions::default(),
        )
        .collect()
        .await;
    assert!(
        !events.iter().any(|event| event.is_err()),
        "the compiler must not reject a request the sanitiser repairs: {:?}",
        events.iter().find(|event| event.is_err())
    );

    let batches = seen.lock().unwrap().clone();
    assert!(!batches.is_empty(), "the model was never called");
    // Auxiliary (memory) calls flatten the transcript into text, so assert on the block: the
    // orphan result must never reach a request as a `tool_result`.
    let leaked = batches.iter().flatten().any(|message| {
        message
            .tool_result_blocks()
            .any(|result| result.id.as_deref() == Some("ghost"))
    });
    assert!(!leaked, "the orphan tool result reached the model");

    let _ = std::fs::remove_dir_all(&dir);
}
