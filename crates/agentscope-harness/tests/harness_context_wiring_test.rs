//! Wiring test for the context compiler: material registered on `HarnessAgent::builder()`
//! must reach the provider through the *real* build path.
//!
//! A unit test that pokes `ContextPipeline` directly cannot catch a missing builder/chain
//! registration (the same class of bug as an unimplemented trait hook), so this test asserts
//! on what the model actually receives.

use std::sync::{Arc, Mutex};

use agentscope_core::agent::StreamOptions;
use agentscope_core::message::{ContentBlock as CoreBlock, Msg, TextBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_harness::agent::context::{
    ContextBlock, ContextSource, ContextSourceOptions, ContextSources,
};
use agentscope_harness::agent::HarnessAgent;

/// Records every message batch it is asked to complete.
struct RecordingModel {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl Model for RecordingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.seen
            .lock()
            .unwrap()
            .push(messages.iter().map(|m| m.text_content()).collect());
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::Text(TextBlock::new("done"))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "recording"
    }

    fn context_window_size(&self) -> usize {
        128_000
    }
}

struct FixedSource(Vec<ContextBlock>);

#[async_trait::async_trait]
impl ContextSource for FixedSource {
    async fn load(
        &self,
        _request: &agentscope_harness::agent::context::ContextRequest,
    ) -> agentscope_core::error::Result<Vec<ContextBlock>> {
        Ok(self.0.clone())
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base =
        std::env::temp_dir().join(format!("agentscope_ctx_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

#[tokio::test]
async fn registered_material_reaches_the_model_through_the_real_build_path() {
    let dir = temp_dir("wiring");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let agent = HarnessAgent::builder()
        .name("ctx")
        .model(Arc::new(RecordingModel {
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .max_iters(1)
        .instruction("change-policy", "Preserve public API behavior")
        .context_source(
            "change-record",
            Arc::new(FixedSource(vec![
                ContextBlock::runtime("status", "Change 42: export fix").unwrap(),
            ])),
            ContextSourceOptions::default(),
        )
        .build();

    let events: Vec<_> = {
        use futures::StreamExt;
        agent
            .stream_events(vec![Msg::user("fix the export")], StreamOptions::default())
            .collect()
            .await
    };
    // The run must complete without surfacing a preparation error.
    assert!(
        !events.iter().any(|event| event.is_err()),
        "agent run surfaced an error"
    );

    let batches = seen.lock().unwrap().clone();
    assert!(!batches.is_empty(), "the model was never called");
    let rendered = batches[0].join("\n");
    // The System instruction and the runtime source both reach the provider.
    assert!(
        rendered.contains("Preserve public API behavior"),
        "instruction missing: {rendered}"
    );
    assert!(
        rendered.contains("Change 42: export fix"),
        "context source missing: {rendered}"
    );
    assert!(rendered.contains("fix the export"), "{rendered}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_agent_without_registered_material_is_unchanged() {
    let dir = temp_dir("noctx");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let agent = HarnessAgent::builder()
        .name("plain")
        .model(Arc::new(RecordingModel {
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .max_iters(1)
        .build();

    {
        use futures::StreamExt;
        let _: Vec<_> = agent
            .stream_events(vec![Msg::user("hello")], StreamOptions::default())
            .collect()
            .await;
    }
    let batches = seen.lock().unwrap().clone();
    assert!(!batches.is_empty());
    // No harness instruction material was injected: the opt-in compiler stays off.
    let rendered = batches[0].join("\n");
    assert!(!rendered.contains("preserve the public API"));
    assert!(!rendered.contains("<HARNESS_CONTEXT>"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_registration_keeps_the_compiler_disabled() {
    // Guards the opt-in rule: `ContextSources::empty()` must not enable the compiler.
    assert!(ContextSources::empty().is_empty());
}
